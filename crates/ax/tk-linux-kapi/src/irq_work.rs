// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Linux `irq_work` for the TheKernel task runtime. A queued callback runs in
//! task context, never synchronously in the producer's IRQ context.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use crate::types::IrqWork;

pub const IRQ_WORK_PENDING: u32 = 0x01;

pub const IRQ_WORK_BUSY: u32 = 0x02;

pub const IRQ_WORK_LAZY: u32 = 0x04;

pub const IRQ_WORK_HARD_IRQ: u32 = 0x08;

pub const IRQ_WORK_CLAIMED: u32 = IRQ_WORK_PENDING | IRQ_WORK_BUSY;

pub trait IrqWorkPtr {
    fn irq_work_ptr(self) -> *mut IrqWork;
}

impl IrqWorkPtr for *mut IrqWork {
    fn irq_work_ptr(self) -> *mut IrqWork {
        self
    }
}

impl IrqWorkPtr for &mut IrqWork {
    fn irq_work_ptr(self) -> *mut IrqWork {
        self
    }
}

impl IrqWorkPtr for &IrqWork {
    fn irq_work_ptr(self) -> *mut IrqWork {
        (self as *const IrqWork).cast_mut()
    }
}

/// Linux 7.2.3 `init_irq_work()` / `IRQ_WORK_INIT()` initializer.
/// The record must not be pending; the caller owns its storage.
pub fn init_irq_work<W: IrqWorkPtr>(work: W, func: unsafe extern "C" fn(*mut IrqWork)) {
    let work = work.irq_work_ptr();
    assert!(!work.is_null());
    unsafe {
        (*work).node.next = core::ptr::null_mut();
        (*work).node.flags.counter = 0;
        (*work).node.src = 0;
        (*work).node.dst = 0;
        (*work).func = Some(func);
        (*work).irqwait = core::ptr::null_mut();
    }
}

#[inline]
unsafe fn flags<'a>(work: *mut IrqWork) -> &'a AtomicU32 {
    assert!(!work.is_null());
    // SAFETY: `work` is live by the caller's contract, and the naturally
    // aligned flag word is accessed atomically for the item's whole lifetime.
    unsafe { &*core::ptr::addr_of_mut!((*work).node.flags.counter).cast::<AtomicU32>() }
}

/// Claim an idle node for deferred execution.
fn claim(flags: &AtomicU32) -> bool {
    let mut old = flags.load(Ordering::Acquire);
    loop {
        if old & IRQ_WORK_PENDING != 0 {
            return false;
        }
        match flags.compare_exchange_weak(
            old,
            old | IRQ_WORK_CLAIMED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => old = actual,
        }
    }
}

/// Queue an irq_work callback once. The callback runs in a task context,
/// never synchronously in the caller's IRQ context.
pub fn irq_work_queue<W: IrqWorkPtr>(work: W) -> bool {
    let work = work.irq_work_ptr();
    assert!(!work.is_null());
    // SAFETY: the caller keeps this initialized work item live through queueing.
    let state = unsafe { flags(work) };
    if !claim(state) {
        return false;
    }
    // The workqueue uses its existing persistent pool and intrusive queue, so
    // claiming an IRQ callback never allocates or starts a task.
    // SAFETY: the claim above owns the node through dispatch; the caller keeps
    // its storage alive until the BUSY flag is retired.
    if unsafe { crate::workqueue::queue_irq_work(work) } {
        return true;
    }
    state.fetch_and(!(IRQ_WORK_PENDING | IRQ_WORK_BUSY), Ordering::Release);
    false
}

/// Run a dequeued callback and retire its flags while RCU protects the
/// embedded node. A callback may drop the final fence reference, which queues
/// reclamation of the allocation containing this `IrqWork`; its RCU callback
/// must not run until the runner has finished touching the node.
///
/// # Safety
/// `work` must point to a live node just dequeued by the workqueue, with a
/// valid callback in `func` and its PENDING/BUSY flags still set.
pub(crate) unsafe fn run_irq_work(work: *mut IrqWork) {
    if work.is_null() {
        return;
    }
    // SAFETY: `work` is live under this function's caller contract.
    let state = unsafe { flags(work) };
    crate::rcu::rcu_read_lock();
    // SAFETY: the callback and embedded node remain alive under the RCU
    // read-side section, even if the callback drops the node's owner.
    let callback = unsafe { (*work).func };
    if let Some(callback) = callback {
        // SAFETY: `func` was installed by `init_irq_work` for this node.
        unsafe { callback(work) };
    }
    state.fetch_and(!(IRQ_WORK_PENDING | IRQ_WORK_BUSY), Ordering::Release);
    crate::rcu::rcu_read_unlock();
}

pub fn irq_work_sync<W: IrqWorkPtr>(work: W) {
    let work = work.irq_work_ptr();
    assert!(!work.is_null());
    while unsafe { flags(work) }.load(Ordering::Acquire) & IRQ_WORK_BUSY != 0 {
        axtask::yield_now();
    }
}
