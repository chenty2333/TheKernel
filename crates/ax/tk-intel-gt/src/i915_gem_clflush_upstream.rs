// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Copyright © 2019 Intel Corporation.
//
//! Linux v7.2.3 i915 GEM clflush and fence-work translations.  Cache flushing
//! uses the target x86 CLFLUSH/CLFLUSHOPT instructions; fence callbacks and
//! reservation entries have real references and run through the native
//! LinuxKPI workqueue rather than being completed inline or discarded.

#![allow(unsafe_code, non_camel_case_types, non_snake_case)]

use alloc::{
    alloc::{alloc, dealloc},
    vec::Vec,
};
use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::{ManuallyDrop, offset_of, size_of},
    ptr,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    i915_gem_object_api_upstream::{
        i915_gem_object_get, i915_gem_object_has_pages, i915_gem_object_put,
        i915_gem_object_unpin_pages,
    },
    i915_gem_object_header_upstream::assert_object_held,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_CACHE_COHERENT_FOR_READ, Page},
    i915_gem_object_upstream::i915_gem_object_has_struct_page,
    i915_gem_pages_upstream::{__i915_gem_object_get_pages, Scatterlist},
    i915_request_upstream::DmaFenceOps,
    i915_sw_fence_upstream::{
        __i915_sw_fence_await_dma_fence, I915SwDmaFenceCb, i915_sw_fence_await_reservation,
        i915_sw_fence_commit, i915_sw_fence_fini, i915_sw_fence_init,
    },
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify},
    intel_context_upstream::{DmaFence, DmaFenceCb, DmaFenceLock, DmaFenceTimestamp, RcuHead},
    intel_engine_cs_upstream::{ListHead, Spinlock, WorkStruct},
    linux::{
        fields::{
            i915_gem_object_cache_coherent, i915_gem_object_cache_dirty,
            i915_gem_object_set_cache_dirty,
        },
        gem::DmaResv,
        highmem::page_address,
        i915::{IS_DGFX, to_i915},
        list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty, list_splice_init},
        locks::{spin_lock_init, spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
        memory::{__GFP_RETRY_MAYFAIL, kfree, kref_init},
        primitives::ktime_get,
        requests::{
            DMA_FENCE_FLAG_SIGNALED_BIT, DMA_FENCE_FLAG_TIMESTAMP_BIT,
            dma_fence_get as requests_dma_fence_get, dma_fence_put as requests_dma_fence_put,
        },
        workqueue::{self, INIT_WORK_C},
    },
    linux_config::{__GFP_NOWARN, DMA_RESV_USAGE_KERNEL, GFP_KERNEL, HZ, PAGE_SHIFT, PAGE_SIZE},
};

const I915_CLFLUSH_FORCE: u32 = 1 << 0;
const I915_CLFLUSH_SYNC: u32 = 1 << 1;
const I915_GEM_DOMAIN_CPU: u32 = 1;
const ORIGIN_CPU: u32 = 0;
const DMA_FENCE_FLAG_USER_BITS: u32 = 6;
const DMA_FENCE_WORK_IMM: u32 = DMA_FENCE_FLAG_USER_BITS;
const ENOMEM: c_int = 12;
const EINVAL: c_int = 22;
const ENOENT: c_int = 2;
const ERESTARTSYS: c_int = 512;
const I915_FENCE_GFP: u32 = GFP_KERNEL | __GFP_RETRY_MAYFAIL | __GFP_NOWARN;
/// Linux DRM cache API used by imported GEM code.  On x86 this selects the
/// optional unordered CLFLUSHOPT instruction only when CPUID advertises it;
/// otherwise it uses CLFLUSH, with full fences bracketing the range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_clflush_virt_range(address: *mut c_void, length: c_ulong) {
    if length == 0 {
        return;
    }
    let Some(caps) = axhal::cache::CacheFlushCaps::discover() else {
        axlog::error!("i915 GEM cache flush refused: CPU has no usable CLFLUSH instruction");
        return;
    };
    let _ = unsafe { caps.flush_range(address as usize, length as usize) };
}

