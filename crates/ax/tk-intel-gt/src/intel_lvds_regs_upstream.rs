// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_lvds_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `LVDS` (intel_lvds_regs.h)
pub const LVDS: I915RegT = mmio(0x61180);

/// `LVDS_PORT_EN` (intel_lvds_regs.h)
pub const LVDS_PORT_EN: u32 = reg_bit(31);

/// `LVDS_PIPE_SEL_MASK` (intel_lvds_regs.h)
pub const LVDS_PIPE_SEL_MASK: u32 = reg_bit(30);

/// `LVDS_PIPE_SEL(pipe)` (intel_lvds_regs.h)
pub const fn LVDS_PIPE_SEL(pipe: u32) -> u32 {
    reg_field_prep(LVDS_PIPE_SEL_MASK, (pipe))
}

/// `LVDS_PIPE_SEL_MASK_CPT` (intel_lvds_regs.h)
pub const LVDS_PIPE_SEL_MASK_CPT: u32 = reg_genmask(30, 29);

/// `LVDS_PIPE_SEL_CPT(pipe)` (intel_lvds_regs.h)
pub const fn LVDS_PIPE_SEL_CPT(pipe: u32) -> u32 {
    reg_field_prep(LVDS_PIPE_SEL_MASK_CPT, (pipe))
}

/// `LVDS_ENABLE_DITHER` (intel_lvds_regs.h)
pub const LVDS_ENABLE_DITHER: u32 = reg_bit(25);

/// `LVDS_VSYNC_POLARITY` (intel_lvds_regs.h)
pub const LVDS_VSYNC_POLARITY: u32 = reg_bit(21);

/// `LVDS_HSYNC_POLARITY` (intel_lvds_regs.h)
pub const LVDS_HSYNC_POLARITY: u32 = reg_bit(20);

/// `LVDS_BORDER_ENABLE` (intel_lvds_regs.h)
pub const LVDS_BORDER_ENABLE: u32 = reg_bit(15);

/// `LVDS_A0A2_CLKA_POWER_MASK` (intel_lvds_regs.h)
pub const LVDS_A0A2_CLKA_POWER_MASK: u32 = reg_genmask(9, 8);

/// `LVDS_A0A2_CLKA_POWER_DOWN` (intel_lvds_regs.h)
pub const LVDS_A0A2_CLKA_POWER_DOWN: u32 = reg_field_prep(LVDS_A0A2_CLKA_POWER_MASK, 0);

/// `LVDS_A0A2_CLKA_POWER_UP` (intel_lvds_regs.h)
pub const LVDS_A0A2_CLKA_POWER_UP: u32 = reg_field_prep(LVDS_A0A2_CLKA_POWER_MASK, 3);

/// `LVDS_A3_POWER_MASK` (intel_lvds_regs.h)
pub const LVDS_A3_POWER_MASK: u32 = reg_genmask(7, 6);

/// `LVDS_A3_POWER_DOWN` (intel_lvds_regs.h)
pub const LVDS_A3_POWER_DOWN: u32 = reg_field_prep(LVDS_A3_POWER_MASK, 0);

/// `LVDS_A3_POWER_UP` (intel_lvds_regs.h)
pub const LVDS_A3_POWER_UP: u32 = reg_field_prep(LVDS_A3_POWER_MASK, 3);

/// `LVDS_CLKB_POWER_MASK` (intel_lvds_regs.h)
pub const LVDS_CLKB_POWER_MASK: u32 = reg_genmask(5, 4);

/// `LVDS_CLKB_POWER_DOWN` (intel_lvds_regs.h)
pub const LVDS_CLKB_POWER_DOWN: u32 = reg_field_prep(LVDS_CLKB_POWER_MASK, 0);

/// `LVDS_CLKB_POWER_UP` (intel_lvds_regs.h)
pub const LVDS_CLKB_POWER_UP: u32 = reg_field_prep(LVDS_CLKB_POWER_MASK, 3);

/// `LVDS_B0B3_POWER_MASK` (intel_lvds_regs.h)
pub const LVDS_B0B3_POWER_MASK: u32 = reg_genmask(3, 2);

/// `LVDS_B0B3_POWER_DOWN` (intel_lvds_regs.h)
pub const LVDS_B0B3_POWER_DOWN: u32 = reg_field_prep(LVDS_B0B3_POWER_MASK, 0);

/// `LVDS_B0B3_POWER_UP` (intel_lvds_regs.h)
pub const LVDS_B0B3_POWER_UP: u32 = reg_field_prep(LVDS_B0B3_POWER_MASK, 3);

/// `PCH_LVDS` (intel_lvds_regs.h)
pub const PCH_LVDS: I915RegT = mmio(0xe1180);

/// `LVDS_DETECTED` (intel_lvds_regs.h)
pub const LVDS_DETECTED: u32 = reg_bit(1);
