// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_color_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_PALETTE_A` (intel_color_regs.h)
pub const _PALETTE_A: u32 = 0xa000;

/// `_PALETTE_B` (intel_color_regs.h)
pub const _PALETTE_B: u32 = 0xa800;

/// `_CHV_PALETTE_C` (intel_color_regs.h)
pub const _CHV_PALETTE_C: u32 = 0xc000;

/// `PALETTE_RED_MASK` (intel_color_regs.h)
pub const PALETTE_RED_MASK: u32 = reg_genmask(23, 16);

/// `PALETTE_GREEN_MASK` (intel_color_regs.h)
pub const PALETTE_GREEN_MASK: u32 = reg_genmask(15, 8);

/// `PALETTE_BLUE_MASK` (intel_color_regs.h)
pub const PALETTE_BLUE_MASK: u32 = reg_genmask(7, 0);

/// `PALETTE_10BIT_RED_LDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_RED_LDW_MASK: u32 = reg_genmask(23, 16);

/// `PALETTE_10BIT_GREEN_LDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_GREEN_LDW_MASK: u32 = reg_genmask(15, 8);

/// `PALETTE_10BIT_BLUE_LDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_BLUE_LDW_MASK: u32 = reg_genmask(7, 0);

/// `PALETTE_10BIT_RED_EXP_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_RED_EXP_MASK: u32 = reg_genmask(23, 22);

/// `PALETTE_10BIT_RED_MANT_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_RED_MANT_MASK: u32 = reg_genmask(21, 18);

/// `PALETTE_10BIT_RED_UDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_RED_UDW_MASK: u32 = reg_genmask(17, 16);

/// `PALETTE_10BIT_GREEN_EXP_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_GREEN_EXP_MASK: u32 = reg_genmask(15, 14);

/// `PALETTE_10BIT_GREEN_MANT_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_GREEN_MANT_MASK: u32 = reg_genmask(13, 10);

/// `PALETTE_10BIT_GREEN_UDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_GREEN_UDW_MASK: u32 = reg_genmask(9, 8);

/// `PALETTE_10BIT_BLUE_EXP_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_BLUE_EXP_MASK: u32 = reg_genmask(7, 6);

/// `PALETTE_10BIT_BLUE_MANT_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_BLUE_MANT_MASK: u32 = reg_genmask(5, 2);

/// `PALETTE_10BIT_BLUE_UDW_MASK` (intel_color_regs.h)
pub const PALETTE_10BIT_BLUE_UDW_MASK: u32 = reg_genmask(1, 0);

/// `_PIPEAGCMAX` (intel_color_regs.h)
pub const _PIPEAGCMAX: u32 = 0x70010;

/// `_PIPEBGCMAX` (intel_color_regs.h)
pub const _PIPEBGCMAX: u32 = 0x71010;

/// `_LGC_PALETTE_A` (intel_color_regs.h)
pub const _LGC_PALETTE_A: u32 = 0x4a000;

/// `_LGC_PALETTE_B` (intel_color_regs.h)
pub const _LGC_PALETTE_B: u32 = 0x4a800;

/// `LGC_PALETTE(pipe, i)` (intel_color_regs.h)
pub const fn LGC_PALETTE(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _LGC_PALETTE_A, _LGC_PALETTE_B) + ((i) * 4)))
}

/// `_PREC_PALETTE_A` (intel_color_regs.h)
pub const _PREC_PALETTE_A: u32 = 0x4b000;

/// `_PREC_PALETTE_B` (intel_color_regs.h)
pub const _PREC_PALETTE_B: u32 = 0x4c000;

/// `PREC_PALETTE_10_RED_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_10_RED_MASK: u32 = reg_genmask(29, 20);

/// `PREC_PALETTE_10_GREEN_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_10_GREEN_MASK: u32 = reg_genmask(19, 10);

/// `PREC_PALETTE_10_BLUE_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_10_BLUE_MASK: u32 = reg_genmask(9, 0);

/// `PREC_PALETTE_12P4_RED_LDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_RED_LDW_MASK: u32 = reg_genmask(29, 24);

/// `PREC_PALETTE_12P4_GREEN_LDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_GREEN_LDW_MASK: u32 = reg_genmask(19, 14);

/// `PREC_PALETTE_12P4_BLUE_LDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_BLUE_LDW_MASK: u32 = reg_genmask(9, 4);

/// `PREC_PALETTE_12P4_RED_UDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_RED_UDW_MASK: u32 = reg_genmask(29, 20);

/// `PREC_PALETTE_12P4_GREEN_UDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_GREEN_UDW_MASK: u32 = reg_genmask(19, 10);

/// `PREC_PALETTE_12P4_BLUE_UDW_MASK` (intel_color_regs.h)
pub const PREC_PALETTE_12P4_BLUE_UDW_MASK: u32 = reg_genmask(9, 0);

/// `PREC_PALETTE(pipe, i)` (intel_color_regs.h)
pub const fn PREC_PALETTE(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _PREC_PALETTE_A, _PREC_PALETTE_B) + ((i) * 4)))
}

/// `_PREC_PIPEAGCMAX` (intel_color_regs.h)
pub const _PREC_PIPEAGCMAX: u32 = 0x4d000;

/// `_PREC_PIPEBGCMAX` (intel_color_regs.h)
pub const _PREC_PIPEBGCMAX: u32 = 0x4d010;