/// Linux DRM scatter-gather cache flush. Each SG segment is physically
/// contiguous by construction; `page_address()` returns its direct mapping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_clflush_sg(table: *mut crate::intel_context_upstream::SgTable) {
    if table.is_null() {
        return;
    }
    let Some(caps) = axhal::cache::CacheFlushCaps::discover() else {
        axlog::error!("i915 GEM cache flush refused: CPU has no usable CLFLUSH instruction");
        return;
    };
    caps.barrier();
    let mut sg = unsafe { (*table).sgl };
    let entries = unsafe { (*table).orig_nents };
    for _ in 0..entries {
        if sg.is_null() {
            break;
        }
        let page = unsafe { crate::i915_gem_pages_upstream::sg_page(sg) };
        let length = unsafe { (*sg).length as usize };
        let offset = unsafe { (*sg).offset as usize };
        if !page.is_null() && length != 0 {
            let address = unsafe { page_address(page) }.cast::<u8>();
            let _ = unsafe { caps.flush_range_unfenced(address as usize + offset, length) };
        }
        sg = unsafe { crate::i915_gem_pages_upstream::sg_next(sg) };
    }
    caps.barrier();
}

/// Linux `struct dma_fence_work_ops` from i915_sw_fence_work.h.
#[repr(C)]
pub struct DmaFenceWorkOps {
    pub name: *const c_char,
    pub work: Option<unsafe extern "C" fn(*mut DmaFenceWork)>,
    pub release: Option<unsafe extern "C" fn(*mut DmaFenceWork)>,
}

unsafe impl Sync for DmaFenceWorkOps {}

/// Linux `struct dma_fence_work` from i915_sw_fence_work.h.
#[repr(C)]
pub struct DmaFenceWork {
    pub dma: DmaFence,
    pub lock: Spinlock,
    pub chain: I915SwFence,
    pub cb: I915SwDmaFenceCb,
    pub work: WorkStruct,
    pub ops: *const DmaFenceWorkOps,
}

#[repr(C)]
struct Clflush {
    base: DmaFenceWork,
    obj: *mut DrmI915GemObject,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DmaResvIter {
    obj: *mut DmaResv,
    usage: u32,
    fence: *mut DmaFence,
    fence_usage: u32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}

#[repr(C)]
struct ReservedFence {
    fence: *mut DmaFence,
    usage: u32,
}

struct ReservationState {
    entries: spin::Mutex<Vec<ReservedFence>>,
}

static DRIVER_NAME: &[u8] = b"dma-fence\0";
static WORK_NAME: &[u8] = b"work\0";
static CLFLUSH_NAME: &[u8] = b"clflush\0";

const _: [(); 48] = [(); size_of::<DmaResvIter>()];
const _: [(); 64] = [(); size_of::<DmaFence>()];

unsafe fn alloc_object<T>(value: T) -> *mut T {
    let layout = core::alloc::Layout::new::<T>();
    let object = unsafe { alloc(layout) }.cast::<T>();
    if object.is_null() {
        return ptr::null_mut();
    }
    unsafe { object.write(value) };
    object
}

unsafe fn free_object<T>(object: *mut T) {
    if object.is_null() {
        return;
    }
    unsafe {
        ptr::drop_in_place(object);
        dealloc(object.cast(), core::alloc::Layout::new::<T>());
    }
}

unsafe fn reservation_state(
    resv: *mut DmaResv,
    create: bool,
) -> Result<*mut ReservationState, c_int> {
    if resv.is_null() {
        return Err(-EINVAL);
    }
    let slot = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
    };
    let current = slot.load(Ordering::Acquire);
    if !current.is_null() || !create {
        return Ok(current);
    }
    let state = unsafe {
        alloc_object(ReservationState {
            entries: spin::Mutex::new(Vec::new()),
        })
    };
    if state.is_null() {
        return Err(-ENOMEM);
    }
    match slot.compare_exchange(ptr::null_mut(), state, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(state),
        Err(existing) => {
            unsafe { free_object(state) };
            Ok(existing)
        }
    }
}

