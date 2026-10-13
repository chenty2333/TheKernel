// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_mchbar.c (Linux v7.2.3)

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_de_upstream::{intel_de_read, intel_de_read16, intel_de_read64_2x32},
    intel_display_core_types_upstream::IntelDisplay,
    intel_display_wa_upstream::{DISPLAY_VER, platform_rocketlake},
    intel_workarounds_types_upstream::I915RegT,
};

const MCHBAR_MIRROR_BASE: u32 = 0x10000;
const MCHBAR_MIRROR_END: u32 = 0x13fff;
const MCHBAR_MIRROR_BASE_SNB: u32 = 0x140000;
const MCHBAR_MIRROR_END_SNB: u32 = 0x147fff;
const MCHBAR_MIRROR_END_ICL_RKL: u32 = 0x14ffff;
const MCHBAR_MIRROR_END_TGL: u32 = 0x15ffff;

fn mchbar_mirror_range(display_ver: u8, rocketlake: bool) -> Option<(u32, u32)> {
    if display_ver >= 14 {
        return None;
    }
    let base = if display_ver >= 6 {
        MCHBAR_MIRROR_BASE_SNB
    } else {
        MCHBAR_MIRROR_BASE
    };
    let end = if display_ver >= 12 && !rocketlake {
        MCHBAR_MIRROR_END_TGL
    } else if display_ver >= 11 {
        MCHBAR_MIRROR_END_ICL_RKL
    } else if display_ver >= 6 {
        MCHBAR_MIRROR_END_SNB
    } else {
        MCHBAR_MIRROR_END
    };
    Some((base, end))
}

fn is_mchbar_reg(display_ver: u8, rocketlake: bool, reg: I915RegT) -> bool {
    mchbar_mirror_range(display_ver, rocketlake)
        .is_some_and(|(start, end)| reg.reg >= start && reg.reg <= end)
}

fn checked_mchbar_reg(display: *mut IntelDisplay, reg: I915RegT) -> bool {
    if display.is_null() {
        return false;
    }
    is_mchbar_reg(
        unsafe { DISPLAY_VER(display) },
        unsafe { platform_rocketlake(display) },
        reg,
    )
}

// upstream: intel_mchbar.c intel_mchbar_read16()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mchbar_read16(display: *mut IntelDisplay, reg: I915RegT) -> u16 {
    if !checked_mchbar_reg(display, reg) {
        return 0;
    }
    unsafe { intel_de_read16(display, reg) }
}

// upstream: intel_mchbar.c intel_mchbar_read()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mchbar_read(display: *mut IntelDisplay, reg: I915RegT) -> u32 {
    if !checked_mchbar_reg(display, reg) {
        return 0;
    }
    unsafe { intel_de_read(display, reg) }
}

// upstream: intel_mchbar.c intel_mchbar_read64_2x32()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_mchbar_read64_2x32(
    display: *mut IntelDisplay,
    reg: I915RegT,
) -> u64 {
    if !checked_mchbar_reg(display, reg) {
        return 0;
    }
    unsafe { intel_de_read64_2x32(display, reg) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_bounds_follow_display_generation() {
        assert!(is_mchbar_reg(13, false, I915RegT { reg: 0x140000 }));
        assert!(is_mchbar_reg(13, false, I915RegT { reg: 0x15ffff }));
        assert!(!is_mchbar_reg(13, false, I915RegT { reg: 0x160000 }));
        assert!(is_mchbar_reg(11, false, I915RegT { reg: 0x14ffff }));
        assert!(!is_mchbar_reg(11, false, I915RegT { reg: 0x150000 }));
        assert!(!is_mchbar_reg(14, false, I915RegT { reg: 0x140000 }));
        assert!(is_mchbar_reg(12, true, I915RegT { reg: 0x14ffff }));
        assert!(!is_mchbar_reg(12, true, I915RegT { reg: 0x150000 }));
    }
}
