// SPDX-License-Identifier: MIT
// Copyright © 2026 Intel Corporation and TheKernel contributors.
// Linux v7.2.3 drivers/gpu/drm/i915/i915_request.h and dma-fence helpers.

#![allow(unsafe_code)]

use core::{
    ffi::{c_long, c_void},
    mem::{ManuallyDrop, MaybeUninit},
    ptr,
    sync::atomic::{AtomicI32, AtomicPtr, Ordering},
    time::Duration,
};

use crate::{
    i915_request_types_upstream::I915Request,
    i915_request_upstream::DmaFenceOps,
    intel_context_types_upstream::I915Active,
    intel_context_upstream::{
        DmaFence, DmaFenceCb, DmaFenceTimestamp, I915GemWwCtx, IrqWork, Kref, RcuHead,
    },
    intel_engine_cs_upstream::AtomicT,
    intel_ring_types_upstream::IntelRing,
    intel_timeline_types_upstream::IntelTimeline,
    linux::{
        contexts::IntelContextPtr,
        memory::{
            atomic_add_unless, atomic_inc, atomic_read, kref_get_unless_zero, kref_init, kref_put,
        },
    },
};

// Linux v7.2.3 include/linux/dma-fence.h: generic per-driver fence-context
// allocator. This is a real kernel C ABI binding, not an i915 substitute.
unsafe extern "C" {
    pub fn dma_fence_context_alloc(num: u32) -> u64;
}

/// `dma_fence_put()` is the header-inline `kref_put(..., dma_fence_release)`
/// operation. It dispatches the optional source ops release callback or
/// reclaims through the `dma_fence_free()` RCU path when none is supplied.
pub unsafe fn dma_fence_put(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    unsafe {
        kref_put(&mut (*fence).refcount, dma_fence_release_i915);
    }
}

/// Linux `dma_fence_wait_timeout()` dispatch for the source-owned i915 fence
/// operations. A signaled fence preserves Linux's timeout return convention;
/// otherwise the driver's wait op performs the ordered enable/wait protocol.
pub unsafe fn dma_fence_wait_timeout(
    fence: *mut DmaFence,
    interruptible: bool,
    timeout: core::ffi::c_long,
) -> core::ffi::c_long {
    assert!(!fence.is_null());
    if crate::linux::bits::test_bit(
        crate::linux::requests::DMA_FENCE_FLAG_SIGNALED_BIT,
        unsafe { &(*fence).flags },
    ) {
        return timeout;
    }
    let ops = unsafe { (*fence).ops.cast::<DmaFenceOps>() };
    assert!(!ops.is_null(), "dma_fence has no operations table");
    let wait = unsafe { (*ops).wait }.expect("i915 dma_fence missing wait operation");
    unsafe { wait(fence, interruptible, timeout) }
}

unsafe extern "C" fn dma_fence_release_i915(refcount: *mut Kref) {
    if refcount.is_null() {
        return;
    }
    let fence = unsafe {
        refcount
            .cast::<u8>()
            .sub(core::mem::offset_of!(DmaFence, refcount))
            .cast::<DmaFence>()
    };
    crate::linux::rcu::rcu_read_lock();
    let ops = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*fence).ops)) }
        .cast::<DmaFenceOps>();
    match if ops.is_null() {
        None
    } else {
        unsafe { (*ops).release }
    } {
        Some(release) => unsafe { release(fence) },
        None => {
            let head =
                unsafe { core::ptr::addr_of_mut!((*fence).timestamp_union.rcu).cast::<RcuHead>() };
            crate::linux::rcu::call_rcu(head, dma_fence_free_rcu);
        }
    }
    crate::linux::rcu::rcu_read_unlock();
}

unsafe extern "C" fn dma_fence_free_rcu(head: *mut RcuHead) {
    if head.is_null() {
        return;
    }
    let fence = unsafe {
        head.cast::<u8>()
            .sub(core::mem::offset_of!(DmaFence, timestamp_union))
            .cast::<DmaFence>()
    };
    unsafe { crate::linux::memory::kfree(fence) };
}

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

static DMA_FENCE_WAITERS: axtask::WaitQueue = axtask::WaitQueue::new();

/// Wake generic dma-fence default waiters after a source fence is signaled.
pub fn wake_dma_fence_waiters() {
    DMA_FENCE_WAITERS.notify_all(false);
}