/// Reserve capacity in the native reservation list. The backing allocation is
/// real and all later fence insertion paths retain a reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_reserve_fences(resv: *mut DmaResv, count: u32) -> c_int {
    let state = match unsafe { reservation_state(resv, true) } {
        Ok(state) => state,
        Err(error) => return error,
    };
    match unsafe { (*state).entries.lock().try_reserve(count as usize) } {
        Ok(()) => 0,
        Err(_) => -ENOMEM,
    }
}

/// Insert a fence into the reservation and retain its ownership reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_add_fence(resv: *mut DmaResv, fence: *mut DmaFence, usage: u32) {
    assert!(!fence.is_null(), "dma_resv_add_fence received a null fence");
    let state = unsafe { reservation_state(resv, true) }
        .unwrap_or_else(|error| panic!("dma_resv_add_fence failed: {error}"));
    let mut entries = unsafe { (*state).entries.lock() };
    if entries.len() == entries.capacity() {
        entries
            .try_reserve(1)
            .expect("dma_resv_add_fence requires successful prior reservation");
    }
    requests_dma_fence_get(fence);
    entries.push(ReservedFence { fence, usage });
}

/// Iterate reservation fences with Linux's usage ordering and a live cursor
/// reference, released by the caller after iteration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_iter_next(cursor: *mut DmaResvIter) -> *mut DmaFence {
    if cursor.is_null() {
        return ptr::null_mut();
    }
    let state = match unsafe { reservation_state((*cursor).obj, false) } {
        Ok(state) if !state.is_null() => state,
        _ => return ptr::null_mut(),
    };
    let entries = unsafe { (*state).entries.lock() };
    let usage = unsafe { (*cursor).usage };
    let start = unsafe { (*cursor).index as usize };
    let next = entries
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, entry)| entry.usage <= usage);
    let Some((index, entry)) = next else {
        return ptr::null_mut();
    };
    let new_fence = requests_dma_fence_get(entry.fence);
    let old_fence = unsafe { (*cursor).fence };
    if !old_fence.is_null() {
        unsafe { requests_dma_fence_put(old_fence) };
    }
    unsafe {
        (*cursor).fence = new_fence;
        (*cursor).fence_usage = entry.usage;
        (*cursor).index = index.saturating_add(1) as u32;
        (*cursor).fences = state.cast();
        (*cursor).num_fences = entries.len() as u32;
        (*cursor).is_restarted = false;
    }
    new_fence
}

/// Tear down the reservation sidecar and release all retained fence refs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_fini(resv: *mut DmaResv) {
    if resv.is_null() {
        return;
    }
    let state = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
            .swap(ptr::null_mut(), Ordering::AcqRel)
    };
    if state.is_null() {
        return;
    }
    for entry in unsafe { (*state).entries.lock() }.iter() {
        unsafe { requests_dma_fence_put(entry.fence) };
    }
    unsafe { free_object(state) };
}

unsafe fn fence_lock(fence: *mut DmaFence, flags: &mut c_ulong) {
    let lock = unsafe { (*fence).lock.extern_lock };
    assert!(!lock.is_null(), "dma fence has no lock");
    unsafe { spin_lock_irqsave_raw(lock, flags) };
}

unsafe fn fence_unlock(fence: *mut DmaFence, flags: c_ulong) {
    let lock = unsafe { (*fence).lock.extern_lock };
    unsafe { spin_unlock_irqrestore_raw(lock, flags) };
}

