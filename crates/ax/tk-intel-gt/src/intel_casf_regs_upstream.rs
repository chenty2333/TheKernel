// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_casf_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_SHARPNESS_CTL_A` (intel_casf_regs.h)
pub const _SHARPNESS_CTL_A: u32 = 0x682B0;

/// `_SHARPNESS_CTL_B` (intel_casf_regs.h)
pub const _SHARPNESS_CTL_B: u32 = 0x68AB0;

/// `SHARPNESS_CTL(pipe)` (intel_casf_regs.h)
pub const fn SHARPNESS_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SHARPNESS_CTL_A, _SHARPNESS_CTL_B)
}

/// `FILTER_EN` (intel_casf_regs.h)
pub const FILTER_EN: u32 = reg_bit(31);

/// `FILTER_STRENGTH_MASK` (intel_casf_regs.h)
pub const FILTER_STRENGTH_MASK: u32 = reg_genmask(15, 8);

/// `FILTER_STRENGTH(x)` (intel_casf_regs.h)
pub const fn FILTER_STRENGTH(x: u32) -> u32 {
    reg_field_prep(FILTER_STRENGTH_MASK, (x))
}

/// `FILTER_SIZE_MASK` (intel_casf_regs.h)
pub const FILTER_SIZE_MASK: u32 = reg_genmask(1, 0);

/// `SHARPNESS_FILTER_SIZE_3X3` (intel_casf_regs.h)
pub const SHARPNESS_FILTER_SIZE_3X3: u32 = reg_field_prep(FILTER_SIZE_MASK, 0);

/// `SHARPNESS_FILTER_SIZE_5X5` (intel_casf_regs.h)
pub const SHARPNESS_FILTER_SIZE_5X5: u32 = reg_field_prep(FILTER_SIZE_MASK, 1);

/// `SHARPNESS_FILTER_SIZE_7X7` (intel_casf_regs.h)
pub const SHARPNESS_FILTER_SIZE_7X7: u32 = reg_field_prep(FILTER_SIZE_MASK, 2);

/// `_SHRPLUT_DATA_A` (intel_casf_regs.h)
pub const _SHRPLUT_DATA_A: u32 = 0x682B8;

/// `_SHRPLUT_DATA_B` (intel_casf_regs.h)
pub const _SHRPLUT_DATA_B: u32 = 0x68AB8;

/// `SHRPLUT_DATA(pipe)` (intel_casf_regs.h)
pub const fn SHRPLUT_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SHRPLUT_DATA_A, _SHRPLUT_DATA_B)
}

/// `_SHRPLUT_INDEX_A` (intel_casf_regs.h)
pub const _SHRPLUT_INDEX_A: u32 = 0x682B4;

/// `_SHRPLUT_INDEX_B` (intel_casf_regs.h)
pub const _SHRPLUT_INDEX_B: u32 = 0x68AB4;

/// `SHRPLUT_INDEX(pipe)` (intel_casf_regs.h)
pub const fn SHRPLUT_INDEX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SHRPLUT_INDEX_A, _SHRPLUT_INDEX_B)
}

/// `INDEX_AUTO_INCR` (intel_casf_regs.h)
pub const INDEX_AUTO_INCR: u32 = reg_bit(10);

/// `INDEX_VALUE_MASK` (intel_casf_regs.h)
pub const INDEX_VALUE_MASK: u32 = reg_genmask(4, 0);

/// `INDEX_VALUE(x)` (intel_casf_regs.h)
pub const fn INDEX_VALUE(x: u32) -> u32 {
    reg_field_prep(INDEX_VALUE_MASK, (x))
}
