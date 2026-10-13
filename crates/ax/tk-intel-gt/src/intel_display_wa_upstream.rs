// SPDX-License-Identifier: MIT
// Copyright © 2023 Intel Corporation
// Source: drivers/gpu/drm/i915/display/intel_display_wa.c and intel_display_wa.h

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_de_upstream::intel_de_rmw;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_device_upstream::{
    STEP_NONE, STEP_A0, STEP_A1, STEP_A2, STEP_B0, STEP_B1, STEP_C0,
    STEP_D0, STEP_D1, STEP_E0, STEP_F0, STEP_G0, STEP_H0, STEP_FUTURE,
};
use crate::intel_display_regs_upstream::{
    DDI_CLOCK_REG_ACCESS, DPCE_GATING_DIS, GEN8_CHICKEN_DCPR_1, GEN9_CLKGATE_DIS_5,
    ICL_DELAY_PMRSP,
};

pub const STEP_FOREVER: i32 = 42;

#[inline]
pub unsafe fn DISPLAY_VER(display: *const IntelDisplay) -> u8 {
    (*display).info.__runtime_info.ip.ver as u8
}

#[inline]
pub unsafe fn DISPLAY_VERx100(display: *const IntelDisplay) -> u32 {
    let ver = (*display).info.__runtime_info.ip.ver as u32;
    let rel = (*display).info.__runtime_info.ip.rel as u32;
    ver * 100 + rel
}

#[inline]
pub unsafe fn IS_DISPLAY_VER(display: *const IntelDisplay, from: u8, until: u8) -> bool {
    let ver = DISPLAY_VER(display);
    ver >= from && ver <= until
}

#[inline]
pub unsafe fn IS_DISPLAY_VERx100(display: *const IntelDisplay, from: u32, until: u32) -> bool {
    let ver_x100 = DISPLAY_VERx100(display);
    ver_x100 >= from && ver_x100 <= until
}

#[inline]
pub unsafe fn INTEL_DISPLAY_STEP(display: *const IntelDisplay) -> i32 {
    (*display).info.__runtime_info.step
}

#[inline]
pub unsafe fn IS_DISPLAY_STEP(display: *const IntelDisplay, since: i32, until: i32) -> bool {
    let step = INTEL_DISPLAY_STEP(display);
    step >= since && step < until
}

#[inline]
pub unsafe fn IS_DISPLAY_VERx100_STEP(display: *const IntelDisplay, ipver: u32, from: i32, until: i32) -> bool {
    IS_DISPLAY_VERx100(display, ipver, ipver) && IS_DISPLAY_STEP(display, from, until)
}

#[inline]
pub unsafe fn platform_alderlake_p(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 52) != 0
}

#[inline]
pub unsafe fn platform_alderlake_s(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 50) != 0
}

#[inline]
pub unsafe fn platform_rocketlake(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 48) != 0
}

#[inline]
pub unsafe fn platform_tigerlake(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 46) != 0
}

#[inline]
pub unsafe fn platform_dg1(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 49) != 0
}

#[inline]
pub unsafe fn platform_dg2(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 56) != 0
}

#[inline]
pub unsafe fn platform_battlemage(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[0] & (1u64 << 63) != 0
}

#[inline]
pub unsafe fn platform_pantherlake(display: *const IntelDisplay) -> bool {
    (*display).platform._anon_0_0.bitmap[1] & (1u64 << 0) != 0
}

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

// upstream: intel_display_wa.c gen11_display_wa_apply()
unsafe fn gen11_display_wa_apply(display: *mut IntelDisplay) {
    // Wa_14010594013
    intel_de_rmw(display, GEN8_CHICKEN_DCPR_1, 0, ICL_DELAY_PMRSP);
}

// upstream: intel_display_wa.c xe_d_display_wa_apply()
// skipped: not display 13
unsafe fn xe_d_display_wa_apply(_display: *mut IntelDisplay) {}

// upstream: intel_display_wa.c xe3plpd_display_wa_apply()
// skipped: not display 13
unsafe fn xe3plpd_display_wa_apply(_display: *mut IntelDisplay) {}

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
    if DISPLAY_VER(display) == 35 {
        xe3plpd_display_wa_apply(display);
    } else if platform_alderlake_p(display) {
        adlp_display_wa_apply(display);
    } else if DISPLAY_VER(display) == 12 {
        xe_d_display_wa_apply(display);
    } else if DISPLAY_VER(display) == 11 {
        gen11_display_wa_apply(display);
    }
}

