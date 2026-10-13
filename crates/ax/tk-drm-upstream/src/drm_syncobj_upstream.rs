// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux DRM syncobj core used by render drivers.
//!
//! Syncobjs own ordinary Linux `dma_fence` references and timeline payloads
//! are Linux `dma_fence_chain` nodes. VirtGPU and other drivers therefore use
//! the same fence/sync-file representation as the rest of the DRM core.
#![allow(non_snake_case, dead_code, unsafe_code)]

use alloc::{sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    ffi::{c_int, c_long, c_void},
    mem::{MaybeUninit, size_of},
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};

use tk_linux_kapi::{
    config::{EFAULT, EINVAL, ENOENT, ENOMEM, EOPNOTSUPP, ERESTARTSYS, ETIME, GFP_KERNEL},
    locks::spin_lock_init,
    memory::{kfree, kmalloc},
    mm_native::{copy_from_user, copy_to_user},
    types::{DmaFence, DmaFenceChain, Spinlock, WaitQueueHead},
    wait::{init_waitqueue_head, wake_up_all},
};

use crate::{
    drm_crtc_upstream::DmaFenceOps,
    drm_device_types::DrmDevice,
    drm_file_types::DrmFile,
    uapi_drm_consts::{
        DRM_SYNCOBJ_CREATE_SIGNALED, DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
        DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE, DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE,
        DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE, DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
    },
    uapi_drm_types::{
        DrmSyncobjArray, DrmSyncobjCreate, DrmSyncobjDestroy, DrmSyncobjEventfd, DrmSyncobjHandle,
        DrmSyncobjTimelineArray, DrmSyncobjTimelineWait, DrmSyncobjTransfer, DrmSyncobjWait,
    },
};

const DRIVER_SYNCOBJ: u32 = 1 << 5;
const DRIVER_SYNCOBJ_TIMELINE: u32 = 1 << 6;
const MAX_SYNCOBJ_COUNT: usize = 1 << 16;
const WAIT_FOR_SUBMIT_TIMEOUT_JIFFIES: u64 = 5000;
const O_CLOEXEC: c_int = 0x80000;

unsafe extern "C" {
    fn dma_fence_init(
        fence: *mut DmaFence,
        ops: *const DmaFenceOps,
        lock: *mut Spinlock,
        context: u64,
        seqno: u64,
    );
    fn dma_fence_context_alloc(num: u32) -> u64;
    fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_put(fence: *mut DmaFence);
    fn dma_fence_signal(fence: *mut DmaFence) -> i32;
    fn dma_fence_set_error(fence: *mut DmaFence, error: i32);
    fn dma_fence_free(fence: *mut DmaFence);
    fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool;
    fn dma_fence_wait(fence: *mut DmaFence, interruptible: bool) -> i32;
    fn dma_fence_wait_timeout(fence: *mut DmaFence, interruptible: bool, timeout: c_long)
    -> c_long;
    fn dma_fence_chain_find_seqno(fence: *mut *mut DmaFence, point: u64) -> i32;
    fn dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence;
    fn dma_fence_chain_free(chain: *mut c_void);
    fn dma_fence_array_create(
        num_fences: u32,
        fences: *mut *mut DmaFence,
        context: u64,
        seqno: u32,
    ) -> *mut DmaFence;
    fn dma_fence_set_deadline(fence: *mut DmaFence, deadline: i64);
    static dma_fence_chain_ops: DmaFenceOps;
    fn sync_file_create(fence: *mut DmaFence) -> *mut c_void;
    fn sync_file_get_fence(fd: c_int) -> *mut DmaFence;
    fn get_unused_fd_flags(flags: c_int) -> c_int;
    fn put_unused_fd(fd: c_int);
    fn fd_install(fd: c_int, file: *mut c_void);
    fn fput(file: *mut c_void);
}

/// An owned reference to the Linux `dma_fence` ABI.
struct FenceRef(*mut DmaFence);

unsafe impl Send for FenceRef {}
unsafe impl Sync for FenceRef {}

impl FenceRef {
    fn get(fence: *mut DmaFence) -> Option<Self> {
        if fence.is_null() {
            None
        } else {
            Some(Self(unsafe { dma_fence_get(fence) }))
        }
    }

    fn into_raw(self) -> *mut DmaFence {
        let fence = self.0;
        core::mem::forget(self);
        fence
    }
}

impl Clone for FenceRef {
    fn clone(&self) -> Self {
        Self(unsafe { dma_fence_get(self.0) })
    }
}

impl Drop for FenceRef {
    fn drop(&mut self) {
        unsafe { dma_fence_put(self.0) };
    }
}

struct SyncobjState {
    fence: Option<FenceRef>,
}

/// Internal syncobj. Its pointer is stored in `drm_file.syncobj_xa` as one
/// `Arc` reference; every core helper returned to a driver owns another.
pub struct DrmSyncobj {
    state: spin::Mutex<SyncobjState>,
    wait: UnsafeCell<WaitQueueHead>,
}

unsafe impl Send for DrmSyncobj {}
unsafe impl Sync for DrmSyncobj {}

