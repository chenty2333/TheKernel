// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/i9xx_plane_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_DSPAADDR_VLV` (i9xx_plane_regs.h)
pub const _DSPAADDR_VLV: u32 = 0x7017C;

/// `_DSPACNTR` (i9xx_plane_regs.h)
pub const _DSPACNTR: u32 = 0x70180;

/// `DISP_ENABLE` (i9xx_plane_regs.h)
pub const DISP_ENABLE: u32 = reg_bit(31);

/// `DISP_PIPE_GAMMA_ENABLE` (i9xx_plane_regs.h)
pub const DISP_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `DISP_FORMAT_MASK` (i9xx_plane_regs.h)
pub const DISP_FORMAT_MASK: u32 = reg_genmask(29, 26);

/// `DISP_FORMAT_8BPP` (i9xx_plane_regs.h)
pub const DISP_FORMAT_8BPP: u32 = reg_field_prep(DISP_FORMAT_MASK, 2);

/// `DISP_FORMAT_BGRA555` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRA555: u32 = reg_field_prep(DISP_FORMAT_MASK, 3);

/// `DISP_FORMAT_BGRX555` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRX555: u32 = reg_field_prep(DISP_FORMAT_MASK, 4);

/// `DISP_FORMAT_BGRX565` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRX565: u32 = reg_field_prep(DISP_FORMAT_MASK, 5);

/// `DISP_FORMAT_BGRX888` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRX888: u32 = reg_field_prep(DISP_FORMAT_MASK, 6);

/// `DISP_FORMAT_BGRA888` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRA888: u32 = reg_field_prep(DISP_FORMAT_MASK, 7);

/// `DISP_FORMAT_RGBX101010` (i9xx_plane_regs.h)
pub const DISP_FORMAT_RGBX101010: u32 = reg_field_prep(DISP_FORMAT_MASK, 8);

/// `DISP_FORMAT_RGBA101010` (i9xx_plane_regs.h)
pub const DISP_FORMAT_RGBA101010: u32 = reg_field_prep(DISP_FORMAT_MASK, 9);

/// `DISP_FORMAT_BGRX101010` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRX101010: u32 = reg_field_prep(DISP_FORMAT_MASK, 10);

/// `DISP_FORMAT_BGRA101010` (i9xx_plane_regs.h)
pub const DISP_FORMAT_BGRA101010: u32 = reg_field_prep(DISP_FORMAT_MASK, 11);

/// `DISP_FORMAT_RGBX161616` (i9xx_plane_regs.h)
pub const DISP_FORMAT_RGBX161616: u32 = reg_field_prep(DISP_FORMAT_MASK, 12);

/// `DISP_FORMAT_RGBX888` (i9xx_plane_regs.h)
pub const DISP_FORMAT_RGBX888: u32 = reg_field_prep(DISP_FORMAT_MASK, 14);

/// `DISP_FORMAT_RGBA888` (i9xx_plane_regs.h)
pub const DISP_FORMAT_RGBA888: u32 = reg_field_prep(DISP_FORMAT_MASK, 15);

/// `DISP_STEREO_ENABLE` (i9xx_plane_regs.h)
pub const DISP_STEREO_ENABLE: u32 = reg_bit(25);

/// `DISP_PIPE_CSC_ENABLE` (i9xx_plane_regs.h)
pub const DISP_PIPE_CSC_ENABLE: u32 = reg_bit(24);

/// `DISP_PIPE_SEL_MASK` (i9xx_plane_regs.h)
pub const DISP_PIPE_SEL_MASK: u32 = reg_genmask(25, 24);

/// `DISP_PIPE_SEL(pipe)` (i9xx_plane_regs.h)
pub const fn DISP_PIPE_SEL(pipe: u32) -> u32 {
    reg_field_prep(DISP_PIPE_SEL_MASK, (pipe))
}

