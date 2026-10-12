// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_cursor_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_CURACNTR` (intel_cursor_regs.h)
pub const _CURACNTR: u32 = 0x70080;

/// `CURSOR_ENABLE` (intel_cursor_regs.h)
pub const CURSOR_ENABLE: u32 = reg_bit(31);

/// `CURSOR_PIPE_GAMMA_ENABLE` (intel_cursor_regs.h)
pub const CURSOR_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `CURSOR_STRIDE_MASK` (intel_cursor_regs.h)
pub const CURSOR_STRIDE_MASK: u32 = reg_genmask(29, 28);

/// `CURSOR_FORMAT_MASK` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_MASK: u32 = reg_genmask(26, 24);

/// `CURSOR_FORMAT_2C` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_2C: u32 = reg_field_prep(CURSOR_FORMAT_MASK, 0);

/// `CURSOR_FORMAT_3C` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_3C: u32 = reg_field_prep(CURSOR_FORMAT_MASK, 1);

/// `CURSOR_FORMAT_4C` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_4C: u32 = reg_field_prep(CURSOR_FORMAT_MASK, 2);

/// `CURSOR_FORMAT_ARGB` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_ARGB: u32 = reg_field_prep(CURSOR_FORMAT_MASK, 4);

/// `CURSOR_FORMAT_XRGB` (intel_cursor_regs.h)
pub const CURSOR_FORMAT_XRGB: u32 = reg_field_prep(CURSOR_FORMAT_MASK, 5);

/// `MCURSOR_ARB_SLOTS_MASK` (intel_cursor_regs.h)
pub const MCURSOR_ARB_SLOTS_MASK: u32 = reg_genmask(30, 28);

/// `MCURSOR_ARB_SLOTS(x)` (intel_cursor_regs.h)
pub const fn MCURSOR_ARB_SLOTS(x: u32) -> u32 {
    reg_field_prep(MCURSOR_ARB_SLOTS_MASK, (x))
}

/// `MCURSOR_PIPE_SEL_MASK` (intel_cursor_regs.h)
pub const MCURSOR_PIPE_SEL_MASK: u32 = reg_genmask(29, 28);

/// `MCURSOR_PIPE_SEL(pipe)` (intel_cursor_regs.h)
pub const fn MCURSOR_PIPE_SEL(pipe: u32) -> u32 {
    reg_field_prep(MCURSOR_PIPE_SEL_MASK, (pipe))
}

/// `MCURSOR_PIPE_GAMMA_ENABLE` (intel_cursor_regs.h)
pub const MCURSOR_PIPE_GAMMA_ENABLE: u32 = reg_bit(26);

/// `MCURSOR_PIPE_CSC_ENABLE` (intel_cursor_regs.h)
pub const MCURSOR_PIPE_CSC_ENABLE: u32 = reg_bit(24);

/// `MCURSOR_ROTATE_180` (intel_cursor_regs.h)
pub const MCURSOR_ROTATE_180: u32 = reg_bit(15);

/// `MCURSOR_TRICKLE_FEED_DISABLE` (intel_cursor_regs.h)
pub const MCURSOR_TRICKLE_FEED_DISABLE: u32 = reg_bit(14);

/// `MCURSOR_MODE_MASK` (intel_cursor_regs.h)
pub const MCURSOR_MODE_MASK: u32 = 0x27;

/// `MCURSOR_MODE_DISABLE` (intel_cursor_regs.h)
pub const MCURSOR_MODE_DISABLE: u32 = 0x00;

/// `MCURSOR_MODE_128_32B_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_128_32B_AX: u32 = 0x02;

/// `MCURSOR_MODE_256_32B_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_256_32B_AX: u32 = 0x03;

/// `MCURSOR_MODE_64_2B` (intel_cursor_regs.h)
pub const MCURSOR_MODE_64_2B: u32 = 0x04;

/// `MCURSOR_MODE_64_32B_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_64_32B_AX: u32 = 0x07;

/// `MCURSOR_MODE_128_ARGB_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_128_ARGB_AX: u32 = ((0x20 | MCURSOR_MODE_128_32B_AX));

/// `MCURSOR_MODE_256_ARGB_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_256_ARGB_AX: u32 = ((0x20 | MCURSOR_MODE_256_32B_AX));

