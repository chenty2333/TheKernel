// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_hti_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `HDPORT_STATE` (intel_hti_regs.h)
pub const HDPORT_STATE: I915RegT = mmio(0x45050);

/// `HDPORT_DPLL_USED_MASK` (intel_hti_regs.h)
pub const HDPORT_DPLL_USED_MASK: u32 = reg_genmask(15, 12);

/// `HDPORT_DDI_USED(phy)` (intel_hti_regs.h)
pub const fn HDPORT_DDI_USED(phy: u32) -> u32 {
    reg_bit(((2 * (phy)) + 1))
}

/// `HDPORT_ENABLED` (intel_hti_regs.h)
pub const HDPORT_ENABLED: u32 = reg_bit(0);
