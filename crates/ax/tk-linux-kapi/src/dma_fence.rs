// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI dma-fence core: Linux 7.2.3 `drivers/dma-buf/dma-fence.c` and
//! `include/linux/dma-fence.h`. Fence containers live in `dma_fence_array` and
//! `dma_fence_chain`; reservation objects live in `dma_resv`.
//!
//! The `dma_fence_*` C ABI is defined here once, so i915 and the kernel link
//! the same symbols.

#![allow(unsafe_code)]

use core::{
    ffi::{c_char, c_int, c_long, c_ulong, c_void},
    mem::ManuallyDrop,
    ptr,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use crate::{
    config::{EINVAL, ENOENT, MAX_SCHEDULE_TIMEOUT},
    list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty, list_splice_init},
    locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
    memory::{kref_get_unless_zero, kref_init, kref_put},
    primitives::ktime_get,
    types::{DmaFence, DmaFenceCb, DmaFenceLock, DmaFenceOps, DmaFenceTimestamp, Kref, ListHead, RcuHead, Spinlock},
    wait::wait_until,
};

/// `enum dma_fence_flag_bits` (include/linux/dma-fence.h).
pub const DMA_FENCE_FLAG_INLINE_LOCK_BIT: u32 = 1;
pub const DMA_FENCE_FLAG_SIGNALED_BIT: u32 = 3;
pub const DMA_FENCE_FLAG_TIMESTAMP_BIT: u32 = 4;
pub const DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT: u32 = 5;
pub const DMA_FENCE_FLAG_USER_BITS: u32 = 6;

// Linux driver-name and timeline-name strings of the generic fence ops.
static DRIVER_NAME: &[u8] = b"dma-fence\0";
static WORK_NAME: &[u8] = b"work\0";
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
        return -(crate::config::EINVAL as c_long);
    }
    let signaled =
        || crate::bits::test_bit(DMA_FENCE_FLAG_SIGNALED_BIT, unsafe { &(*fence).flags });
    if signaled() {
        return timeout;
    }
    if interruptible && unsafe { crate::signal::signal_pending_state(1, current_task_ptr()) }
    {
        return -512; // -ERESTARTSYS
    }
    if timeout == 0 {
        return 0;
    }

    let start = axhal::time::monotonic_time_nanos();
    let result = if timeout == crate::config::MAX_SCHEDULE_TIMEOUT as c_long {
        if interruptible {
            DMA_FENCE_WAITERS.wait_until_interruptible(signaled)
        } else {
            DMA_FENCE_WAITERS.wait_until(signaled)
        }
        .map(|_| false)
    } else {
        let ticks = timeout.max(0) as u64;
        let hz = u64::from(crate::config::CONFIG_HZ);
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
                elapsed.saturating_mul(u64::from(crate::config::CONFIG_HZ)) / 1_000_000_000;
            timeout.saturating_sub(elapsed_ticks as c_long).max(1)
        }
        Ok(false) => 0,
        Err(axtask::WaitError::Interrupted) => -512, // -ERESTARTSYS
        Err(_) => -(crate::config::EIO as c_long),
    }
}

fn current_task_ptr() -> *mut c_void {
    axhal::percpu::current_task_ptr::<()>().cast_mut().cast()
}

/// `dma_fence_get()` requires the caller to already own a live fence
/// reference. Null remains interoperable with null request/fence pointers.
#[unsafe(no_mangle)]
pub extern "C" fn dma_fence_get(fence: *mut DmaFence) -> *mut DmaFence {
    if fence.is_null() {
        return fence;
    }
    let got =
        unsafe { crate::memory::refcount_inc_not_zero(&mut (*fence).refcount.refcount) };
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

/// `dma_fence_put()` is the header-inline `kref_put(..., dma_fence_release)`
/// operation. It dispatches the optional source ops release callback or
/// reclaims through the `dma_fence_free()` RCU path when none is supplied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_put(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    unsafe {
        kref_put(&mut (*fence).refcount, dma_fence_release_i915);
    }
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
    crate::rcu::rcu_read_lock();
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
            crate::rcu::call_rcu(head, dma_fence_free_rcu);
        }
    }
    crate::rcu::rcu_read_unlock();
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
    unsafe { crate::memory::kfree(fence) };
}

/// Linux `dma_fence_set_deadline()`: propagate an absolute monotonic deadline
/// through every member of a fence chain and into the signaler's driver hook.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_set_deadline(fence: *mut DmaFence, deadline: i64) {
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