/// `MCURSOR_MODE_64_ARGB_AX` (intel_cursor_regs.h)
pub const MCURSOR_MODE_64_ARGB_AX: u32 = ((0x20 | MCURSOR_MODE_64_32B_AX));

/// `_CURABASE` (intel_cursor_regs.h)
pub const _CURABASE: u32 = 0x70084;

/// `_CURAPOS` (intel_cursor_regs.h)
pub const _CURAPOS: u32 = 0x70088;

/// `CURSOR_POS_Y_SIGN` (intel_cursor_regs.h)
pub const CURSOR_POS_Y_SIGN: u32 = reg_bit(31);

/// `CURSOR_POS_Y_MASK` (intel_cursor_regs.h)
pub const CURSOR_POS_Y_MASK: u32 = reg_genmask(30, 16);

/// `CURSOR_POS_Y(y)` (intel_cursor_regs.h)
pub const fn CURSOR_POS_Y(y: u32) -> u32 {
    reg_field_prep(CURSOR_POS_Y_MASK, (y))
}

/// `CURSOR_POS_X_SIGN` (intel_cursor_regs.h)
pub const CURSOR_POS_X_SIGN: u32 = reg_bit(15);

/// `CURSOR_POS_X_MASK` (intel_cursor_regs.h)
pub const CURSOR_POS_X_MASK: u32 = reg_genmask(14, 0);

/// `CURSOR_POS_X(x)` (intel_cursor_regs.h)
pub const fn CURSOR_POS_X(x: u32) -> u32 {
    reg_field_prep(CURSOR_POS_X_MASK, (x))
}

/// `_CURAPOS_ERLY_TPT` (intel_cursor_regs.h)
pub const _CURAPOS_ERLY_TPT: u32 = 0x7008c;

/// `_CURASIZE` (intel_cursor_regs.h)
pub const _CURASIZE: u32 = 0x700a0;

/// `CURSOR_HEIGHT_MASK` (intel_cursor_regs.h)
pub const CURSOR_HEIGHT_MASK: u32 = reg_genmask(21, 12);

/// `CURSOR_HEIGHT(h)` (intel_cursor_regs.h)
pub const fn CURSOR_HEIGHT(h: u32) -> u32 {
    reg_field_prep(CURSOR_HEIGHT_MASK, (h))
}

/// `CURSOR_WIDTH_MASK` (intel_cursor_regs.h)
pub const CURSOR_WIDTH_MASK: u32 = reg_genmask(9, 0);

/// `CURSOR_WIDTH(w)` (intel_cursor_regs.h)
pub const fn CURSOR_WIDTH(w: u32) -> u32 {
    reg_field_prep(CURSOR_WIDTH_MASK, (w))
}

/// `_CUR_FBC_CTL_A` (intel_cursor_regs.h)
pub const _CUR_FBC_CTL_A: u32 = 0x700a0;

/// `CUR_FBC_EN` (intel_cursor_regs.h)
pub const CUR_FBC_EN: u32 = reg_bit(31);

/// `CUR_FBC_HEIGHT_MASK` (intel_cursor_regs.h)
pub const CUR_FBC_HEIGHT_MASK: u32 = reg_genmask(7, 0);

/// `CUR_FBC_HEIGHT(h)` (intel_cursor_regs.h)
pub const fn CUR_FBC_HEIGHT(h: u32) -> u32 {
    reg_field_prep(CUR_FBC_HEIGHT_MASK, (h))
}

/// `_CUR_CHICKEN_A` (intel_cursor_regs.h)
pub const _CUR_CHICKEN_A: u32 = 0x700a4;

/// `_CURASURFLIVE` (intel_cursor_regs.h)
pub const _CURASURFLIVE: u32 = 0x700ac;

/// `_CUR_WM_A_0` (intel_cursor_regs.h)
pub const _CUR_WM_A_0: u32 = 0x70140;

/// `_CUR_WM_B_0` (intel_cursor_regs.h)
pub const _CUR_WM_B_0: u32 = 0x71140;

/// `CUR_WM(pipe, level)` (intel_cursor_regs.h)
pub const fn CUR_WM(pipe: u32, level: u32) -> I915RegT {
    mmio((_PIPE((pipe), _CUR_WM_A_0, _CUR_WM_B_0) + ((level) * 4)))
}