impl DrmSyncobj {
    fn new() -> Result<Arc<Self>, i32> {
        let mut object = Arc::new(Self {
            state: spin::Mutex::new(SyncobjState { fence: None }),
            wait: UnsafeCell::new(unsafe { MaybeUninit::zeroed().assume_init() }),
        });
        init_waitqueue_head(unsafe { &mut *Arc::get_mut(&mut object).unwrap().wait.get() });
        Ok(object)
    }

    fn fence(&self) -> Option<FenceRef> {
        self.state.lock().fence.clone()
    }

    fn replace_fence(&self, fence: Option<FenceRef>) {
        let retired = {
            let mut state = self.state.lock();
            core::mem::replace(&mut state.fence, fence)
        };
        wake_up_all(unsafe { &mut *self.wait.get() });
        drop(retired);
    }

    fn point_available(&self, point: u64) -> bool {
        let Some(mut head) = self.fence() else {
            return false;
        };
        if point == 0 {
            return true;
        }
        let mut raw = head.into_raw();
        let result = unsafe { dma_fence_chain_find_seqno(&mut raw, point) };
        if !raw.is_null() {
            unsafe { dma_fence_put(raw) };
        }
        result == 0
    }

    fn find_point(&self, point: u64) -> Result<Option<FenceRef>, i32> {
        let Some(head) = self.fence() else {
            return Ok(None);
        };
        if point == 0 {
            return Ok(Some(head));
        }
        let mut raw = head.into_raw();
        let result = unsafe { dma_fence_chain_find_seqno(&mut raw, point) };
        if result != 0 {
            if !raw.is_null() {
                unsafe { dma_fence_put(raw) };
            }
            return Ok(None);
        }
        if raw.is_null() {
            return create_software_fence(true)
                .map(|fence| Some(FenceRef(fence)))
                .ok_or(-ENOMEM);
        }
        Ok(Some(FenceRef(raw)))
    }

    fn add_point(&self, chain: *mut DmaFenceChain, fence: *mut DmaFence, point: u64) -> i32 {
        if chain.is_null() || fence.is_null() {
            return -EINVAL;
        }
        let retired = {
            let mut state = self.state.lock();
            let previous = state.fence.as_ref().map(|old| old.0);
            if unsafe { init_chain(chain, previous, fence, point) } != 0 {
                return -EINVAL;
            }
            let base = unsafe { ptr::addr_of_mut!((*chain).base) };
            core::mem::replace(&mut state.fence, Some(FenceRef(base)))
        };
        wake_up_all(unsafe { &mut *self.wait.get() });
        drop(retired);
        0
    }
}

/// Return the generic DRM syncobj feature requirements for ioctl callers.
#[inline]
unsafe fn has_feature(dev: *mut DrmDevice, feature: u32) -> bool {
    !dev.is_null()
        && unsafe { !(*dev).driver.is_null() }
        && unsafe { (*(*dev).driver).driver_features & feature != 0 }
}

#[inline]
unsafe fn syncobj_xa(file: *mut DrmFile) -> *mut tk_linux_kapi::types::XArray {
    unsafe { ptr::addr_of_mut!((*file).syncobj_xa) }
}

fn create_software_fence(signaled: bool) -> Option<*mut DmaFence> {
    let fence = unsafe { kmalloc(size_of::<DmaFence>(), GFP_KERNEL) }.cast::<DmaFence>();
    if fence.is_null() {
        return None;
    }
    unsafe {
        ptr::write_bytes(fence.cast::<u8>(), 0, size_of::<DmaFence>());
        let lock = ptr::addr_of_mut!((*fence).lock.inline_lock);
        spin_lock_init(&mut *lock);
        dma_fence_init(
            fence,
            &SOFTWARE_FENCE_OPS,
            lock,
            dma_fence_context_alloc(1),
            NEXT_FENCE_SEQNO.fetch_add(1, Ordering::Relaxed) as u64,
        );
        if signaled {
            let _ = dma_fence_signal(fence);
        }
    }
    Some(fence)
}

static NEXT_FENCE_SEQNO: AtomicU32 = AtomicU32::new(1);

unsafe extern "C" fn software_fence_driver_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"drm-syncobj".as_ptr()
}

unsafe extern "C" fn software_fence_timeline_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"software".as_ptr()
}

unsafe extern "C" fn software_fence_enable_signaling(_: *mut DmaFence) -> bool {
    true
}

unsafe extern "C" fn software_fence_release(fence: *mut DmaFence) {
    // Fence readers may still be inside dma_fence_get_rcu() after the final
    // reference is dropped. Use the Linux dma-fence RCU free path.
    unsafe { dma_fence_free(fence) };
}

static SOFTWARE_FENCE_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(software_fence_driver_name),
    get_timeline_name: Some(software_fence_timeline_name),
    enable_signaling: Some(software_fence_enable_signaling),
    signaled: None,
    wait: Some(dma_fence_default_wait),
    release: Some(software_fence_release),
    set_deadline: None,
};

unsafe extern "C" {
    fn dma_fence_default_wait(fence: *mut DmaFence, interruptible: bool, timeout: c_long)
    -> c_long;
}

