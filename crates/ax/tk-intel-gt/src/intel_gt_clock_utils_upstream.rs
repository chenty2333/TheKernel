// SPDX-License-Identifier: MIT
// Copyright © 2020 Intel Corporation.
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_gt_clock_utils.c.
// Header constants are from i915_reg.h and gt/intel_gt_regs.h; grant in ../LICENSE-MIT.

#![allow(unsafe_code, non_snake_case, dead_code)]

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_uncore_types_upstream::{IntelUncore, intel_uncore_read},
    linux::{
        i915::{GRAPHICS_VER, IS_G4X, IS_GEN9_LP},
        registers::_MMIO,
    },
    linux_i915_private::DrmI915Private,
};

const NSEC_PER_SEC: u64 = 1_000_000_000;
const USEC_PER_SEC: u64 = 1_000_000;
const S32_MAX: u64 = i32::MAX as u64;

const GEN9_TIMESTAMP_OVERRIDE: u32 = 0x44074;
const GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_SHIFT: u32 = 0;
const GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_MASK: u32 = 0x3ff;
const GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_SHIFT: u32 = 12;
const GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_MASK: u32 = 0xf << 12;
const CTC_MODE: u32 = 0xa26c;
const CTC_SOURCE_PARAMETER_MASK: u32 = 1;
const CTC_SOURCE_DIVIDE_LOGIC: u32 = 1;
const CTC_SHIFT_PARAMETER_MASK: u32 = 0x6;
const GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK: u32 = 0x6;
const RPM_CONFIG0: u32 = 0x0d00;
const GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_MASK: u32 = 0x7 << 3;
const GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_24_MHZ: u32 = 0 << 3;
const GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_19_2_MHZ: u32 = 1 << 3;
const GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_38_4_MHZ: u32 = 2 << 3;
const GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_25_MHZ: u32 = 3 << 3;

#[inline]
fn div_round_closest(value: u32, divisor: u32) -> u32 {
    (value + divisor / 2) / divisor
}

#[inline]
fn reg_field_get(mask: u32, value: u32) -> u32 {
    (value & mask) >> mask.trailing_zeros()
}

/// Header dependency from i915_freq.h; implemented by the companion
/// `i915_freq.c` translation owned by the coordinating GT pass.
unsafe extern "C" {
    fn i9xx_fsb_freq(i915: *mut DrmI915Private) -> u32;
}

// upstream: intel_gt_clock_utils.c read_reference_ts_freq()
unsafe fn read_reference_ts_freq(uncore: *mut IntelUncore) -> u32 {
    let ts_override = unsafe { intel_uncore_read(uncore, _MMIO(GEN9_TIMESTAMP_OVERRIDE)) };
    let mut base_freq;
    let mut frac_freq;

    base_freq = ((ts_override & GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_MASK)
        >> GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DIVIDER_SHIFT)
        + 1;
    base_freq *= 1_000_000;

    frac_freq = ((ts_override & GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_MASK)
        >> GEN9_TIMESTAMP_OVERRIDE_US_COUNTER_DENOMINATOR_SHIFT);
    frac_freq = 1_000_000 / (frac_freq + 1);

    base_freq + frac_freq
}

// upstream: intel_gt_clock_utils.c gen11_get_crystal_clock_freq()
unsafe fn gen11_get_crystal_clock_freq(_uncore: *mut IntelUncore, rpm_config_reg: u32) -> u32 {
    let f19_2_mhz = 19_200_000;
    let f24_mhz = 24_000_000;
    let f25_mhz = 25_000_000;
    let f38_4_mhz = 38_400_000;
    let crystal_clock = rpm_config_reg & GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_MASK;

    match crystal_clock {
        GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_24_MHZ => f24_mhz,
        GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_19_2_MHZ => f19_2_mhz,
        GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_38_4_MHZ => f38_4_mhz,
        GEN11_RPM_CONFIG0_CRYSTAL_CLOCK_FREQ_25_MHZ => f25_mhz,
        _ => {
            MISSING_CASE!(crystal_clock);
            0
        }
    }
}

// upstream: intel_gt_clock_utils.c gen11_read_clock_frequency()
unsafe fn gen11_read_clock_frequency(uncore: *mut IntelUncore) -> u32 {
    let ctc_reg = unsafe { intel_uncore_read(uncore, _MMIO(CTC_MODE)) };
    let mut freq = 0;

    if ctc_reg & CTC_SOURCE_PARAMETER_MASK == CTC_SOURCE_DIVIDE_LOGIC {
        freq = unsafe { read_reference_ts_freq(uncore) };
    } else {
        let c0 = unsafe { intel_uncore_read(uncore, _MMIO(RPM_CONFIG0)) };
        freq = unsafe { gen11_get_crystal_clock_freq(uncore, c0) };
        freq >>= 3 - reg_field_get(GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK, c0);
    }
    freq
}

// upstream: intel_gt_clock_utils.c gen9_read_clock_frequency()
unsafe fn gen9_read_clock_frequency(uncore: *mut IntelUncore) -> u32 {
    let ctc_reg = unsafe { intel_uncore_read(uncore, _MMIO(CTC_MODE)) };
    let mut freq = 0;

    if ctc_reg & CTC_SOURCE_PARAMETER_MASK == CTC_SOURCE_DIVIDE_LOGIC {
        freq = unsafe { read_reference_ts_freq(uncore) };
    } else {
        freq = if unsafe { IS_GEN9_LP((*uncore).i915) } {
            19_200_000
        } else {
            24_000_000
        };
        freq >>= 3 - reg_field_get(CTC_SHIFT_PARAMETER_MASK, ctc_reg);
    }
    freq
}

