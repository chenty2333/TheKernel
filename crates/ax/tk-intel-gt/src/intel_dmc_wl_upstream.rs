// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_dmc_wl.c (Linux v7.2.3)

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_display_core_types_upstream::IntelDisplay, intel_display_wa_upstream::DISPLAY_VER,
    intel_workarounds_types_upstream::I915RegT,
};

const ENABLE_DMC_WL_DISABLED: i32 = 0;
const ENABLE_DMC_WL_ENABLED: i32 = 1;
const ENABLE_DMC_WL_MAX: i32 = 4;

fn sanitize_enable_dmc_wl(display_ver: u8, setting: i32) -> i32 {
    if display_ver < 20 {
        ENABLE_DMC_WL_DISABLED
    } else if setting < 0 {
        if display_ver >= 30 {
            ENABLE_DMC_WL_ENABLED
        } else {
            ENABLE_DMC_WL_DISABLED
        }
    } else if setting >= ENABLE_DMC_WL_MAX {
        ENABLE_DMC_WL_ENABLED
    } else {
        setting
    }
}

// upstream: intel_dmc_wl.c intel_dmc_wl_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_init(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let version = unsafe { DISPLAY_VER(display) };
    let configured = unsafe { (*display).params.enable_dmc_wl };
    let sanitized = sanitize_enable_dmc_wl(version, configured);
    // Linux's wakelock interface first exists on Xe2-LPD. The current native
    // display target is ADL-P (display 13), and this port does not implement
    // its future-generation spinlock/timer MMIO protocol. Keep the feature
    // explicitly disabled rather than exposing a no-op lock as functional.
    unsafe { (*display).params.enable_dmc_wl = ENABLE_DMC_WL_DISABLED.min(sanitized) };
}

// ADL-P has enable_dmc_wl sanitized to zero by the upstream < 20 rule.
// These callbacks therefore take the exact Linux early-return path for the
// supported platform. The nonzero path is deliberately not emulated here.

// upstream: intel_dmc_wl.c intel_dmc_wl_get()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_get(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_put()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_put(_display: *mut IntelDisplay, _reg: I915RegT) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_get_noreg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_get_noreg(display: *mut IntelDisplay) {
    unsafe { intel_dmc_wl_get(display, I915RegT { reg: 0 }) };
}

// upstream: intel_dmc_wl.c intel_dmc_wl_put_noreg()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_put_noreg(display: *mut IntelDisplay) {
    unsafe { intel_dmc_wl_put(display, I915RegT { reg: 0 }) };
}

// upstream: intel_dmc_wl.c intel_dmc_wl_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_enable(_display: *mut IntelDisplay, _dc_state: u32) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_disable(_display: *mut IntelDisplay) {}

// upstream: intel_dmc_wl.c intel_dmc_wl_flush_release_work()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dmc_wl_flush_release_work(_display: *mut IntelDisplay) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_sanitizer_disables_adlp_and_defaults_newer_platforms() {
        assert_eq!(sanitize_enable_dmc_wl(13, -1), ENABLE_DMC_WL_DISABLED);
        assert_eq!(sanitize_enable_dmc_wl(20, -1), ENABLE_DMC_WL_DISABLED);
        assert_eq!(sanitize_enable_dmc_wl(30, -1), ENABLE_DMC_WL_ENABLED);
        assert_eq!(sanitize_enable_dmc_wl(30, 4), ENABLE_DMC_WL_ENABLED);
    }
}
