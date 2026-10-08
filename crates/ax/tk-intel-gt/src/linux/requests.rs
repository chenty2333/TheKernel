// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation and TheKernel contributors.
// Linux v7.2.3 drivers/gpu/drm/i915/i915_request.h and dma-fence helpers.

#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicI32, Ordering},
};

use crate::{
    i915_request_types_upstream::I915Request,
    intel_context_upstream::{DmaFence, I915Active, I915GemWwCtx},
    intel_ring_types_upstream::IntelRing,
    linux::{
        contexts::IntelContextPtr,
        memory::{atomic_add_unless, atomic_inc, atomic_read, kref_get_unless_zero},
    },
};

pub const DMA_FENCE_FLAG_SIGNALED_BIT: u32 = 3;
pub const DMA_FENCE_FLAG_TIMESTAMP_BIT: u32 = 4;
pub const DMA_FENCE_FLAG_USER_BITS: u32 = 6;
pub const I915_FENCE_FLAG_ACTIVE: u32 = 6;
pub const I915_FENCE_FLAG_PQUEUE: u32 = 7;
pub const I915_FENCE_FLAG_HOLD: u32 = 8;
pub const I915_FENCE_FLAG_INITIAL_BREADCRUMB: u32 = 9;
pub const I915_FENCE_FLAG_SIGNAL: u32 = 10;
pub const I915_FENCE_FLAG_NOPREEMPT: u32 = 11;
pub const I915_FENCE_FLAG_SENTINEL: u32 = 12;
pub const I915_FENCE_FLAG_BOOST: u32 = 13;
pub const I915_FENCE_FLAG_SUBMIT_PARALLEL: u32 = 14;
pub const I915_FENCE_FLAG_SKIP_PARALLEL: u32 = 15;
pub const I915_FENCE_FLAG_COMPOSITE: u32 = 16;

/// `dma_fence_get()` requires the caller to already own a live fence
/// reference. Null remains interoperable with null request/fence pointers.
pub fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence {
    if fence.is_null() {
        return fence;
    }
    let got =
        unsafe { crate::linux_memory::refcount_inc_not_zero(&mut (*fence).refcount.refcount) };
    assert!(got, "dma_fence_get requires an owned live fence reference");
    fence
}

/// `dma_fence_get_rcu()` is called inside the caller's RCU read-side section;
/// it returns null if the final reference has already been dropped.
pub fn dma_fence_get_rcu(fence: *mut DmaFence) -> *mut DmaFence {
    if fence.is_null() {
        return fence;
    }
    unsafe {
        if kref_get_unless_zero(&mut (*fence).refcount) {
            fence
        } else {
            core::ptr::null_mut()
        }
    }
}

pub trait I915RequestPtr {
    fn i915_request_ptr(self) -> *mut I915Request;
}

/// `intel_ring_advance()` from gt/intel_ring.h is intentionally a runtime
/// no-op apart from asserting that the caller emitted at the reserved end.
pub unsafe fn intel_ring_advance<R: I915RequestPtr>(request: R, cs: *mut u32) {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    let ring: *mut IntelRing = unsafe { (*request).ring };
    assert!(!ring.is_null());
    let expected = unsafe {
        (*ring)
            .vaddr
            .cast::<u8>()
            .add((*ring).emit as usize)
            .cast::<u32>()
    };
    assert_eq!(
        expected, cs,
        "intel_ring_advance does not match reserved span"
    );
}

/// C's `struct i915_active *` is also passed as a mutable reference by the
/// translated inline helpers.
pub trait I915ActivePtr {
    fn i915_active_ptr(self) -> *mut I915Active;
}
impl I915ActivePtr for *mut I915Active {
    fn i915_active_ptr(self) -> *mut I915Active {
        self
    }
}
impl I915ActivePtr for *const I915Active {
    fn i915_active_ptr(self) -> *mut I915Active {
        self.cast_mut()
    }
}
impl I915ActivePtr for &I915Active {
    fn i915_active_ptr(self) -> *mut I915Active {
        (self as *const I915Active).cast_mut()
    }
}
impl I915ActivePtr for &mut I915Active {
    fn i915_active_ptr(self) -> *mut I915Active {
        self
    }
}

/// `i915_active_is_idle()` from i915_active.h.
#[inline]
pub fn i915_active_is_idle<A: I915ActivePtr>(active: A) -> bool {
    let active = active.i915_active_ptr();
    assert!(!active.is_null());
    unsafe { atomic_read(&(*active).count) == 0 }
}

/// `i915_active_acquire_if_busy()` from i915_active.c. It succeeds only when
/// the tracker already has an active reference and never invokes activation.
#[inline]
pub fn i915_active_acquire_if_busy<A: I915ActivePtr>(active: A) -> bool {
    let active = active.i915_active_ptr();
    assert!(!active.is_null());
    unsafe { atomic_add_unless(&mut (*active).count, 1, 0) }
}

/// `intel_context_pin_if_active()` from intel_context.h.
#[inline]
pub fn intel_context_pin_if_active<C: IntelContextPtr>(context: C) -> bool {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { atomic_add_unless(&mut (*context).pin_count, 1, 0) }
}