/// Allocate a single timeline chain node over the previous head and new
/// completion fence. The caller serializes `syncobj` replacement.
unsafe fn init_chain(
    chain: *mut DmaFenceChain,
    previous: Option<*mut DmaFence>,
    fence: *mut DmaFence,
    point: u64,
) -> i32 {
    if chain.is_null() || fence.is_null() {
        return -EINVAL;
    }
    unsafe { ptr::write_bytes(chain.cast::<u8>(), 0, size_of::<DmaFenceChain>()) };
    let previous = previous.unwrap_or(ptr::null_mut());
    let mut previous_point = 0;
    let (context, seqno) = if previous.is_null() {
        (unsafe { dma_fence_context_alloc(1) }, point)
    } else if unsafe { (*previous).ops == ptr::addr_of!(dma_fence_chain_ops).cast() } {
        previous_point = unsafe { (*previous).seqno };
        if point > previous_point {
            (unsafe { (*previous).context }, point)
        } else {
            (
                unsafe { dma_fence_context_alloc(1) },
                point.max(previous_point),
            )
        }
    } else {
        (unsafe { dma_fence_context_alloc(1) }, point)
    };
    unsafe {
        let lock = ptr::addr_of_mut!((*chain).base.lock.inline_lock);
        spin_lock_init(&mut *lock);
        dma_fence_init(
            ptr::addr_of_mut!((*chain).base),
            &dma_fence_chain_ops,
            lock,
            context,
            seqno,
        );
        (*chain).prev = if previous.is_null() {
            ptr::null_mut()
        } else {
            dma_fence_get(previous)
        };
        (*chain).prev_seqno = previous_point;
        (*chain).fence = dma_fence_get(fence);
    }
    0
}

/// Allocate a timeline node on behalf of driver submit paths.
pub fn drm_syncobj_chain_alloc() -> *mut DmaFenceChain {
    unsafe { kmalloc(size_of::<DmaFenceChain>(), GFP_KERNEL) }.cast()
}

/// Drop an uninitialized timeline chain allocation.
pub unsafe fn drm_syncobj_chain_free(chain: *mut DmaFenceChain) {
    if !chain.is_null() {
        unsafe { kfree(chain.cast::<c_void>()) };
    }
}

/// Create an owned Linux dma-fence for asynchronous driver work.
pub fn drm_syncobj_fence_create() -> *mut DmaFence {
    create_software_fence(false).unwrap_or(ptr::null_mut())
}

/// Publish a terminal result to a driver-created software fence.
pub unsafe fn drm_syncobj_fence_signal(fence: *mut DmaFence, error: i32) {
    if fence.is_null() {
        return;
    }
    unsafe {
        if error != 0 {
            dma_fence_set_error(fence, error);
        }
        let _ = dma_fence_signal(fence);
    }
}

/// Release one Linux dma-fence reference.
pub unsafe fn drm_syncobj_fence_put(fence: *mut DmaFence) {
    unsafe { dma_fence_put(fence) };
}

/// Wait on an owned dma-fence dependency from a render submit.
pub unsafe fn drm_syncobj_wait_fence(fence: *mut DmaFence, interruptible: bool) -> i32 {
    if fence.is_null() {
        return -EINVAL;
    }
    unsafe { dma_fence_wait(fence, interruptible) }
}

/// Resolve an input sync-file descriptor to an owned dma-fence reference.
pub unsafe fn drm_syncobj_sync_file_get_fence(fd: c_int) -> *mut DmaFence {
    unsafe { sync_file_get_fence(fd) }
}

/// Create a sync-file wrapper retaining the supplied dma-fence.
pub unsafe fn drm_syncobj_sync_file_create(fence: *mut DmaFence) -> *mut c_void {
    unsafe { sync_file_create(fence) }
}

/// Return the Linux file pointer at the head of an upstream sync-file wrapper.
pub unsafe fn drm_syncobj_sync_file_file(sync_file: *mut c_void) -> *mut c_void {
    if sync_file.is_null() {
        return ptr::null_mut();
    }
    unsafe { *sync_file.cast::<*mut c_void>() }
}

/// Release an unpublished sync-file wrapper.
pub unsafe fn drm_syncobj_sync_file_fput(file: *mut c_void) {
    unsafe { fput(file) };
}

/// Linux `drm_syncobj_open()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_open(file: *mut DrmFile) {
    if file.is_null() {
        return;
    }
    unsafe {
        tk_linux_kapi::xarray::xa_init_flags(
            &mut (*file).syncobj_xa,
            tk_linux_kapi::xarray::XA_FLAGS_ALLOC,
        );
    }
}

/// Linux `drm_syncobj_release()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_release(file: *mut DrmFile) {
    if file.is_null() {
        return;
    }
    let xa = unsafe { syncobj_xa(file) };
    // Final DRM-file release cannot race ioctls using its handle table. Take
    // the snapshot outside the XArray spinlock and drop each object after its
    // independently locked erase, since a final fence put may run callbacks.
    for (handle, _) in tk_linux_kapi::xarray::xa_snapshot(unsafe { &mut *xa }) {
        let object =
            tk_linux_kapi::xarray::xa_erase_irq::<_, DrmSyncobj>(unsafe { &mut *xa }, handle);
        if !object.is_null() {
            unsafe { drop(Arc::from_raw(object)) };
        }
    }
    tk_linux_kapi::xarray::xa_destroy(unsafe { &mut *xa });
}

