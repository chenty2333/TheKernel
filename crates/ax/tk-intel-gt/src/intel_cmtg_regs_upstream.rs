// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_cmtg_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `CMTG_CLK_SEL` (intel_cmtg_regs.h)
pub const CMTG_CLK_SEL: I915RegT = mmio(0x46160);

/// `CMTG_CLK_SEL_A_MASK` (intel_cmtg_regs.h)
pub const CMTG_CLK_SEL_A_MASK: u32 = reg_genmask(31, 29);

/// `CMTG_CLK_SEL_A_DISABLED` (intel_cmtg_regs.h)
pub const CMTG_CLK_SEL_A_DISABLED: u32 = reg_field_prep(CMTG_CLK_SEL_A_MASK, 0);

/// `CMTG_CLK_SEL_B_MASK` (intel_cmtg_regs.h)
pub const CMTG_CLK_SEL_B_MASK: u32 = reg_genmask(15, 13);

/// `CMTG_CLK_SEL_B_DISABLED` (intel_cmtg_regs.h)
pub const CMTG_CLK_SEL_B_DISABLED: u32 = reg_field_prep(CMTG_CLK_SEL_B_MASK, 0);

/// `TRANS_CMTG_CTL_A` (intel_cmtg_regs.h)
pub const TRANS_CMTG_CTL_A: I915RegT = mmio(0x6fa88);

/// `TRANS_CMTG_CTL_B` (intel_cmtg_regs.h)
pub const TRANS_CMTG_CTL_B: I915RegT = mmio(0x6fb88);

/// `CMTG_ENABLE` (intel_cmtg_regs.h)
pub const CMTG_ENABLE: u32 = reg_bit(31);