/// Linux DMA-fence initializer used by request and fence-work records.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_init(
    fence: *mut DmaFence,
    ops: *const DmaFenceOps,
    lock: *mut Spinlock,
    context: u64,
    seqno: u64,
) {
    assert!(!fence.is_null() && !ops.is_null() && !lock.is_null());
    unsafe {
        (*fence).lock = DmaFenceLock { extern_lock: lock };
        (*fence).ops = ops.cast();
        (*fence).timestamp_union = DmaFenceTimestamp {
            cb_list: ManuallyDrop::new(ListHead {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
            }),
        };
        INIT_LIST_HEAD(ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast());
        (*fence).context = context;
        (*fence).seqno = seqno;
        (*fence).flags = 0;
        (*fence).error = 0;
        kref_init(ptr::addr_of_mut!((*fence).refcount));
    }
}

/// Linux `dma_fence_set_error()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_set_error(fence: *mut DmaFence, error: c_int) {
    if !fence.is_null() && error != 0 {
        unsafe { (*fence).error = error };
    }
}

/// Test the Linux signaled flag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool {
    !fence.is_null()
        && unsafe {
            AtomicUsize::from_ptr(ptr::addr_of_mut!((*fence).flags).cast()).load(Ordering::Acquire)
                & (1usize << DMA_FENCE_FLAG_SIGNALED_BIT)
                != 0
        }
}

unsafe fn dma_fence_signal_locked_inner(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    if unsafe {
        AtomicUsize::from_ptr(ptr::addr_of_mut!((*fence).flags).cast()).load(Ordering::Acquire)
            & (1usize << DMA_FENCE_FLAG_SIGNALED_BIT)
            != 0
    } {
        return;
    }
    let mut callback_list = ListHead {
        next: ptr::null_mut(),
        prev: ptr::null_mut(),
    };
    unsafe {
        INIT_LIST_HEAD(&mut callback_list);
        let callbacks = ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast::<ListHead>();
        list_splice_init(callbacks, &mut callback_list);
        (*fence).timestamp_union.timestamp = ktime_get();
        AtomicUsize::from_ptr(ptr::addr_of_mut!((*fence).flags).cast()).fetch_or(
            (1usize << DMA_FENCE_FLAG_SIGNALED_BIT) | (1usize << DMA_FENCE_FLAG_TIMESTAMP_BIT),
            Ordering::Release,
        );
    }
    while !list_empty(&callback_list) {
        let node = callback_list.next;
        unsafe { list_del_init(node) };
        let callback = node.cast::<DmaFenceCb>();
        if let Some(function) = unsafe { (*callback).func } {
            unsafe { function(fence, callback) };
        }
    }
    crate::linux::requests::wake_dma_fence_waiters();
}

/// Signal a fence and run registered completion callbacks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_signal(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    let mut flags = 0;
    unsafe { fence_lock(fence, &mut flags) };
    unsafe { dma_fence_signal_locked_inner(fence) };
    unsafe { fence_unlock(fence, flags) };
}

/// Locked DMA-fence signal entry point used by the i915 request path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_signal_locked(fence: *mut DmaFence) {
    unsafe { dma_fence_signal_locked_inner(fence) };
}

/// Add a callback or report that the fence has already signaled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_add_callback(
    fence: *mut DmaFence,
    callback: *mut DmaFenceCb,
    function: unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb),
) -> c_int {
    if fence.is_null() || callback.is_null() {
        return -EINVAL;
    }
    let mut flags = 0;
    unsafe { fence_lock(fence, &mut flags) };
    if unsafe {
        AtomicUsize::from_ptr(ptr::addr_of_mut!((*fence).flags).cast()).load(Ordering::Acquire)
            & (1usize << DMA_FENCE_FLAG_SIGNALED_BIT)
            != 0
    } {
        unsafe { fence_unlock(fence, flags) };
        return -ENOENT;
    }
    unsafe {
        (*callback).func = Some(function);
        INIT_LIST_HEAD(ptr::addr_of_mut!((*callback).node));
        list_add_tail(
            ptr::addr_of_mut!((*callback).node),
            ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast(),
        );
        fence_unlock(fence, flags);
    }
    0
}

