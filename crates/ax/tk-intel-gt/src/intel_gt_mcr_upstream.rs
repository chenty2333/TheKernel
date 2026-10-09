// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// API and macro translation of Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_gt_mcr.h.

#![allow(unsafe_code)]

use crate::{
    intel_engine_types_upstream::IntelEngineCs, intel_gt_types_upstream::IntelGt,
    intel_workarounds_types_upstream::I915McrRegT, linux::fields::DrmPrinter,
};

unsafe extern "C" {
    pub fn intel_gt_mcr_init(gt: *mut IntelGt);
    pub fn intel_gt_mcr_lock(gt: *mut IntelGt, flags: *mut usize);
    pub fn intel_gt_mcr_unlock(gt: *mut IntelGt, flags: usize);
    pub fn intel_gt_mcr_lock_sanitize(gt: *mut IntelGt);
    pub fn intel_gt_mcr_read(gt: *mut IntelGt, reg: I915McrRegT, group: i32, instance: i32) -> u32;
    pub fn intel_gt_mcr_read_any_fw(gt: *mut IntelGt, reg: I915McrRegT) -> u32;
    pub fn intel_gt_mcr_read_any(gt: *mut IntelGt, reg: I915McrRegT) -> u32;
    pub fn intel_gt_mcr_unicast_write(
        gt: *mut IntelGt,
        reg: I915McrRegT,
        value: u32,
        group: i32,
        instance: i32,
    );
    pub fn intel_gt_mcr_multicast_write(gt: *mut IntelGt, reg: I915McrRegT, value: u32);
    pub fn intel_gt_mcr_multicast_write_fw(gt: *mut IntelGt, reg: I915McrRegT, value: u32);
    pub fn intel_gt_mcr_multicast_rmw(
        gt: *mut IntelGt,
        reg: I915McrRegT,
        clear: u32,
        set: u32,
    ) -> u32;
    pub fn intel_gt_mcr_get_nonterminated_steering(
        gt: *mut IntelGt,
        reg: I915McrRegT,
        group: *mut u8,
        instance: *mut u8,
    );
    pub fn intel_gt_mcr_report_steering(
        printer: *mut DrmPrinter,
        gt: *mut IntelGt,
        dump_table: bool,
    );
    pub fn intel_gt_mcr_get_ss_steering(
        gt: *mut IntelGt,
        dss: u32,
        group: *mut u32,
        instance: *mut u32,
    );
    pub fn intel_gt_mcr_wait_for_reg(
        gt: *mut IntelGt,
        reg: I915McrRegT,
        mask: u32,
        value: u32,
        fast_timeout_us: u32,
        slow_timeout_ms: u32,
    ) -> i32;
}

/// `_HAS_SS()` from intel_gt_mcr.h, using source topology before IP 12.55 and
/// DSS-index lookup on Xe_HP and newer.
#[inline]
pub unsafe fn has_subslice_steering(
    gt: *mut IntelGt,
    dss: usize,
    group: u32,
    instance: u32,
) -> bool {
    let i915 = unsafe { (*gt).i915 };
    if crate::linux::i915::GRAPHICS_VER_FULL(i915) >= crate::linux::i915::IP_VER(12, 55) {
        unsafe {
            crate::intel_sseu_types_upstream::intel_sseu_has_subslice(
                &(*gt).info.sseu,
                0,
                dss as i32,
            )
        }
    } else {
        unsafe {
            crate::intel_sseu_types_upstream::intel_sseu_has_subslice(
                &(*gt).info.sseu,
                group as i32,
                instance as i32,
            )
        }
    }
}

/// Source `for_each_ss_steering()` expansion, including its per-DSS steering
/// lookup before topology testing and index increment ordering.
#[macro_export]
macro_rules! for_each_ss_steering {
    ($dss:ident, $gt:expr, $group:ident, $instance:ident, $body:block) => {{
        let __gt = $gt;
        let mut $dss: usize = 0;
        let mut $group: u32 = 0;
        let mut $instance: u32 = 0;
        while $dss < $crate::intel_sseu_types_upstream::I915_MAX_SS_FUSE_BITS {
            unsafe {
                $crate::intel_gt_mcr_upstream::intel_gt_mcr_get_ss_steering(
                    __gt,
                    $dss as u32,
                    core::ptr::addr_of_mut!($group),
                    core::ptr::addr_of_mut!($instance),
                );
            }
            if unsafe {
                $crate::intel_gt_mcr_upstream::has_subslice_steering(__gt, $dss, $group, $instance)
            } {
                $body
            }
            $dss += 1;
        }
    }};
}
