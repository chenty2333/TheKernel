// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_pipe_crc_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_PIPE_CRC_CTL_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_CTL_A: u32 = 0x60050;

/// `PIPE_CRC_ENABLE` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_ENABLE: u32 = reg_bit(31);

/// `PIPE_CRC_SOURCE_MASK_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_MASK_SKL: u32 = reg_genmask(30, 28);

/// `PIPE_CRC_SOURCE_PLANE_1_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_1_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 0);

/// `PIPE_CRC_SOURCE_PLANE_2_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_2_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 2);

/// `PIPE_CRC_SOURCE_DMUX_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DMUX_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 4);

/// `PIPE_CRC_SOURCE_PLANE_3_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_3_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 6);

/// `PIPE_CRC_SOURCE_PLANE_4_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_4_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 7);

/// `PIPE_CRC_SOURCE_PLANE_5_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_5_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 5);

/// `PIPE_CRC_SOURCE_PLANE_6_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_6_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 3);

/// `PIPE_CRC_SOURCE_PLANE_7_SKL` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PLANE_7_SKL: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_SKL, 1);

/// `PIPE_CRC_SOURCE_MASK_IVB` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_MASK_IVB: u32 = reg_genmask(30, 29);

/// `PIPE_CRC_SOURCE_PRIMARY_IVB` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PRIMARY_IVB: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_IVB, 0);

/// `PIPE_CRC_SOURCE_SPRITE_IVB` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_SPRITE_IVB: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_IVB, 1);

/// `PIPE_CRC_SOURCE_PF_IVB` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PF_IVB: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_IVB, 2);

/// `PIPE_CRC_SOURCE_MASK_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_MASK_ILK: u32 = reg_genmask(30, 28);

/// `PIPE_CRC_SOURCE_PRIMARY_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PRIMARY_ILK: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_ILK, 0);

/// `PIPE_CRC_SOURCE_SPRITE_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_SPRITE_ILK: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_ILK, 1);

/// `PIPE_CRC_SOURCE_PIPE_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PIPE_ILK: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_ILK, 2);

/// `PIPE_CRC_SOURCE_PORT_A_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PORT_A_ILK: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_ILK, 4);

/// `PIPE_CRC_SOURCE_FDI_ILK` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_FDI_ILK: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_ILK, 5);

/// `PIPE_CRC_SOURCE_MASK_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_MASK_VLV: u32 = reg_genmask(30, 27);

/// `PIPE_CRC_SOURCE_PIPE_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PIPE_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 0);

/// `PIPE_CRC_SOURCE_HDMIB_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_HDMIB_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 1);

/// `PIPE_CRC_SOURCE_HDMIC_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_HDMIC_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 2);

/// `PIPE_CRC_SOURCE_DP_D_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_D_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 3);

/// `PIPE_CRC_SOURCE_DP_B_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_B_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 6);

/// `PIPE_CRC_SOURCE_DP_C_VLV` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_C_VLV: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_VLV, 7);

/// `PIPE_CRC_SOURCE_MASK_I9XX` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_MASK_I9XX: u32 = reg_genmask(30, 28);

/// `PIPE_CRC_SOURCE_PIPE_I9XX` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_PIPE_I9XX: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 0);

/// `PIPE_CRC_SOURCE_SDVOB_I9XX` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_SDVOB_I9XX: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 1);

/// `PIPE_CRC_SOURCE_SDVOC_I9XX` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_SDVOC_I9XX: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 2);

/// `PIPE_CRC_SOURCE_DP_D_G4X` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_D_G4X: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 3);

/// `PIPE_CRC_SOURCE_TV_PRE` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_TV_PRE: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 4);

/// `PIPE_CRC_SOURCE_TV_POST` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_TV_POST: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 5);

/// `PIPE_CRC_SOURCE_DP_B_G4X` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_B_G4X: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 6);

/// `PIPE_CRC_SOURCE_DP_C_G4X` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_SOURCE_DP_C_G4X: u32 = reg_field_prep(PIPE_CRC_SOURCE_MASK_I9XX, 7);

/// `PIPE_CRC_INCLUDE_BORDER_I8XX` (intel_pipe_crc_regs.h)
pub const PIPE_CRC_INCLUDE_BORDER_I8XX: u32 = reg_bit(30);

/// `_PIPE_CRC_EXP_GREEN_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_GREEN_A: u32 = 0x60054;

/// `_PIPE_CRC_EXP_BLUE_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_BLUE_A: u32 = 0x60058;

/// `_PIPE_CRC_EXP_RES1_A_I915` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_RES1_A_I915: u32 = 0x6005c;

/// `_PIPE_CRC_EXP_RES2_A_G4X` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_RES2_A_G4X: u32 = 0x60080;

/// `_PIPE_CRC_RES_RED_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_RED_A: u32 = 0x60060;

/// `_PIPE_CRC_RES_GREEN_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_GREEN_A: u32 = 0x60064;

/// `_PIPE_CRC_RES_BLUE_A` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_BLUE_A: u32 = 0x60068;

/// `_PIPE_CRC_RES_RES1_A_I915` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_RES1_A_I915: u32 = 0x6006c;