/// Synchronous Linux fence wait used by source-order i915 helpers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_wait(fence: *mut DmaFence, _intr: bool) -> c_int {
    if fence.is_null() {
        return -EINVAL;
    }
    while !unsafe { dma_fence_is_signaled(fence) } {
        axtask::yield_now();
    }
    0
}

/// Resolve the Linux driver-name callback, retaining the stable static result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_driver_name(fence: *mut DmaFence) -> *const c_char {
    if fence.is_null() {
        return DRIVER_NAME.as_ptr().cast();
    }
    let ops = unsafe { (*fence).ops.cast::<DmaFenceOps>() };
    if ops.is_null() {
        return DRIVER_NAME.as_ptr().cast();
    }
    unsafe {
        (*ops)
            .get_driver_name
            .map_or(DRIVER_NAME.as_ptr().cast(), |get| get(fence))
    }
}

/// Resolve the Linux timeline-name callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_timeline_name(fence: *mut DmaFence) -> *const c_char {
    if fence.is_null() {
        return WORK_NAME.as_ptr().cast();
    }
    let ops = unsafe { (*fence).ops.cast::<DmaFenceOps>() };
    if ops.is_null() {
        return WORK_NAME.as_ptr().cast();
    }
    if let Some(get) = unsafe { (*ops).get_timeline_name } {
        unsafe { get(fence) }
    } else {
        WORK_NAME.as_ptr().cast()
    }
}

unsafe extern "C" fn dma_fence_free_rcu(head: *mut RcuHead) {
    if head.is_null() {
        return;
    }
    let fence = unsafe {
        head.cast::<u8>()
            .sub(offset_of!(DmaFence, timestamp_union))
            .cast::<DmaFence>()
    };
    unsafe { kfree(fence) };
}

/// Default Linux `dma_fence_free()` RCU reclamation path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_free(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*fence).timestamp_union.rcu).cast::<RcuHead>() };
    crate::linux::rcu::call_rcu(head, dma_fence_free_rcu);
}

/// C ABI references used by i915's existing LinuxKPI sw-fence translation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence {
    requests_dma_fence_get(fence)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_put(fence: *mut DmaFence) {
    unsafe { requests_dma_fence_put(fence) };
}

/// Initialize a reservation sidecar when a framework owner initializes its
/// `dma_resv` record, including its native WW mutex.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_init(resv: *mut DmaResv) {
    if resv.is_null() {
        return;
    }
    unsafe {
        crate::linux::mutex::mutex_init(ptr::addr_of_mut!((*resv).lock.base));
        (*resv).lock.ctx = ptr::null_mut();
    }
    let slot = unsafe {
        AtomicPtr::from_ptr(ptr::addr_of_mut!((*resv).fences).cast::<*mut ReservationState>())
    };
    slot.store(ptr::null_mut(), Ordering::Release);
}

// upstream: i915_config.c i915_fence_context_timeout()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_fence_context_timeout(context: u64) -> c_ulong {
    const CONFIG_DRM_I915_FENCE_TIMEOUT_MS: u64 = 10_000;
    if CONFIG_DRM_I915_FENCE_TIMEOUT_MS != 0 && context != 0 {
        crate::linux::primitives::msecs_to_jiffies(CONFIG_DRM_I915_FENCE_TIMEOUT_MS)
            .min(i64::MAX as u64) as c_ulong
    } else {
        0
    }
}

// upstream: i915_config.h i915_fence_timeout()
fn i915_fence_timeout() -> c_ulong {
    unsafe { i915_fence_context_timeout(u64::MAX) }
}

// upstream: i915_sw_fence_work.c fence_complete()
unsafe fn fence_complete(work: *mut DmaFenceWork) {
    let ops = unsafe { (*work).ops };
    if !ops.is_null() {
        if let Some(release) = unsafe { (*ops).release } {
            unsafe { release(work) };
        }
    }
    unsafe { dma_fence_signal(ptr::addr_of_mut!((*work).dma)) };
}

