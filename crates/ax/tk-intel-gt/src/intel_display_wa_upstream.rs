// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_wa.c and intel_display_wa.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_de_upstream::intel_de_rmw;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_regs_upstream::{
    DDI_CLOCK_REG_ACCESS, DPCE_GATING_DIS, GEN8_CHICKEN_DCPR_1, GEN9_CLKGATE_DIS_5,
};

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum IntelDisplayWa {
    Wa1409120013,
    Wa1409767108,
    Wa13012396614,
    Wa14010477008,
    Wa14010480278,
    Wa14010547955,
    Wa14010685332,
    Wa14011294188,
    Wa14011503030,
    Wa14011503117,
    Wa14011508470,
    Wa14011765242,
    Wa14014143976,
    Wa14016740474,
    Wa14020863754,
    Wa14025769978,
    Wa15013987218,
    Wa15018326506,
    Wa16011181250,
    Wa16011303918,
    Wa16011342517,
    Wa16011863758,
    Wa16023588340,
    Wa16025573575,
    Wa16025596647,
    Wa16029024088,
    Wa18034343758,
    Wa22010178259,
    Wa22010947358,
    Wa22011320316,
    Wa22012278275,
    Wa22012358565,
    Wa22014263786,
    Wa22021048059,
}

// upstream: intel_display_wa.c adlp_display_wa_apply()
unsafe fn adlp_display_wa_apply(display: *mut IntelDisplay) {
    // Wa_22011091694:adlp
    intel_de_rmw(display, GEN9_CLKGATE_DIS_5, 0, DPCE_GATING_DIS);

    // Bspec/49189 Initialize Sequence
    intel_de_rmw(display, GEN8_CHICKEN_DCPR_1, DDI_CLOCK_REG_ACCESS, 0);
}

// upstream: intel_display_wa.c intel_display_wa_apply()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_wa_apply(display: *mut IntelDisplay) {
    if display.is_null() {
        return;
    }
    let ver = (*display).info.__runtime_info.ip.ver;
    let is_adlp = (*display).platform._anon_0_0.bitmap[0] & (1 << 3) != 0; // alderlake_p bit

    if is_adlp || ver == 13 {
        adlp_display_wa_apply(display);
    }
    // skipped: not display 13 (xe3plpd, xe_d, gen11)
}

// upstream: intel_display_wa.c intel_display_needs_wa_16023588340()
// skipped: not display 13
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_needs_wa_16023588340(_display: *mut IntelDisplay) -> bool {
    false
}

// upstream: intel_display_wa.c __intel_display_wa()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __intel_display_wa(
    display: *mut IntelDisplay,
    wa: IntelDisplayWa,
    _name: *const core::ffi::c_char,
) -> bool {
    if display.is_null() {
        return false;
    }
    let ver = (*display).info.__runtime_info.ip.ver;
    let step = (*display).info.__runtime_info.step;
    let is_adlp = (*display).platform._anon_0_0.bitmap[0] & (1 << 3) != 0;

    match wa {
        IntelDisplayWa::Wa14011503030
        | IntelDisplayWa::Wa14011503117
        | IntelDisplayWa::Wa22012358565 => ver == 13,

        IntelDisplayWa::Wa14011508470 => (12..=13).contains(&ver),

        IntelDisplayWa::Wa16011303918 | IntelDisplayWa::Wa22011320316 => {
            is_adlp && step <= 1 // STEP_B0
        }
        IntelDisplayWa::Wa16011342517 => {
            is_adlp && step <= 3 // STEP_D0
        }
        IntelDisplayWa::Wa22012278275 => {
            is_adlp && step <= 4 // STEP_E0
        }
        IntelDisplayWa::Wa16011863758 => ver >= 11,
        IntelDisplayWa::Wa22010947358 => is_adlp,
        IntelDisplayWa::Wa22014263786 => (11..=14).contains(&ver),
        // other workarounds skipped (not display 13 / ADL-P)
        _ => false,
    }
}