/// Linux `drm_syncobj_find()`: one owned reference, or NULL for a missing handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_find(file: *mut DrmFile, handle: u32) -> *mut DrmSyncobj {
    if file.is_null() || handle == 0 {
        return ptr::null_mut();
    }
    let xa = unsafe { syncobj_xa(file) };
    tk_linux_kapi::xarray::xa_lock(unsafe { &mut *xa });
    let syncobj = tk_linux_kapi::xarray::xa_load::<_, DrmSyncobj>(unsafe { &mut *xa }, handle);
    if !syncobj.is_null() {
        unsafe { Arc::increment_strong_count(syncobj) };
    }
    tk_linux_kapi::xarray::xa_unlock(unsafe { &mut *xa });
    syncobj
}

/// Linux `drm_syncobj_put()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_put(syncobj: *mut DrmSyncobj) {
    if !syncobj.is_null() {
        unsafe { Arc::decrement_strong_count(syncobj) };
    }
}

fn create_syncobj_handle(file: *mut DrmFile, syncobj: Arc<DrmSyncobj>) -> Result<u32, i32> {
    let raw = Arc::into_raw(syncobj).cast_mut();
    let xa = unsafe { syncobj_xa(file) };
    let mut handle = 0u32;
    let mut next = 1u32;
    let ret = tk_linux_kapi::xarray::xa_alloc_cyclic_irq(
        unsafe { &mut *xa },
        &mut handle,
        raw,
        tk_linux_kapi::xarray::XaLimit {
            min: 1,
            max: i32::MAX as u32,
        },
        &mut next,
        GFP_KERNEL,
    );
    if ret != 0 {
        unsafe { drop(Arc::from_raw(raw)) };
        Err(ret)
    } else {
        Ok(handle)
    }
}

fn remove_syncobj_handle(file: *mut DrmFile, handle: u32) -> Result<(), i32> {
    if handle == 0 {
        return Err(-ENOENT);
    }
    let xa = unsafe { syncobj_xa(file) };
    let raw = tk_linux_kapi::xarray::xa_erase_irq::<_, DrmSyncobj>(unsafe { &mut *xa }, handle);
    if raw.is_null() {
        Err(-ENOENT)
    } else {
        unsafe { drop(Arc::from_raw(raw)) };
        Ok(())
    }
}

/// Linux `drm_syncobj_fence_get()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_fence_get(syncobj: *mut DrmSyncobj) -> *mut DmaFence {
    if syncobj.is_null() {
        return ptr::null_mut();
    }
    unsafe { &*syncobj }
        .fence()
        .map_or(ptr::null_mut(), FenceRef::into_raw)
}

/// Linux `drm_syncobj_replace_fence()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_replace_fence(syncobj: *mut DrmSyncobj, fence: *mut DmaFence) {
    if syncobj.is_null() {
        return;
    }
    let fence = FenceRef::get(fence);
    unsafe { &*syncobj }.replace_fence(fence);
}

/// Linux `drm_syncobj_add_point()` with caller-owned chain storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_add_point(
    syncobj: *mut DrmSyncobj,
    chain: *mut DmaFenceChain,
    fence: *mut DmaFence,
    point: u64,
) {
    if syncobj.is_null() || chain.is_null() || fence.is_null() {
        return;
    }
    let _ = unsafe { &*syncobj }.add_point(chain, fence, point);
}

/// Append a point using an internally allocated chain node.
pub unsafe fn drm_syncobj_add_point_auto(
    syncobj: *mut DrmSyncobj,
    fence: *mut DmaFence,
    point: u64,
) -> i32 {
    if syncobj.is_null() || fence.is_null() {
        return -EINVAL;
    }
    let chain = drm_syncobj_chain_alloc();
    if chain.is_null() {
        return -ENOMEM;
    }
    let ret = unsafe { &*syncobj }.add_point(chain, fence, point);
    if ret != 0 {
        unsafe { drm_syncobj_chain_free(chain) };
    }
    ret
}

/// Linux `drm_syncobj_find_fence()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_find_fence(
    file: *mut DrmFile,
    handle: u32,
    point: u64,
    flags: u64,
    out: *mut *mut DmaFence,
) -> i32 {
    if file.is_null() || out.is_null() {
        return -EINVAL;
    }
    unsafe { *out = ptr::null_mut() };
    if flags & !(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT as u64) != 0 {
        return -EINVAL;
    }
    let syncobj = unsafe { drm_syncobj_find(file, handle) };
    if syncobj.is_null() {
        return -ENOENT;
    }
    let result = loop {
        match unsafe { &*syncobj }.find_point(point) {
            Ok(Some(fence)) => break Ok(fence.into_raw()),
            Err(error) => break Err(error),
            Ok(None) if flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT as u64 == 0 => {
                break Err(-EINVAL);
            }
            Ok(None) => {
                let timeout =
                    tk_linux_kapi::primitives::msecs_to_jiffies(WAIT_FOR_SUBMIT_TIMEOUT_JIFFIES);
                let wait = tk_linux_kapi::wait_event_interruptible_timeout!(
                    unsafe { &mut *(*syncobj).wait.get() },
                    unsafe { &*syncobj }.point_available(point),
                    timeout,
                );
                if wait == 0 {
                    break Err(-ETIME);
                }
                if wait < 0 {
                    break Err(-ERESTARTSYS);
                }
            }
        }
    };
    unsafe { drm_syncobj_put(syncobj) };
    match result {
        Ok(fence) => {
            unsafe { *out = fence };
            0
        }
        Err(error) => error,
    }
}