/// Linux `dma_fence_default_wait()`: wait on the source signaled flag, honor
/// interruptible waits, and return remaining jiffies (or Linux's timeout /
/// restart error value). The source callback list is woken by the shared
/// dma-fence signal path; spurious notifications always recheck the flag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_default_wait(
    fence: *mut DmaFence,
    interruptible: bool,
    timeout: c_long,
) -> c_long {
    if fence.is_null() {
        return -(crate::linux_config::EINVAL as c_long);
    }
    let signaled =
        || crate::linux::bits::test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*fence).flags });
    if signaled() {
        return timeout;
    }
    if interruptible && unsafe { crate::linux::signal::signal_pending_state(1, current_task_ptr()) }
    {
        return -512; // -ERESTARTSYS
    }
    if timeout == 0 {
        return 0;
    }

    let start = axhal::time::monotonic_time_nanos();
    let result = if timeout == crate::linux_config::MAX_SCHEDULE_TIMEOUT as c_long {
        if interruptible {
            DMA_FENCE_WAITERS.wait_until_interruptible(signaled)
        } else {
            DMA_FENCE_WAITERS.wait_until(signaled)
        }
        .map(|_| false)
    } else {
        let ticks = timeout.max(0) as u64;
        let hz = u64::from(crate::linux_config::CONFIG_HZ);
        let duration = Duration::from_secs(ticks / hz)
            .saturating_add(Duration::from_nanos((ticks % hz) * 1_000_000_000 / hz));
        if interruptible {
            DMA_FENCE_WAITERS.wait_timeout_until_interruptible(duration, signaled)
        } else {
            DMA_FENCE_WAITERS.wait_timeout_until(duration, signaled)
        }
    };

    match result {
        // The Linux wait-queue API returns false when its condition wins and
        // true when the deadline expires.
        Ok(true) => 0,
        Ok(false) if signaled() => {
            let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
            let elapsed_ticks =
                elapsed.saturating_mul(u64::from(crate::linux_config::CONFIG_HZ)) / 1_000_000_000;
            timeout.saturating_sub(elapsed_ticks as c_long).max(1)
        }
        Ok(false) => 0,
        Err(axtask::WaitError::Interrupted) => -512, // -ERESTARTSYS
        Err(_) => -(crate::linux_config::EIO as c_long),
    }
}

fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}
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

// Linux i915 pointer-tag helpers from i915_ptr_util.h. The tag occupies the
// low `nbits` of an aligned kernel pointer; callers retain the same pointer
// type and are responsible for providing a pointer with those bits clear.
#[inline]
pub fn ptr_pack_bits<T>(pointer: *mut T, bits: u32, nbits: u32) -> *mut T {
    assert!(nbits < usize::BITS);
    let mask = (1usize << nbits) - 1;
    assert_eq!(
        (bits as usize) & !mask,
        0,
        "pointer tag exceeds its allocated bits"
    );
    assert_eq!(
        (pointer as usize) & mask,
        0,
        "tagged pointer is not aligned"
    );
    ((pointer as usize) | bits as usize) as *mut T
}

#[inline]
pub fn ptr_unpack_bits<T>(pointer: *mut T, bits: &mut u32, nbits: u32) -> *mut T {
    assert!(nbits < usize::BITS);
    let mask = (1usize << nbits) - 1;
    *bits = (pointer as usize & mask) as u32;
    ((pointer as usize) & !mask) as *mut T
}

#[inline]
pub fn ptr_mask_bits<T>(pointer: *mut T, nbits: u32) -> *mut T {
    assert!(nbits < usize::BITS);
    ((pointer as usize) & !((1usize << nbits) - 1)) as *mut T
}

// `i915_request_next()` from i915_request.h. The timeline's intrusive list
// contains requests by their `link` member and uses the list head as sentinel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_request_next(
    request: *mut I915Request,
    timeline: *mut IntelTimeline,
) -> *mut I915Request {
    if request.is_null() || timeline.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        let link = ptr::addr_of_mut!((*request).link);
        let next = (*link).next;
        let head = ptr::addr_of_mut!((*timeline).requests);
        if next == head {
            ptr::null_mut()
        } else {
            next.cast::<u8>()
                .sub(core::mem::offset_of!(I915Request, link))
                .cast()
        }
    }
}