/// `PREC_PIPEGCMAX(pipe, i)` (intel_color_regs.h)
pub const fn PREC_PIPEGCMAX(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _PIPEAGCMAX, _PIPEBGCMAX) + ((i) * 4)))
}

/// `_GAMMA_MODE_A` (intel_color_regs.h)
pub const _GAMMA_MODE_A: u32 = 0x4a480;

/// `_GAMMA_MODE_B` (intel_color_regs.h)
pub const _GAMMA_MODE_B: u32 = 0x4ac80;

/// `GAMMA_MODE(pipe)` (intel_color_regs.h)
pub const fn GAMMA_MODE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _GAMMA_MODE_A, _GAMMA_MODE_B)
}

/// `PRE_CSC_GAMMA_ENABLE` (intel_color_regs.h)
pub const PRE_CSC_GAMMA_ENABLE: u32 = reg_bit(31);

/// `POST_CSC_GAMMA_ENABLE` (intel_color_regs.h)
pub const POST_CSC_GAMMA_ENABLE: u32 = reg_bit(30);

/// `PALETTE_ANTICOL_DISABLE` (intel_color_regs.h)
pub const PALETTE_ANTICOL_DISABLE: u32 = reg_bit(15);

/// `GAMMA_MODE_MODE_MASK` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_MASK: u32 = reg_genmask(1, 0);

/// `GAMMA_MODE_MODE_8BIT` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_8BIT: u32 = reg_field_prep(GAMMA_MODE_MODE_MASK, 0);

/// `GAMMA_MODE_MODE_10BIT` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_10BIT: u32 = reg_field_prep(GAMMA_MODE_MODE_MASK, 1);

/// `GAMMA_MODE_MODE_12BIT` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_12BIT: u32 = reg_field_prep(GAMMA_MODE_MODE_MASK, 2);

/// `GAMMA_MODE_MODE_SPLIT` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_SPLIT: u32 = reg_field_prep(GAMMA_MODE_MODE_MASK, 3);

/// `GAMMA_MODE_MODE_12BIT_MULTI_SEG` (intel_color_regs.h)
pub const GAMMA_MODE_MODE_12BIT_MULTI_SEG: u32 = reg_field_prep(GAMMA_MODE_MODE_MASK, 3);

/// `_PIPE_A_CSC_COEFF_RY_GY` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_RY_GY: u32 = 0x49010;

/// `_PIPE_A_CSC_COEFF_BY` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_BY: u32 = 0x49014;

/// `_PIPE_A_CSC_COEFF_RU_GU` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_RU_GU: u32 = 0x49018;

/// `_PIPE_A_CSC_COEFF_BU` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_BU: u32 = 0x4901c;

/// `_PIPE_A_CSC_COEFF_RV_GV` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_RV_GV: u32 = 0x49020;

/// `_PIPE_A_CSC_COEFF_BV` (intel_color_regs.h)
pub const _PIPE_A_CSC_COEFF_BV: u32 = 0x49024;

/// `_PIPE_A_CSC_MODE` (intel_color_regs.h)
pub const _PIPE_A_CSC_MODE: u32 = 0x49028;

/// `ICL_CSC_ENABLE` (intel_color_regs.h)
pub const ICL_CSC_ENABLE: u32 = ((1 << 31));

/// `ICL_OUTPUT_CSC_ENABLE` (intel_color_regs.h)
pub const ICL_OUTPUT_CSC_ENABLE: u32 = ((1 << 30));

/// `CSC_BLACK_SCREEN_OFFSET` (intel_color_regs.h)
pub const CSC_BLACK_SCREEN_OFFSET: u32 = ((1 << 2));

/// `CSC_POSITION_BEFORE_GAMMA` (intel_color_regs.h)
pub const CSC_POSITION_BEFORE_GAMMA: u32 = ((1 << 1));

/// `CSC_MODE_YUV_TO_RGB` (intel_color_regs.h)
pub const CSC_MODE_YUV_TO_RGB: u32 = ((1 << 0));

/// `_PIPE_A_CSC_PREOFF_HI` (intel_color_regs.h)
pub const _PIPE_A_CSC_PREOFF_HI: u32 = 0x49030;

/// `_PIPE_A_CSC_PREOFF_ME` (intel_color_regs.h)
pub const _PIPE_A_CSC_PREOFF_ME: u32 = 0x49034;

/// `_PIPE_A_CSC_PREOFF_LO` (intel_color_regs.h)
pub const _PIPE_A_CSC_PREOFF_LO: u32 = 0x49038;

/// `_PIPE_A_CSC_POSTOFF_HI` (intel_color_regs.h)
pub const _PIPE_A_CSC_POSTOFF_HI: u32 = 0x49040;

/// `_PIPE_A_CSC_POSTOFF_ME` (intel_color_regs.h)
pub const _PIPE_A_CSC_POSTOFF_ME: u32 = 0x49044;

/// `_PIPE_A_CSC_POSTOFF_LO` (intel_color_regs.h)
pub const _PIPE_A_CSC_POSTOFF_LO: u32 = 0x49048;

/// `_PIPE_B_CSC_COEFF_RY_GY` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_RY_GY: u32 = 0x49110;

/// `_PIPE_B_CSC_COEFF_BY` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_BY: u32 = 0x49114;