/// Signal a software fence and wake generic syncobj waiters. Drivers use this
/// to expose an asynchronous `dma_fence` for their device-owned completion.
pub unsafe fn drm_syncobj_signal_fence(fence: *mut DmaFence, error: i32) {
    unsafe { drm_syncobj_fence_signal(fence, error) };
    let mut wait = unsafe { MaybeUninit::<WaitQueueHead>::zeroed().assume_init() };
    init_waitqueue_head(&mut wait);
    wake_up_all(&mut wait);
}

/// Install one completion fence in a binary payload, or append it at a
/// timeline point when `point` is nonzero.
pub unsafe fn drm_syncobj_submit_point(
    syncobj: *mut DrmSyncobj,
    fence: *mut DmaFence,
    point: u64,
) -> i32 {
    if point == 0 {
        unsafe { drm_syncobj_replace_fence(syncobj, fence) };
        0
    } else {
        unsafe { drm_syncobj_add_point_auto(syncobj, fence, point) }
    }
}

/// Create a new signaled Linux fence for DRM's signaled-stub semantics.
fn signaled_stub() -> Result<FenceRef, i32> {
    create_software_fence(true).map(FenceRef).ok_or(-ENOMEM)
}

/// Linux `dma_fence_unwrap_merge()` for the transfer-to-timeline path. Chain
/// nodes may not themselves be contained in a new chain node, so flatten the
/// pending contained fences and join them with the Linux fence-array provider.
fn unwrap_merge_fence(fence: *mut DmaFence) -> Result<FenceRef, i32> {
    if fence.is_null() {
        return Err(-EINVAL);
    }
    if unsafe { (*fence).ops != ptr::addr_of!(dma_fence_chain_ops).cast() } {
        return FenceRef::get(fence).ok_or(-EINVAL);
    }

    let mut pending = Vec::<*mut DmaFence>::new();
    let mut current = unsafe { dma_fence_get(fence) };
    while !current.is_null() {
        let chain = unsafe { (*current).ops == ptr::addr_of!(dma_fence_chain_ops).cast() };
        if chain {
            let child = unsafe { (*current.cast::<DmaFenceChain>()).fence };
            if !child.is_null() && !unsafe { dma_fence_is_signaled(child) } {
                if pending.try_reserve(1).is_err() {
                    unsafe { dma_fence_put(current) };
                    for child in pending.drain(..) {
                        unsafe { dma_fence_put(child) };
                    }
                    return Err(-ENOMEM);
                }
                pending.push(unsafe { dma_fence_get(child) });
            }
            current = unsafe { dma_fence_chain_walk(current) };
        } else {
            if !unsafe { dma_fence_is_signaled(current) } {
                if pending.try_reserve(1).is_err() {
                    unsafe { dma_fence_put(current) };
                    for child in pending.drain(..) {
                        unsafe { dma_fence_put(child) };
                    }
                    return Err(-ENOMEM);
                }
                pending.push(current);
            } else {
                unsafe { dma_fence_put(current) };
            }
            current = ptr::null_mut();
        }
    }

    match pending.len() {
        0 => signaled_stub(),
        1 => Ok(FenceRef(pending.pop().unwrap())),
        count if count <= u32::MAX as usize => {
            let Some(bytes) = count.checked_mul(size_of::<*mut DmaFence>()) else {
                for child in pending.drain(..) {
                    unsafe { dma_fence_put(child) };
                }
                return Err(-ENOMEM);
            };
            let fences = kmalloc(bytes, GFP_KERNEL).cast::<*mut DmaFence>();
            if fences.is_null() {
                for child in pending.drain(..) {
                    unsafe { dma_fence_put(child) };
                }
                return Err(-ENOMEM);
            }
            unsafe { ptr::copy_nonoverlapping(pending.as_ptr(), fences, count) };
            let array = unsafe {
                dma_fence_array_create(
                    count as u32,
                    fences,
                    dma_fence_context_alloc(1),
                    NEXT_FENCE_SEQNO.fetch_add(1, Ordering::Relaxed),
                )
            };
            if array.is_null() {
                unsafe { kfree(fences.cast::<c_void>()) };
                for child in pending.drain(..) {
                    unsafe { dma_fence_put(child) };
                }
                Err(-ENOMEM)
            } else {
                // The fence array now owns each reference and the pointer
                // vector; only the temporary Vec storage remains to drop.
                pending.clear();
                Ok(FenceRef(array))
            }
        }
        _ => {
            for child in pending.drain(..) {
                unsafe { dma_fence_put(child) };
            }
            Err(-ENOMEM)
        }
    }
}

fn create_syncobj(signaled: bool) -> Result<Arc<DrmSyncobj>, i32> {
    let object = DrmSyncobj::new()?;
    if signaled {
        object.replace_fence(Some(signaled_stub()?));
    }
    Ok(object)
}