/// `_PIPE_CRC_RES_RES2_A_G4X` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_RES2_A_G4X: u32 = 0x60080;

/// `_PIPE_CRC_EXP_2_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_2_A_IVB: u32 = 0x60054;

/// `_PIPE_CRC_EXP_2_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_2_B_IVB: u32 = 0x61054;

/// `PIPE_CRC_EXP_2_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_EXP_2_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_CRC_EXP_2_A_IVB, _PIPE_CRC_EXP_2_B_IVB)
}

/// `_PIPE_CRC_EXP_3_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_3_A_IVB: u32 = 0x60058;

/// `_PIPE_CRC_EXP_3_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_3_B_IVB: u32 = 0x61058;

/// `PIPE_CRC_EXP_3_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_EXP_3_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_CRC_EXP_3_A_IVB, _PIPE_CRC_EXP_3_B_IVB)
}

/// `_PIPE_CRC_EXP_4_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_4_A_IVB: u32 = 0x6005c;

/// `_PIPE_CRC_EXP_4_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_4_B_IVB: u32 = 0x6105c;

/// `PIPE_CRC_EXP_4_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_EXP_4_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_CRC_EXP_2_A_IVB, _PIPE_CRC_EXP_2_B_IVB)
}

/// `_PIPE_CRC_EXP_5_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_5_A_IVB: u32 = 0x60060;

/// `_PIPE_CRC_EXP_5_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_5_B_IVB: u32 = 0x61060;

/// `PIPE_CRC_EXP_5_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_EXP_5_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_CRC_EXP_2_A_IVB, _PIPE_CRC_EXP_2_B_IVB)
}

/// `_PIPE_CRC_RES_1_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_1_A_IVB: u32 = 0x60064;

/// `_PIPE_CRC_RES_1_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_1_B_IVB: u32 = 0x61064;

/// `PIPE_CRC_RES_1_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_1_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_1_A_IVB, _PIPE_CRC_RES_1_B_IVB)
}

/// `_PIPE_CRC_RES_2_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_2_A_IVB: u32 = 0x60068;

/// `_PIPE_CRC_RES_2_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_2_B_IVB: u32 = 0x61068;

/// `PIPE_CRC_RES_2_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_2_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_2_A_IVB, _PIPE_CRC_RES_2_B_IVB)
}

/// `_PIPE_CRC_RES_3_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_3_A_IVB: u32 = 0x6006c;

/// `_PIPE_CRC_RES_3_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_3_B_IVB: u32 = 0x6106c;

/// `PIPE_CRC_RES_3_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_3_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_3_A_IVB, _PIPE_CRC_RES_3_B_IVB)
}

/// `_PIPE_CRC_RES_4_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_4_A_IVB: u32 = 0x60070;

/// `_PIPE_CRC_RES_4_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_4_B_IVB: u32 = 0x61070;

/// `PIPE_CRC_RES_4_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_4_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_4_A_IVB, _PIPE_CRC_RES_4_B_IVB)
}

/// `_PIPE_CRC_RES_5_A_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_5_A_IVB: u32 = 0x60074;

/// `_PIPE_CRC_RES_5_B_IVB` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_5_B_IVB: u32 = 0x61074;

/// `PIPE_CRC_RES_5_IVB(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_5_IVB(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_5_A_IVB, _PIPE_CRC_RES_5_B_IVB)
}

/// `_PIPE_CRC_EXP_A_HSW` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_A_HSW: u32 = 0x60054;

/// `_PIPE_CRC_EXP_B_HSW` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_EXP_B_HSW: u32 = 0x61054;

/// `PIPE_CRC_EXP_HSW(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_EXP_HSW(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_EXP_A_HSW, _PIPE_CRC_EXP_B_HSW)
}

/// `_PIPE_CRC_RES_A_HSW` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_A_HSW: u32 = 0x60064;

/// `_PIPE_CRC_RES_B_HSW` (intel_pipe_crc_regs.h)
pub const _PIPE_CRC_RES_B_HSW: u32 = 0x61064;

/// `PIPE_CRC_RES_HSW(pipe)` (intel_pipe_crc_regs.h)
pub const fn PIPE_CRC_RES_HSW(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_CRC_RES_A_HSW, _PIPE_CRC_RES_B_HSW)
}

/// `PIPE_CRC_CTL(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_CTL(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_CTL_A)
}

/// `PIPE_CRC_RES_RED(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_RES_RED(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_RES_RED_A)
}

/// `PIPE_CRC_RES_GREEN(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_RES_GREEN(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_RES_GREEN_A)
}

/// `PIPE_CRC_RES_BLUE(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_RES_BLUE(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_RES_BLUE_A)
}

/// `PIPE_CRC_RES_RES1_I915(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_RES_RES1_I915(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_RES_RES1_A_I915)
}

/// `PIPE_CRC_RES_RES2_G4X(dev_priv, pipe)` (intel_pipe_crc_regs.h)
pub unsafe fn PIPE_CRC_RES_RES2_G4X(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPE_CRC_RES_RES2_A_G4X)
}