/// `_PIPE_B_CSC_COEFF_RU_GU` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_RU_GU: u32 = 0x49118;

/// `_PIPE_B_CSC_COEFF_BU` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_BU: u32 = 0x4911c;

/// `_PIPE_B_CSC_COEFF_RV_GV` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_RV_GV: u32 = 0x49120;

/// `_PIPE_B_CSC_COEFF_BV` (intel_color_regs.h)
pub const _PIPE_B_CSC_COEFF_BV: u32 = 0x49124;

/// `_PIPE_B_CSC_MODE` (intel_color_regs.h)
pub const _PIPE_B_CSC_MODE: u32 = 0x49128;

/// `_PIPE_B_CSC_PREOFF_HI` (intel_color_regs.h)
pub const _PIPE_B_CSC_PREOFF_HI: u32 = 0x49130;

/// `_PIPE_B_CSC_PREOFF_ME` (intel_color_regs.h)
pub const _PIPE_B_CSC_PREOFF_ME: u32 = 0x49134;

/// `_PIPE_B_CSC_PREOFF_LO` (intel_color_regs.h)
pub const _PIPE_B_CSC_PREOFF_LO: u32 = 0x49138;

/// `_PIPE_B_CSC_POSTOFF_HI` (intel_color_regs.h)
pub const _PIPE_B_CSC_POSTOFF_HI: u32 = 0x49140;

/// `_PIPE_B_CSC_POSTOFF_ME` (intel_color_regs.h)
pub const _PIPE_B_CSC_POSTOFF_ME: u32 = 0x49144;

/// `_PIPE_B_CSC_POSTOFF_LO` (intel_color_regs.h)
pub const _PIPE_B_CSC_POSTOFF_LO: u32 = 0x49148;

/// `PIPE_CSC_COEFF_RY_GY(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_RY_GY(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_RY_GY, _PIPE_B_CSC_COEFF_RY_GY)
}

/// `PIPE_CSC_COEFF_BY(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_BY(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_BY, _PIPE_B_CSC_COEFF_BY)
}

/// `PIPE_CSC_COEFF_RU_GU(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_RU_GU(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_RU_GU, _PIPE_B_CSC_COEFF_RU_GU)
}

/// `PIPE_CSC_COEFF_BU(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_BU(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_BU, _PIPE_B_CSC_COEFF_BU)
}

/// `PIPE_CSC_COEFF_RV_GV(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_RV_GV(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_RV_GV, _PIPE_B_CSC_COEFF_RV_GV)
}

/// `PIPE_CSC_COEFF_BV(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_COEFF_BV(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_COEFF_BV, _PIPE_B_CSC_COEFF_BV)
}

/// `PIPE_CSC_MODE(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_MODE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_MODE, _PIPE_B_CSC_MODE)
}

/// `PIPE_CSC_PREOFF_HI(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_PREOFF_HI(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_PREOFF_HI, _PIPE_B_CSC_PREOFF_HI)
}

/// `PIPE_CSC_PREOFF_ME(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_PREOFF_ME(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_PREOFF_ME, _PIPE_B_CSC_PREOFF_ME)
}

/// `PIPE_CSC_PREOFF_LO(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_PREOFF_LO(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_PREOFF_LO, _PIPE_B_CSC_PREOFF_LO)
}

/// `PIPE_CSC_POSTOFF_HI(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_POSTOFF_HI(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_POSTOFF_HI, _PIPE_B_CSC_POSTOFF_HI)
}

/// `PIPE_CSC_POSTOFF_ME(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_POSTOFF_ME(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_POSTOFF_ME, _PIPE_B_CSC_POSTOFF_ME)
}

/// `PIPE_CSC_POSTOFF_LO(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_POSTOFF_LO(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_CSC_POSTOFF_LO, _PIPE_B_CSC_POSTOFF_LO)
}

/// `_PIPE_A_OUTPUT_CSC_COEFF_RY_GY` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_RY_GY: u32 = 0x49050;

/// `_PIPE_A_OUTPUT_CSC_COEFF_BY` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_BY: u32 = 0x49054;

/// `_PIPE_A_OUTPUT_CSC_COEFF_RU_GU` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_RU_GU: u32 = 0x49058;

/// `_PIPE_A_OUTPUT_CSC_COEFF_BU` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_BU: u32 = 0x4905c;

/// `_PIPE_A_OUTPUT_CSC_COEFF_RV_GV` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_RV_GV: u32 = 0x49060;

/// `_PIPE_A_OUTPUT_CSC_COEFF_BV` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_COEFF_BV: u32 = 0x49064;

/// `_PIPE_A_OUTPUT_CSC_PREOFF_HI` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_PREOFF_HI: u32 = 0x49068;

/// `_PIPE_A_OUTPUT_CSC_PREOFF_ME` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_PREOFF_ME: u32 = 0x4906c;

/// `_PIPE_A_OUTPUT_CSC_PREOFF_LO` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_PREOFF_LO: u32 = 0x49070;

/// `_PIPE_A_OUTPUT_CSC_POSTOFF_HI` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_POSTOFF_HI: u32 = 0x49074;

/// `_PIPE_A_OUTPUT_CSC_POSTOFF_ME` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_POSTOFF_ME: u32 = 0x49078;

/// `_PIPE_A_OUTPUT_CSC_POSTOFF_LO` (intel_color_regs.h)
pub const _PIPE_A_OUTPUT_CSC_POSTOFF_LO: u32 = 0x4907c;