fn copy_handles(address: u64, count: u32) -> Result<Vec<u32>, i32> {
    let count = count as usize;
    if count > MAX_SYNCOBJ_COUNT || (count != 0 && address == 0) {
        return Err(-EINVAL);
    }
    let mut handles = Vec::new();
    handles.try_reserve_exact(count).map_err(|_| -ENOMEM)?;
    handles.resize(count, 0);
    if count != 0
        && unsafe {
            copy_from_user(
                handles.as_mut_ptr().cast(),
                address as usize as *const c_void,
                count * size_of::<u32>(),
            )
        } != 0
    {
        return Err(-EFAULT);
    }
    Ok(handles)
}

fn find_syncobjs(file: *mut DrmFile, handles: &[u32]) -> Result<Vec<Arc<DrmSyncobj>>, i32> {
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(handles.len())
        .map_err(|_| -ENOMEM)?;
    for handle in handles {
        let raw = unsafe { drm_syncobj_find(file, *handle) };
        if raw.is_null() {
            return Err(-ENOENT);
        }
        objects.push(unsafe { Arc::from_raw(raw) });
    }
    Ok(objects)
}

fn read_points(address: u64, count: usize) -> Result<Vec<u64>, i32> {
    if count > MAX_SYNCOBJ_COUNT {
        return Err(-EINVAL);
    }
    let mut points = Vec::new();
    points.try_reserve_exact(count).map_err(|_| -ENOMEM)?;
    points.resize(count, 0);
    if address != 0
        && count != 0
        && unsafe {
            copy_from_user(
                points.as_mut_ptr().cast(),
                address as usize as *const c_void,
                count * size_of::<u64>(),
            )
        } != 0
    {
        return Err(-EFAULT);
    }
    Ok(points)
}

fn wait_timeout_jiffies(timeout_nsec: i64) -> u64 {
    let now = tk_linux_kapi::primitives::ktime_get().max(0) as u64;
    if timeout_nsec <= 0 || timeout_nsec as u64 <= now {
        return 0;
    }
    let remaining_ms = (timeout_nsec as u64 - now).saturating_add(999_999) / 1_000_000;
    tk_linux_kapi::primitives::msecs_to_jiffies(remaining_ms).max(1)
}

fn chain_query_point(fence: Option<FenceRef>, last_submitted: bool) -> u64 {
    let Some(head) = fence else {
        return 0;
    };
    if unsafe { (*head.0).ops != ptr::addr_of!(dma_fence_chain_ops).cast() } {
        return 0;
    }
    if last_submitted {
        return unsafe { (*head.0).seqno };
    }
    let context = unsafe { (*head.0).context };
    let mut last = head.clone();
    let mut current = FenceRef(unsafe { dma_fence_get(head.0) });
    loop {
        if unsafe { (*current.0).context != context } {
            break;
        }
        last = current.clone();
        let previous = unsafe { dma_fence_chain_walk(current.into_raw()) };
        if previous.is_null() {
            break;
        }
        current = FenceRef(previous);
    }
    if unsafe { dma_fence_is_signaled(last.0) } {
        unsafe { (*last.0).seqno }
    } else if unsafe { (*last.0).ops == ptr::addr_of!(dma_fence_chain_ops).cast() } {
        unsafe { (*last.0.cast::<DmaFenceChain>()).prev_seqno }
    } else {
        0
    }
}

fn wait_syncobjs(
    objects: &[Arc<DrmSyncobj>],
    points: &[u64],
    flags: u32,
    timeout_nsec: i64,
    deadline_nsec: u64,
) -> Result<u32, i32> {
    if objects.is_empty() || points.len() != objects.len() {
        return Err(-EINVAL);
    }
    let wait_all = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
    let relaxed = flags
        & (DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE)
        != 0;
    let timeout_jiffies = wait_timeout_jiffies(timeout_nsec);
    let expiry = tk_linux_kapi::primitives::ktime_get()
        .saturating_add(tk_linux_kapi::primitives::jiffies_to_nsecs(timeout_jiffies) as i64);
    let mut deadline_set = false;
    loop {
        let mut first = u32::MAX;
        let mut ready_count = 0usize;
        let mut missing = false;
        for (index, (object, point)) in objects.iter().zip(points).enumerate() {
            let fence = match object.find_point(*point) {
                Ok(fence) => fence,
                Err(error) => return Err(error),
            };
            let Some(fence) = fence else {
                missing = true;
                continue;
            };
            let ready = flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0
                || unsafe { dma_fence_is_signaled(fence.0) };
            if ready {
                ready_count += 1;
                if first == u32::MAX {
                    first = index as u32;
                }
            }
        }
        if if wait_all {
            !missing && ready_count == objects.len()
        } else {
            ready_count != 0
        } {
            return Ok(if wait_all { u32::MAX } else { first });
        }
        if !relaxed && missing {
            return Err(-EINVAL);
        }
        let now = tk_linux_kapi::primitives::ktime_get();
        if timeout_jiffies == 0 || now >= expiry {
            return Err(-ETIME);
        }
        if !deadline_set && flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE != 0 && deadline_nsec != 0
        {
            for (object, point) in objects.iter().zip(points) {
                if let Ok(Some(fence)) = object.find_point(*point) {
                    unsafe { dma_fence_set_deadline(fence.0, deadline_nsec as i64) };
                }
            }
            deadline_set = true;
        }
        let remaining_ns = expiry.saturating_sub(now).max(0) as u64;
        let sleep_ms = ((remaining_ns.min(1_000_000) + 999_999) / 1_000_000).max(1);
        let sleep = tk_linux_kapi::primitives::msecs_to_jiffies(sleep_ms).max(1);
        let mut queue = unsafe { MaybeUninit::<WaitQueueHead>::zeroed().assume_init() };
        init_waitqueue_head(&mut queue);
        let _ = tk_linux_kapi::wait_event_timeout!(queue, false, sleep as c_long);
    }
}

