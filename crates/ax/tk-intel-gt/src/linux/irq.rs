// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux irq_work callbacks are deferred onto a task context so IRQ producers
//! never invoke an i915 callback inline. The generic irq_work queue lives in
//! tk-linux-kapi (`irq_work`); this module keeps the i915 breadcrumb and
//! hardirq-sync entry points.

#![allow(unsafe_code)]

use kernel_guard::{BaseGuard, NoPreempt};

use crate::{
    intel_breadcrumbs_types_upstream::IntelBreadcrumbs, intel_engine_cs_upstream::IntelEngineCs,
};

pub use tk_linux_kapi::irq_work::{init_irq_work, irq_work_queue, irq_work_sync};

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

/// Linux atomic-context predicate mapped to the native task's guard nesting.
#[inline]
pub fn in_atomic() -> bool {
    axtask::current_may_uninit().is_some_and(|task| task.preempt_disable_count() != 0)
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