/// `_PIPE_B_OUTPUT_CSC_COEFF_RY_GY` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_RY_GY: u32 = 0x49150;

/// `_PIPE_B_OUTPUT_CSC_COEFF_BY` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_BY: u32 = 0x49154;

/// `_PIPE_B_OUTPUT_CSC_COEFF_RU_GU` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_RU_GU: u32 = 0x49158;

/// `_PIPE_B_OUTPUT_CSC_COEFF_BU` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_BU: u32 = 0x4915c;

/// `_PIPE_B_OUTPUT_CSC_COEFF_RV_GV` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_RV_GV: u32 = 0x49160;

/// `_PIPE_B_OUTPUT_CSC_COEFF_BV` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_COEFF_BV: u32 = 0x49164;

/// `_PIPE_B_OUTPUT_CSC_PREOFF_HI` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_PREOFF_HI: u32 = 0x49168;

/// `_PIPE_B_OUTPUT_CSC_PREOFF_ME` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_PREOFF_ME: u32 = 0x4916c;

/// `_PIPE_B_OUTPUT_CSC_PREOFF_LO` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_PREOFF_LO: u32 = 0x49170;

/// `_PIPE_B_OUTPUT_CSC_POSTOFF_HI` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_POSTOFF_HI: u32 = 0x49174;

/// `_PIPE_B_OUTPUT_CSC_POSTOFF_ME` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_POSTOFF_ME: u32 = 0x49178;

/// `_PIPE_B_OUTPUT_CSC_POSTOFF_LO` (intel_color_regs.h)
pub const _PIPE_B_OUTPUT_CSC_POSTOFF_LO: u32 = 0x4917c;

/// `PIPE_CSC_OUTPUT_COEFF_RY_GY(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_RY_GY(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_RY_GY, _PIPE_B_OUTPUT_CSC_COEFF_RY_GY)
}

/// `PIPE_CSC_OUTPUT_COEFF_BY(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_BY(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_BY, _PIPE_B_OUTPUT_CSC_COEFF_BY)
}

/// `PIPE_CSC_OUTPUT_COEFF_RU_GU(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_RU_GU(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_RU_GU, _PIPE_B_OUTPUT_CSC_COEFF_RU_GU)
}

/// `PIPE_CSC_OUTPUT_COEFF_BU(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_BU(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_BU, _PIPE_B_OUTPUT_CSC_COEFF_BU)
}

/// `PIPE_CSC_OUTPUT_COEFF_RV_GV(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_RV_GV(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_RV_GV, _PIPE_B_OUTPUT_CSC_COEFF_RV_GV)
}

/// `PIPE_CSC_OUTPUT_COEFF_BV(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_COEFF_BV(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_COEFF_BV, _PIPE_B_OUTPUT_CSC_COEFF_BV)
}

/// `PIPE_CSC_OUTPUT_PREOFF_HI(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_PREOFF_HI(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_PREOFF_HI, _PIPE_B_OUTPUT_CSC_PREOFF_HI)
}

/// `PIPE_CSC_OUTPUT_PREOFF_ME(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_PREOFF_ME(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_PREOFF_ME, _PIPE_B_OUTPUT_CSC_PREOFF_ME)
}

/// `PIPE_CSC_OUTPUT_PREOFF_LO(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_PREOFF_LO(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_PREOFF_LO, _PIPE_B_OUTPUT_CSC_PREOFF_LO)
}

/// `PIPE_CSC_OUTPUT_POSTOFF_HI(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_POSTOFF_HI(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_POSTOFF_HI, _PIPE_B_OUTPUT_CSC_POSTOFF_HI)
}

/// `PIPE_CSC_OUTPUT_POSTOFF_ME(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_POSTOFF_ME(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_POSTOFF_ME, _PIPE_B_OUTPUT_CSC_POSTOFF_ME)
}

/// `PIPE_CSC_OUTPUT_POSTOFF_LO(pipe)` (intel_color_regs.h)
pub const fn PIPE_CSC_OUTPUT_POSTOFF_LO(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_A_OUTPUT_CSC_POSTOFF_LO, _PIPE_B_OUTPUT_CSC_POSTOFF_LO)
}

/// `_PAL_PREC_INDEX_A` (intel_color_regs.h)
pub const _PAL_PREC_INDEX_A: u32 = 0x4A400;

/// `_PAL_PREC_INDEX_B` (intel_color_regs.h)
pub const _PAL_PREC_INDEX_B: u32 = 0x4AC00;

/// `_PAL_PREC_INDEX_C` (intel_color_regs.h)
pub const _PAL_PREC_INDEX_C: u32 = 0x4B400;

/// `PAL_PREC_SPLIT_MODE` (intel_color_regs.h)
pub const PAL_PREC_SPLIT_MODE: u32 = reg_bit(31);

/// `PAL_PREC_AUTO_INCREMENT` (intel_color_regs.h)
pub const PAL_PREC_AUTO_INCREMENT: u32 = reg_bit(15);

/// `PAL_PREC_INDEX_VALUE_MASK` (intel_color_regs.h)
pub const PAL_PREC_INDEX_VALUE_MASK: u32 = reg_genmask(9, 0);