// upstream: intel_display_wa.c intel_display_needs_wa_16025573575()
unsafe fn intel_display_needs_wa_16025573575(display: *mut IntelDisplay) -> bool {
    DISPLAY_VERx100(display) == 3000
        || DISPLAY_VERx100(display) == 3002
        || DISPLAY_VERx100(display) == 3500
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

    match wa {
        IntelDisplayWa::Wa1409120013 => IS_DISPLAY_VER(display, 11, 12),
        IntelDisplayWa::Wa1409767108 => {
            platform_alderlake_s(display)
                || (platform_rocketlake(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_B0))
        }
        IntelDisplayWa::Wa13012396614 => {
            DISPLAY_VERx100(display) == 3000 || DISPLAY_VERx100(display) == 3500
        }
        IntelDisplayWa::Wa14010477008 => {
            platform_dg1(display)
                || platform_rocketlake(display)
                || (platform_tigerlake(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_D0))
        }
        IntelDisplayWa::Wa14010480278 => IS_DISPLAY_VER(display, 10, 12),
        IntelDisplayWa::Wa14010547955 => platform_dg2(display),
        IntelDisplayWa::Wa14010685332 => false, // PCH_CNP..PCH_DG1 not applicable to display 13
        IntelDisplayWa::Wa14011294188 => false, // PCH_TGP..PCH_DG1 not applicable to display 13
        IntelDisplayWa::Wa14011503030
        | IntelDisplayWa::Wa14011503117
        | IntelDisplayWa::Wa22012358565 => DISPLAY_VER(display) == 13,
        IntelDisplayWa::Wa14011508470 => IS_DISPLAY_VERx100(display, 1200, 1300),
        IntelDisplayWa::Wa14011765242 => {
            platform_alderlake_s(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_A2)
        }
        IntelDisplayWa::Wa14014143976 => IS_DISPLAY_STEP(display, STEP_E0, STEP_FOREVER),
        IntelDisplayWa::Wa14016740474 => {
            IS_DISPLAY_VERx100_STEP(display, 1400, STEP_A0, STEP_C0)
        }
        IntelDisplayWa::Wa14020863754 => {
            DISPLAY_VERx100(display) == 3000
                || DISPLAY_VERx100(display) == 2000
                || DISPLAY_VERx100(display) == 1401
        }
        IntelDisplayWa::Wa14025769978 => DISPLAY_VER(display) == 35,
        IntelDisplayWa::Wa15013987218 => DISPLAY_VER(display) == 20,
        IntelDisplayWa::Wa15018326506 => platform_battlemage(display),
        IntelDisplayWa::Wa16011303918 | IntelDisplayWa::Wa22011320316 => {
            platform_alderlake_p(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_B0)
        }
        IntelDisplayWa::Wa16011181250 => {
            platform_rocketlake(display) || platform_alderlake_s(display) || platform_dg2(display)
        }
        IntelDisplayWa::Wa16011342517 => {
            platform_alderlake_p(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_D0)
        }
        IntelDisplayWa::Wa16011863758 => DISPLAY_VER(display) >= 11,
        IntelDisplayWa::Wa16023588340 => intel_display_needs_wa_16023588340(display),
        IntelDisplayWa::Wa16025573575 => intel_display_needs_wa_16025573575(display),
        IntelDisplayWa::Wa16025596647 => {
            DISPLAY_VER(display) == 20 && IS_DISPLAY_VERx100_STEP(display, 3000, STEP_A0, STEP_B0)
        }
        IntelDisplayWa::Wa16029024088 => DISPLAY_VER(display) >= 35,
        IntelDisplayWa::Wa18034343758 => {
            DISPLAY_VER(display) == 20
                || (platform_pantherlake(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_B0))
        }
        IntelDisplayWa::Wa22010178259 => DISPLAY_VER(display) == 12,
        IntelDisplayWa::Wa22010947358 => platform_alderlake_p(display),
        IntelDisplayWa::Wa22012278275 => {
            platform_alderlake_p(display) && IS_DISPLAY_STEP(display, STEP_A0, STEP_E0)
        }
        IntelDisplayWa::Wa22014263786 => IS_DISPLAY_VERx100(display, 1100, 1400),
        IntelDisplayWa::Wa22021048059 => IS_DISPLAY_VER(display, 14, 35),
    }
}
