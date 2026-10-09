// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/uc/intel_guc_submission.h.

use crate::{
    i915_request_types_upstream::{DrmPrinter, I915Request},
    intel_engine_cs_upstream::AtomicT,
    intel_engine_types_upstream::IntelEngineCs,
    intel_gt_types_upstream::IntelGt,
    intel_guc_types_upstream::{IntelGuc, intel_guc_is_used},
};

// Out-of-line submission entry points declared by intel_guc_submission.h.
// The implementation remains owned by intel_guc_submission.c.
unsafe extern "C" {
    pub fn intel_guc_submission_init_early(guc: *mut IntelGuc);
    pub fn intel_guc_submission_init(guc: *mut IntelGuc) -> i32;
    pub fn intel_guc_submission_enable(guc: *mut IntelGuc) -> i32;
    pub fn intel_guc_submission_disable(guc: *mut IntelGuc);
    pub fn intel_guc_submission_fini(guc: *mut IntelGuc);
    pub fn intel_guc_preempt_work_create(guc: *mut IntelGuc) -> i32;
    pub fn intel_guc_preempt_work_destroy(guc: *mut IntelGuc);
    pub fn intel_guc_submission_setup(engine: *mut IntelEngineCs) -> i32;
    pub fn intel_guc_submission_print_info(guc: *mut IntelGuc, printer: *mut DrmPrinter);
    pub fn intel_guc_submission_print_context_info(guc: *mut IntelGuc, printer: *mut DrmPrinter);
    pub fn intel_guc_dump_active_requests(
        engine: *mut IntelEngineCs,
        hung_rq: *mut I915Request,
        printer: *mut DrmPrinter,
    );
    pub fn intel_guc_busyness_park(gt: *mut IntelGt);
    pub fn intel_guc_busyness_unpark(gt: *mut IntelGt);
    pub fn intel_guc_virtual_engine_has_heartbeat(engine: *const IntelEngineCs) -> bool;
    pub fn intel_guc_wait_for_pending_msg(
        guc: *mut IntelGuc,
        wait_var: *mut AtomicT,
        interruptible: bool,
        timeout: isize,
    ) -> i32;
    pub fn intel_guc_submission_flush_work(guc: *mut IntelGuc);
}

/// `intel_guc_submission_is_supported()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_submission_is_supported(guc: *mut IntelGuc) -> bool {
    unsafe { (*guc).submission_supported }
}

/// `intel_guc_submission_is_wanted()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_submission_is_wanted(guc: *mut IntelGuc) -> bool {
    unsafe { (*guc).submission_selected }
}

/// `intel_guc_submission_is_used()`.
///
/// `intel_guc_is_used()` is owned by the canonical `intel_guc.h` translation;
/// this helper deliberately delegates rather than duplicating its firmware
/// status checks.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_submission_is_used(guc: *mut IntelGuc) -> bool {
    unsafe { intel_guc_is_used(guc) && intel_guc_submission_is_wanted(guc) }
}