// Generic Linux dma-fence containers used by execbuffer's sync-file fences.
// Their ABI layouts follow include/linux/dma-fence-{array,chain}.h.
const DMA_FENCE_ENABLE_SIGNAL_BIT: u32 = 5;
const DMA_FENCE_ARRAY_PENDING_ERROR: i32 = 1;

#[repr(C)]
struct DmaFenceArrayCallback {
    callback: DmaFenceCb,
    array: *mut DmaFenceArray,
}

#[repr(C)]
pub struct DmaFenceArray {
    pub base: DmaFence,
    num_fences: u32,
    num_pending: AtomicT,
    fences: *mut *mut DmaFence,
    work: IrqWork,
    callbacks: [MaybeUninit<DmaFenceArrayCallback>; 0],
}

#[repr(C)]
struct DmaFenceChain {
    base: DmaFence,
    prev: *mut DmaFence,
    prev_seqno: u64,
    fence: *mut DmaFence,
    // Linux overlays the chain callback and irq_work at this exact offset.
    work: IrqWork,
}

const _: [(); 32] = [(); core::mem::size_of::<DmaFenceArrayCallback>()];
const _: [(); 112] = [(); core::mem::size_of::<DmaFenceArray>()];
const _: [(); 120] = [(); core::mem::size_of::<DmaFenceChain>()];
const _: [(); 80] = [(); core::mem::offset_of!(DmaFenceArray, work)];
const _: [(); 112] = [(); core::mem::offset_of!(DmaFenceArray, callbacks)];
const _: [(); 88] = [(); core::mem::offset_of!(DmaFenceChain, work)];

fn dma_fence_chain_is(fence: *const DmaFence) -> bool {
    !fence.is_null()
        && unsafe { (*fence).ops == ptr::addr_of!(dma_fence_chain_ops).cast::<c_void>() }
}

unsafe fn dma_fence_chain_contained(fence: *mut DmaFence) -> *mut DmaFence {
    if dma_fence_chain_is(fence) {
        unsafe { (*fence.cast::<DmaFenceChain>()).fence }
    } else {
        fence
    }
}

#[unsafe(no_mangle)]
pub static dma_fence_array_ops: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(dma_fence_array_driver_name),
    get_timeline_name: Some(dma_fence_array_timeline_name),
    enable_signaling: Some(dma_fence_array_enable_signaling),
    signaled: Some(dma_fence_array_signaled),
    wait: Some(dma_fence_default_wait),
    release: Some(dma_fence_array_release),
    set_deadline: Some(dma_fence_array_set_deadline),
};

#[unsafe(no_mangle)]
pub static dma_fence_chain_ops: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(dma_fence_chain_driver_name),
    get_timeline_name: Some(dma_fence_chain_timeline_name),
    enable_signaling: Some(dma_fence_chain_enable_signaling),
    signaled: Some(dma_fence_chain_signaled),
    wait: Some(dma_fence_default_wait),
    release: Some(dma_fence_chain_release),
    set_deadline: Some(dma_fence_chain_set_deadline),
};

unsafe extern "C" fn dma_fence_array_driver_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"dma_fence_array".as_ptr()
}

unsafe extern "C" fn dma_fence_array_timeline_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"unbound".as_ptr()
}

unsafe extern "C" fn dma_fence_chain_driver_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"dma_fence_chain".as_ptr()
}

unsafe extern "C" fn dma_fence_chain_timeline_name(_: *mut DmaFence) -> *const core::ffi::c_char {
    c"unbound".as_ptr()
}