/// `PAL_PREC_INDEX_VALUE(x)` (intel_color_regs.h)
pub const fn PAL_PREC_INDEX_VALUE(x: u32) -> u32 {
    reg_field_prep(PAL_PREC_INDEX_VALUE_MASK, (x))
}

/// `_PAL_PREC_DATA_A` (intel_color_regs.h)
pub const _PAL_PREC_DATA_A: u32 = 0x4A404;

/// `_PAL_PREC_DATA_B` (intel_color_regs.h)
pub const _PAL_PREC_DATA_B: u32 = 0x4AC04;

/// `_PAL_PREC_DATA_C` (intel_color_regs.h)
pub const _PAL_PREC_DATA_C: u32 = 0x4B404;

/// `_PAL_PREC_GC_MAX_A` (intel_color_regs.h)
pub const _PAL_PREC_GC_MAX_A: u32 = 0x4A410;

/// `_PAL_PREC_GC_MAX_B` (intel_color_regs.h)
pub const _PAL_PREC_GC_MAX_B: u32 = 0x4AC10;

/// `_PAL_PREC_GC_MAX_C` (intel_color_regs.h)
pub const _PAL_PREC_GC_MAX_C: u32 = 0x4B410;

/// `_PAL_PREC_EXT_GC_MAX_A` (intel_color_regs.h)
pub const _PAL_PREC_EXT_GC_MAX_A: u32 = 0x4A420;

/// `_PAL_PREC_EXT_GC_MAX_B` (intel_color_regs.h)
pub const _PAL_PREC_EXT_GC_MAX_B: u32 = 0x4AC20;

/// `_PAL_PREC_EXT_GC_MAX_C` (intel_color_regs.h)
pub const _PAL_PREC_EXT_GC_MAX_C: u32 = 0x4B420;

/// `_PAL_PREC_EXT2_GC_MAX_A` (intel_color_regs.h)
pub const _PAL_PREC_EXT2_GC_MAX_A: u32 = 0x4A430;

/// `_PAL_PREC_EXT2_GC_MAX_B` (intel_color_regs.h)
pub const _PAL_PREC_EXT2_GC_MAX_B: u32 = 0x4AC30;

/// `_PAL_PREC_EXT2_GC_MAX_C` (intel_color_regs.h)
pub const _PAL_PREC_EXT2_GC_MAX_C: u32 = 0x4B430;

/// `PREC_PAL_INDEX(pipe)` (intel_color_regs.h)
pub const fn PREC_PAL_INDEX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PAL_PREC_INDEX_A, _PAL_PREC_INDEX_B)
}

/// `PREC_PAL_DATA(pipe)` (intel_color_regs.h)
pub const fn PREC_PAL_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PAL_PREC_DATA_A, _PAL_PREC_DATA_B)
}

/// `PREC_PAL_GC_MAX(pipe, i)` (intel_color_regs.h)
pub const fn PREC_PAL_GC_MAX(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _PAL_PREC_GC_MAX_A, _PAL_PREC_GC_MAX_B) + ((i) * 4)))
}

/// `PREC_PAL_EXT_GC_MAX(pipe, i)` (intel_color_regs.h)
pub const fn PREC_PAL_EXT_GC_MAX(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _PAL_PREC_EXT_GC_MAX_A, _PAL_PREC_EXT_GC_MAX_B) + ((i) * 4)))
}

/// `PREC_PAL_EXT2_GC_MAX(pipe, i)` (intel_color_regs.h)
pub const fn PREC_PAL_EXT2_GC_MAX(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _PAL_PREC_EXT2_GC_MAX_A, _PAL_PREC_EXT2_GC_MAX_B) + ((i) * 4)))
}

/// `_PRE_CSC_GAMC_INDEX_A` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_INDEX_A: u32 = 0x4A484;

/// `_PRE_CSC_GAMC_INDEX_B` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_INDEX_B: u32 = 0x4AC84;

/// `_PRE_CSC_GAMC_INDEX_C` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_INDEX_C: u32 = 0x4B484;

/// `PRE_CSC_GAMC_AUTO_INCREMENT` (intel_color_regs.h)
pub const PRE_CSC_GAMC_AUTO_INCREMENT: u32 = reg_bit(10);

/// `PRE_CSC_GAMC_INDEX_VALUE_MASK` (intel_color_regs.h)
pub const PRE_CSC_GAMC_INDEX_VALUE_MASK: u32 = reg_genmask(7, 0);

/// `PRE_CSC_GAMC_INDEX_VALUE(x)` (intel_color_regs.h)
pub const fn PRE_CSC_GAMC_INDEX_VALUE(x: u32) -> u32 {
    reg_field_prep(PRE_CSC_GAMC_INDEX_VALUE_MASK, (x))
}

/// `_PRE_CSC_GAMC_DATA_A` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_DATA_A: u32 = 0x4A488;

/// `_PRE_CSC_GAMC_DATA_B` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_DATA_B: u32 = 0x4AC88;

/// `_PRE_CSC_GAMC_DATA_C` (intel_color_regs.h)
pub const _PRE_CSC_GAMC_DATA_C: u32 = 0x4B488;

/// `PRE_CSC_GAMC_INDEX(pipe)` (intel_color_regs.h)
pub const fn PRE_CSC_GAMC_INDEX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PRE_CSC_GAMC_INDEX_A, _PRE_CSC_GAMC_INDEX_B)
}

