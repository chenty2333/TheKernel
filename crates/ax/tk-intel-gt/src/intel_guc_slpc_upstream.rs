// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
//
//! SLPC declarations and inline helpers from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/uc/intel_guc_slpc.h`.

#![allow(unsafe_code)]

use crate::{
    i915_request_types_upstream::DrmPrinter, intel_gt_types_upstream::IntelGt,
    intel_guc_slpc_types_upstream::IntelGucSlpc,
    intel_guc_submission_types_upstream::intel_guc_submission_is_used,
    intel_guc_types_upstream::IntelGuc,
};

pub const SLPC_MAX_FREQ_MHZ: i32 = 4250;

/// `intel_guc_slpc_is_supported()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_slpc_is_supported(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).slpc.supported }
}

/// `intel_guc_slpc_is_wanted()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_slpc_is_wanted(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { (*guc).slpc.selected }
}

/// `intel_guc_slpc_is_used()`.
///
/// # Safety
/// `guc` must point to a live `intel_guc`.
pub unsafe fn intel_guc_slpc_is_used(guc: *mut IntelGuc) -> bool {
    assert!(!guc.is_null());
    unsafe { intel_guc_submission_is_used(guc) && intel_guc_slpc_is_wanted(guc) }
}

// Out-of-line implementations owned by intel_guc_slpc.c.
unsafe extern "C" {
    pub fn intel_guc_slpc_init_early(slpc: *mut IntelGucSlpc);
    pub fn intel_guc_slpc_init(slpc: *mut IntelGucSlpc) -> i32;
    pub fn intel_guc_slpc_enable(slpc: *mut IntelGucSlpc) -> i32;
    pub fn intel_guc_slpc_fini(slpc: *mut IntelGucSlpc);
    pub fn intel_guc_slpc_set_max_freq(slpc: *mut IntelGucSlpc, value: u32) -> i32;
    pub fn intel_guc_slpc_set_min_freq(slpc: *mut IntelGucSlpc, value: u32) -> i32;
    pub fn intel_guc_slpc_set_boost_freq(slpc: *mut IntelGucSlpc, value: u32) -> i32;
    pub fn intel_guc_slpc_get_max_freq(slpc: *mut IntelGucSlpc, value: *mut u32) -> i32;
    pub fn intel_guc_slpc_get_min_freq(slpc: *mut IntelGucSlpc, value: *mut u32) -> i32;
    pub fn intel_guc_slpc_print_info(slpc: *mut IntelGucSlpc, printer: *mut DrmPrinter) -> i32;
    pub fn intel_guc_slpc_set_media_ratio_mode(slpc: *mut IntelGucSlpc, value: u32) -> i32;
    pub fn intel_guc_pm_intrmsk_enable(gt: *mut IntelGt);
    pub fn intel_guc_slpc_boost(slpc: *mut IntelGucSlpc);
    pub fn intel_guc_slpc_dec_waiters(slpc: *mut IntelGucSlpc);
    pub fn intel_guc_slpc_set_ignore_eff_freq(slpc: *mut IntelGucSlpc, value: bool) -> i32;
    pub fn intel_guc_slpc_set_strategy(slpc: *mut IntelGucSlpc, value: u32) -> i32;
    pub fn intel_guc_slpc_set_power_profile(slpc: *mut IntelGucSlpc, value: u32) -> i32;
}