// upstream: intel_gt_clock_utils.c gen6_read_clock_frequency()
unsafe fn gen6_read_clock_frequency(_uncore: *mut IntelUncore) -> u32 {
    // PRMs: timestamp is bits 38:3 of the PCU TSC, with 80ns granularity.
    12_500_000
}

// upstream: intel_gt_clock_utils.c gen5_read_clock_frequency()
unsafe fn gen5_read_clock_frequency(_uncore: *mut IntelUncore) -> u32 {
    1_000_000_000 / 1_000
}

// upstream: intel_gt_clock_utils.c g4x_read_clock_frequency()
unsafe fn g4x_read_clock_frequency(_uncore: *mut IntelUncore) -> u32 {
    1_000_000_000 / 1_024
}

// upstream: intel_gt_clock_utils.c gen4_read_clock_frequency()
unsafe fn gen4_read_clock_frequency(uncore: *mut IntelUncore) -> u32 {
    // The source notes empirical hardware evidence disproving the documented /16.
    let fsb = unsafe { i9xx_fsb_freq((*uncore).i915) };
    div_round_closest(fsb, 4) * 1_000
}

// upstream: intel_gt_clock_utils.c read_clock_frequency()
unsafe fn read_clock_frequency(uncore: *mut IntelUncore) -> u32 {
    let i915 = unsafe { (*uncore).i915 };
    if unsafe { GRAPHICS_VER(i915) } >= 11 {
        unsafe { gen11_read_clock_frequency(uncore) }
    } else if unsafe { GRAPHICS_VER(i915) } >= 9 {
        unsafe { gen9_read_clock_frequency(uncore) }
    } else if unsafe { GRAPHICS_VER(i915) } >= 6 {
        unsafe { gen6_read_clock_frequency(uncore) }
    } else if unsafe { GRAPHICS_VER(i915) } == 5 {
        unsafe { gen5_read_clock_frequency(uncore) }
    } else if unsafe { IS_G4X(i915) } {
        unsafe { g4x_read_clock_frequency(uncore) }
    } else if unsafe { GRAPHICS_VER(i915) } == 4 {
        unsafe { gen4_read_clock_frequency(uncore) }
    } else {
        0
    }
}

// upstream: intel_gt_clock_utils.c intel_gt_init_clock_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_init_clock_frequency(gt: *mut IntelGt) {
    unsafe { (*gt).clock_frequency = read_clock_frequency((*gt).uncore) };
    if unsafe { GRAPHICS_VER((*gt).i915) } == 11 {
        unsafe { (*gt).clock_period_ns = (NSEC_PER_SEC / 13_750_000) as u32 };
    } else if unsafe { (*gt).clock_frequency } != 0 {
        unsafe { (*gt).clock_period_ns = intel_gt_clock_interval_to_ns(gt, 1) as u32 };
    }

    GT_TRACE!(
        gt,
        "Using clock frequency: %dkHz, period: %dns, wrap: %lldms\n",
        unsafe { (*gt).clock_frequency / 1000 },
        unsafe { (*gt).clock_period_ns },
        div_u64(
            (unsafe { (*gt).clock_period_ns } as u64).wrapping_mul(S32_MAX),
            USEC_PER_SEC,
        )
    );
}

// upstream: intel_gt_clock_utils.c intel_gt_check_clock_frequency()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_check_clock_frequency(gt: *const IntelGt) {
    if unsafe { (*gt).clock_frequency } != unsafe { read_clock_frequency((*gt).uncore) } {
        gt_err!(
            gt,
            "GT clock frequency changed, was %uHz, now %uHz!\n",
            unsafe { (*gt).clock_frequency },
            unsafe { read_clock_frequency((*gt).uncore) }
        );
    }
}

// upstream: intel_gt_clock_utils.c div_u64_roundup()
unsafe fn div_u64_roundup(nom: u64, den: u32) -> u64 {
    div_u64(nom.wrapping_add(den as u64 - 1), den as u64)
}

#[inline]
fn mul_u64_u32_div(value: u64, multiplier: u32, divisor: u32) -> u64 {
    ((value as u128 * multiplier as u128) / divisor as u128) as u64
}

// upstream: intel_gt_clock_utils.c intel_gt_clock_interval_to_ns()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_clock_interval_to_ns(gt: *const IntelGt, count: u64) -> u64 {
    unsafe { mul_u64_u32_div(count, NSEC_PER_SEC as u32, (*gt).clock_frequency) }
}

// upstream: intel_gt_clock_utils.c intel_gt_pm_interval_to_ns()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_pm_interval_to_ns(gt: *const IntelGt, count: u64) -> u64 {
    unsafe { intel_gt_clock_interval_to_ns(gt, count.wrapping_mul(16)) }
}

// upstream: intel_gt_clock_utils.c intel_gt_ns_to_clock_interval()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_ns_to_clock_interval(gt: *const IntelGt, ns: u64) -> u64 {
    unsafe { mul_u64_u32_div(ns, (*gt).clock_frequency, NSEC_PER_SEC as u32) }
}

// upstream: intel_gt_clock_utils.c intel_gt_ns_to_pm_interval()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_gt_ns_to_pm_interval(gt: *const IntelGt, ns: u64) -> u64 {
    let mut val = unsafe { div_u64_roundup(intel_gt_ns_to_clock_interval(gt, ns), 16) };
    if unsafe { GRAPHICS_VER((*gt).i915) } == 6 {
        val = unsafe { div_u64_roundup(val, 25) }.wrapping_mul(25);
    }
    val
}

#[inline]
unsafe fn div_u64(numerator: u64, denominator: u64) -> u64 {
    numerator / denominator
}
