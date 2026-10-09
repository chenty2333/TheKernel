// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation.
//! Linux v7.2.3 `drivers/gpu/drm/i915/i915_freq.c` helpers used by the
//! compatibility branch in `gt/intel_gt_clock_utils.c`.

#![allow(unsafe_code, non_snake_case)]

use core::ptr;

use crate::{
    intel_uncore_types_upstream::{intel_uncore_read, intel_uncore_read16},
    intel_workarounds_types_upstream::I915RegT,
    linux::i915::{IS_MOBILE, IS_PINEVIEW},
    linux_i915_private::DrmI915Private,
};

const MCHBAR_MIRROR_BASE: u32 = 0x1_0000;
const CLKCFG: u32 = MCHBAR_MIRROR_BASE + 0x0c00;
const CSIPLL0: u32 = MCHBAR_MIRROR_BASE + 0x2c10;
const DDRMPLL1: u32 = MCHBAR_MIRROR_BASE + 0x2c20;
const CLKCFG_FSB_400: u32 = 0 << 0;
const CLKCFG_FSB_400_ALT: u32 = 5 << 0;
const CLKCFG_FSB_533: u32 = 1 << 0;
const CLKCFG_FSB_667: u32 = 3 << 0;
const CLKCFG_FSB_800: u32 = 2 << 0;
const CLKCFG_FSB_1067: u32 = 6 << 0;
const CLKCFG_FSB_1067_ALT: u32 = 0 << 0;
const CLKCFG_FSB_1333: u32 = 7 << 0;
const CLKCFG_FSB_1333_ALT: u32 = 4 << 0;
const CLKCFG_FSB_1600_ALT: u32 = 6 << 0;
const CLKCFG_FSB_MASK: u32 = 7 << 0;

unsafe fn primary_uncore(
    i915: *mut DrmI915Private,
) -> *mut crate::intel_uncore_types_upstream::IntelUncore {
    assert!(!i915.is_null());
    let gt = unsafe { (*i915).gt[0] };
    assert!(!gt.is_null(), "i915 primary GT is absent");
    unsafe { (*gt).uncore }
}

// upstream: i915_freq.c i9xx_fsb_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i9xx_fsb_freq(i915: *mut DrmI915Private) -> u32 {
    let fsb = unsafe { intel_uncore_read(primary_uncore(i915), I915RegT { reg: CLKCFG }) }
        & CLKCFG_FSB_MASK;
    if unsafe { IS_PINEVIEW(i915) || IS_MOBILE(i915) } {
        match fsb {
            CLKCFG_FSB_400 => 400_000,
            CLKCFG_FSB_533 => 533_333,
            CLKCFG_FSB_667 => 666_667,
            CLKCFG_FSB_800 => 800_000,
            CLKCFG_FSB_1067 => 1_066_667,
            CLKCFG_FSB_1333 => 1_333_333,
            _ => {
                MISSING_CASE!(fsb);
                1_333_333
            }
        }
    } else {
        match fsb {
            CLKCFG_FSB_400_ALT => 400_000,
            CLKCFG_FSB_533 => 533_333,
            CLKCFG_FSB_667 => 666_667,
            CLKCFG_FSB_800 => 800_000,
            CLKCFG_FSB_1067_ALT => 1_066_667,
            CLKCFG_FSB_1333_ALT => 1_333_333,
            CLKCFG_FSB_1600_ALT => 1_600_000,
            _ => {
                MISSING_CASE!(fsb);
                1_333_333
            }
        }
    }
}

// upstream: i915_freq.c ilk_fsb_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_fsb_freq(i915: *mut DrmI915Private) -> u32 {
    let fsb =
        unsafe { intel_uncore_read16(primary_uncore(i915), I915RegT { reg: CSIPLL0 }) } & 0x3ff;
    match fsb {
        0x00c => 3_200_000,
        0x00e => 3_733_333,
        0x010 => 4_266_667,
        0x012 => 4_800_000,
        0x014 => 5_333_333,
        0x016 => 5_866_667,
        0x018 => 6_400_000,
        _ => {
            drm_dbg!(
                unsafe { ptr::addr_of_mut!((*i915).drm) },
                "unknown fsb frequency 0x%04x\n",
                fsb
            );
            0
        }
    }
}

// upstream: i915_freq.c ilk_mem_freq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_mem_freq(i915: *mut DrmI915Private) -> u32 {
    let ddrpll = unsafe { intel_uncore_read16(primary_uncore(i915), I915RegT { reg: DDRMPLL1 }) };
    match ddrpll & 0xff {
        0x0c => 800_000,
        0x10 => 1_066_667,
        0x14 => 1_333_333,
        0x18 => 1_600_000,
        _ => {
            drm_dbg!(
                unsafe { ptr::addr_of_mut!((*i915).drm) },
                "unknown memory frequency 0x%02x\n",
                ddrpll & 0xff
            );
            0
        }
    }
}