#[inline]
unsafe fn feature(dev: *mut DrmDevice, flag: u32) -> bool {
    !dev.is_null() && unsafe { (*dev).driver_features & flag != 0 }
}

/// Linux `drm_syncobj_create_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_create_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &mut *data.cast::<DrmSyncobjCreate>() };
    if args.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
        return -EINVAL;
    }
    match create_syncobj(args.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0)
        .and_then(|object| create_syncobj_handle(file, object))
    {
        Ok(handle) => {
            args.handle = handle;
            0
        }
        Err(error) => error,
    }
}

/// Linux `drm_syncobj_destroy_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_destroy_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjDestroy>() };
    if args.pad != 0 {
        return -EINVAL;
    }
    match remove_syncobj_handle(file, args.handle) {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// Linux `drm_syncobj_handle_to_fd_ioctl()`. Sync-file export is supported;
/// syncobj-object fd export requires a VFS object-fd provider not present in
/// this portable DRM core, so it is rejected rather than exporting a snapshot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_handle_to_fd_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &mut *data.cast::<DrmSyncobjHandle>() };
    let flags =
        DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE | DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE;
    if args.pad != 0 || args.flags & !flags != 0 {
        return -EINVAL;
    }
    if args.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE == 0 {
        return if args.point == 0 {
            -EOPNOTSUPP
        } else {
            -EINVAL
        };
    }
    let point = if args.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE != 0 {
        args.point
    } else {
        0
    };
    let mut fence = ptr::null_mut();
    let ret = unsafe { drm_syncobj_find_fence(file, args.handle, point, 0, &mut fence) };
    if ret != 0 {
        return ret;
    }
    let fd = unsafe { get_unused_fd_flags(O_CLOEXEC) };
    if fd < 0 {
        unsafe { dma_fence_put(fence) };
        return fd;
    }
    let sync_file = unsafe { sync_file_create(fence) };
    unsafe { dma_fence_put(fence) };
    if sync_file.is_null() {
        unsafe { put_unused_fd(fd) };
        return -ENOMEM;
    }
    let file_ptr = unsafe { *(sync_file.cast::<*mut c_void>()) };
    if file_ptr.is_null() {
        unsafe {
            fput(file_ptr);
            put_unused_fd(fd);
        }
        return -EINVAL;
    }
    unsafe { fd_install(fd, file_ptr) };
    args.fd = fd;
    0
}

/// Linux `drm_syncobj_fd_to_handle_ioctl()` for sync-file imports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_fd_to_handle_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &mut *data.cast::<DrmSyncobjHandle>() };
    let flags =
        DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE | DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE;
    if args.pad != 0 || args.flags & !flags != 0 {
        return -EINVAL;
    }
    if args.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE == 0 {
        return if args.point == 0 {
            -EOPNOTSUPP
        } else {
            -EINVAL
        };
    }
    let fence = unsafe { sync_file_get_fence(args.fd) };
    if fence.is_null() {
        return -EINVAL;
    }
    let syncobj = unsafe { drm_syncobj_find(file, args.handle) };
    if syncobj.is_null() {
        unsafe { dma_fence_put(fence) };
        return -ENOENT;
    }
    let point = if args.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE != 0 {
        args.point
    } else {
        0
    };
    let ret = unsafe { drm_syncobj_submit_point(syncobj, fence, point) };
    unsafe {
        dma_fence_put(fence);
        drm_syncobj_put(syncobj);
    }
    ret
}

/// Linux `drm_syncobj_transfer_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_transfer_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ_TIMELINE) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjTransfer>() };
    if args.pad != 0 || args.flags & !(DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT) != 0 {
        return -EINVAL;
    }
    let destination = unsafe { drm_syncobj_find(file, args.dst_handle) };
    if destination.is_null() {
        return -ENOENT;
    }
    let mut fence = ptr::null_mut();
    let ret = unsafe {
        drm_syncobj_find_fence(
            file,
            args.src_handle,
            args.src_point,
            args.flags as u64,
            &mut fence,
        )
    };
    if ret != 0 {
        unsafe { drm_syncobj_put(destination) };
        return ret;
    }
    let timeline_fence = if args.dst_point != 0 {
        match unwrap_merge_fence(fence) {
            Ok(fence) => Some(fence),
            Err(error) => {
                unsafe {
                    dma_fence_put(fence);
                    drm_syncobj_put(destination);
                }
                return error;
            }
        }
    } else {
        None
    };
    let submit_fence = timeline_fence.as_ref().map_or(fence, |f| f.0);
    let ret = unsafe { drm_syncobj_submit_point(destination, submit_fence, args.dst_point) };
    unsafe {
        dma_fence_put(fence);
        drm_syncobj_put(destination);
    }
    drop(timeline_fence);
    ret
}