/// `CUR_WM_EN` (intel_cursor_regs.h)
pub const CUR_WM_EN: u32 = reg_bit(31);

/// `CUR_WM_IGNORE_LINES` (intel_cursor_regs.h)
pub const CUR_WM_IGNORE_LINES: u32 = reg_bit(30);

/// `CUR_WM_LINES_MASK` (intel_cursor_regs.h)
pub const CUR_WM_LINES_MASK: u32 = reg_genmask(26, 14);

/// `CUR_WM_BLOCKS_MASK` (intel_cursor_regs.h)
pub const CUR_WM_BLOCKS_MASK: u32 = reg_genmask(11, 0);

/// `_CUR_WM_SAGV_A` (intel_cursor_regs.h)
pub const _CUR_WM_SAGV_A: u32 = 0x70158;

/// `_CUR_WM_SAGV_B` (intel_cursor_regs.h)
pub const _CUR_WM_SAGV_B: u32 = 0x71158;

/// `CUR_WM_SAGV(pipe)` (intel_cursor_regs.h)
pub const fn CUR_WM_SAGV(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _CUR_WM_SAGV_A, _CUR_WM_SAGV_B)
}

/// `_CUR_WM_SAGV_TRANS_A` (intel_cursor_regs.h)
pub const _CUR_WM_SAGV_TRANS_A: u32 = 0x7015C;

/// `_CUR_WM_SAGV_TRANS_B` (intel_cursor_regs.h)
pub const _CUR_WM_SAGV_TRANS_B: u32 = 0x7115C;

/// `CUR_WM_SAGV_TRANS(pipe)` (intel_cursor_regs.h)
pub const fn CUR_WM_SAGV_TRANS(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _CUR_WM_SAGV_TRANS_A, _CUR_WM_SAGV_TRANS_B)
}

/// `_CUR_WM_TRANS_A` (intel_cursor_regs.h)
pub const _CUR_WM_TRANS_A: u32 = 0x70168;

/// `_CUR_WM_TRANS_B` (intel_cursor_regs.h)
pub const _CUR_WM_TRANS_B: u32 = 0x71168;

/// `CUR_WM_TRANS(pipe)` (intel_cursor_regs.h)
pub const fn CUR_WM_TRANS(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _CUR_WM_TRANS_A, _CUR_WM_TRANS_B)
}

/// `_CUR_BUF_CFG_A` (intel_cursor_regs.h)
pub const _CUR_BUF_CFG_A: u32 = 0x7017c;

/// `_CUR_BUF_CFG_B` (intel_cursor_regs.h)
pub const _CUR_BUF_CFG_B: u32 = 0x7117c;

/// `CUR_BUF_CFG(pipe)` (intel_cursor_regs.h)
pub const fn CUR_BUF_CFG(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _CUR_BUF_CFG_A, _CUR_BUF_CFG_B)
}

/// `CUR_BUF_END_MASK` (intel_cursor_regs.h)
pub const CUR_BUF_END_MASK: u32 = reg_genmask(27, 16);

/// `CUR_BUF_END(end)` (intel_cursor_regs.h)
pub const fn CUR_BUF_END(end: u32) -> u32 {
    reg_field_prep(CUR_BUF_END_MASK, (end))
}

/// `CUR_BUF_START_MASK` (intel_cursor_regs.h)
pub const CUR_BUF_START_MASK: u32 = reg_genmask(11, 0);

/// `CUR_BUF_START(start)` (intel_cursor_regs.h)
pub const fn CUR_BUF_START(start: u32) -> u32 {
    reg_field_prep(CUR_BUF_START_MASK, (start))
}

/// `_SEL_FETCH_CUR_CTL_A` (intel_cursor_regs.h)
pub const _SEL_FETCH_CUR_CTL_A: u32 = 0x70880;

/// `_SEL_FETCH_CUR_CTL_B` (intel_cursor_regs.h)
pub const _SEL_FETCH_CUR_CTL_B: u32 = 0x71880;

/// `SEL_FETCH_CUR_CTL(pipe)` (intel_cursor_regs.h)
pub const fn SEL_FETCH_CUR_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _SEL_FETCH_CUR_CTL_A, _SEL_FETCH_CUR_CTL_B)
}
