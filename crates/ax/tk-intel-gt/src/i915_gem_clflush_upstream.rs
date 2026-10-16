// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// Copyright © 2019 Intel Corporation.
//
//! Linux v7.2.3 i915 GEM clflush and fence-work translations.  Cache flushing
//! uses the target x86 CLFLUSH/CLFLUSHOPT instructions; fence-work records are
//! dma-fences on the kapi fence core (`tk_linux_kapi::dma_fence`), and the
//! reservation entries they attach are kapi `dma_resv` records.

#![allow(unsafe_code, non_camel_case_types, non_snake_case)]

use core::{
    ffi::{c_char, c_int, c_ulong, c_void},
    mem::size_of,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use tk_linux_kapi::{
    dma_fence::{dma_fence_free, dma_fence_get, dma_fence_init, dma_fence_put, dma_fence_set_error, dma_fence_signal},
    dma_resv::{dma_resv_add_fence, dma_resv_reserve_fences},
};

use crate::{
    i915_gem_object_api_upstream::{
        i915_gem_object_get, i915_gem_object_has_pages, i915_gem_object_put,
        i915_gem_object_unpin_pages,
    },
    i915_gem_object_header_upstream::assert_object_held,
    i915_gem_object_types_upstream::{DrmI915GemObject, I915_BO_CACHE_COHERENT_FOR_READ},
    i915_gem_object_upstream::i915_gem_object_has_struct_page,
    i915_gem_pages_upstream::__i915_gem_object_get_pages,
    i915_request_upstream::DmaFenceOps,
    i915_sw_fence_upstream::{
        __i915_sw_fence_await_dma_fence, I915SwDmaFenceCb, i915_sw_fence_await_reservation,
        i915_sw_fence_commit, i915_sw_fence_fini, i915_sw_fence_init,
    },
    intel_context_types_upstream::{I915SwFence, I915SwFenceNotify},
    intel_context_upstream::DmaFence,
    intel_engine_cs_upstream::{Spinlock, WorkStruct},
    linux::{
        fields::{
            i915_gem_object_cache_coherent, i915_gem_object_cache_dirty,
            i915_gem_object_set_cache_dirty,
        },
        gem::DmaResv,
        highmem::page_address,
        i915::{IS_DGFX, to_i915},
        locks::spin_lock_init,
        memory::{__GFP_RETRY_MAYFAIL, kfree},
        workqueue::{self, INIT_WORK_C},
    },
    linux_config::{__GFP_NOWARN, DMA_RESV_USAGE_KERNEL, GFP_KERNEL},
};

const I915_CLFLUSH_FORCE: u32 = 1 << 0;
const I915_CLFLUSH_SYNC: u32 = 1 << 1;
const I915_GEM_DOMAIN_CPU: u32 = 1;
const ORIGIN_CPU: u32 = 0;
const DMA_FENCE_FLAG_USER_BITS: u32 = 6;
const DMA_FENCE_WORK_IMM: u32 = DMA_FENCE_FLAG_USER_BITS;
const ENOMEM: c_int = 12;
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




static DRIVER_NAME: &[u8] = b"dma-fence\0";
static WORK_NAME: &[u8] = b"work\0";
static CLFLUSH_NAME: &[u8] = b"clflush\0";

const _: [(); 64] = [(); size_of::<DmaFence>()];

























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

/// Reserve capacity in the native reservation list. The backing allocation is
/// real and all later fence insertion paths retain a reference.

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
    let i915 = unsafe { to_i915((&(*obj).base.base).dev.cast()) };
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
                I915_FENCE_GFP as u32,
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