/// Linux `drm_syncobj_wait_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_wait_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &mut *data.cast::<DrmSyncobjWait>() };
    let allowed = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
    if args.flags & !allowed != 0 || args.pad != 0 {
        return -EINVAL;
    }
    if args.count_handles == 0 {
        return 0;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    let points = alloc::vec![0u64; objects.len()];
    match wait_syncobjs(
        &objects,
        &points,
        args.flags,
        args.timeout_nsec,
        args.deadline_nsec,
    ) {
        Ok(first) => {
            args.first_signaled = first;
            0
        }
        Err(error) => error,
    }
}

/// Linux `drm_syncobj_timeline_wait_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_timeline_wait_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ_TIMELINE) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &mut *data.cast::<DrmSyncobjTimelineWait>() };
    let allowed = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
    if args.flags & !allowed != 0 || args.pad != 0 {
        return -EINVAL;
    }
    if args.count_handles == 0 {
        return 0;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    let points = match read_points(args.points, objects.len()) {
        Ok(points) => points,
        Err(error) => return error,
    };
    match wait_syncobjs(
        &objects,
        &points,
        args.flags,
        args.timeout_nsec,
        args.deadline_nsec,
    ) {
        Ok(first) => {
            args.first_signaled = first;
            0
        }
        Err(error) => error,
    }
}

/// Linux `drm_syncobj_eventfd_ioctl()`. The kernel's eventfd file-table
/// provider is not exposed to this portable DRM core yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_eventfd_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    _file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ_TIMELINE) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjEventfd>() };
    if args.flags & !DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0 || args.pad != 0 {
        return -EINVAL;
    }
    -EOPNOTSUPP
}

/// Linux `drm_syncobj_reset_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_reset_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjArray>() };
    if args.pad != 0 || args.count_handles == 0 {
        return -EINVAL;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    for object in objects {
        object.replace_fence(None);
    }
    0
}

/// Linux `drm_syncobj_signal_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_signal_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjArray>() };
    if args.pad != 0 || args.count_handles == 0 {
        return -EINVAL;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    let fence = match signaled_stub() {
        Ok(fence) => fence,
        Err(error) => return error,
    };
    for object in objects {
        object.replace_fence(Some(fence.clone()));
    }
    0
}

/// Linux `drm_syncobj_timeline_signal_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_timeline_signal_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ_TIMELINE) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjTimelineArray>() };
    if args.flags != 0 || args.count_handles == 0 {
        return -EINVAL;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let points = match read_points(args.points, handles.len()) {
        Ok(points) => points,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    // Allocate every fence and chain node before publishing any point. A
    // later allocation failure must not leave a prefix of the requested
    // timelines signaled.
    let mut fences = Vec::new();
    let mut chains = Vec::new();
    if fences.try_reserve_exact(objects.len()).is_err()
        || chains.try_reserve_exact(objects.len()).is_err()
    {
        return -ENOMEM;
    }
    for _ in 0..objects.len() {
        let fence = match signaled_stub() {
            Ok(fence) => fence,
            Err(error) => {
                for chain in chains.drain(..) {
                    unsafe { drm_syncobj_chain_free(chain) };
                }
                return error;
            }
        };
        let chain = drm_syncobj_chain_alloc();
        if chain.is_null() {
            for chain in chains.drain(..) {
                unsafe { drm_syncobj_chain_free(chain) };
            }
            return -ENOMEM;
        }
        fences.push(fence);
        chains.push(chain);
    }
    for (index, (object, point)) in objects.iter().zip(points).enumerate() {
        let chain = core::mem::replace(&mut chains[index], ptr::null_mut());
        let ret = object.add_point(chain, fences[index].0, point);
        if ret != 0 {
            unsafe { drm_syncobj_chain_free(chain) };
            for chain in chains.drain(..).filter(|chain| !chain.is_null()) {
                unsafe { drm_syncobj_chain_free(chain) };
            }
            return ret;
        }
    }
    0
}

/// Linux `drm_syncobj_query_ioctl()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_syncobj_query_ioctl(
    dev: *mut DrmDevice,
    data: *mut c_void,
    file: *mut DrmFile,
) -> i32 {
    if !unsafe { feature(dev, DRIVER_SYNCOBJ_TIMELINE) } {
        return -EOPNOTSUPP;
    }
    if data.is_null() || file.is_null() {
        return -EINVAL;
    }
    let args = unsafe { &*data.cast::<DrmSyncobjTimelineArray>() };
    if args.flags & !DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0 || args.count_handles == 0 {
        return -EINVAL;
    }
    let handles = match copy_handles(args.handles, args.count_handles) {
        Ok(handles) => handles,
        Err(error) => return error,
    };
    let objects = match find_syncobjs(file, &handles) {
        Ok(objects) => objects,
        Err(error) => return error,
    };
    if args.points == 0 {
        return -EFAULT;
    }
    for (index, object) in objects.iter().enumerate() {
        let point = chain_query_point(
            object.fence(),
            args.flags & DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0,
        );
        let destination = (args.points as usize as *mut u64).wrapping_add(index);
        if unsafe {
            copy_to_user(
                destination.cast(),
                (&point as *const u64).cast(),
                size_of::<u64>(),
            )
        } != 0
        {
            return -EFAULT;
        }
    }
    0
}
