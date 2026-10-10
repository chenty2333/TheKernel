// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
// Source-order translation of Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_rc.c.

#![allow(unsafe_code, non_snake_case)]

use core::{ffi::c_void, ptr};

use crate::{
    intel_engine_cs_upstream::IntelGt,
    intel_gt_api_upstream::{guc_to_gt, guc_to_i915},
    intel_guc_actions_abi_types_upstream::{IntelGucAction, IntelGucRcOptions},
    intel_guc_ct_upstream::intel_guc_ct_send,
    intel_guc_rc_types_upstream::intel_guc_rc_is_supported,
    intel_guc_types_upstream::{IntelGuc, intel_guc_is_fw_running},
    intel_uc_types_upstream::intel_uc_uses_guc_rc,
    linux_config::{EINVAL, EOPNOTSUPP, EPROTO},
};

#[inline]
fn str_enable_disable(enable: bool) -> &'static str {
    if enable { "enable" } else { "disable" }
}

#[inline]
fn str_enabled_disabled(enable: bool) -> &'static str {
    if enable { "enabled" } else { "disabled" }
}

// upstream: intel_guc_rc.c __guc_rc_supported()
unsafe fn __guc_rc_supported(guc: *mut IntelGuc) -> bool {
    let submission_supported = unsafe { (*guc).submission_supported };
    let graphics_version = unsafe { crate::linux::i915::GRAPHICS_VER(guc_to_i915(guc)) };
    submission_supported && graphics_version >= 12
}

// upstream: intel_guc_rc.c __guc_rc_selected()
unsafe fn __guc_rc_selected(guc: *mut IntelGuc) -> bool {
    if !unsafe { intel_guc_rc_is_supported(guc) } {
        return false;
    }
    unsafe { (*guc).submission_selected }
}

// upstream: intel_guc_rc.c intel_guc_rc_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_rc_init_early(guc: *mut IntelGuc) {
    unsafe {
        (*guc).rc_supported = __guc_rc_supported(guc);
        (*guc).rc_selected = __guc_rc_selected(guc);
    }
}

// upstream: intel_guc_rc.c guc_action_control_gucrc()
unsafe fn guc_action_control_gucrc(guc: *mut IntelGuc, enable: bool) -> i32 {
    let rc_mode = if enable {
        IntelGucRcOptions::INTEL_GUCRC_FIRMWARE_CONTROL as u32
    } else {
        IntelGucRcOptions::INTEL_GUCRC_HOST_CONTROL as u32
    };
    let action = [
        IntelGucAction::INTEL_GUC_ACTION_SETUP_PC_GUCRC as u32,
        rc_mode,
    ];
    let ret = unsafe {
        intel_guc_ct_send(
            ptr::addr_of_mut!((*guc).ct),
            action.as_ptr(),
            action.len() as u32,
            ptr::null_mut(),
            0,
            0,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_rc.c __guc_rc_control()
unsafe fn __guc_rc_control(guc: *mut IntelGuc, enable: bool) -> i32 {
    let gt: *mut IntelGt = unsafe { guc_to_gt(guc) };
    if !unsafe { intel_uc_uses_guc_rc(ptr::addr_of_mut!((*gt).uc)) } {
        return -EOPNOTSUPP;
    }

    let ready = unsafe { intel_guc_is_fw_running(guc) } && unsafe { (*guc).ct.enabled };
    if !ready {
        return -EINVAL;
    }

    let ret = unsafe { guc_action_control_gucrc(guc, enable) };
    if ret != 0 {
        guc_probe_error!(
            guc,
            "Failed to %s RC (%pe)\n",
            str_enable_disable(enable),
            crate::linux_config::ERR_PTR::<c_void>(ret),
        );
        return ret;
    }

    guc_info!(guc, "RC %s\n", str_enabled_disabled(enable));
    0
}

// upstream: intel_guc_rc.c intel_guc_rc_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_rc_enable(guc: *mut IntelGuc) -> i32 {
    unsafe { __guc_rc_control(guc, true) }
}

// upstream: intel_guc_rc.c intel_guc_rc_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_rc_disable(guc: *mut IntelGuc) -> i32 {
    unsafe { __guc_rc_control(guc, false) }
}