unsafe fn dma_fence_container_init(
    fence: *mut DmaFence,
    ops: *const DmaFenceOps,
    context: u64,
    seqno: u64,
) {
    assert!(!fence.is_null() && !ops.is_null());
    unsafe {
        let lock = ptr::addr_of_mut!((*fence).lock.inline_lock);
        crate::linux::locks::spin_lock_init(&mut *lock);
        (*fence).ops = ops.cast();
        (*fence).timestamp_union = DmaFenceTimestamp {
            cb_list: ManuallyDrop::new(crate::intel_engine_cs_upstream::ListHead {
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
            }),
        };
        crate::linux::list::INIT_LIST_HEAD(
            ptr::addr_of_mut!((*fence).timestamp_union.cb_list).cast(),
        );
        (*fence).context = context;
        (*fence).seqno = seqno;
        (*fence).flags = 0;
        kref_init(ptr::addr_of_mut!((*fence).refcount));
        (*fence).error = 0;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_array_create(
    num_fences: u32,
    fences: *mut *mut DmaFence,
    context: u64,
    seqno: u32,
) -> *mut DmaFenceArray {
    if num_fences == 0 || fences.is_null() {
        return ptr::null_mut();
    }
    let Some(callback_bytes) =
        (num_fences as usize).checked_mul(core::mem::size_of::<DmaFenceArrayCallback>())
    else {
        return ptr::null_mut();
    };
    let Some(bytes) = core::mem::size_of::<DmaFenceArray>().checked_add(callback_bytes) else {
        return ptr::null_mut();
    };
    let array = crate::linux::memory::kzalloc(bytes, crate::linux_config::GFP_KERNEL)
        .cast::<DmaFenceArray>();
    if array.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        (*array).num_fences = num_fences;
        (*array).num_pending.counter = num_fences as i32;
        (*array).fences = fences;
        crate::linux::irq::init_irq_work(ptr::addr_of_mut!((*array).work), dma_fence_array_work);
        dma_fence_container_init(
            ptr::addr_of_mut!((*array).base),
            ptr::addr_of!(dma_fence_array_ops),
            context,
            u64::from(seqno),
        );
        (*array).base.error = DMA_FENCE_ARRAY_PENDING_ERROR;
    }
    array
}

unsafe extern "C" fn dma_fence_array_work(work: *mut IrqWork) {
    let array = unsafe {
        work.cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceArray, work))
            .cast::<DmaFenceArray>()
    };
    unsafe {
        dma_fence_array_clear_pending_error(array);
        crate::i915_gem_clflush_upstream::dma_fence_signal(ptr::addr_of_mut!((*array).base));
        dma_fence_put(ptr::addr_of_mut!((*array).base));
    }
}

unsafe extern "C" fn dma_fence_array_callback(fence: *mut DmaFence, callback: *mut DmaFenceCb) {
    let array_cb = unsafe { callback.cast::<DmaFenceArrayCallback>() };
    let array = unsafe { (*array_cb).array };
    unsafe {
        dma_fence_array_set_pending_error(array, (*fence).error);
        if crate::linux::memory::atomic_sub_and_test(1, &mut (*array).num_pending) {
            crate::linux::irq::irq_work_queue(ptr::addr_of_mut!((*array).work));
        } else {
            dma_fence_put(ptr::addr_of_mut!((*array).base));
        }
    }
}

unsafe extern "C" fn dma_fence_array_enable_signaling(fence: *mut DmaFence) -> bool {
    let array = fence.cast::<DmaFenceArray>();
    for index in 0..unsafe { (*array).num_fences } {
        let callback = unsafe {
            ptr::addr_of_mut!((*array).callbacks)
                .cast::<DmaFenceArrayCallback>()
                .add(index as usize)
        };
        unsafe {
            (*callback).array = array;
            dma_fence_get(ptr::addr_of_mut!((*array).base));
            let child = *(*array).fences.add(index as usize);
            let err = crate::i915_gem_clflush_upstream::dma_fence_add_callback(
                child,
                ptr::addr_of_mut!((*callback).callback),
                dma_fence_array_callback,
            );
            if err != 0 {
                dma_fence_array_set_pending_error(array, (*child).error);
                dma_fence_put(ptr::addr_of_mut!((*array).base));
                if crate::linux::memory::atomic_sub_and_test(1, &mut (*array).num_pending) {
                    dma_fence_array_clear_pending_error(array);
                    return false;
                }
            }
        }
    }
    true
}

unsafe extern "C" fn dma_fence_array_signaled(fence: *mut DmaFence) -> bool {
    let array = fence.cast::<DmaFenceArray>();
    let mut pending = unsafe { crate::linux::memory::atomic_read(&(*array).num_pending) };
    if crate::linux::bits::test_bit(DMA_FENCE_ENABLE_SIGNAL_BIT, unsafe { &(*fence).flags }) {
        if pending <= 0 {
            unsafe { dma_fence_array_clear_pending_error(array) };
            return true;
        }
        return false;
    }
    for index in 0..unsafe { (*array).num_fences } {
        let child = unsafe { *(*array).fences.add(index as usize) };
        if unsafe { crate::i915_gem_clflush_upstream::dma_fence_is_signaled(child) } {
            pending -= 1;
            if pending == 0 {
                unsafe { dma_fence_array_clear_pending_error(array) };
                return true;
            }
        }
    }
    false
}

