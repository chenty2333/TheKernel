// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! LinuxKPI dma-fence and reservation-object helpers (Linux 7.2.3
//! `drivers/dma-buf/dma-fence.c`, `dma-resv.c` and `include/linux/dma-fence.h`).
//!
//! Reservation fences live in the sidecar installed by
//! `i915_gem_clflush_upstream`; this module reads them through the same
//! `dma_resv_iter_next()` entry point, so both sides see one list.

#![allow(unsafe_code)]

use core::{
    ffi::{c_int, c_long, c_ulong, c_void},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    intel_context_upstream::{DmaFence, DmaFenceCb},
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux::{
        locks::{spin_lock_irqsave_raw, spin_unlock_irqrestore_raw},
        requests::{dma_fence_array_create, dma_fence_get, dma_fence_put},
        wait::wait_until,
    },
    linux_config::MAX_SCHEDULE_TIMEOUT,
    linux_list::{list_del_init, list_empty},
};

/// `enum dma_fence_flag_bits` from include/linux/dma-fence.h.
const DMA_FENCE_FLAG_INLINE_LOCK_BIT: u32 = 1;
const DMA_FENCE_FLAG_SIGNALED_BIT: u32 = 3;
const DMA_FENCE_FLAG_ENABLE_SIGNAL_BIT: u32 = 5;

unsafe extern "C" {
    fn dma_fence_is_signaled(fence: *mut DmaFence) -> bool;
    fn dma_fence_signal_locked(fence: *mut DmaFence);
    fn dma_resv_iter_next(cursor: *mut ResvCursor) -> *mut DmaFence;
}

/// Cursor for `dma_resv_iter_next()`: the layout of `struct dma_resv_iter`
/// as used by the reservation sidecar (48 bytes on x86_64).
#[repr(C)]
#[derive(Clone, Copy)]
struct ResvCursor {
    obj: *mut c_void,
    usage: u32,
    fence: *mut DmaFence,
    fence_usage: u32,
    index: u32,
    fences: *mut c_void,
    num_fences: u32,
    is_restarted: bool,
}
const _: () = assert!(core::mem::size_of::<ResvCursor>() == 48);

impl ResvCursor {
    fn new(resv: *mut c_void, usage: u32) -> Self {
        Self {
            obj: resv,
            usage,
            fence: core::ptr::null_mut(),
            fence_usage: 0,
            index: 0,
            fences: core::ptr::null_mut(),
            num_fences: 0,
            is_restarted: false,
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
        let ops = unsafe { (*fence).ops.cast::<crate::i915_request_upstream::DmaFenceOps>() };
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
    let tick_ns = axhal::time::NANOS_PER_SEC as u64 / crate::linux_config::CONFIG_HZ as u64;
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
        return 1;
    }
    let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
    (timeout - (elapsed / tick_ns.max(1)) as c_long).max(1)
}

/// Release the reference the reservation cursor holds on its current fence.
unsafe fn resv_iter_end(cursor: &mut ResvCursor) {
    if !cursor.fence.is_null() {
        unsafe { dma_fence_put(cursor.fence) };
        cursor.fence = core::ptr::null_mut();
    }
}

/// Linux `dma_resv_test_signaled()`: true when every fence at or below
/// `usage` has signaled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_test_signaled(resv: *mut c_void, usage: c_int) -> bool {
    assert!(!resv.is_null());
    let mut cursor = ResvCursor::new(resv, usage as u32);
    let mut all = true;
    loop {
        // The cursor keeps the returned fence referenced until the next step.
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        if !unsafe { dma_fence_is_signaled(fence) } {
            all = false;
            break;
        }
    }
    unsafe { resv_iter_end(&mut cursor) };
    all
}

/// Linux `dma_resv_wait_timeout()`: wait for every fence at or below `usage`.
/// Returns the remaining jiffies (at least 1), or 0 when the wait timed out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_wait_timeout(
    resv: *mut c_void,
    usage: c_int,
    intr: bool,
    timeout: c_long,
) -> c_long {
    assert!(!resv.is_null());
    let mut cursor = ResvCursor::new(resv, usage as u32);
    let mut remaining = timeout;
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        let left = unsafe { dma_fence_wait_timeout(fence, intr, remaining) };
        if left == 0 {
            unsafe { resv_iter_end(&mut cursor) };
            return 0;
        }
        remaining = left;
    }
    unsafe { resv_iter_end(&mut cursor) };
    if remaining != 0 { remaining } else { 1 }
}

/// Linux `dma_resv_get_singleton()`: one fence that stands for every fence at
/// or below `usage`. No fence gives NULL, one fence is returned by reference,
/// and several are gathered into a fence array that signals when all do.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dma_resv_get_singleton(
    resv: *mut c_void,
    usage: c_int,
    fence_out: *mut *mut DmaFence,
) -> c_int {
    assert!(!resv.is_null() && !fence_out.is_null());
    let mut cursor = ResvCursor::new(resv, usage as u32);
    // Every gathered fence takes its own reference; the cursor's reference is
    // released once iteration ends.
    let mut fences: alloc::vec::Vec<*mut DmaFence> = alloc::vec::Vec::new();
    loop {
        let fence = unsafe { dma_resv_iter_next(&mut cursor) };
        if fence.is_null() {
            break;
        }
        fences.push(unsafe { dma_fence_get(fence) });
    }
    unsafe { resv_iter_end(&mut cursor) };
    let result = match fences.len() {
        0 => core::ptr::null_mut(),
        1 => fences[0],
        n => {
            let array = unsafe {
                dma_fence_array_create(n as u32, fences.as_mut_ptr(), dma_fence_context_alloc(1), 1)
            };
            array.cast::<DmaFence>()
        }
    };
    unsafe { *fence_out = result };
    0
}