/// `PRE_CSC_GAMC_DATA(pipe)` (intel_color_regs.h)
pub const fn PRE_CSC_GAMC_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PRE_CSC_GAMC_DATA_A, _PRE_CSC_GAMC_DATA_B)
}

/// `_PAL_PREC_MULTI_SEG_INDEX_A` (intel_color_regs.h)
pub const _PAL_PREC_MULTI_SEG_INDEX_A: u32 = 0x4A408;

/// `_PAL_PREC_MULTI_SEG_INDEX_B` (intel_color_regs.h)
pub const _PAL_PREC_MULTI_SEG_INDEX_B: u32 = 0x4AC08;

/// `PAL_PREC_MULTI_SEG_AUTO_INCREMENT` (intel_color_regs.h)
pub const PAL_PREC_MULTI_SEG_AUTO_INCREMENT: u32 = reg_bit(15);

/// `PAL_PREC_MULTI_SEG_INDEX_VALUE_MASK` (intel_color_regs.h)
pub const PAL_PREC_MULTI_SEG_INDEX_VALUE_MASK: u32 = reg_genmask(4, 0);

/// `PAL_PREC_MULTI_SEG_INDEX_VALUE(x)` (intel_color_regs.h)
pub const fn PAL_PREC_MULTI_SEG_INDEX_VALUE(x: u32) -> u32 {
    reg_field_prep(PAL_PREC_MULTI_SEG_INDEX_VALUE_MASK, (x))
}

/// `_PAL_PREC_MULTI_SEG_DATA_A` (intel_color_regs.h)
pub const _PAL_PREC_MULTI_SEG_DATA_A: u32 = 0x4A40C;

/// `_PAL_PREC_MULTI_SEG_DATA_B` (intel_color_regs.h)
pub const _PAL_PREC_MULTI_SEG_DATA_B: u32 = 0x4AC0C;

/// `PREC_PAL_MULTI_SEG_INDEX(pipe)` (intel_color_regs.h)
pub const fn PREC_PAL_MULTI_SEG_INDEX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PAL_PREC_MULTI_SEG_INDEX_A, _PAL_PREC_MULTI_SEG_INDEX_B)
}

/// `PREC_PAL_MULTI_SEG_DATA(pipe)` (intel_color_regs.h)
pub const fn PREC_PAL_MULTI_SEG_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PAL_PREC_MULTI_SEG_DATA_A, _PAL_PREC_MULTI_SEG_DATA_B)
}

/// `_PIPE_A_WGC_C01_C00` (intel_color_regs.h)
pub const _PIPE_A_WGC_C01_C00: u32 = 0x600B0;

/// `_PIPE_A_WGC_C02` (intel_color_regs.h)
pub const _PIPE_A_WGC_C02: u32 = 0x600B4;

/// `_PIPE_A_WGC_C11_C10` (intel_color_regs.h)
pub const _PIPE_A_WGC_C11_C10: u32 = 0x600B8;

/// `_PIPE_A_WGC_C12` (intel_color_regs.h)
pub const _PIPE_A_WGC_C12: u32 = 0x600BC;

/// `_PIPE_A_WGC_C21_C20` (intel_color_regs.h)
pub const _PIPE_A_WGC_C21_C20: u32 = 0x600C0;

/// `_PIPE_A_WGC_C22` (intel_color_regs.h)
pub const _PIPE_A_WGC_C22: u32 = 0x600C4;

/// `_CGM_PIPE_A_CSC_COEFF01` (intel_color_regs.h)
pub const _CGM_PIPE_A_CSC_COEFF01: u32 = ((VLV_DISPLAY_BASE + 0x67900));

/// `_CGM_PIPE_A_CSC_COEFF23` (intel_color_regs.h)
pub const _CGM_PIPE_A_CSC_COEFF23: u32 = ((VLV_DISPLAY_BASE + 0x67904));

/// `_CGM_PIPE_A_CSC_COEFF45` (intel_color_regs.h)
pub const _CGM_PIPE_A_CSC_COEFF45: u32 = ((VLV_DISPLAY_BASE + 0x67908));

/// `_CGM_PIPE_A_CSC_COEFF67` (intel_color_regs.h)
pub const _CGM_PIPE_A_CSC_COEFF67: u32 = ((VLV_DISPLAY_BASE + 0x6790C));

/// `_CGM_PIPE_A_CSC_COEFF8` (intel_color_regs.h)
pub const _CGM_PIPE_A_CSC_COEFF8: u32 = ((VLV_DISPLAY_BASE + 0x67910));

/// `_CGM_PIPE_A_DEGAMMA` (intel_color_regs.h)
pub const _CGM_PIPE_A_DEGAMMA: u32 = ((VLV_DISPLAY_BASE + 0x66000));

/// `CGM_PIPE_DEGAMMA_GREEN_LDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_DEGAMMA_GREEN_LDW_MASK: u32 = reg_genmask(29, 16);

/// `CGM_PIPE_DEGAMMA_BLUE_LDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_DEGAMMA_BLUE_LDW_MASK: u32 = reg_genmask(13, 0);

/// `CGM_PIPE_DEGAMMA_RED_UDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_DEGAMMA_RED_UDW_MASK: u32 = reg_genmask(13, 0);

/// `_CGM_PIPE_A_GAMMA` (intel_color_regs.h)
pub const _CGM_PIPE_A_GAMMA: u32 = ((VLV_DISPLAY_BASE + 0x67000));