unsafe fn dma_fence_array_set_pending_error(array: *mut DmaFenceArray, error: i32) {
    if error != 0 {
        unsafe {
            let err = AtomicI32::from_ptr(ptr::addr_of_mut!((*array).base.error));
            let _ = err.compare_exchange(
                DMA_FENCE_ARRAY_PENDING_ERROR,
                error,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

unsafe fn dma_fence_array_clear_pending_error(array: *mut DmaFenceArray) {
    unsafe {
        let err = AtomicI32::from_ptr(ptr::addr_of_mut!((*array).base.error));
        let _ = err.compare_exchange(
            DMA_FENCE_ARRAY_PENDING_ERROR,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

unsafe extern "C" fn dma_fence_array_release(fence: *mut DmaFence) {
    let array = fence.cast::<DmaFenceArray>();
    unsafe {
        for index in 0..(*array).num_fences {
            dma_fence_put(*(*array).fences.add(index as usize));
        }
        crate::linux::memory::kfree((*array).fences);
        crate::i915_gem_clflush_upstream::dma_fence_free(fence);
    }
}

unsafe extern "C" fn dma_fence_array_set_deadline(fence: *mut DmaFence, deadline: i64) {
    let array = fence.cast::<DmaFenceArray>();
    unsafe {
        for index in 0..(*array).num_fences {
            dma_fence_set_deadline(*(*array).fences.add(index as usize), deadline);
        }
    }
}

unsafe extern "C" fn dma_fence_chain_enable_signaling(fence: *mut DmaFence) -> bool {
    let chain = fence.cast::<DmaFenceChain>();
    let base = unsafe { ptr::addr_of_mut!((*chain).base) };
    dma_fence_get(base);
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if child.is_null() {
            iter = unsafe { dma_fence_chain_walk(iter) };
            continue;
        }
        dma_fence_get(child);
        let err = unsafe {
            crate::i915_gem_clflush_upstream::dma_fence_add_callback(
                child,
                ptr::addr_of_mut!((*chain).work).cast::<DmaFenceCb>(),
                dma_fence_chain_callback,
            )
        };
        if err == 0 {
            unsafe { dma_fence_put(iter) };
            return true;
        }
        unsafe { dma_fence_put(child) };
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe { dma_fence_put(base) };
    false
}

unsafe extern "C" fn dma_fence_chain_callback(fence: *mut DmaFence, callback: *mut DmaFenceCb) {
    let chain = unsafe {
        callback
            .cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceChain, work))
            .cast::<DmaFenceChain>()
    };
    unsafe {
        crate::linux::irq::init_irq_work(ptr::addr_of_mut!((*chain).work), dma_fence_chain_work);
        crate::linux::irq::irq_work_queue(ptr::addr_of_mut!((*chain).work));
        dma_fence_put(fence);
    }
}

unsafe extern "C" fn dma_fence_chain_work(work: *mut IrqWork) {
    let chain = unsafe {
        work.cast::<u8>()
            .sub(core::mem::offset_of!(DmaFenceChain, work))
            .cast::<DmaFenceChain>()
    };
    unsafe {
        if !dma_fence_chain_enable_signaling(ptr::addr_of_mut!((*chain).base)) {
            crate::i915_gem_clflush_upstream::dma_fence_signal(ptr::addr_of_mut!((*chain).base));
        }
        dma_fence_put(ptr::addr_of_mut!((*chain).base));
    }
}

unsafe extern "C" fn dma_fence_chain_signaled(fence: *mut DmaFence) -> bool {
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if child.is_null()
            || !unsafe { crate::i915_gem_clflush_upstream::dma_fence_is_signaled(child) }
        {
            unsafe { dma_fence_put(iter) };
            return false;
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    true
}

unsafe extern "C" fn dma_fence_chain_release(fence: *mut DmaFence) {
    let chain = fence.cast::<DmaFenceChain>();
    let mut prev = unsafe { (*chain).prev };
    while !prev.is_null() {
        if !dma_fence_chain_is(prev) || unsafe { crate::linux::memory::kref_read(&(*prev).refcount) } > 1 {
            break;
        }
        let prev_chain = prev.cast::<DmaFenceChain>();
        let next = unsafe { (*prev_chain).prev };
        unsafe {
            (*chain).prev = next;
            (*prev_chain).prev = ptr::null_mut();
            dma_fence_put(prev);
        }
        prev = next;
    }
    unsafe {
        dma_fence_put(prev);
        dma_fence_put((*chain).fence);
        crate::i915_gem_clflush_upstream::dma_fence_free(fence);
    }
}

unsafe extern "C" fn dma_fence_chain_set_deadline(fence: *mut DmaFence, deadline: i64) {
    let mut iter = dma_fence_get(fence);
    while !iter.is_null() {
        let child = unsafe { dma_fence_chain_contained(iter) };
        if !child.is_null() {
            unsafe { dma_fence_set_deadline(child, deadline) };
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
}

unsafe fn dma_fence_set_deadline(fence: *mut DmaFence, deadline: i64) {
    if fence.is_null() {
        return;
    }
    let ops = unsafe { (*fence).ops.cast::<DmaFenceOps>() };
    if !ops.is_null() {
        if let Some(set_deadline) = unsafe { (*ops).set_deadline } {
            unsafe { set_deadline(fence, deadline) };
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_walk(fence: *mut DmaFence) -> *mut DmaFence {
    if !dma_fence_chain_is(fence) {
        unsafe { dma_fence_put(fence) };
        return ptr::null_mut();
    }
    let chain = fence.cast::<DmaFenceChain>();
    let mut result = ptr::null_mut();
    loop {
        crate::linux::rcu::rcu_read_lock();
        let prev = unsafe { AtomicPtr::from_ptr(ptr::addr_of_mut!((*chain).prev)) }
            .load(Ordering::Acquire);
        let prev_ref = dma_fence_get_rcu(prev);
        crate::linux::rcu::rcu_read_unlock();
        if prev_ref.is_null() {
            break;
        }
        let replacement = if dma_fence_chain_is(prev_ref) {
            let previous = prev_ref.cast::<DmaFenceChain>();
            let child = unsafe { (*previous).fence };
            if child.is_null()
                || !unsafe { crate::i915_gem_clflush_upstream::dma_fence_is_signaled(child) }
            {
                result = prev_ref;
                break;
            }
            crate::linux::rcu::rcu_read_lock();
            let replacement = unsafe {
                dma_fence_get_rcu(
                    AtomicPtr::from_ptr(ptr::addr_of_mut!((*previous).prev))
                        .load(Ordering::Acquire),
                )
            };
            crate::linux::rcu::rcu_read_unlock();
            replacement
        } else if unsafe { !crate::i915_gem_clflush_upstream::dma_fence_is_signaled(prev_ref) } {
            result = prev_ref;
            break;
        } else {
            ptr::null_mut()
        };
        let result = unsafe {
            AtomicPtr::from_ptr(ptr::addr_of_mut!((*chain).prev)).compare_exchange(
                prev_ref,
                replacement,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
        };
        if result.is_ok() {
            unsafe { dma_fence_put(prev_ref) };
        } else {
            unsafe { dma_fence_put(replacement) };
        }
        unsafe { dma_fence_put(prev_ref) };
    }
    unsafe { dma_fence_put(fence) };
    result
}

pub fn dma_fence_chain_alloc<T>() -> *mut T {
    crate::linux::memory::kmalloc(
        core::mem::size_of::<DmaFenceChain>(),
        crate::linux_config::GFP_KERNEL,
    )
    .cast()
}

pub unsafe fn dma_fence_chain_free<T>(chain: *mut T) {
    if !chain.is_null() {
        unsafe { crate::linux::memory::kfree(chain) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_chain_find_seqno(fence: *mut *mut DmaFence, seqno: u64) -> i32 {
    if seqno == 0 {
        return 0;
    }
    if fence.is_null() || unsafe { (*fence).is_null() } {
        return -(crate::linux_config::EINVAL as i32);
    }
    let start = unsafe { *fence };
    if !dma_fence_chain_is(start) || unsafe { (*start).seqno < seqno } {
        return -(crate::linux_config::EINVAL as i32);
    }
    let root = start.cast::<DmaFenceChain>();
    let context = unsafe { (*root).base.context };
    let mut iter = dma_fence_get(start);
    loop {
        if !dma_fence_chain_is(iter) {
            break;
        }
        let current = iter.cast::<DmaFenceChain>();
        if unsafe { (*iter).context != context || (*current).prev_seqno < seqno } {
            break;
        }
        iter = unsafe { dma_fence_chain_walk(iter) };
    }
    unsafe {
        *fence = iter;
        dma_fence_put(start);
    }
    0
}