// upstream: i915_sw_fence_work.c fence_work()
unsafe extern "C" fn fence_work(work: *mut WorkStruct) {
    let work = container_of!(work, DmaFenceWork, work);
    let ops = unsafe { (*work).ops };
    assert!(!ops.is_null());
    if let Some(callback) = unsafe { (*ops).work } {
        unsafe { callback(work) };
    }
    unsafe { fence_complete(work) };
    unsafe { dma_fence_put(ptr::addr_of_mut!((*work).dma)) };
}

// upstream: i915_sw_fence_work.c fence_notify()
unsafe extern "C" fn fence_notify(fence: *mut I915SwFence, state: I915SwFenceNotify) -> c_int {
    let work = container_of!(fence, DmaFenceWork, chain);
    match state {
        I915SwFenceNotify::FenceComplete => {
            let error = unsafe { (*fence).error };
            if error != 0 {
                unsafe { dma_fence_set_error(ptr::addr_of_mut!((*work).dma), error) };
            }
            if unsafe { (*work).dma.error == 0 } {
                unsafe { dma_fence_get(ptr::addr_of_mut!((*work).dma)) };
                let immediate =
                    unsafe { (*work).dma.flags & (1usize << DMA_FENCE_WORK_IMM) as c_ulong != 0 };
                if immediate {
                    unsafe { fence_work(ptr::addr_of_mut!((*work).work)) };
                } else {
                    unsafe {
                        workqueue::queue_work(
                            workqueue::system_dfl_wq,
                            ptr::addr_of_mut!((*work).work),
                        )
                    };
                }
            } else {
                unsafe { fence_complete(work) };
            }
        }
        I915SwFenceNotify::FenceFree => unsafe {
            dma_fence_put(ptr::addr_of_mut!((*work).dma));
        },
    }
    0
}

// upstream: i915_sw_fence_work.c get_driver_name()
unsafe extern "C" fn get_driver_name(_fence: *mut DmaFence) -> *const c_char {
    DRIVER_NAME.as_ptr().cast()
}

// upstream: i915_sw_fence_work.c get_timeline_name()
unsafe extern "C" fn get_timeline_name(fence: *mut DmaFence) -> *const c_char {
    let work = container_of!(fence, DmaFenceWork, dma);
    let name = unsafe { (*(*work).ops).name };
    if name.is_null() {
        WORK_NAME.as_ptr().cast()
    } else {
        name
    }
}

// upstream: i915_sw_fence_work.c fence_release()
unsafe extern "C" fn fence_release(fence: *mut DmaFence) {
    let work = container_of!(fence, DmaFenceWork, dma);
    unsafe {
        i915_sw_fence_fini(ptr::addr_of_mut!((*work).chain));
        dma_fence_free(fence);
    }
}

static FENCE_WORK_FENCE_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(get_driver_name),
    get_timeline_name: Some(get_timeline_name),
    enable_signaling: None,
    signaled: None,
    wait: None,
    release: Some(fence_release),
    set_deadline: None,
};

// upstream: i915_sw_fence_work.c dma_fence_work_init()
pub unsafe fn dma_fence_work_init(work: *mut DmaFenceWork, ops: *const DmaFenceWorkOps) {
    assert!(!work.is_null() && !ops.is_null());
    unsafe {
        (*work).ops = ops;
        spin_lock_init(&mut (*work).lock);
        dma_fence_init(
            ptr::addr_of_mut!((*work).dma),
            &FENCE_WORK_FENCE_OPS,
            ptr::addr_of_mut!((*work).lock),
            0,
            0,
        );
        i915_sw_fence_init(ptr::addr_of_mut!((*work).chain), Some(fence_notify));
        INIT_WORK_C(&mut (*work).work, fence_work);
    }
}