/// The spinlock protecting `fence`: the inline lock when the fence says so,
/// otherwise the external lock it was initialised with.
#[inline]
unsafe fn fence_spinlock(fence: *mut DmaFence) -> *mut Spinlock {
    let flags = unsafe { (*fence).flags };
    if flags & (1 << DMA_FENCE_FLAG_INLINE_LOCK_BIT) != 0 {
        unsafe { core::ptr::addr_of_mut!((*fence).lock).cast::<Spinlock>() }
    } else {
        unsafe { (*fence).lock.extern_lock }
    }
}

/// Linux `dma_fence_spinlock()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_spinlock(fence: *mut DmaFence) -> *mut Spinlock {
    assert!(!fence.is_null());
    unsafe { fence_spinlock(fence) }
}

/// Linux `dma_fence_lock_irqsave()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_lock_irqsave(fence: *mut DmaFence, flags: *mut c_ulong) {
    assert!(!fence.is_null() && !flags.is_null());
    unsafe { spin_lock_irqsave_raw(fence_spinlock(fence), &mut *flags) };
}

/// Linux `dma_fence_unlock_irqrestore()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_unlock_irqrestore(fence: *mut DmaFence, flags: c_ulong) {
    assert!(!fence.is_null());
    unsafe { spin_unlock_irqrestore_raw(fence_spinlock(fence), flags) };
}

static DMA_FENCE_CONTEXT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Linux `dma_fence_context_alloc()`: reserve `num` consecutive context ids
/// and return the first.
#[unsafe(no_mangle)]
pub extern "C" fn dma_fence_context_alloc(num: u32) -> u64 {
    DMA_FENCE_CONTEXT_COUNTER.fetch_add(u64::from(num), Ordering::Relaxed)
}

/// Linux `dma_fence_enable_sw_signaling()`: arm the fence's driver signaling
/// once. When the driver's `enable_signaling` reports the fence already done,
/// the fence is signaled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_enable_sw_signaling(fence: *mut DmaFence) {
    assert!(!fence.is_null());
    let mut flags = 0;
    unsafe { dma_fence_lock_irqsave(fence, &mut flags) };
    let bit = 1usize << DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT;
    let previously = unsafe {
        core::sync::atomic::AtomicUsize::from_ptr(core::ptr::addr_of_mut!((*fence).flags).cast())
            .fetch_or(bit, Ordering::AcqRel)
    };
    let signaled = unsafe { (*fence).flags } & (1 << DMA_FENCE_FLAG_SIGNALED_BIT) != 0;
    if previously & bit == 0 && !signaled {
        let ops = unsafe { (*fence).ops.cast::<crate::types::DmaFenceOps>() };
        if let Some(enable) = unsafe { ops.as_ref() }.and_then(|ops| ops.enable_signaling) {
            if !unsafe { enable(fence) } {
                unsafe { dma_fence_signal_locked(fence) };
            }
        }
    }
    unsafe { dma_fence_unlock_irqrestore(fence, flags) };
}

/// Linux `dma_fence_remove_callback()`: true when `cb` was still queued on the
/// fence (and is now removed), false when it had already run.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_remove_callback(fence: *mut DmaFence, cb: *mut DmaFenceCb) -> bool {
    assert!(!fence.is_null() && !cb.is_null());
    let mut flags = 0;
    unsafe { dma_fence_lock_irqsave(fence, &mut flags) };
    let queued = unsafe { !list_empty(&(*cb).node) };
    if queued {
        unsafe { list_del_init(core::ptr::addr_of_mut!((*cb).node)) };
    }
    unsafe { dma_fence_unlock_irqrestore(fence, flags) };
    queued
}

/// Linux `dma_fence_wait_timeout()`: wait up to `timeout` jiffies for `fence`.
/// Returns the remaining jiffies (at least 1) once signaled, or 0 on timeout.
/// A timeout of `MAX_SCHEDULE_TIMEOUT` waits without bound.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_wait_timeout(
    fence: *mut DmaFence,
    _interruptible: bool,
    timeout: c_long,
) -> c_long {
    assert!(!fence.is_null());
    assert!(timeout >= 0, "dma_fence_wait_timeout() with negative timeout");
    if unsafe { dma_fence_is_signaled(fence) } {
        return if timeout != 0 { timeout } else { 1 };
    }
    if timeout == 0 {
        return 0;
    }
    unsafe { dma_fence_enable_sw_signaling(fence) };
    let tick_ns = axhal::time::NANOS_PER_SEC as u64 / crate::config::CONFIG_HZ as u64;
    let unbounded = timeout as u64 >= MAX_SCHEDULE_TIMEOUT;
    let nanos = if unbounded {
        u64::MAX
    } else {
        (timeout as u64).saturating_mul(tick_ns)
    };
    let start = axhal::time::monotonic_time_nanos();
    let timed_out = wait_until(nanos, || unsafe { dma_fence_is_signaled(fence) }, true);
    if timed_out {
        return 0;
    }
    if unbounded {
        // Linux returns the (huge) remaining timeout; returning 1 would make
        // dma_resv_wait_timeout() time out the next fence after one tick.
        return timeout;
    }
    let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
    (timeout - (elapsed / tick_ns.max(1)) as c_long).max(1)
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
    wake_dma_fence_waiters();
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

/// Default Linux `dma_fence_free()` RCU reclamation path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_fence_free(fence: *mut DmaFence) {
    if fence.is_null() {
        return;
    }
    let head = unsafe { ptr::addr_of_mut!((*fence).timestamp_union.rcu).cast::<RcuHead>() };
    crate::rcu::call_rcu(head, dma_fence_free_rcu);
}

