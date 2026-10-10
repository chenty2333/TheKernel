// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux irq_work callbacks are deferred onto a task context so IRQ producers
//! never invoke an i915 callback inline. Pending/busy state uses the source
//! irq_work atomic flags word.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicU32, Ordering};

use kernel_guard::{BaseGuard, NoPreempt};

use crate::{
    intel_breadcrumbs_types_upstream::IntelBreadcrumbs, intel_context_upstream::IrqWork,
    intel_engine_cs_upstream::IntelEngineCs,
};

pub const IRQ_WORK_PENDING: u32 = 0x01;
pub const IRQ_WORK_BUSY: u32 = 0x02;
pub const IRQ_WORK_LAZY: u32 = 0x04;
pub const IRQ_WORK_HARD_IRQ: u32 = 0x08;
pub const IRQ_WORK_CLAIMED: u32 = IRQ_WORK_PENDING | IRQ_WORK_BUSY;

/// Linux bottom-half exclusion maps to non-preemptible task context here;
/// tasklet callbacks are dispatched as axtasks rather than softirq vectors.
#[inline]
pub fn local_bh_disable() {
    #[cfg(target_os = "none")]
    NoPreempt::acquire();
}

#[inline]
pub fn local_bh_enable() {
    #[cfg(target_os = "none")]
    NoPreempt::release(());
}

pub trait IrqWorkPtr {
    fn irq_work_ptr(self) -> *mut IrqWork;
}
impl IrqWorkPtr for *mut IrqWork {
    fn irq_work_ptr(self) -> *mut IrqWork {
        self
    }
}
impl IrqWorkPtr for *const IrqWork {
    fn irq_work_ptr(self) -> *mut IrqWork {
        self.cast_mut()
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
unsafe fn flags(work: *mut IrqWork) -> &'static AtomicU32 {
    assert!(!work.is_null());
    &*core::ptr::addr_of_mut!((*work).node.flags.counter).cast::<AtomicU32>()
}

/// Queue an irq_work callback once. The callback runs in a task context,
/// never synchronously in the caller's IRQ context.
pub fn irq_work_queue<W: IrqWorkPtr>(work: W) -> bool {
    let work = work.irq_work_ptr();
    assert!(!work.is_null());
    let state = unsafe { flags(work) };
    let mut old = state.load(Ordering::Acquire);
    loop {
        if old & IRQ_WORK_PENDING != 0 {
            return false;
        }
        match state.compare_exchange_weak(
            old,
            old | IRQ_WORK_CLAIMED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => old = actual,
        }
    }

    let address = work as usize;
    let queued = axtask::spawn(move || {
        let work = address as *mut IrqWork;
        let state = unsafe { flags(work) };
        let callback = unsafe { (*work).func };
        if let Some(callback) = callback {
            unsafe { callback(work) };
        }
        state.fetch_and(!(IRQ_WORK_PENDING | IRQ_WORK_BUSY), Ordering::Release);
    });
    if queued.is_err() {
        // Do not leave a permanently pending node if task allocation fails.
        state.fetch_and(!(IRQ_WORK_PENDING | IRQ_WORK_BUSY), Ordering::Release);
        return false;
    }
    true
}

/// Linux `intel_engine_signal_breadcrumbs()` queues the engine's irq_work
/// callback and never signals fences synchronously in the caller context.
pub unsafe fn intel_engine_signal_breadcrumbs(engine: *mut IntelEngineCs) {
    assert!(!engine.is_null());
    let breadcrumbs = unsafe { (*engine).breadcrumbs.cast::<IntelBreadcrumbs>() };
    assert!(!breadcrumbs.is_null());
    let work = unsafe { core::ptr::addr_of_mut!((*breadcrumbs).irq_work) };
    let _already_pending = irq_work_queue(work);
}

pub fn irq_work_sync<W: IrqWorkPtr>(work: W) {
    let work = work.irq_work_ptr();
    assert!(!work.is_null());
    while unsafe { flags(work) }.load(Ordering::Acquire) & IRQ_WORK_BUSY != 0 {
        axtask::yield_now();
    }
}

/// Linux atomic-context predicate mapped to the native task's guard nesting.
#[inline]
pub fn in_atomic() -> bool {
    axtask::current_may_uninit().is_some_and(|task| task.preempt_disable_count() != 0)
}

// upstream: i915_irq.c intel_synchronize_hardirq()
pub unsafe fn intel_synchronize_hardirq(i915: *mut crate::linux_i915_private::DrmI915Private) {
    let vector = crate::linux::dma::device_irq(unsafe { (*i915).drm.dev });
    axhal::irq::synchronize_hardirq(vector);
}

/// Linux i915 `intel_synchronize_irq()` for this kernel's single-stage IRQ
/// dispatch. TheKernel has no threaded-IRQ handler queue, so waiting for the
/// in-flight hard handler is the complete registered-handler grace period.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_synchronize_irq(
    i915: *mut crate::linux_i915_private::DrmI915Private,
) {
    unsafe { intel_synchronize_hardirq(i915) };
}