// upstream: i915_sw_fence_work.c dma_fence_work_chain()
pub unsafe fn dma_fence_work_chain(work: *mut DmaFenceWork, signal: *mut DmaFence) -> c_int {
    if signal.is_null() {
        return 0;
    }
    unsafe {
        __i915_sw_fence_await_dma_fence(
            ptr::addr_of_mut!((*work).chain),
            signal,
            ptr::addr_of_mut!((*work).cb),
        )
    }
}

// upstream: i915_sw_fence_work.h dma_fence_work_commit()
pub unsafe fn dma_fence_work_commit(work: *mut DmaFenceWork) {
    unsafe { i915_sw_fence_commit(ptr::addr_of_mut!((*work).chain)) };
}

// upstream: i915_sw_fence_work.h dma_fence_work_commit_imm()
pub unsafe fn dma_fence_work_commit_imm(work: *mut DmaFenceWork) {
    if crate::linux::memory::atomic_read(unsafe { &(*work).chain.pending }) <= 1 {
        let flags = unsafe { &*ptr::addr_of_mut!((*work).dma.flags).cast::<AtomicUsize>() };
        flags.fetch_or(1usize << DMA_FENCE_WORK_IMM, Ordering::Relaxed);
    }
    unsafe { dma_fence_work_commit(work) };
}

static CLFLUSH_WORK_OPS: DmaFenceWorkOps = DmaFenceWorkOps {
    name: CLFLUSH_NAME.as_ptr().cast(),
    work: Some(clflush_work),
    release: Some(clflush_release),
};

// upstream: i915_gem_clflush.c __do_clflush()
unsafe fn __do_clflush(obj: *mut DrmI915GemObject) {
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pages(obj) });
    unsafe { drm_clflush_sg((*obj).mm.pages) };
    unsafe { crate::i915_gem_core_upstream::i915_gem_object_frontbuffer_flush(obj, ORIGIN_CPU) };
}

// upstream: i915_gem_clflush.c clflush_work()
unsafe extern "C" fn clflush_work(base: *mut DmaFenceWork) {
    let clflush = base.cast::<Clflush>();
    unsafe { __do_clflush((*clflush).obj) };
}

// upstream: i915_gem_clflush.c clflush_release()
unsafe extern "C" fn clflush_release(base: *mut DmaFenceWork) {
    let clflush = base.cast::<Clflush>();
    unsafe {
        i915_gem_object_unpin_pages((*clflush).obj);
        i915_gem_object_put((*clflush).obj);
    }
}

// upstream: i915_gem_clflush.c clflush_work_create()
unsafe fn clflush_work_create(obj: *mut DrmI915GemObject) -> *mut Clflush {
    GEM_BUG_ON!(!unsafe { i915_gem_object_cache_dirty(obj) });
    let clflush = crate::linux::memory::kmalloc_obj::<Clflush>(GFP_KERNEL);
    if clflush.is_null() {
        return ptr::null_mut();
    }
    if unsafe { __i915_gem_object_get_pages(obj) } < 0 {
        unsafe { kfree(clflush) };
        return ptr::null_mut();
    }
    unsafe {
        dma_fence_work_init(ptr::addr_of_mut!((*clflush).base), &CLFLUSH_WORK_OPS);
        (*clflush).obj = i915_gem_object_get(obj);
    }
    clflush
}

