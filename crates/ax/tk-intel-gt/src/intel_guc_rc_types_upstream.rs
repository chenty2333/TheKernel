// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
//
//! GuC RC declarations and inline state helpers from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_guc_rc.h`.

#![allow(unsafe_code)]

use crate::{
    intel_guc_submission_types_upstream::intel_guc_submission_is_used,
    intel_guc_types_upstream::IntelGuc,
};

// Out-of-line implementations owned by intel_guc_rc.c.
unsafe extern "C" {
    pub fn intel_guc_rc_init_early(guc: *mut IntelGuc);
    pub fn intel_guc_rc_enable(guc: *mut IntelGuc) -> i32;
    pub fn intel_guc_rc_disable(guc: *mut IntelGuc) -> i32;
}

/// `intel_guc_rc_is_supported()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_rc_is_supported(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).rc_supported }
}

/// `intel_guc_rc_is_wanted()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_rc_is_wanted(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).submission_selected && intel_guc_rc_is_supported(guc) }
}

/// `intel_guc_rc_is_used()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_rc_is_used(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { intel_guc_submission_is_used(guc) && intel_guc_rc_is_wanted(guc) }
}