/// Linux `dma_fence_get_status_locked()`: 0 while pending, otherwise the
/// fence's error (negative) or 1 when it completed without one.
unsafe fn dma_fence_get_status_locked(fence: *mut DmaFence) -> c_int {
    if unsafe { dma_fence_is_signaled(fence) } {
        match unsafe { (*fence).error } {
            0 => 1,
            error => error,
        }
    } else {
        0
    }
}

/// Linux `dma_fence_get_status()`.
///
/// # Safety
/// `fence` must be a live fence.
pub unsafe fn dma_fence_get_status(fence: *mut DmaFence) -> c_int {
    let mut flags = 0;
    unsafe { fence_lock(fence, &mut flags) };
    let status = unsafe { dma_fence_get_status_locked(fence) };
    unsafe { fence_unlock(fence, flags) };
    status
}

/// Linux `dma_fence_timestamp()` (include/linux/dma-fence.h): the signal time
/// of a signaled fence; `ktime_get()` when asked about a pending one.
///
/// # Safety
/// `fence` must be a live fence.
pub unsafe fn dma_fence_timestamp(fence: *mut DmaFence) -> i64 {
    if !unsafe { dma_fence_is_signaled(fence) } {
        return ktime_get();
    }
    while unsafe {
        AtomicUsize::from_ptr(ptr::addr_of_mut!((*fence).flags).cast()).load(Ordering::Acquire)
            & (1usize << DMA_FENCE_FLAG_TIMESTAMP_BIT)
            == 0
    } {
        core::hint::spin_loop();
    }
    unsafe { (*fence).timestamp_union.timestamp }
}

/// Linux `dma_fence_is_later()` for fences of one ops table: sequence numbers
/// compare with wrap-around. Fences of different ops are never ordered.
///
/// # Safety
/// Both pointers must be live fences.
pub unsafe fn dma_fence_is_later(f1: *mut DmaFence, f2: *mut DmaFence) -> bool {
    if unsafe { (*f1).ops != (*f2).ops } {
        return false;
    }
    unsafe { ((*f1).seqno.wrapping_sub((*f2).seqno) as i64) > 0 }
}

/// Lock shared by the private stub fences: Linux gives each its inline lock,
/// and a stub is signaled at creation so the lock only serves status queries.
struct StubLock(core::cell::UnsafeCell<Spinlock>);
// SAFETY: the Spinlock is only used through the fence lock helpers.
unsafe impl Sync for StubLock {}
static STUB_LOCK: StubLock = StubLock(core::cell::UnsafeCell::new(Spinlock::new()));

unsafe extern "C" fn dma_fence_stub_get_name(_: *mut DmaFence) -> *const c_char {
    c"Unbound".as_ptr()
}

static DMA_FENCE_STUB_OPS: DmaFenceOps = DmaFenceOps {
    get_driver_name: Some(dma_fence_stub_get_name),
    get_timeline_name: Some(dma_fence_stub_get_name),
    enable_signaling: None,
    signaled: None,
    wait: None,
    release: None,
    set_deadline: None,
};

/// Linux `dma_fence_allocate_private_stub()`: a fence already signaled at
/// `timestamp`, not shared with any driver. Null on allocation failure.
///
/// # Safety
/// Returned fences are released with `dma_fence_put` like any other fence.
pub unsafe fn dma_fence_allocate_private_stub(timestamp: i64) -> *mut DmaFence {
    let fence = crate::memory::kzalloc_obj::<DmaFence>();
    if fence.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        dma_fence_init(
            fence,
            ptr::addr_of!(DMA_FENCE_STUB_OPS),
            STUB_LOCK.0.get(),
            0,
            0,
        );
        (*fence).flags |= 1 << DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT;
        // Linux dma_fence_signal_timestamp() with no callbacks queued.
        (*fence).timestamp_union.timestamp = timestamp;
        (*fence).flags |= (1 << DMA_FENCE_FLAG_SIGNALED_BIT) | (1 << DMA_FENCE_FLAG_TIMESTAMP_BIT);
    }
    fence
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
