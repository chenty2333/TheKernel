// SPDX-License-Identifier: MIT
// Copyright 2026 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_clock_gating.c and intel_display_clock_gating.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_de_upstream::intel_de_rmw;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_regs_upstream::{
    CHICKEN_PAR1_1, GEN8_CHICKEN_DCPR_1, MASK_WAKEMEM, SKL_EDP_PSR_FIX_RDWRAP,
};
use crate::intel_workarounds_types_upstream::I915RegT;

const DISP_ARB_CTL: I915RegT = I915RegT { reg: 0x45000 };
const DISP_FBC_MEMORY_WAKE: u32 = 1 << 31;

// upstream: intel_display_clock_gating.c intel_display_gen9_init_clock_gating()
unsafe fn intel_display_gen9_init_clock_gating(display: *mut IntelDisplay) {
    // See Bspec note for PSR2_CTL bit 31, Wa#828:skl,bxt,kbl,cfl
    intel_de_rmw(display, CHICKEN_PAR1_1, 0, SKL_EDP_PSR_FIX_RDWRAP);

    // WaEnableChickenDCPR:skl,bxt,kbl,glk,cfl
    intel_de_rmw(display, GEN8_CHICKEN_DCPR_1, 0, MASK_WAKEMEM);

    // WaFbcWakeMemOn:skl,bxt,kbl,glk,cfl
    // Display WA #0859: skl,bxt,kbl,glk,cfl
    intel_de_rmw(display, DISP_ARB_CTL, 0, DISP_FBC_MEMORY_WAKE);
}

// upstream: intel_display_clock_gating.c intel_display_adlp_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_adlp_init_clock_gating(display: *mut IntelDisplay) {
    // Clock gating for ADL-P is handled via display workarounds (adlp_display_wa_apply)
    crate::intel_display_wa_upstream::intel_display_wa_apply(display);
}

// upstream: intel_display_clock_gating.c intel_display_skl_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_skl_init_clock_gating(display: *mut IntelDisplay) {
    intel_display_gen9_init_clock_gating(display);
}

// upstream: intel_display_clock_gating.c intel_display_kbl_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_kbl_init_clock_gating(display: *mut IntelDisplay) {
    intel_display_gen9_init_clock_gating(display);
}

// upstream: intel_display_clock_gating.c intel_display_cfl_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_cfl_init_clock_gating(display: *mut IntelDisplay) {
    intel_display_gen9_init_clock_gating(display);
}

// upstream: intel_display_clock_gating.c intel_display_bxt_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_bxt_init_clock_gating(display: *mut IntelDisplay) {
    intel_display_gen9_init_clock_gating(display);
}

// upstream: intel_display_clock_gating.c intel_display_glk_init_clock_gating()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_glk_init_clock_gating(display: *mut IntelDisplay) {
    intel_display_gen9_init_clock_gating(display);
}

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
