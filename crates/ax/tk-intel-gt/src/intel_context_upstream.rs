// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
// Source-faithful Rust transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_context.c. The module is registered, and the
// IntelContext data layout is bound below. Remaining i915/GEM/RCU/locking
// operations are integration points, not substitute implementations. Keep
// source order and lifetime/locking edges intact when integrating them.

use core::{
    ffi::{c_ulong, c_void},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

use crate::{
    i915_active_upstream::{
        __i915_active_acquire, __i915_active_init, i915_active_acquire,
        i915_active_acquire_barrier, i915_active_acquire_preallocate_barrier,
        i915_active_add_request, i915_active_fence_set, i915_active_fini, i915_active_release,
    },
    i915_drm_client_upstream::i915_drm_client_add_context_objects,
    i915_gem_context_types_upstream::I915GemContext,
    i915_gem_context_upstream::{fput, i915_gem_context_put},
    i915_gem_object_api_upstream::i915_gem_object_lock,
    i915_gem_ww_upstream::{
        i915_gem_ww_ctx_backoff, i915_gem_ww_ctx_fini, i915_gem_ww_ctx_init,
        i915_gem_ww_unlock_single,
    },
    i915_request_types_upstream::*,
    i915_request_upstream::i915_request_create,
    i915_scheduler_types_upstream::{
        I915Dependency, I915Priolist, I915SchedAttr, I915SchedEngine, I915SchedNode, TaskletStruct,
    },
    i915_sw_fence_upstream::{__i915_sw_fence_init, i915_sw_fence_commit, i915_sw_fence_fini},
    i915_vma_api_upstream::*,
    intel_context_api_upstream as ctx_api,
    intel_context_api_upstream::{
        intel_context_clock, intel_context_has_own_state, intel_context_set_banned,
        intel_context_set_exiting, mutex_lock_interruptible,
    },
    intel_context_types_upstream::*,
    intel_gtt_api_upstream::{i915_vm_get, i915_vm_put},
    intel_ring_upstream::{__intel_ring_pin, intel_ring_pin, intel_ring_unpin},
    intel_sseu_types_upstream::IntelSseu,
    intel_timeline_types_upstream::{I915Syncmap, IntelTimeline},
    intel_timeline_upstream::{__intel_timeline_pin, intel_timeline_pin, intel_timeline_unpin},
    linux::{
        average::{ewma_runtime_init, ewma_runtime_read},
        i915_trace::{
            trace_intel_context_ban, trace_intel_context_create, trace_intel_context_do_pin,
            trace_intel_context_do_unpin, trace_intel_context_free,
        },
    },
};
pub use crate::{
    i915_gem_object_types_upstream::{
        DrmI915GemObject, I915GemObjectMm, I915GemObjectMmo, I915GemObjectPageIter,
        I915GemObjectVma,
    },
    i915_gem_ww_upstream::I915GemWwCtx,
    i915_vma_resource_types_upstream::{I915PageSizes, I915VmaResource},
    i915_vma_types_upstream::I915Vma,
    intel_ggtt_fencing_types_upstream::I915FenceReg,
    intel_gtt_api_upstream::I915AddressSpace,
    intel_ring_types_upstream::IntelRing,
};
pub type IntelWakerefHandle = crate::intel_context_types_upstream::IntelWakerefT;
pub type RefTracker = IntelRefTracker;
use crate::{
    guc_submission::GUC_INVALID_CONTEXT_ID,
    intel_engine_cs_upstream::{
        AtomicT, DelayedWork, ListHead, LlistHead, LlistNode, Mutex, RbNode, RbRoot, RbRootCached,
        Spinlock, WorkStruct,
    },
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_types_upstream::IntelGt,
    linux::gem_memory::IntelMemoryRegion,
    linux_config::*,
    linux_heap::{
        KmCache, SLAB_HWCACHE_ALIGN, kmem_cache_destroy, kmem_cache_free, kmem_cache_zalloc,
    },
    linux_list::*,
};

// The following layout-only types match the wt-dev Linux 7.2.3 x86_64/SMP
// configuration: PREEMPT_RT, LOCKDEP, DEBUG_LOCK_ALLOC, DEBUG_MUTEXES and
// DEBUG_SPINLOCK are unset; MUTEX_SPIN_ON_OWNER is enabled.  They describe
// embedded kernel-owned storage only; operations remain bound to the real
// kernel implementations. CONFIG_DRM_I915_SELFTEST and
// CONFIG_DRM_I915_SW_FENCE_CHECK_DAG are unset, so those conditional fields
// are absent here.

#[repr(C)]
pub struct RefcountT {
    pub refs: AtomicT,
}

#[repr(C)]
pub struct Kref {
    pub refcount: RefcountT,
}

#[repr(C, align(8))]
pub struct RcuHead {
    pub next: *mut RcuHead,
    pub func: Option<unsafe extern "C" fn(*mut RcuHead)>,
}

#[repr(C)]
pub union DmaFenceLock {
    pub extern_lock: *mut Spinlock,
    pub inline_lock: Spinlock,
}

#[repr(C)]
pub union DmaFenceTimestamp {
    pub cb_list: ManuallyDrop<ListHead>,
    pub timestamp: i64,
    pub rcu: ManuallyDrop<RcuHead>,
}

#[repr(C)]
pub struct DmaFence {
    pub lock: DmaFenceLock,
    pub ops: *const c_void,
    pub timestamp_union: DmaFenceTimestamp,
    pub context: u64,
    pub seqno: u64,
    pub flags: c_ulong,
    pub refcount: Kref,
    pub error: i32,
}

impl core::ops::Deref for DmaFence {
    type Target = DmaFenceTimestamp;

    fn deref(&self) -> &Self::Target {
        &self.timestamp_union
    }
}

const _: [(); 64] = [(); core::mem::size_of::<DmaFence>()];
const _: [(); 48] = [(); core::mem::offset_of!(DmaFence, flags)];
const _: [(); 56] = [(); core::mem::offset_of!(DmaFence, refcount)];

#[repr(C)]
pub struct DmaFenceCb {
    pub node: ListHead,
    pub func: Option<unsafe extern "C" fn(*mut DmaFence, *mut DmaFenceCb)>,
}

#[repr(C)]
pub struct WaitQueueHead {
    pub lock: Spinlock,
    pub head: ListHead,
}

#[repr(C)]
pub struct I915ActiveFence {
    pub fence: *mut DmaFence,
    pub cb: DmaFenceCb,
}

#[repr(C)]
pub struct XArray {
    pub xa_lock: Spinlock,
    pub xa_flags: u32,
    pub xa_head: *mut c_void,
}
const _: [(); 16] = [(); core::mem::size_of::<XArray>()];

// Linux v7.2.3 framework records embedded by value in i915_vma and
// i915_request. Their storage is kernel-owned; only their ABI size/alignment
// is needed by this source file.
#[repr(C, align(8))]
pub struct DrmMmNode {
    pub color: c_ulong,
    pub start: u64,
    pub size: u64,
    pub mm: *mut crate::linux::gem_memory::DrmMm,
    pub node_list: ListHead,
    pub hole_stack: ListHead,
    pub rb: RbNode,
    pub rb_hole_size: RbNode,
    pub rb_hole_addr: RbNode,
    pub subtree_last: u64,
    pub hole_size: u64,
    pub subtree_max_hole: u64,
    pub flags: c_ulong,
}
const _: [(); 168] = [(); core::mem::size_of::<DrmMmNode>()];
const _: [(); 160] = [(); core::mem::offset_of!(DrmMmNode, flags)];

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct IntelPartialInfo {
    pub offset: u64,
    pub size: u32,
}

#[repr(C)]
pub union I915GttViewInfo {
    pub partial: IntelPartialInfo,
    _opaque: [u8; 52],
}

#[repr(C)]
pub struct I915GttView {
    pub r#type: u32,
    pub info: I915GttViewInfo,
}
const _: [(); 56] = [(); core::mem::size_of::<I915GttView>()];
const _: [(); 0] = [(); core::mem::offset_of!(I915GttView, r#type)];
const _: [(); 12] = [(); core::mem::size_of::<IntelPartialInfo>()];
const _: [(); 8] = [(); core::mem::offset_of!(IntelPartialInfo, size)];
const _: [(); 4] = [(); core::mem::align_of::<I915GttView>()];
const _: [(); 4] = [(); core::mem::offset_of!(I915GttView, info)];

#[repr(C, align(8))]
pub struct IrqWork {
    pub node: IrqWorkNode,
    pub func: Option<unsafe extern "C" fn(*mut IrqWork)>,
    pub irqwait: *mut c_void,
}

#[repr(C)]
pub struct IrqWorkNode {
    pub next: *mut IrqWorkNode,
    pub flags: AtomicT,
    pub src: u16,
    pub dst: u16,
}

#[repr(C, align(8))]
pub struct Hrtimer {
    _opaque: [u8; 80],
}

#[repr(C, align(8))]
pub struct WaitQueueEntry {
    pub flags: u32,
    pub private: *mut c_void,
    pub func: Option<unsafe extern "C" fn(*mut WaitQueueEntry, u32, i32, *mut c_void) -> i32>,
    pub entry: ListHead,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PinCookie;

// Pointer-only Linux types. These are intentionally incomplete just like C
// forward declarations; fields embedded in the records below have explicit
// source-derived layouts.
#[repr(C)]
pub struct I915VmaOps {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct SgTable {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct SgEntry {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct I915MmapOffset {
    pub vma_node: DrmVmaOffsetNode,
    pub obj: *mut DrmI915GemObject,
    pub mmap_type: i32,
    _pad: [u8; 4],
    pub offset: RbNode,
}
const _: [(); 232] = [(); core::mem::size_of::<I915MmapOffset>()];
const _: [(); 192] = [(); core::mem::offset_of!(I915MmapOffset, obj)];
const _: [(); 208] = [(); core::mem::offset_of!(I915MmapOffset, offset)];

#[repr(C, align(8))]
pub struct DrmVmaOffsetNode {
    _vm_lock: [u8; 8],
    pub vm_node: DrmMmNode,
    pub vm_files: crate::intel_engine_cs_upstream::RbRoot,
    pub driver_private: *mut c_void,
}
const _: [(); 192] = [(); core::mem::size_of::<DrmVmaOffsetNode>()];
const _: [(); 8] = [(); core::mem::offset_of!(DrmVmaOffsetNode, vm_node)];
const _: [(); 176] = [(); core::mem::offset_of!(DrmVmaOffsetNode, vm_files)];
const _: [(); 184] = [(); core::mem::offset_of!(DrmVmaOffsetNode, driver_private)];

#[repr(C)]
pub struct RadixTreeRoot {
    pub height: u32,
    pub gfp_mask: u32,
    pub rnode: *mut c_void,
}
const _: [(); 16] = [(); core::mem::size_of::<RadixTreeRoot>()];

/// Exact Linux v7.2.3 x86_64 `drm_gem_object` fields accessed by the i915 GEM
/// paths. Its 480-byte storage reflects the C union with `ttm_buffer_object`;
/// only the DRM-object member fields used here are named.
#[repr(C, align(8))]
pub struct DrmGemObjectBaseLayout {
    pub refcount: Kref,
    _refcount_padding: [u8; 4],
    pub dev: *mut c_void,
    pub filp: *mut c_void,
    pub vma_node: DrmVmaOffsetNode,
    pub size: u64,
    _name_and_padding: [u8; 8],
    pub dma_buf: *mut c_void,
    pub import_attach: *mut c_void,
    pub resv: *mut c_void,
    pub _resv: crate::linux::gem::DmaResv,
    _gpuva: [u8; 40],
    pub funcs: *const c_void,
    _lru_node: [u8; 16],
    _lru: *mut c_void,
    _union_tail: [u8; 112],
}

const _: [(); 480] = [(); core::mem::size_of::<DrmGemObjectBaseLayout>()];
const _: [(); 8] = [(); core::mem::offset_of!(DrmGemObjectBaseLayout, dev)];
const _: [(); 16] = [(); core::mem::offset_of!(DrmGemObjectBaseLayout, filp)];
const _: [(); 216] = [(); core::mem::offset_of!(DrmGemObjectBaseLayout, size)];
const _: [(); 248] = [(); core::mem::offset_of!(DrmGemObjectBaseLayout, resv)];
const _: [(); 336] = [(); core::mem::offset_of!(DrmGemObjectBaseLayout, funcs)];

static mut SLAB_CE: *mut KmCache = core::ptr::null_mut();

// upstream: intel_context.c intel_context_alloc()
unsafe fn intel_context_alloc() -> *mut IntelContext {
    kmem_cache_zalloc(SLAB_CE, GFP_KERNEL)
}

// upstream: intel_context.c rcu_context_free()
unsafe extern "C" fn rcu_context_free(rcu: *mut RcuHead) {
    let ce = container_of!(rcu, IntelContext, r#ref);

    trace_intel_context_free(ce);
    if intel_context_has_own_state(ce) {
        fput((*ce).default_state.cast());
    }
    kmem_cache_free(SLAB_CE, ce.cast::<c_void>());
}

// upstream: intel_context.c intel_context_free()
pub unsafe fn intel_context_free(ce: *mut IntelContext) {
    call_rcu(
        (&mut (*ce).r#ref.rcu as *mut ManuallyDrop<RcuHead>).cast::<RcuHead>(),
        rcu_context_free,
    );
}

// upstream: intel_context.c intel_context_create()
pub unsafe fn intel_context_create(engine: *mut IntelEngineCs) -> *mut IntelContext {
    let ce = intel_context_alloc();
    if ce.is_null() {
        return ERR_PTR(-ENOMEM);
    }

    intel_context_init(ce, engine);
    trace_intel_context_create(ce);
    ce
}

// upstream: intel_context.c intel_context_alloc_state()
pub unsafe fn intel_context_alloc_state(ce: *mut IntelContext) -> i32 {
    if mutex_lock_interruptible(&mut (*ce).pin_mutex) != 0 {
        return -EINTR;
    }

    let err = (|| {
        let mut ctx: *mut I915GemContext;

        if !test_bit(CONTEXT_ALLOC_BIT, &(*ce).flags) {
            if ctx_api::intel_context_is_banned(ce) {
                return -EIO;
            }

            let err = ((*(*ce).ops)
                .alloc
                .expect("source context ops allocator must be installed"))(ce);
            if unlikely(err != 0) {
                return err;
            }

            set_bit(CONTEXT_ALLOC_BIT, &mut (*ce).flags);

            rcu_read_lock();
            ctx = rcu_dereference!((*ce).gem_context);
            if !ctx.is_null() && !kref_get_unless_zero(&mut (*ctx).r#ref) {
                ctx = core::ptr::null_mut();
            }
            rcu_read_unlock();
            if !ctx.is_null() {
                if !(*ctx).client.is_null() {
                    i915_drm_client_add_context_objects((*ctx).client, ce);
                }
                i915_gem_context_put(ctx);
            }
        }

        0
    })();
    mutex_unlock(&mut (*ce).pin_mutex);
    err
}

// upstream: intel_context.c intel_context_active_acquire()
unsafe fn intel_context_active_acquire(ce: *mut IntelContext) -> i32 {
    __i915_active_acquire(&mut (*ce).active);

    if ctx_api::intel_context_is_barrier(ce)
        || intel_engine_uses_guc((*ce).engine)
        || ctx_api::intel_context_is_parallel(ce)
    {
        return 0;
    }

    // Preallocate tracking nodes.
    let err = i915_active_acquire_preallocate_barrier(&mut (*ce).active, (*ce).engine);
    if err != 0 {
        i915_active_release(&mut (*ce).active);
    }
    err
}

// upstream: intel_context.c intel_context_active_release()
unsafe fn intel_context_active_release(ce: *mut IntelContext) {
    // Nodes preallocated in intel_context_active().
    i915_active_acquire_barrier(&mut (*ce).active);
    i915_active_release(&mut (*ce).active);
}

// upstream: intel_context.c __context_pin_state()
unsafe fn __context_pin_state(vma: *mut I915Vma, ww: *mut I915GemWwCtx) -> i32 {
    let bias = i915_ggtt_pin_bias(vma) | PIN_OFFSET_BIAS as u32;
    let err = i915_ggtt_pin(vma, ww, 0, bias | PIN_HIGH as u32);
    if err != 0 {
        return err;
    }

    let err = i915_active_acquire(&mut (*vma).active);
    if err != 0 {
        i915_vma_unpin(vma);
        return err;
    }

    // Mark it globally pinned so the shrinker cannot reclaim it before release.
    i915_vma_make_unshrinkable(vma);
    (*(*vma).obj).mm.set_dirty(true);
    0
}

// upstream: intel_context.c __context_unpin_state()
unsafe fn __context_unpin_state(vma: *mut I915Vma) {
    i915_vma_make_shrinkable(vma);
    i915_active_release(&mut (*vma).active);
    __i915_vma_unpin(vma);
}

// upstream: intel_context.c __ring_active()
unsafe fn __ring_active(ring: *mut IntelRing, ww: *mut I915GemWwCtx) -> i32 {
    let err = intel_ring_pin(ring, ww);
    if err != 0 {
        return err;
    }

    let err = i915_active_acquire(&mut (*(*ring).vma).active);
    if err != 0 {
        intel_ring_unpin(ring);
        return err;
    }

    0
}

// upstream: intel_context.c __ring_retire()
unsafe fn __ring_retire(ring: *mut IntelRing) {
    i915_active_release(&mut (*(*ring).vma).active);
    intel_ring_unpin(ring);
}

// upstream: intel_context.c intel_context_pre_pin()
unsafe fn intel_context_pre_pin(ce: *mut IntelContext, ww: *mut I915GemWwCtx) -> i32 {
    CE_TRACE!(ce, "active\n");

    let err = __ring_active((*ce).ring, ww);
    if err != 0 {
        return err;
    }

    let err = intel_timeline_pin((*ce).timeline, ww);
    if err != 0 {
        __ring_retire((*ce).ring);
        return err;
    }

    if (*ce).state.is_null() {
        return 0;
    }

    let err = __context_pin_state((*ce).state, ww);
    if err != 0 {
        intel_timeline_unpin((*ce).timeline);
        __ring_retire((*ce).ring);
        return err;
    }
    0
}

// upstream: intel_context.c intel_context_post_unpin()
unsafe fn intel_context_post_unpin(ce: *mut IntelContext) {
    if !(*ce).state.is_null() {
        __context_unpin_state((*ce).state);
    }

    intel_timeline_unpin((*ce).timeline);
    __ring_retire((*ce).ring);
}

// upstream: intel_context.c __intel_context_do_pin_ww()
pub unsafe fn __intel_context_do_pin_ww(ce: *mut IntelContext, ww: *mut I915GemWwCtx) -> i32 {
    let mut handoff = false;
    let mut vaddr: *mut c_void = core::ptr::null_mut();
    let mut err = 0;

    if unlikely(!test_bit(CONTEXT_ALLOC_BIT, &(*ce).flags)) {
        err = intel_context_alloc_state(ce);
        if err != 0 {
            return err;
        }
    }

    // Always pin context/ring/timeline here to hold a reference for
    // __intel_context_active(), avoiding pin_mutex versus dma_resv_lock inversion.
    err = i915_gem_object_lock((*(*(*ce).timeline).hwsp_ggtt).obj, ww);
    if err == 0 {
        err = i915_gem_object_lock((*(*(*ce).ring).vma).obj, ww);
    }
    if err == 0 && !(*ce).state.is_null() {
        err = i915_gem_object_lock((*(*ce).state).obj, ww);
    }
    if err == 0 {
        err = intel_context_pre_pin(ce, ww);
    }
    if err != 0 {
        return err;
    }

    err = ((*(*ce).ops)
        .pre_pin
        .expect("IntelContextOps.pre_pin is required"))(ce, ww, &mut vaddr);
    if err != 0 {
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    err = i915_active_acquire(&mut (*ce).active);
    if err != 0 {
        ((*(*ce).ops)
            .post_unpin
            .expect("IntelContextOps.post_unpin is required"))(ce);
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    err = mutex_lock_interruptible(&mut (*ce).pin_mutex);
    if err != 0 {
        i915_active_release(&mut (*ce).active);
        ((*(*ce).ops)
            .post_unpin
            .expect("IntelContextOps.post_unpin is required"))(ce);
        intel_context_post_unpin(ce);
        i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
        return err;
    }

    intel_engine_pm_might_get((*ce).engine);

    if unlikely(ctx_api::intel_context_is_closed(ce)) {
        err = -ENOENT;
    } else if likely(!atomic_add_unless(&mut (*ce).pin_count, 1, 0)) {
        err = intel_context_active_acquire(ce);
        if err == 0 {
            err = ((*(*ce).ops).pin.expect("IntelContextOps.pin is required"))(ce, vaddr);
            if err != 0 {
                intel_context_active_release(ce);
            } else {
                CE_TRACE!(
                    ce,
                    "pin ring:{start:%08x, head:%04x, tail:%04x}\n",
                    i915_ggtt_offset((*(*ce).ring).vma),
                    (*(*ce).ring).head,
                    (*(*ce).ring).tail,
                );

                handoff = true;
                crate::linux::primitives::mb(); // smp_mb__before_atomic(): publish pin before visibility.
                atomic_inc(&mut (*ce).pin_count);
            }
        }
    }

    if err == 0 {
        GEM_BUG_ON!(!ctx_api::intel_context_is_pinned(ce)); // No overflow.
        trace_intel_context_do_pin(ce);
    }

    mutex_unlock(&mut (*ce).pin_mutex);
    i915_active_release(&mut (*ce).active);
    if !handoff {
        ((*(*ce).ops)
            .post_unpin
            .expect("IntelContextOps.post_unpin is required"))(ce);
    }
    intel_context_post_unpin(ce);

    // Unlock the shared hwsp_ggtt object. The other locked global state is
    // pinned and stays resident until explicitly unpinned.
    i915_gem_ww_unlock_single((*(*(*ce).timeline).hwsp_ggtt).obj);
    err
}

// upstream: intel_context.c __intel_context_do_pin()
pub unsafe fn __intel_context_do_pin(ce: *mut IntelContext) -> i32 {
    let mut ww = I915GemWwCtx::default();
    i915_gem_ww_ctx_init(&mut ww, true);
    let mut err;
    loop {
        err = __intel_context_do_pin_ww(ce, &mut ww);
        if err != -EDEADLK {
            break;
        }
        err = i915_gem_ww_ctx_backoff(&mut ww);
        if err != 0 {
            break;
        }
    }
    i915_gem_ww_ctx_fini(&mut ww);
    err
}

// upstream: intel_context.c __intel_context_do_unpin()
pub unsafe fn __intel_context_do_unpin(ce: *mut IntelContext, sub: i32) {
    if !atomic_sub_and_test(sub, &mut (*ce).pin_count) {
        return;
    }

    CE_TRACE!(ce, "unpin\n");
    ((*(*ce).ops)
        .unpin
        .expect("IntelContextOps.unpin is required"))(ce);
    ((*(*ce).ops)
        .post_unpin
        .expect("IntelContextOps.post_unpin is required"))(ce);

    // Keep an extra reference: active_release() may asynchronously drop the
    // only reference keeping this context alive.
    ctx_api::intel_context_get(ce);
    intel_context_active_release(ce);
    trace_intel_context_do_unpin(ce);
    ctx_api::intel_context_put(ce);
}

// upstream: intel_context.c __intel_context_retire()
unsafe extern "C" fn __intel_context_retire(active: *mut I915Active) {
    let ce = container_of!(active, IntelContext, active);

    CE_TRACE!(
        ce,
        "retire runtime: {{ total:%lluns, avg:%lluns }}\n",
        intel_context_get_total_runtime_ns(ce),
        intel_context_get_avg_runtime_ns(ce),
    );

    set_bit(CONTEXT_VALID_BIT, &mut (*ce).flags);
    intel_context_post_unpin(ce);
    ctx_api::intel_context_put(ce);
}

// upstream: intel_context.c __intel_context_active()
unsafe extern "C" fn __intel_context_active(active: *mut I915Active) -> i32 {
    let ce = container_of!(active, IntelContext, active);

    ctx_api::intel_context_get(ce);

    // Everything should already be activated by intel_context_pre_pin().
    GEM_WARN_ON!(!i915_active_acquire_if_busy(
        &mut (*(*(*ce).ring).vma).active,
    ));
    __intel_ring_pin((*ce).ring);

    __intel_timeline_pin((*ce).timeline);

    if !(*ce).state.is_null() {
        GEM_WARN_ON!(!i915_active_acquire_if_busy(&mut (*(*ce).state).active));
        __i915_vma_pin((*ce).state);
        i915_vma_make_unshrinkable((*ce).state);
    }

    0
}

// upstream: intel_context.c sw_fence_dummy_notify()
unsafe extern "C" fn sw_fence_dummy_notify(
    _sf: *mut I915SwFence,
    _state: I915SwFenceNotify,
) -> i32 {
    NOTIFY_DONE
}

// upstream: intel_context.c intel_context_init()
pub unsafe fn intel_context_init(ce: *mut IntelContext, engine: *mut IntelEngineCs) {
    GEM_BUG_ON!((*engine).cops.is_null());
    GEM_BUG_ON!((*(*engine).gt).vm.is_null());

    kref_init((&mut (*ce).r#ref.refcount as *mut ManuallyDrop<Kref>).cast::<Kref>());

    (*ce).engine = engine;
    (*ce).ops = (*engine).cops.cast::<IntelContextOps>();
    (*ce).sseu = (*engine).sseu;
    (*ce).ring = core::ptr::null_mut();
    (*ce).ring_size = SZ_4K as u32;

    ewma_runtime_init(&mut (*ce).stats.runtime.avg);

    (*ce).vm = i915_vm_get((*(*engine).gt).vm.cast::<I915AddressSpace>());

    // signal_link/lock is used under RCU.
    spin_lock_init(&mut (*ce).signal_lock);
    INIT_LIST_HEAD(&mut (*ce).signals);

    mutex_init(&mut (*ce).pin_mutex);

    spin_lock_init(&mut (*ce).guc_state.lock);
    INIT_LIST_HEAD(&mut (*ce).guc_state.fences);
    INIT_LIST_HEAD(&mut (*ce).guc_state.requests);

    (*ce).guc_id.id = GUC_INVALID_CONTEXT_ID as u16;
    INIT_LIST_HEAD(&mut (*ce).guc_id.link);

    INIT_LIST_HEAD(&mut (*ce).destroyed_link);

    INIT_LIST_HEAD(
        (&mut (*ce).parallel.children.child_list as *mut ManuallyDrop<ListHead>).cast::<ListHead>(),
    );

    // Initialize fence as complete unless schedule-disable is pending.
    __i915_sw_fence_init(
        &mut (*ce).guc_state.blocked,
        Some(sw_fence_dummy_notify),
        core::ptr::null(),
        core::ptr::null_mut(),
    );
    i915_sw_fence_commit(&mut (*ce).guc_state.blocked);

    __i915_active_init(
        &mut (*ce).active,
        Some(__intel_context_active),
        Some(__intel_context_retire),
        0,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
}

// upstream: intel_context.c intel_context_fini()
pub unsafe fn intel_context_fini(ce: *mut IntelContext) {
    let mut child: *mut IntelContext;
    let mut next: *mut IntelContext;

    if !(*ce).timeline.is_null() {
        intel_timeline_put((*ce).timeline);
    }
    i915_vm_put((*ce).vm);

    // Drop the creation reference held for each child.
    if ctx_api::intel_context_is_parent(ce) {
        for_each_child_safe!(ce, child, next, {
            ctx_api::intel_context_put(child);
        });
    }

    mutex_destroy(&mut (*ce).pin_mutex);
    i915_active_fini(&mut (*ce).active);
    i915_sw_fence_fini(&mut (*ce).guc_state.blocked);
}

// upstream: intel_context.c i915_context_module_exit()
pub unsafe fn i915_context_module_exit() {
    kmem_cache_destroy(SLAB_CE);
}

// upstream: intel_context.c i915_context_module_init()
pub unsafe fn i915_context_module_init() -> i32 {
    SLAB_CE = KMEM_CACHE!(IntelContext, SLAB_HWCACHE_ALIGN);
    if SLAB_CE.is_null() {
        return -ENOMEM;
    }

    0
}

// upstream: intel_context.c intel_context_enter_engine()
pub unsafe extern "C" fn intel_context_enter_engine(ce: *mut IntelContext) {
    intel_engine_pm_get((*ce).engine);
    intel_timeline_enter((*ce).timeline);
}

// upstream: intel_context.c intel_context_exit_engine()
pub unsafe extern "C" fn intel_context_exit_engine(ce: *mut IntelContext) {
    intel_timeline_exit((*ce).timeline);
    intel_engine_pm_put((*ce).engine);
}

// upstream: intel_context.c intel_context_prepare_remote_request()
pub unsafe fn intel_context_prepare_remote_request(
    ce: *mut IntelContext,
    rq: *mut I915Request,
) -> i32 {
    let tl = (*ce).timeline;

    // This function is only suitable for remotely modifying this context.
    GEM_BUG_ON!((*rq).context == ce);

    if unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*rq).timeline)) } != tl {
        // Timeline sharing: queue this switch after current activity.
        let err = i915_active_fence_set(&mut (*tl).last_request, rq);
        if err != 0 {
            return err;
        }
    }

    // Keep context image and timeline pinned until modifying request retires;
    // transfer the already-pinned ce reference into the tracked active request.
    GEM_BUG_ON!(i915_active_is_idle(&mut (*ce).active));
    i915_active_add_request(&mut (*ce).active, rq)
}

// upstream: intel_context.c intel_context_create_request()
pub unsafe fn intel_context_create_request(ce: *mut IntelContext) -> *mut I915Request {
    let mut ww = I915GemWwCtx::default();
    let mut rq: *mut I915Request;
    let mut err;

    i915_gem_ww_ctx_init(&mut ww, true);
    loop {
        err = ctx_api::intel_context_pin_ww(ce, &mut ww);
        if err == 0 {
            rq = i915_request_create(ce);
            ctx_api::intel_context_unpin(ce);
            break;
        } else if err == -EDEADLK {
            err = i915_gem_ww_ctx_backoff(&mut ww);
            if err == 0 {
                continue;
            }
            rq = ERR_PTR(err);
            break;
        } else {
            rq = ERR_PTR(err);
            break;
        }
    }

    i915_gem_ww_ctx_fini(&mut ww);

    if IS_ERR(rq) {
        return rq;
    }

    // timeline->mutex is logically inner but used as outer; retain the
    // selftest lockdep workaround and its exact order.
    // CONFIG_LOCKDEP=n: Linux expands lockdep_unpin_lock() to no code.
    mutex_release!(&mut (*(*ce).timeline).mutex.dep_map, _RET_IP_);
    mutex_acquire!(
        &mut (*(*ce).timeline).mutex.dep_map,
        SINGLE_DEPTH_NESTING,
        0,
        _RET_IP_,
    );
    // CONFIG_LOCKDEP=n: Linux lockdep_pin_lock() yields NIL_COOKIE.
    (*rq).cookie = PinCookie;

    rq
}

// upstream: intel_context.c intel_context_get_active_request()
pub unsafe fn intel_context_get_active_request(ce: *mut IntelContext) -> *mut I915Request {
    let parent = ctx_api::intel_context_to_parent(ce);
    let mut rq: *mut I915Request = core::ptr::null_mut();
    let mut active: *mut I915Request = core::ptr::null_mut();
    let mut flags = 0;

    GEM_BUG_ON!(!intel_engine_uses_guc((*ce).engine));

    // The parent list includes all contexts in this relationship, so compare
    // each request's context while searching newest-to-oldest.
    spin_lock_irqsave(&mut (*parent).guc_state.lock, &mut flags);
    list_for_each_entry_reverse!(rq, &(*parent).guc_state.requests, sched.link, {
        if (*rq).context != ce {
            continue;
        }
        if i915_request_completed(rq) {
            break;
        }

        active = rq;
    });
    if !active.is_null() {
        active = i915_request_get_rcu(active);
    }
    spin_unlock_irqrestore(&mut (*parent).guc_state.lock, flags);

    active
}

// upstream: intel_context.c intel_context_bind_parent_child()
pub unsafe fn intel_context_bind_parent_child(parent: *mut IntelContext, child: *mut IntelContext) {
    // Caller validates usage; keep the upstream assertions as the contract.
    GEM_BUG_ON!(ctx_api::intel_context_is_pinned(parent));
    GEM_BUG_ON!(ctx_api::intel_context_is_child(parent));
    GEM_BUG_ON!(ctx_api::intel_context_is_pinned(child));
    GEM_BUG_ON!(ctx_api::intel_context_is_child(child));
    GEM_BUG_ON!(ctx_api::intel_context_is_parent(child));

    (*parent).parallel.child_index = (*parent).parallel.number_children;
    (*parent).parallel.number_children += 1;
    list_add_tail(
        (&mut (*child).parallel.children.child_link as *mut ManuallyDrop<ListHead>)
            .cast::<ListHead>(),
        (&mut (*parent).parallel.children.child_list as *mut ManuallyDrop<ListHead>)
            .cast::<ListHead>(),
    );
    (*child).parallel.parent = parent;
}

// upstream: intel_context.c intel_context_get_total_runtime_ns()
pub unsafe fn intel_context_get_total_runtime_ns(ce: *mut IntelContext) -> u64 {
    if let Some(update_stats) = (*(*ce).ops).update_stats {
        update_stats(ce);
    }

    let mut total = (*ce).stats.runtime.total;
    if (*(*ce).ops).flags & COPS_RUNTIME_CYCLES != 0 {
        total *= (*(*(*ce).engine).gt).clock_period_ns as u64;
    }

    let mut active = READ_ONCE!((*ce).stats.active);
    if active != 0 {
        active = intel_context_clock() - active;
    }

    total + active
}

// upstream: intel_context.c intel_context_get_avg_runtime_ns()
pub unsafe fn intel_context_get_avg_runtime_ns(ce: *mut IntelContext) -> u64 {
    let mut avg = ewma_runtime_read(&(*ce).stats.runtime.avg);

    if (*(*ce).ops).flags & COPS_RUNTIME_CYCLES != 0 {
        avg *= (*(*(*ce).engine).gt).clock_period_ns as u64;
    }

    avg
}

// upstream: intel_context.c intel_context_ban()
pub unsafe fn intel_context_ban(ce: *mut IntelContext, rq: *mut I915Request) -> bool {
    let ret = intel_context_set_banned(ce);

    trace_intel_context_ban(ce);

    if let Some(revoke) = (*(*ce).ops).revoke {
        revoke(ce, rq, INTEL_CONTEXT_BANNED_PREEMPT_TIMEOUT_MS);
    }

    ret
}

// upstream: intel_context.c intel_context_revoke()
pub unsafe fn intel_context_revoke(ce: *mut IntelContext) -> bool {
    let ret = intel_context_set_exiting(ce);

    if let Some(revoke) = (*(*ce).ops).revoke {
        revoke(
            ce,
            core::ptr::null_mut(),
            (*(*ce).engine).props.preempt_timeout_ms as u32,
        );
    }

    ret
}

// Upstream's trailing CONFIG_DRM_I915_SELFTEST include is not an intel_context.c
// function; the test implementation remains in the separate source file.