// upstream: i915_gem_clflush.c i915_gem_clflush_object()
pub unsafe fn i915_gem_clflush_object(obj: *mut DrmI915GemObject, flags: u32) -> bool {
    let i915 = unsafe { to_i915((&(*obj).base.base).dev) };
    let mut clflush: *mut Clflush;

    unsafe { assert_object_held(obj) };

    if unsafe { IS_DGFX(i915) } {
        WARN_ON_ONCE!(unsafe { i915_gem_object_cache_dirty(obj) });
        return false;
    }

    if !unsafe { i915_gem_object_has_struct_page(obj) } {
        unsafe { i915_gem_object_set_cache_dirty(obj, false) };
        return false;
    }

    if flags & I915_CLFLUSH_FORCE == 0
        && unsafe { i915_gem_object_cache_coherent(obj) } & I915_BO_CACHE_COHERENT_FOR_READ != 0
    {
        return false;
    }

    crate::linux::i915_trace::trace_i915_gem_object_clflush(obj);

    clflush = ptr::null_mut();
    if flags & I915_CLFLUSH_SYNC == 0
        && unsafe { dma_resv_reserve_fences((&(*obj).base.base).resv.cast::<DmaResv>(), 1) } == 0
    {
        clflush = unsafe { clflush_work_create(obj) };
    }
    if !clflush.is_null() {
        let resv = unsafe { (&(*obj).base.base).resv.cast::<DmaResv>() };
        unsafe {
            i915_sw_fence_await_reservation(
                ptr::addr_of_mut!((*clflush).base.chain),
                resv.cast::<crate::i915_sw_fence_upstream::DmaResv>(),
                true,
                i915_fence_timeout(),
                I915_FENCE_GFP as c_ulong,
            );
            dma_resv_add_fence(
                resv,
                ptr::addr_of_mut!((*clflush).base.dma),
                DMA_RESV_USAGE_KERNEL,
            );
            dma_fence_work_commit(ptr::addr_of_mut!((*clflush).base));
            i915_gem_object_set_cache_dirty(obj, false);
        }
    } else if !unsafe { (*obj).mm.pages }.is_null() {
        unsafe {
            __do_clflush(obj);
            i915_gem_object_set_cache_dirty(obj, false);
        }
    } else {
        GEM_BUG_ON!(unsafe { (*obj).write_domain as u32 } != I915_GEM_DOMAIN_CPU);
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn completed(_: *mut DmaFence, _: *mut DmaFenceCb) {
        CALLBACKS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn fence_callbacks_and_reservation_references_have_real_lifetimes() {
        unsafe {
            let mut fence: DmaFence = core::mem::zeroed();
            let mut lock: Spinlock = core::mem::zeroed();
            let ops: DmaFenceOps = core::mem::zeroed();
            spin_lock_init(&mut lock);
            dma_fence_init(&mut fence, &ops, &mut lock, 1, 2);
            let mut callback: DmaFenceCb = core::mem::zeroed();
            assert_eq!(
                dma_fence_add_callback(&mut fence, &mut callback, completed),
                0
            );
            assert!(!dma_fence_is_signaled(&mut fence));
            dma_fence_signal(&mut fence);
            dma_fence_signal(&mut fence);
            assert!(dma_fence_is_signaled(&mut fence));
            assert_eq!(CALLBACKS.load(Ordering::Relaxed), 1);
            assert_eq!(
                dma_fence_add_callback(&mut fence, &mut callback, completed),
                -ENOENT
            );
            // Also exercises the empty callback list and timestamp union switch.
            let mut empty: DmaFence = core::mem::zeroed();
            dma_fence_init(&mut empty, &ops, &mut lock, 1, 3);
            dma_fence_signal(&mut empty);
            let mut resv: DmaResv = core::mem::zeroed();
            dma_resv_init(&mut resv);
            assert_eq!(dma_resv_reserve_fences(&mut resv, 1), 0);
            dma_resv_add_fence(&mut resv, &mut fence, DMA_RESV_USAGE_KERNEL);
            let mut cursor: DmaResvIter = core::mem::zeroed();
            cursor.obj = &mut resv;
            cursor.usage = DMA_RESV_USAGE_KERNEL;
            assert_eq!(dma_resv_iter_next(&mut cursor), &mut fence as *mut _);
            assert!(dma_resv_iter_next(&mut cursor).is_null());
            requests_dma_fence_put(cursor.fence);
            dma_resv_fini(&mut resv);
            assert_eq!(fence.refcount.refcount.refs.counter, 1);
        }
    }
}