/// `DISP_SRC_KEY_ENABLE` (i9xx_plane_regs.h)
pub const DISP_SRC_KEY_ENABLE: u32 = reg_bit(22);

/// `DISP_LINE_DOUBLE` (i9xx_plane_regs.h)
pub const DISP_LINE_DOUBLE: u32 = reg_bit(20);

/// `DISP_STEREO_POLARITY_SECOND` (i9xx_plane_regs.h)
pub const DISP_STEREO_POLARITY_SECOND: u32 = reg_bit(18);

/// `DISP_ALPHA_PREMULTIPLY` (i9xx_plane_regs.h)
pub const DISP_ALPHA_PREMULTIPLY: u32 = reg_bit(16);

/// `DISP_ROTATE_180` (i9xx_plane_regs.h)
pub const DISP_ROTATE_180: u32 = reg_bit(15);

/// `DISP_ALPHA_TRANS_ENABLE` (i9xx_plane_regs.h)
pub const DISP_ALPHA_TRANS_ENABLE: u32 = reg_bit(15);

/// `DISP_TRICKLE_FEED_DISABLE` (i9xx_plane_regs.h)
pub const DISP_TRICKLE_FEED_DISABLE: u32 = reg_bit(14);

/// `DISP_TILED` (i9xx_plane_regs.h)
pub const DISP_TILED: u32 = reg_bit(10);

/// `DISP_ASYNC_FLIP` (i9xx_plane_regs.h)
pub const DISP_ASYNC_FLIP: u32 = reg_bit(9);

/// `DISP_MIRROR` (i9xx_plane_regs.h)
pub const DISP_MIRROR: u32 = reg_bit(8);

/// `DISP_SPRITE_ABOVE_OVERLAY` (i9xx_plane_regs.h)
pub const DISP_SPRITE_ABOVE_OVERLAY: u32 = reg_bit(0);

/// `_DSPAADDR` (i9xx_plane_regs.h)
pub const _DSPAADDR: u32 = 0x70184;

/// `_DSPALINOFF` (i9xx_plane_regs.h)
pub const _DSPALINOFF: u32 = 0x70184;

/// `_DSPASTRIDE` (i9xx_plane_regs.h)
pub const _DSPASTRIDE: u32 = 0x70188;

/// `_DSPAPOS` (i9xx_plane_regs.h)
pub const _DSPAPOS: u32 = 0x7018C;

/// `DISP_POS_Y_MASK` (i9xx_plane_regs.h)
pub const DISP_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `DISP_POS_Y(y)` (i9xx_plane_regs.h)
pub const fn DISP_POS_Y(y: u32) -> u32 {
    reg_field_prep(DISP_POS_Y_MASK, (y))
}

/// `DISP_POS_X_MASK` (i9xx_plane_regs.h)
pub const DISP_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `DISP_POS_X(x)` (i9xx_plane_regs.h)
pub const fn DISP_POS_X(x: u32) -> u32 {
    reg_field_prep(DISP_POS_X_MASK, (x))
}

/// `_DSPASIZE` (i9xx_plane_regs.h)
pub const _DSPASIZE: u32 = 0x70190;

/// `DISP_HEIGHT_MASK` (i9xx_plane_regs.h)
pub const DISP_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `DISP_HEIGHT(h)` (i9xx_plane_regs.h)
pub const fn DISP_HEIGHT(h: u32) -> u32 {
    reg_field_prep(DISP_HEIGHT_MASK, (h))
}

/// `DISP_WIDTH_MASK` (i9xx_plane_regs.h)
pub const DISP_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `DISP_WIDTH(w)` (i9xx_plane_regs.h)
pub const fn DISP_WIDTH(w: u32) -> u32 {
    reg_field_prep(DISP_WIDTH_MASK, (w))
}

/// `_DSPASURF` (i9xx_plane_regs.h)
pub const _DSPASURF: u32 = 0x7019C;

