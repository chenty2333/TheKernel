// SPDX-License-Identifier: MIT
// Copyright 2026 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_clock_gating.c and intel_display_clock_gating.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

// upstream: intel_display_clock_gating.c intel_display_adlp_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_adlp_init_clock_gating(display: *mut IntelDisplay) {
    // Clock gating for ADL-P is handled via display workarounds (adlp_display_wa_apply)
    crate::intel_display_wa_upstream::intel_display_wa_apply(display);
}

// upstream: intel_display_clock_gating.c intel_display_skl_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_skl_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_kbl_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_kbl_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_cfl_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_cfl_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_bxt_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_bxt_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_glk_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_glk_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_bdw_clock_gating_disable_fbcq()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_bdw_clock_gating_disable_fbcq(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_bdw_clock_gating_vblank_in_srd()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_bdw_clock_gating_vblank_in_srd(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_bdw_clock_gating_kvm_notif()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_bdw_clock_gating_kvm_notif(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_hsw_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_hsw_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_disable_trickle_feed()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_disable_trickle_feed(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_ilk_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_ilk_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_gen6_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_gen6_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_ivb_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_ivb_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_g4x_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_g4x_init_clock_gating(_display: *mut IntelDisplay) {}

// upstream: intel_display_clock_gating.c intel_display_i965gm_init_clock_gating()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_i965gm_init_clock_gating(_display: *mut IntelDisplay) {}