/// `intel_context_pin()` from intel_context.h; the slow path preserves the
/// source translation's GEM/WW locking and context activation sequence.
#[inline]
pub fn intel_context_pin<C: IntelContextPtr>(context: C) -> i32 {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    if intel_context_pin_if_active(context) {
        0
    } else {
        unsafe { crate::intel_context_upstream::__intel_context_do_pin(context) }
    }
}

/// `intel_context_pin_ww()` from intel_context.h. The caller supplies the
/// real initialized wound/wait context; this helper does not manufacture one.
#[inline]
pub unsafe fn intel_context_pin_ww<C: IntelContextPtr>(context: C, ww: *mut I915GemWwCtx) -> i32 {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    if intel_context_pin_if_active(context) {
        0
    } else {
        unsafe { crate::intel_context_upstream::__intel_context_do_pin_ww(context, ww) }
    }
}

/// `__intel_context_pin()` asserts that another pin is already owned, then
/// takes the extra pin reference.
#[inline]
pub fn __intel_context_pin<C: IntelContextPtr>(context: C) {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    assert!(crate::linux::contexts::intel_context_is_pinned(context));
    unsafe { atomic_inc(&mut (*context).pin_count) };
}

/// `intel_context_unpin()` releases exactly one pin reference.
#[inline]
pub fn intel_context_unpin<C: IntelContextPtr>(context: C) {
    let context = context.intel_context_ptr();
    assert!(!context.is_null());
    unsafe { crate::intel_context_upstream::__intel_context_do_unpin(context, 1) };
}
impl I915RequestPtr for *mut I915Request {
    fn i915_request_ptr(self) -> *mut I915Request {
        self
    }
}
impl I915RequestPtr for *const I915Request {
    fn i915_request_ptr(self) -> *mut I915Request {
        self.cast_mut()
    }
}
impl I915RequestPtr for &I915Request {
    fn i915_request_ptr(self) -> *mut I915Request {
        (self as *const I915Request).cast_mut()
    }
}
impl I915RequestPtr for &mut I915Request {
    fn i915_request_ptr(self) -> *mut I915Request {
        self
    }
}

#[inline]
pub fn i915_seqno_passed(seq1: u32, seq2: u32) -> bool {
    seq1.wrapping_sub(seq2) as i32 >= 0
}

/// Linux `__hwsp_seqno()`: load the request's HWSP pointer and then the
/// device-updated seqno while the caller holds the RCU read-side section.
unsafe fn read_hwsp_seqno(request: *const I915Request) -> Option<u32> {
    if request.is_null() {
        return None;
    }
    // SAFETY: the caller holds RCU, so the source HWSP lifetime is pinned.
    let hwsp = unsafe { ptr::read_volatile(ptr::addr_of!((*request).hwsp_seqno)) };
    if hwsp.is_null() {
        None
    } else {
        // SAFETY: Linux's HWSP allocation stays alive under the RCU lock held
        // by `hwsp_seqno`, and READ_ONCE maps to this volatile load.
        Some(unsafe { ptr::read_volatile(hwsp) })
    }
}

#[inline]
pub fn hwsp_seqno<R: I915RequestPtr>(request: R) -> u32 {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::rcu::rcu_read_lock();
    let seqno = unsafe { read_hwsp_seqno(request) };
    crate::linux::rcu::rcu_read_unlock();
    seqno.expect("live i915 request has no HWSP seqno pointer")
}

#[inline]
pub fn __i915_request_is_complete<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    unsafe { i915_seqno_passed(hwsp_seqno(request), (*request).fence.seqno as u32) }
}

#[inline]
pub fn i915_request_signaled<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe {
        &(*request).fence.flags
    })
}

#[inline]
fn request_error_is_fatal(error: i32) -> bool {
    error != 0 && error != -crate::linux_config::EAGAIN && error != -crate::linux_config::ETIMEDOUT
}

/// `i915_request_set_error_once()` from i915_request.c.
pub fn i915_request_set_error_once<R: I915RequestPtr>(request: R, error: i32) -> bool {
    assert!(
        error < 0,
        "GEM_BUG_ON: request error must be a negative errno"
    );
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    if i915_request_signaled(request) {
        return false;
    }
    let error_ptr = unsafe { ptr::addr_of_mut!((*request).fence.error) };
    let error_atomic = unsafe { &*error_ptr.cast::<AtomicI32>() };
    let mut old = error_atomic.load(Ordering::Relaxed);
    loop {
        if request_error_is_fatal(old) {
            return false;
        }
        match error_atomic.compare_exchange_weak(old, error, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => old = current,
        }
    }
}

/// `i915_request_mark_complete()` from i915_request.h. Point HWSP polling at
/// the little-endian low dword of the fence seqno, decoupling completion from
/// the request's mapped HWSP storage.
pub fn i915_request_mark_complete<R: I915RequestPtr>(request: R) {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    unsafe {
        let seqno = ptr::addr_of!((*request).fence.seqno).cast::<u32>();
        ptr::addr_of_mut!((*request).hwsp_seqno).write_volatile(seqno);
    }
}