/// `CGM_PIPE_GAMMA_GREEN_LDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_GAMMA_GREEN_LDW_MASK: u32 = reg_genmask(25, 16);

/// `CGM_PIPE_GAMMA_BLUE_LDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_GAMMA_BLUE_LDW_MASK: u32 = reg_genmask(9, 0);

/// `CGM_PIPE_GAMMA_RED_UDW_MASK` (intel_color_regs.h)
pub const CGM_PIPE_GAMMA_RED_UDW_MASK: u32 = reg_genmask(9, 0);

/// `_CGM_PIPE_A_MODE` (intel_color_regs.h)
pub const _CGM_PIPE_A_MODE: u32 = ((VLV_DISPLAY_BASE + 0x67A00));

/// `CGM_PIPE_MODE_GAMMA` (intel_color_regs.h)
pub const CGM_PIPE_MODE_GAMMA: u32 = ((1 << 2));

/// `CGM_PIPE_MODE_CSC` (intel_color_regs.h)
pub const CGM_PIPE_MODE_CSC: u32 = ((1 << 1));

/// `CGM_PIPE_MODE_DEGAMMA` (intel_color_regs.h)
pub const CGM_PIPE_MODE_DEGAMMA: u32 = ((1 << 0));

/// `_CGM_PIPE_B_CSC_COEFF01` (intel_color_regs.h)
pub const _CGM_PIPE_B_CSC_COEFF01: u32 = ((VLV_DISPLAY_BASE + 0x69900));

/// `_CGM_PIPE_B_CSC_COEFF23` (intel_color_regs.h)
pub const _CGM_PIPE_B_CSC_COEFF23: u32 = ((VLV_DISPLAY_BASE + 0x69904));

/// `_CGM_PIPE_B_CSC_COEFF45` (intel_color_regs.h)
pub const _CGM_PIPE_B_CSC_COEFF45: u32 = ((VLV_DISPLAY_BASE + 0x69908));

/// `_CGM_PIPE_B_CSC_COEFF67` (intel_color_regs.h)
pub const _CGM_PIPE_B_CSC_COEFF67: u32 = ((VLV_DISPLAY_BASE + 0x6990C));

/// `_CGM_PIPE_B_CSC_COEFF8` (intel_color_regs.h)
pub const _CGM_PIPE_B_CSC_COEFF8: u32 = ((VLV_DISPLAY_BASE + 0x69910));

/// `_CGM_PIPE_B_DEGAMMA` (intel_color_regs.h)
pub const _CGM_PIPE_B_DEGAMMA: u32 = ((VLV_DISPLAY_BASE + 0x68000));

/// `_CGM_PIPE_B_GAMMA` (intel_color_regs.h)
pub const _CGM_PIPE_B_GAMMA: u32 = ((VLV_DISPLAY_BASE + 0x69000));

/// `_CGM_PIPE_B_MODE` (intel_color_regs.h)
pub const _CGM_PIPE_B_MODE: u32 = ((VLV_DISPLAY_BASE + 0x69A00));

/// `CGM_PIPE_CSC_COEFF01(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_CSC_COEFF01(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_CSC_COEFF01, _CGM_PIPE_B_CSC_COEFF01)
}

/// `CGM_PIPE_CSC_COEFF23(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_CSC_COEFF23(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_CSC_COEFF23, _CGM_PIPE_B_CSC_COEFF23)
}

/// `CGM_PIPE_CSC_COEFF45(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_CSC_COEFF45(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_CSC_COEFF45, _CGM_PIPE_B_CSC_COEFF45)
}

/// `CGM_PIPE_CSC_COEFF67(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_CSC_COEFF67(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_CSC_COEFF67, _CGM_PIPE_B_CSC_COEFF67)
}

/// `CGM_PIPE_CSC_COEFF8(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_CSC_COEFF8(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_CSC_COEFF8, _CGM_PIPE_B_CSC_COEFF8)
}

/// `CGM_PIPE_DEGAMMA(pipe, i, w)` (intel_color_regs.h)
pub const fn CGM_PIPE_DEGAMMA(pipe: u32, i: u32, w: u32) -> I915RegT {
    mmio(((_PIPE(pipe, _CGM_PIPE_A_DEGAMMA, _CGM_PIPE_B_DEGAMMA) + ((i) * 8)) + ((w) * 4)))
}

/// `CGM_PIPE_GAMMA(pipe, i, w)` (intel_color_regs.h)
pub const fn CGM_PIPE_GAMMA(pipe: u32, i: u32, w: u32) -> I915RegT {
    mmio(((_PIPE(pipe, _CGM_PIPE_A_GAMMA, _CGM_PIPE_B_GAMMA) + ((i) * 8)) + ((w) * 4)))
}

/// `CGM_PIPE_MODE(pipe)` (intel_color_regs.h)
pub const fn CGM_PIPE_MODE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CGM_PIPE_A_MODE, _CGM_PIPE_B_MODE)
}

/// `_SKL_BOTTOM_COLOR_A` (intel_color_regs.h)
pub const _SKL_BOTTOM_COLOR_A: u32 = 0x70034;

/// `_SKL_BOTTOM_COLOR_B` (intel_color_regs.h)
pub const _SKL_BOTTOM_COLOR_B: u32 = 0x71034;