/// `DISP_ADDR_MASK` (i9xx_plane_regs.h)
pub const DISP_ADDR_MASK: u32 = reg_genmask(31, 12);

/// `_DSPATILEOFF` (i9xx_plane_regs.h)
pub const _DSPATILEOFF: u32 = 0x701A4;

/// `DISP_OFFSET_Y_MASK` (i9xx_plane_regs.h)
pub const DISP_OFFSET_Y_MASK: u32 = reg_genmask(31, 16);

/// `DISP_OFFSET_Y(y)` (i9xx_plane_regs.h)
pub const fn DISP_OFFSET_Y(y: u32) -> u32 {
    reg_field_prep(DISP_OFFSET_Y_MASK, (y))
}

/// `DISP_OFFSET_X_MASK` (i9xx_plane_regs.h)
pub const DISP_OFFSET_X_MASK: u32 = reg_genmask(15, 0);

/// `DISP_OFFSET_X(x)` (i9xx_plane_regs.h)
pub const fn DISP_OFFSET_X(x: u32) -> u32 {
    reg_field_prep(DISP_OFFSET_X_MASK, (x))
}

/// `_DSPAOFFSET` (i9xx_plane_regs.h)
pub const _DSPAOFFSET: u32 = 0x701A4;

/// `_DSPASURFLIVE` (i9xx_plane_regs.h)
pub const _DSPASURFLIVE: u32 = 0x701AC;

/// `_DSPAGAMC` (i9xx_plane_regs.h)
pub const _DSPAGAMC: u32 = 0x701E0;

/// `_PRIMPOS_A` (i9xx_plane_regs.h)
pub const _PRIMPOS_A: u32 = 0x60a08;

/// `PRIM_POS_Y_MASK` (i9xx_plane_regs.h)
pub const PRIM_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `PRIM_POS_Y(y)` (i9xx_plane_regs.h)
pub const fn PRIM_POS_Y(y: u32) -> u32 {
    reg_field_prep(PRIM_POS_Y_MASK, (y))
}

/// `PRIM_POS_X_MASK` (i9xx_plane_regs.h)
pub const PRIM_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `PRIM_POS_X(x)` (i9xx_plane_regs.h)
pub const fn PRIM_POS_X(x: u32) -> u32 {
    reg_field_prep(PRIM_POS_X_MASK, (x))
}

/// `_PRIMSIZE_A` (i9xx_plane_regs.h)
pub const _PRIMSIZE_A: u32 = 0x60a0c;

/// `PRIM_HEIGHT_MASK` (i9xx_plane_regs.h)
pub const PRIM_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `PRIM_HEIGHT(h)` (i9xx_plane_regs.h)
pub const fn PRIM_HEIGHT(h: u32) -> u32 {
    reg_field_prep(PRIM_HEIGHT_MASK, (h))
}

/// `PRIM_WIDTH_MASK` (i9xx_plane_regs.h)
pub const PRIM_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `PRIM_WIDTH(w)` (i9xx_plane_regs.h)
pub const fn PRIM_WIDTH(w: u32) -> u32 {
    reg_field_prep(PRIM_WIDTH_MASK, (w))
}

/// `_PRIMCNSTALPHA_A` (i9xx_plane_regs.h)
pub const _PRIMCNSTALPHA_A: u32 = 0x60a10;

/// `PRIM_CONST_ALPHA_ENABLE` (i9xx_plane_regs.h)
pub const PRIM_CONST_ALPHA_ENABLE: u32 = reg_bit(31);

/// `PRIM_CONST_ALPHA_MASK` (i9xx_plane_regs.h)
pub const PRIM_CONST_ALPHA_MASK: u32 = reg_genmask(7, 0);

/// `PRIM_CONST_ALPHA(alpha)` (i9xx_plane_regs.h)
pub const fn PRIM_CONST_ALPHA(alpha: u32) -> u32 {
    reg_field_prep(PRIM_CONST_ALPHA_MASK, (alpha))
}