/// `i915_request_mark_eio()` from i915_request.c. The input reference is a
/// live caller-owned request; returned non-null ownership carries a new fence
/// reference, while already-complete requests return null.
pub fn i915_request_mark_eio(request: &mut I915Request) -> *mut I915Request {
    if __i915_request_is_complete(&*request) {
        return ptr::null_mut();
    }
    assert!(
        !i915_request_signaled(&*request),
        "GEM_BUG_ON: request is signaled"
    );
    let request = i915_request_get(&mut *request);
    i915_request_set_error_once(request, -crate::linux_config::EIO);
    i915_request_mark_complete(request);
    request
}

/// `__i915_request_has_started()` from i915_request.h.
pub fn __i915_request_has_started<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    let target = unsafe { ((*request).fence.seqno as u32).wrapping_sub(1) };
    i915_seqno_passed(hwsp_seqno(request), target)
}

/// `i915_request_on_hold()` from i915_request.h.
pub fn i915_request_on_hold<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(I915_FENCE_FLAG_HOLD, unsafe { &(*request).fence.flags })
}

/// Linux 7.2.3 `i915_request_started()` with its source RCU recheck around
/// the HWSP read. A completed request is considered started.
pub fn i915_request_started<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    if i915_request_signaled(request) {
        return true;
    }
    crate::linux::rcu::rcu_read_lock();
    let started = if i915_request_signaled(request) {
        true
    } else {
        __i915_request_has_started(request)
    };
    crate::linux::rcu::rcu_read_unlock();
    started
}

/// Linux 7.2.3 `i915_request_has_sentinel()`.
pub fn i915_request_has_sentinel<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(I915_FENCE_FLAG_SENTINEL, unsafe { &(*request).fence.flags })
}

#[inline]
pub fn i915_request_completed<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    if i915_request_signaled(request) {
        return true;
    }

    crate::linux::rcu::rcu_read_lock();
    let complete = if i915_request_signaled(request) {
        true
    } else {
        __i915_request_is_complete(request)
    };
    crate::linux::rcu::rcu_read_unlock();
    complete
}

#[inline]
pub fn i915_request_is_active<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(I915_FENCE_FLAG_ACTIVE, unsafe { &(*request).fence.flags })
}

#[inline]
pub fn i915_request_is_ready<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    unsafe { !crate::linux_list::list_empty(&(*request).sched.link) }
}

#[inline]
pub fn i915_request_in_priority_queue<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(I915_FENCE_FLAG_PQUEUE, unsafe { &(*request).fence.flags })
}

#[inline]
pub fn i915_request_has_initial_breadcrumb<R: I915RequestPtr>(request: R) -> bool {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    crate::linux::bits::test_bit(I915_FENCE_FLAG_INITIAL_BREADCRUMB, unsafe {
        &(*request).fence.flags
    })
}

#[repr(C)]
struct DmaFenceOpsPrefix {
    _get_driver_name: *const c_void,
    _get_timeline_name: *const c_void,
    _enable_signaling: *const c_void,
    _signaled: *const c_void,
    _wait: *const c_void,
    release: Option<unsafe extern "C" fn(*mut DmaFence)>,
}

/// `dma_fence_get()` increments the embedded fence reference and returns the
/// original request pointer.
#[inline]
pub fn i915_request_get<R: I915RequestPtr>(request: R) -> *mut I915Request {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    dma_fence_get(unsafe { core::ptr::addr_of_mut!((*request).fence) });
    request
}

#[inline]
pub fn i915_request_get_rcu<R: I915RequestPtr>(request: R) -> *mut I915Request {
    // Mirrors dma_fence_get_rcu(): the caller's RCU read-side section keeps
    // the containing request memory valid while the kref is acquired.
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    dma_fence_get_rcu(unsafe { core::ptr::addr_of_mut!((*request).fence) }).cast()
}

/// `i915_request_put()` drops the embedded dma-fence refcount. All i915
/// requests use `i915_fence_ops` with a release callback; unlike generic
/// `dma_fence_put()`, this helper intentionally refuses an unknown fence ops
/// table instead of guessing its fallback allocator. The callback offset is
/// from include/linux/dma-fence.h's fixed ops sequence.
#[inline]
pub fn i915_request_put<R: I915RequestPtr>(request: R) {
    let request = request.i915_request_ptr();
    assert!(!request.is_null());
    unsafe {
        let fence = core::ptr::addr_of_mut!((*request).fence);
        if crate::linux_memory::refcount_dec_and_test(&mut (*fence).refcount.refcount) {
            let ops = (*fence).ops;
            assert!(!ops.is_null(), "live i915 request has no dma-fence ops");
            let release = (*ops.cast::<DmaFenceOpsPrefix>())
                .release
                .expect("i915_fence_ops must provide its release callback");
            crate::linux::rcu::rcu_read_lock();
            release(fence);
            crate::linux::rcu::rcu_read_unlock();
        }
    }
}