/// `SKL_BOTTOM_COLOR_GAMMA_ENABLE` (intel_color_regs.h)
pub const SKL_BOTTOM_COLOR_GAMMA_ENABLE: u32 = reg_bit(31);

/// `SKL_BOTTOM_COLOR_CSC_ENABLE` (intel_color_regs.h)
pub const SKL_BOTTOM_COLOR_CSC_ENABLE: u32 = reg_bit(30);

/// `SKL_BOTTOM_COLOR(pipe)` (intel_color_regs.h)
pub const fn SKL_BOTTOM_COLOR(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SKL_BOTTOM_COLOR_A, _SKL_BOTTOM_COLOR_B)
}

/// `_LUT_3D_CTL_A` (intel_color_regs.h)
pub const _LUT_3D_CTL_A: u32 = 0x490A4;

/// `_LUT_3D_CTL_B` (intel_color_regs.h)
pub const _LUT_3D_CTL_B: u32 = 0x491A4;

/// `LUT_3D_CTL(pipe)` (intel_color_regs.h)
pub const fn LUT_3D_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _LUT_3D_CTL_A, _LUT_3D_CTL_B)
}

/// `LUT_3D_ENABLE` (intel_color_regs.h)
pub const LUT_3D_ENABLE: u32 = reg_bit(31);

/// `LUT_3D_READY` (intel_color_regs.h)
pub const LUT_3D_READY: u32 = reg_bit(30);

/// `LUT_3D_BINDING_MASK` (intel_color_regs.h)
pub const LUT_3D_BINDING_MASK: u32 = reg_genmask(23, 22);

/// `LUT_3D_BIND_PIPE` (intel_color_regs.h)
pub const LUT_3D_BIND_PIPE: u32 = reg_field_prep(LUT_3D_BINDING_MASK, 0);

/// `LUT_3D_BIND_PLANE_1` (intel_color_regs.h)
pub const LUT_3D_BIND_PLANE_1: u32 = reg_field_prep(LUT_3D_BINDING_MASK, 1);

/// `LUT_3D_BIND_PLANE_2` (intel_color_regs.h)
pub const LUT_3D_BIND_PLANE_2: u32 = reg_field_prep(LUT_3D_BINDING_MASK, 2);

/// `LUT_3D_BIND_PLANE_3` (intel_color_regs.h)
pub const LUT_3D_BIND_PLANE_3: u32 = reg_field_prep(LUT_3D_BINDING_MASK, 3);

/// `_LUT_3D_INDEX_A` (intel_color_regs.h)
pub const _LUT_3D_INDEX_A: u32 = 0x490A8;

/// `_LUT_3D_INDEX_B` (intel_color_regs.h)
pub const _LUT_3D_INDEX_B: u32 = 0x491A8;

/// `LUT_3D_INDEX(pipe)` (intel_color_regs.h)
pub const fn LUT_3D_INDEX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _LUT_3D_INDEX_A, _LUT_3D_INDEX_B)
}

/// `LUT_3D_AUTO_INCREMENT` (intel_color_regs.h)
pub const LUT_3D_AUTO_INCREMENT: u32 = reg_bit(13);

/// `LUT_3D_INDEX_VALUE_MASK` (intel_color_regs.h)
pub const LUT_3D_INDEX_VALUE_MASK: u32 = reg_genmask(12, 0);

/// `LUT_3D_INDEX_VALUE(x)` (intel_color_regs.h)
pub const fn LUT_3D_INDEX_VALUE(x: u32) -> u32 {
    reg_field_prep(LUT_3D_INDEX_VALUE_MASK, (x))
}

/// `_LUT_3D_DATA_A` (intel_color_regs.h)
pub const _LUT_3D_DATA_A: u32 = 0x490AC;

/// `_LUT_3D_DATA_B` (intel_color_regs.h)
pub const _LUT_3D_DATA_B: u32 = 0x491AC;

/// `LUT_3D_DATA(pipe)` (intel_color_regs.h)
pub const fn LUT_3D_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _LUT_3D_DATA_A, _LUT_3D_DATA_B)
}

/// `LUT_3D_DATA_RED_MASK` (intel_color_regs.h)
pub const LUT_3D_DATA_RED_MASK: u32 = reg_genmask(29, 20);

/// `LUT_3D_DATA_GREEN_MASK` (intel_color_regs.h)
pub const LUT_3D_DATA_GREEN_MASK: u32 = reg_genmask(19, 10);

/// `LUT_3D_DATA_BLUE_MASK` (intel_color_regs.h)
pub const LUT_3D_DATA_BLUE_MASK: u32 = reg_genmask(9, 0);

/// `LUT_3D_DATA_RED(x)` (intel_color_regs.h)
pub const fn LUT_3D_DATA_RED(x: u32) -> u32 {
    reg_field_prep(LUT_3D_DATA_RED_MASK, (x))
}

/// `LUT_3D_DATA_GREEN(x)` (intel_color_regs.h)
pub const fn LUT_3D_DATA_GREEN(x: u32) -> u32 {
    reg_field_prep(LUT_3D_DATA_GREEN_MASK, (x))
}

/// `LUT_3D_DATA_BLUE(x)` (intel_color_regs.h)
pub const fn LUT_3D_DATA_BLUE(x: u32) -> u32 {
    reg_field_prep(LUT_3D_DATA_BLUE_MASK, (x))
}
