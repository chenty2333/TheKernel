// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_sprite_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `_DVSACNTR` (intel_sprite_regs.h)
pub const _DVSACNTR: u32 = 0x72180;

/// `_DVSBCNTR` (intel_sprite_regs.h)
pub const _DVSBCNTR: u32 = 0x73180;

/// `DVSCNTR(pipe)` (intel_sprite_regs.h)
pub const fn DVSCNTR(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSACNTR, _DVSBCNTR)
}

/// `DVS_ENABLE` (intel_sprite_regs.h)
pub const DVS_ENABLE: u32 = reg_bit(31);

/// `DVS_PIPE_GAMMA_ENABLE` (intel_sprite_regs.h)
pub const DVS_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `DVS_YUV_RANGE_CORRECTION_DISABLE` (intel_sprite_regs.h)
pub const DVS_YUV_RANGE_CORRECTION_DISABLE: u32 = reg_bit(27);

/// `DVS_FORMAT_MASK` (intel_sprite_regs.h)
pub const DVS_FORMAT_MASK: u32 = reg_genmask(26, 25);

/// `DVS_FORMAT_YUV422` (intel_sprite_regs.h)
pub const DVS_FORMAT_YUV422: u32 = reg_field_prep(DVS_FORMAT_MASK, 0);

/// `DVS_FORMAT_RGBX101010` (intel_sprite_regs.h)
pub const DVS_FORMAT_RGBX101010: u32 = reg_field_prep(DVS_FORMAT_MASK, 1);

/// `DVS_FORMAT_RGBX888` (intel_sprite_regs.h)
pub const DVS_FORMAT_RGBX888: u32 = reg_field_prep(DVS_FORMAT_MASK, 2);

/// `DVS_FORMAT_RGBX161616` (intel_sprite_regs.h)
pub const DVS_FORMAT_RGBX161616: u32 = reg_field_prep(DVS_FORMAT_MASK, 3);

/// `DVS_PIPE_CSC_ENABLE` (intel_sprite_regs.h)
pub const DVS_PIPE_CSC_ENABLE: u32 = reg_bit(24);

/// `DVS_SOURCE_KEY` (intel_sprite_regs.h)
pub const DVS_SOURCE_KEY: u32 = reg_bit(22);

/// `DVS_RGB_ORDER_XBGR` (intel_sprite_regs.h)
pub const DVS_RGB_ORDER_XBGR: u32 = reg_bit(20);

/// `DVS_YUV_FORMAT_BT709` (intel_sprite_regs.h)
pub const DVS_YUV_FORMAT_BT709: u32 = reg_bit(18);

/// `DVS_YUV_ORDER_MASK` (intel_sprite_regs.h)
pub const DVS_YUV_ORDER_MASK: u32 = reg_genmask(17, 16);

/// `DVS_YUV_ORDER_YUYV` (intel_sprite_regs.h)
pub const DVS_YUV_ORDER_YUYV: u32 = reg_field_prep(DVS_YUV_ORDER_MASK, 0);

/// `DVS_YUV_ORDER_UYVY` (intel_sprite_regs.h)
pub const DVS_YUV_ORDER_UYVY: u32 = reg_field_prep(DVS_YUV_ORDER_MASK, 1);

/// `DVS_YUV_ORDER_YVYU` (intel_sprite_regs.h)
pub const DVS_YUV_ORDER_YVYU: u32 = reg_field_prep(DVS_YUV_ORDER_MASK, 2);

/// `DVS_YUV_ORDER_VYUY` (intel_sprite_regs.h)
pub const DVS_YUV_ORDER_VYUY: u32 = reg_field_prep(DVS_YUV_ORDER_MASK, 3);

/// `DVS_ROTATE_180` (intel_sprite_regs.h)
pub const DVS_ROTATE_180: u32 = reg_bit(15);

/// `DVS_TRICKLE_FEED_DISABLE` (intel_sprite_regs.h)
pub const DVS_TRICKLE_FEED_DISABLE: u32 = reg_bit(14);

/// `DVS_TILED` (intel_sprite_regs.h)
pub const DVS_TILED: u32 = reg_bit(10);

/// `DVS_DEST_KEY` (intel_sprite_regs.h)
pub const DVS_DEST_KEY: u32 = reg_bit(2);

/// `_DVSALINOFF` (intel_sprite_regs.h)
pub const _DVSALINOFF: u32 = 0x72184;

/// `_DVSBLINOFF` (intel_sprite_regs.h)
pub const _DVSBLINOFF: u32 = 0x73184;

/// `DVSLINOFF(pipe)` (intel_sprite_regs.h)
pub const fn DVSLINOFF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSALINOFF, _DVSBLINOFF)
}

/// `_DVSASTRIDE` (intel_sprite_regs.h)
pub const _DVSASTRIDE: u32 = 0x72188;

/// `_DVSBSTRIDE` (intel_sprite_regs.h)
pub const _DVSBSTRIDE: u32 = 0x73188;

/// `DVSSTRIDE(pipe)` (intel_sprite_regs.h)
pub const fn DVSSTRIDE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSASTRIDE, _DVSBSTRIDE)
}

/// `_DVSAPOS` (intel_sprite_regs.h)
pub const _DVSAPOS: u32 = 0x7218c;

/// `_DVSBPOS` (intel_sprite_regs.h)
pub const _DVSBPOS: u32 = 0x7318c;

/// `DVSPOS(pipe)` (intel_sprite_regs.h)
pub const fn DVSPOS(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSAPOS, _DVSBPOS)
}

/// `DVS_POS_Y_MASK` (intel_sprite_regs.h)
pub const DVS_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `DVS_POS_Y(y)` (intel_sprite_regs.h)
pub const fn DVS_POS_Y(y: u32) -> u32 {
    reg_field_prep(DVS_POS_Y_MASK, (y))
}

/// `DVS_POS_X_MASK` (intel_sprite_regs.h)
pub const DVS_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `DVS_POS_X(x)` (intel_sprite_regs.h)
pub const fn DVS_POS_X(x: u32) -> u32 {
    reg_field_prep(DVS_POS_X_MASK, (x))
}

/// `_DVSASIZE` (intel_sprite_regs.h)
pub const _DVSASIZE: u32 = 0x72190;

/// `_DVSBSIZE` (intel_sprite_regs.h)
pub const _DVSBSIZE: u32 = 0x73190;

/// `DVSSIZE(pipe)` (intel_sprite_regs.h)
pub const fn DVSSIZE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSASIZE, _DVSBSIZE)
}

/// `DVS_HEIGHT_MASK` (intel_sprite_regs.h)
pub const DVS_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `DVS_HEIGHT(h)` (intel_sprite_regs.h)
pub const fn DVS_HEIGHT(h: u32) -> u32 {
    reg_field_prep(DVS_HEIGHT_MASK, (h))
}

/// `DVS_WIDTH_MASK` (intel_sprite_regs.h)
pub const DVS_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `DVS_WIDTH(w)` (intel_sprite_regs.h)
pub const fn DVS_WIDTH(w: u32) -> u32 {
    reg_field_prep(DVS_WIDTH_MASK, (w))
}

/// `_DVSAKEYVAL` (intel_sprite_regs.h)
pub const _DVSAKEYVAL: u32 = 0x72194;

/// `_DVSBKEYVAL` (intel_sprite_regs.h)
pub const _DVSBKEYVAL: u32 = 0x73194;

/// `DVSKEYVAL(pipe)` (intel_sprite_regs.h)
pub const fn DVSKEYVAL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSAKEYVAL, _DVSBKEYVAL)
}

/// `_DVSAKEYMSK` (intel_sprite_regs.h)
pub const _DVSAKEYMSK: u32 = 0x72198;

/// `_DVSBKEYMSK` (intel_sprite_regs.h)
pub const _DVSBKEYMSK: u32 = 0x73198;

/// `DVSKEYMSK(pipe)` (intel_sprite_regs.h)
pub const fn DVSKEYMSK(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSAKEYMSK, _DVSBKEYMSK)
}

/// `_DVSASURF` (intel_sprite_regs.h)
pub const _DVSASURF: u32 = 0x7219c;

/// `_DVSBSURF` (intel_sprite_regs.h)
pub const _DVSBSURF: u32 = 0x7319c;

/// `DVSSURF(pipe)` (intel_sprite_regs.h)
pub const fn DVSSURF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSASURF, _DVSBSURF)
}

/// `DVS_ADDR_MASK` (intel_sprite_regs.h)
pub const DVS_ADDR_MASK: u32 = reg_genmask(31, 12);

/// `_DVSAKEYMAXVAL` (intel_sprite_regs.h)
pub const _DVSAKEYMAXVAL: u32 = 0x721a0;

/// `_DVSBKEYMAXVAL` (intel_sprite_regs.h)
pub const _DVSBKEYMAXVAL: u32 = 0x731a0;

/// `DVSKEYMAX(pipe)` (intel_sprite_regs.h)
pub const fn DVSKEYMAX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSAKEYMAXVAL, _DVSBKEYMAXVAL)
}

/// `_DVSATILEOFF` (intel_sprite_regs.h)
pub const _DVSATILEOFF: u32 = 0x721a4;

/// `_DVSBTILEOFF` (intel_sprite_regs.h)
pub const _DVSBTILEOFF: u32 = 0x731a4;

/// `DVSTILEOFF(pipe)` (intel_sprite_regs.h)
pub const fn DVSTILEOFF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSATILEOFF, _DVSBTILEOFF)
}

/// `DVS_OFFSET_Y_MASK` (intel_sprite_regs.h)
pub const DVS_OFFSET_Y_MASK: u32 = reg_genmask(31, 16);

/// `DVS_OFFSET_Y(y)` (intel_sprite_regs.h)
pub const fn DVS_OFFSET_Y(y: u32) -> u32 {
    reg_field_prep(DVS_OFFSET_Y_MASK, (y))
}

/// `DVS_OFFSET_X_MASK` (intel_sprite_regs.h)
pub const DVS_OFFSET_X_MASK: u32 = reg_genmask(15, 0);

/// `DVS_OFFSET_X(x)` (intel_sprite_regs.h)
pub const fn DVS_OFFSET_X(x: u32) -> u32 {
    reg_field_prep(DVS_OFFSET_X_MASK, (x))
}

/// `_DVSASURFLIVE` (intel_sprite_regs.h)
pub const _DVSASURFLIVE: u32 = 0x721ac;

/// `_DVSBSURFLIVE` (intel_sprite_regs.h)
pub const _DVSBSURFLIVE: u32 = 0x731ac;

/// `DVSSURFLIVE(pipe)` (intel_sprite_regs.h)
pub const fn DVSSURFLIVE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSASURFLIVE, _DVSBSURFLIVE)
}

/// `_DVSAGAMC_G4X` (intel_sprite_regs.h)
pub const _DVSAGAMC_G4X: u32 = 0x721e0;

/// `_DVSBGAMC_G4X` (intel_sprite_regs.h)
pub const _DVSBGAMC_G4X: u32 = 0x731e0;

/// `DVSGAMC_G4X(pipe, i)` (intel_sprite_regs.h)
pub const fn DVSGAMC_G4X(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _DVSAGAMC_G4X, _DVSBGAMC_G4X) + (((5 - (i))) * 4)))
}

/// `_DVSASCALE` (intel_sprite_regs.h)
pub const _DVSASCALE: u32 = 0x72204;

/// `_DVSBSCALE` (intel_sprite_regs.h)
pub const _DVSBSCALE: u32 = 0x73204;

/// `DVSSCALE(pipe)` (intel_sprite_regs.h)
pub const fn DVSSCALE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DVSASCALE, _DVSBSCALE)
}

/// `DVS_SCALE_ENABLE` (intel_sprite_regs.h)
pub const DVS_SCALE_ENABLE: u32 = reg_bit(31);

/// `DVS_FILTER_MASK` (intel_sprite_regs.h)
pub const DVS_FILTER_MASK: u32 = reg_genmask(30, 29);

/// `DVS_FILTER_MEDIUM` (intel_sprite_regs.h)
pub const DVS_FILTER_MEDIUM: u32 = reg_field_prep(DVS_FILTER_MASK, 0);

/// `DVS_FILTER_ENHANCING` (intel_sprite_regs.h)
pub const DVS_FILTER_ENHANCING: u32 = reg_field_prep(DVS_FILTER_MASK, 1);

/// `DVS_FILTER_SOFTENING` (intel_sprite_regs.h)
pub const DVS_FILTER_SOFTENING: u32 = reg_field_prep(DVS_FILTER_MASK, 2);

/// `DVS_VERTICAL_OFFSET_HALF` (intel_sprite_regs.h)
pub const DVS_VERTICAL_OFFSET_HALF: u32 = reg_bit(28);

/// `DVS_VERTICAL_OFFSET_ENABLE` (intel_sprite_regs.h)
pub const DVS_VERTICAL_OFFSET_ENABLE: u32 = reg_bit(27);

/// `DVS_SRC_WIDTH_MASK` (intel_sprite_regs.h)
pub const DVS_SRC_WIDTH_MASK: u32 = reg_genmask(26, 16);

/// `DVS_SRC_WIDTH(w)` (intel_sprite_regs.h)
pub const fn DVS_SRC_WIDTH(w: u32) -> u32 {
    reg_field_prep(DVS_SRC_WIDTH_MASK, (w))
}

/// `DVS_SRC_HEIGHT_MASK` (intel_sprite_regs.h)
pub const DVS_SRC_HEIGHT_MASK: u32 = reg_genmask(10, 0);

/// `DVS_SRC_HEIGHT(h)` (intel_sprite_regs.h)
pub const fn DVS_SRC_HEIGHT(h: u32) -> u32 {
    reg_field_prep(DVS_SRC_HEIGHT_MASK, (h))
}

/// `_DVSAGAMC_ILK` (intel_sprite_regs.h)
pub const _DVSAGAMC_ILK: u32 = 0x72300;

/// `_DVSBGAMC_ILK` (intel_sprite_regs.h)
pub const _DVSBGAMC_ILK: u32 = 0x73300;

/// `DVSGAMC_ILK(pipe, i)` (intel_sprite_regs.h)
pub const fn DVSGAMC_ILK(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _DVSAGAMC_ILK, _DVSBGAMC_ILK) + ((i) * 4)))
}

/// `_DVSAGAMCMAX_ILK` (intel_sprite_regs.h)
pub const _DVSAGAMCMAX_ILK: u32 = 0x72340;

/// `_DVSBGAMCMAX_ILK` (intel_sprite_regs.h)
pub const _DVSBGAMCMAX_ILK: u32 = 0x73340;

/// `DVSGAMCMAX_ILK(pipe, i)` (intel_sprite_regs.h)
pub const fn DVSGAMCMAX_ILK(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _DVSAGAMCMAX_ILK, _DVSBGAMCMAX_ILK) + ((i) * 4)))
}

/// `_SPRA_CTL` (intel_sprite_regs.h)
pub const _SPRA_CTL: u32 = 0x70280;

/// `_SPRB_CTL` (intel_sprite_regs.h)
pub const _SPRB_CTL: u32 = 0x71280;

/// `SPRCTL(pipe)` (intel_sprite_regs.h)
pub const fn SPRCTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_CTL, _SPRB_CTL)
}

/// `SPRITE_ENABLE` (intel_sprite_regs.h)
pub const SPRITE_ENABLE: u32 = reg_bit(31);

/// `SPRITE_PIPE_GAMMA_ENABLE` (intel_sprite_regs.h)
pub const SPRITE_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `SPRITE_YUV_RANGE_CORRECTION_DISABLE` (intel_sprite_regs.h)
pub const SPRITE_YUV_RANGE_CORRECTION_DISABLE: u32 = reg_bit(28);

/// `SPRITE_FORMAT_MASK` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_MASK: u32 = reg_genmask(27, 25);

/// `SPRITE_FORMAT_YUV422` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_YUV422: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 0);

/// `SPRITE_FORMAT_RGBX101010` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_RGBX101010: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 1);

/// `SPRITE_FORMAT_RGBX888` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_RGBX888: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 2);

/// `SPRITE_FORMAT_RGBX161616` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_RGBX161616: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 3);

/// `SPRITE_FORMAT_YUV444` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_YUV444: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 4);

/// `SPRITE_FORMAT_XR_BGR101010` (intel_sprite_regs.h)
pub const SPRITE_FORMAT_XR_BGR101010: u32 = reg_field_prep(SPRITE_FORMAT_MASK, 5);

/// `SPRITE_PIPE_CSC_ENABLE` (intel_sprite_regs.h)
pub const SPRITE_PIPE_CSC_ENABLE: u32 = reg_bit(24);

/// `SPRITE_SOURCE_KEY` (intel_sprite_regs.h)
pub const SPRITE_SOURCE_KEY: u32 = reg_bit(22);

/// `SPRITE_RGB_ORDER_RGBX` (intel_sprite_regs.h)
pub const SPRITE_RGB_ORDER_RGBX: u32 = reg_bit(20);

/// `SPRITE_YUV_TO_RGB_CSC_DISABLE` (intel_sprite_regs.h)
pub const SPRITE_YUV_TO_RGB_CSC_DISABLE: u32 = reg_bit(19);

/// `SPRITE_YUV_TO_RGB_CSC_FORMAT_BT709` (intel_sprite_regs.h)
pub const SPRITE_YUV_TO_RGB_CSC_FORMAT_BT709: u32 = reg_bit(18);

/// `SPRITE_YUV_ORDER_MASK` (intel_sprite_regs.h)
pub const SPRITE_YUV_ORDER_MASK: u32 = reg_genmask(17, 16);

/// `SPRITE_YUV_ORDER_YUYV` (intel_sprite_regs.h)
pub const SPRITE_YUV_ORDER_YUYV: u32 = reg_field_prep(SPRITE_YUV_ORDER_MASK, 0);

/// `SPRITE_YUV_ORDER_UYVY` (intel_sprite_regs.h)
pub const SPRITE_YUV_ORDER_UYVY: u32 = reg_field_prep(SPRITE_YUV_ORDER_MASK, 1);

/// `SPRITE_YUV_ORDER_YVYU` (intel_sprite_regs.h)
pub const SPRITE_YUV_ORDER_YVYU: u32 = reg_field_prep(SPRITE_YUV_ORDER_MASK, 2);

/// `SPRITE_YUV_ORDER_VYUY` (intel_sprite_regs.h)
pub const SPRITE_YUV_ORDER_VYUY: u32 = reg_field_prep(SPRITE_YUV_ORDER_MASK, 3);

/// `SPRITE_ROTATE_180` (intel_sprite_regs.h)
pub const SPRITE_ROTATE_180: u32 = reg_bit(15);

/// `SPRITE_TRICKLE_FEED_DISABLE` (intel_sprite_regs.h)
pub const SPRITE_TRICKLE_FEED_DISABLE: u32 = reg_bit(14);

/// `SPRITE_PLANE_GAMMA_DISABLE` (intel_sprite_regs.h)
pub const SPRITE_PLANE_GAMMA_DISABLE: u32 = reg_bit(13);

/// `SPRITE_TILED` (intel_sprite_regs.h)
pub const SPRITE_TILED: u32 = reg_bit(10);

/// `SPRITE_DEST_KEY` (intel_sprite_regs.h)
pub const SPRITE_DEST_KEY: u32 = reg_bit(2);

/// `_SPRA_LINOFF` (intel_sprite_regs.h)
pub const _SPRA_LINOFF: u32 = 0x70284;

/// `_SPRB_LINOFF` (intel_sprite_regs.h)
pub const _SPRB_LINOFF: u32 = 0x71284;

/// `SPRLINOFF(pipe)` (intel_sprite_regs.h)
pub const fn SPRLINOFF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_LINOFF, _SPRB_LINOFF)
}

/// `_SPRA_STRIDE` (intel_sprite_regs.h)
pub const _SPRA_STRIDE: u32 = 0x70288;

/// `_SPRB_STRIDE` (intel_sprite_regs.h)
pub const _SPRB_STRIDE: u32 = 0x71288;

/// `SPRSTRIDE(pipe)` (intel_sprite_regs.h)
pub const fn SPRSTRIDE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_STRIDE, _SPRB_STRIDE)
}

/// `_SPRA_POS` (intel_sprite_regs.h)
pub const _SPRA_POS: u32 = 0x7028c;

/// `_SPRB_POS` (intel_sprite_regs.h)
pub const _SPRB_POS: u32 = 0x7128c;

/// `SPRPOS(pipe)` (intel_sprite_regs.h)
pub const fn SPRPOS(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_POS, _SPRB_POS)
}

/// `SPRITE_POS_Y_MASK` (intel_sprite_regs.h)
pub const SPRITE_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `SPRITE_POS_Y(y)` (intel_sprite_regs.h)
pub const fn SPRITE_POS_Y(y: u32) -> u32 {
    reg_field_prep(SPRITE_POS_Y_MASK, (y))
}

/// `SPRITE_POS_X_MASK` (intel_sprite_regs.h)
pub const SPRITE_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `SPRITE_POS_X(x)` (intel_sprite_regs.h)
pub const fn SPRITE_POS_X(x: u32) -> u32 {
    reg_field_prep(SPRITE_POS_X_MASK, (x))
}

/// `_SPRA_SIZE` (intel_sprite_regs.h)
pub const _SPRA_SIZE: u32 = 0x70290;

/// `_SPRB_SIZE` (intel_sprite_regs.h)
pub const _SPRB_SIZE: u32 = 0x71290;

/// `SPRSIZE(pipe)` (intel_sprite_regs.h)
pub const fn SPRSIZE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_SIZE, _SPRB_SIZE)
}

/// `SPRITE_HEIGHT_MASK` (intel_sprite_regs.h)
pub const SPRITE_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `SPRITE_HEIGHT(h)` (intel_sprite_regs.h)
pub const fn SPRITE_HEIGHT(h: u32) -> u32 {
    reg_field_prep(SPRITE_HEIGHT_MASK, (h))
}

/// `SPRITE_WIDTH_MASK` (intel_sprite_regs.h)
pub const SPRITE_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `SPRITE_WIDTH(w)` (intel_sprite_regs.h)
pub const fn SPRITE_WIDTH(w: u32) -> u32 {
    reg_field_prep(SPRITE_WIDTH_MASK, (w))
}

/// `_SPRA_KEYVAL` (intel_sprite_regs.h)
pub const _SPRA_KEYVAL: u32 = 0x70294;

/// `_SPRB_KEYVAL` (intel_sprite_regs.h)
pub const _SPRB_KEYVAL: u32 = 0x71294;

/// `SPRKEYVAL(pipe)` (intel_sprite_regs.h)
pub const fn SPRKEYVAL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_KEYVAL, _SPRB_KEYVAL)
}

/// `_SPRA_KEYMSK` (intel_sprite_regs.h)
pub const _SPRA_KEYMSK: u32 = 0x70298;

/// `_SPRB_KEYMSK` (intel_sprite_regs.h)
pub const _SPRB_KEYMSK: u32 = 0x71298;

/// `SPRKEYMSK(pipe)` (intel_sprite_regs.h)
pub const fn SPRKEYMSK(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_KEYMSK, _SPRB_KEYMSK)
}

/// `_SPRA_SURF` (intel_sprite_regs.h)
pub const _SPRA_SURF: u32 = 0x7029c;

/// `_SPRB_SURF` (intel_sprite_regs.h)
pub const _SPRB_SURF: u32 = 0x7129c;

/// `SPRSURF(pipe)` (intel_sprite_regs.h)
pub const fn SPRSURF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_SURF, _SPRB_SURF)
}

/// `SPRITE_ADDR_MASK` (intel_sprite_regs.h)
pub const SPRITE_ADDR_MASK: u32 = reg_genmask(31, 12);

/// `_SPRA_KEYMAX` (intel_sprite_regs.h)
pub const _SPRA_KEYMAX: u32 = 0x702a0;

/// `_SPRB_KEYMAX` (intel_sprite_regs.h)
pub const _SPRB_KEYMAX: u32 = 0x712a0;

/// `SPRKEYMAX(pipe)` (intel_sprite_regs.h)
pub const fn SPRKEYMAX(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_KEYMAX, _SPRB_KEYMAX)
}

/// `_SPRA_TILEOFF` (intel_sprite_regs.h)
pub const _SPRA_TILEOFF: u32 = 0x702a4;

/// `_SPRB_TILEOFF` (intel_sprite_regs.h)
pub const _SPRB_TILEOFF: u32 = 0x712a4;

/// `SPRTILEOFF(pipe)` (intel_sprite_regs.h)
pub const fn SPRTILEOFF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_TILEOFF, _SPRB_TILEOFF)
}

/// `SPRITE_OFFSET_Y_MASK` (intel_sprite_regs.h)
pub const SPRITE_OFFSET_Y_MASK: u32 = reg_genmask(31, 16);

/// `SPRITE_OFFSET_Y(y)` (intel_sprite_regs.h)
pub const fn SPRITE_OFFSET_Y(y: u32) -> u32 {
    reg_field_prep(SPRITE_OFFSET_Y_MASK, (y))
}

/// `SPRITE_OFFSET_X_MASK` (intel_sprite_regs.h)
pub const SPRITE_OFFSET_X_MASK: u32 = reg_genmask(15, 0);

/// `SPRITE_OFFSET_X(x)` (intel_sprite_regs.h)
pub const fn SPRITE_OFFSET_X(x: u32) -> u32 {
    reg_field_prep(SPRITE_OFFSET_X_MASK, (x))
}

/// `_SPRA_OFFSET` (intel_sprite_regs.h)
pub const _SPRA_OFFSET: u32 = 0x702a4;

/// `_SPRB_OFFSET` (intel_sprite_regs.h)
pub const _SPRB_OFFSET: u32 = 0x712a4;

/// `SPROFFSET(pipe)` (intel_sprite_regs.h)
pub const fn SPROFFSET(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_OFFSET, _SPRB_OFFSET)
}

/// `_SPRA_SURFLIVE` (intel_sprite_regs.h)
pub const _SPRA_SURFLIVE: u32 = 0x702ac;

/// `_SPRB_SURFLIVE` (intel_sprite_regs.h)
pub const _SPRB_SURFLIVE: u32 = 0x712ac;

/// `SPRSURFLIVE(pipe)` (intel_sprite_regs.h)
pub const fn SPRSURFLIVE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_SURFLIVE, _SPRB_SURFLIVE)
}

/// `_SPRA_SCALE` (intel_sprite_regs.h)
pub const _SPRA_SCALE: u32 = 0x70304;

/// `_SPRB_SCALE` (intel_sprite_regs.h)
pub const _SPRB_SCALE: u32 = 0x71304;

/// `SPRSCALE(pipe)` (intel_sprite_regs.h)
pub const fn SPRSCALE(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _SPRA_SCALE, _SPRB_SCALE)
}

/// `SPRITE_SCALE_ENABLE` (intel_sprite_regs.h)
pub const SPRITE_SCALE_ENABLE: u32 = reg_bit(31);

/// `SPRITE_FILTER_MASK` (intel_sprite_regs.h)
pub const SPRITE_FILTER_MASK: u32 = reg_genmask(30, 29);

/// `SPRITE_FILTER_MEDIUM` (intel_sprite_regs.h)
pub const SPRITE_FILTER_MEDIUM: u32 = reg_field_prep(SPRITE_FILTER_MASK, 0);

/// `SPRITE_FILTER_ENHANCING` (intel_sprite_regs.h)
pub const SPRITE_FILTER_ENHANCING: u32 = reg_field_prep(SPRITE_FILTER_MASK, 1);

/// `SPRITE_FILTER_SOFTENING` (intel_sprite_regs.h)
pub const SPRITE_FILTER_SOFTENING: u32 = reg_field_prep(SPRITE_FILTER_MASK, 2);

/// `SPRITE_VERTICAL_OFFSET_HALF` (intel_sprite_regs.h)
pub const SPRITE_VERTICAL_OFFSET_HALF: u32 = reg_bit(28);

/// `SPRITE_VERTICAL_OFFSET_ENABLE` (intel_sprite_regs.h)
pub const SPRITE_VERTICAL_OFFSET_ENABLE: u32 = reg_bit(27);

/// `SPRITE_SRC_WIDTH_MASK` (intel_sprite_regs.h)
pub const SPRITE_SRC_WIDTH_MASK: u32 = reg_genmask(26, 16);

/// `SPRITE_SRC_WIDTH(w)` (intel_sprite_regs.h)
pub const fn SPRITE_SRC_WIDTH(w: u32) -> u32 {
    reg_field_prep(SPRITE_SRC_WIDTH_MASK, (w))
}

/// `SPRITE_SRC_HEIGHT_MASK` (intel_sprite_regs.h)
pub const SPRITE_SRC_HEIGHT_MASK: u32 = reg_genmask(10, 0);

/// `SPRITE_SRC_HEIGHT(h)` (intel_sprite_regs.h)
pub const fn SPRITE_SRC_HEIGHT(h: u32) -> u32 {
    reg_field_prep(SPRITE_SRC_HEIGHT_MASK, (h))
}

/// `_SPRA_GAMC` (intel_sprite_regs.h)
pub const _SPRA_GAMC: u32 = 0x70400;

/// `_SPRB_GAMC` (intel_sprite_regs.h)
pub const _SPRB_GAMC: u32 = 0x71400;

/// `SPRGAMC(pipe, i)` (intel_sprite_regs.h)
pub const fn SPRGAMC(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _SPRA_GAMC, _SPRB_GAMC) + ((i) * 4)))
}

/// `_SPRA_GAMC16` (intel_sprite_regs.h)
pub const _SPRA_GAMC16: u32 = 0x70440;

/// `_SPRB_GAMC16` (intel_sprite_regs.h)
pub const _SPRB_GAMC16: u32 = 0x71440;

/// `SPRGAMC16(pipe, i)` (intel_sprite_regs.h)
pub const fn SPRGAMC16(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _SPRA_GAMC16, _SPRB_GAMC16) + ((i) * 4)))
}

/// `_SPRA_GAMC17` (intel_sprite_regs.h)
pub const _SPRA_GAMC17: u32 = 0x7044c;

/// `_SPRB_GAMC17` (intel_sprite_regs.h)
pub const _SPRB_GAMC17: u32 = 0x7144c;

/// `SPRGAMC17(pipe, i)` (intel_sprite_regs.h)
pub const fn SPRGAMC17(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _SPRA_GAMC17, _SPRB_GAMC17) + ((i) * 4)))
}

/// `_VLV_SPR(pipe, plane_id, reg_a, reg_b)` (intel_sprite_regs.h)
pub const fn _VLV_SPR(pipe: u32, plane_id: u32, reg_a: u32, reg_b: u32) -> u32 {
    _PIPE(((((pipe) * 2) + (plane_id)) - (PLANE_SPRITE0 as u32)), (reg_a), (reg_b))
}

/// `_MMIO_VLV_SPR(pipe, plane_id, reg_a, reg_b)` (intel_sprite_regs.h)
pub const fn _MMIO_VLV_SPR(pipe: u32, plane_id: u32, reg_a: u32, reg_b: u32) -> I915RegT {
    mmio(_VLV_SPR((pipe), (plane_id), (reg_a), (reg_b)))
}

/// `_SPACNTR` (intel_sprite_regs.h)
pub const _SPACNTR: u32 = ((VLV_DISPLAY_BASE + 0x72180));

/// `_SPBCNTR` (intel_sprite_regs.h)
pub const _SPBCNTR: u32 = ((VLV_DISPLAY_BASE + 0x72280));

/// `SPCNTR(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPCNTR(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPACNTR, _SPBCNTR)
}

/// `SP_ENABLE` (intel_sprite_regs.h)
pub const SP_ENABLE: u32 = reg_bit(31);

/// `SP_PIPE_GAMMA_ENABLE` (intel_sprite_regs.h)
pub const SP_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `SP_FORMAT_MASK` (intel_sprite_regs.h)
pub const SP_FORMAT_MASK: u32 = reg_genmask(29, 26);

/// `SP_FORMAT_YUV422` (intel_sprite_regs.h)
pub const SP_FORMAT_YUV422: u32 = reg_field_prep(SP_FORMAT_MASK, 0);

/// `SP_FORMAT_8BPP` (intel_sprite_regs.h)
pub const SP_FORMAT_8BPP: u32 = reg_field_prep(SP_FORMAT_MASK, 2);

/// `SP_FORMAT_BGR565` (intel_sprite_regs.h)
pub const SP_FORMAT_BGR565: u32 = reg_field_prep(SP_FORMAT_MASK, 5);

/// `SP_FORMAT_BGRX8888` (intel_sprite_regs.h)
pub const SP_FORMAT_BGRX8888: u32 = reg_field_prep(SP_FORMAT_MASK, 6);

/// `SP_FORMAT_BGRA8888` (intel_sprite_regs.h)
pub const SP_FORMAT_BGRA8888: u32 = reg_field_prep(SP_FORMAT_MASK, 7);

/// `SP_FORMAT_RGBX1010102` (intel_sprite_regs.h)
pub const SP_FORMAT_RGBX1010102: u32 = reg_field_prep(SP_FORMAT_MASK, 8);

/// `SP_FORMAT_RGBA1010102` (intel_sprite_regs.h)
pub const SP_FORMAT_RGBA1010102: u32 = reg_field_prep(SP_FORMAT_MASK, 9);

/// `SP_FORMAT_BGRX1010102` (intel_sprite_regs.h)
pub const SP_FORMAT_BGRX1010102: u32 = reg_field_prep(SP_FORMAT_MASK, 10);

/// `SP_FORMAT_BGRA1010102` (intel_sprite_regs.h)
pub const SP_FORMAT_BGRA1010102: u32 = reg_field_prep(SP_FORMAT_MASK, 11);

/// `SP_FORMAT_RGBX8888` (intel_sprite_regs.h)
pub const SP_FORMAT_RGBX8888: u32 = reg_field_prep(SP_FORMAT_MASK, 14);

/// `SP_FORMAT_RGBA8888` (intel_sprite_regs.h)
pub const SP_FORMAT_RGBA8888: u32 = reg_field_prep(SP_FORMAT_MASK, 15);

/// `SP_ALPHA_PREMULTIPLY` (intel_sprite_regs.h)
pub const SP_ALPHA_PREMULTIPLY: u32 = reg_bit(23);

/// `SP_SOURCE_KEY` (intel_sprite_regs.h)
pub const SP_SOURCE_KEY: u32 = reg_bit(22);

/// `SP_YUV_FORMAT_BT709` (intel_sprite_regs.h)
pub const SP_YUV_FORMAT_BT709: u32 = reg_bit(18);

/// `SP_YUV_ORDER_MASK` (intel_sprite_regs.h)
pub const SP_YUV_ORDER_MASK: u32 = reg_genmask(17, 16);

/// `SP_YUV_ORDER_YUYV` (intel_sprite_regs.h)
pub const SP_YUV_ORDER_YUYV: u32 = reg_field_prep(SP_YUV_ORDER_MASK, 0);

/// `SP_YUV_ORDER_UYVY` (intel_sprite_regs.h)
pub const SP_YUV_ORDER_UYVY: u32 = reg_field_prep(SP_YUV_ORDER_MASK, 1);

/// `SP_YUV_ORDER_YVYU` (intel_sprite_regs.h)
pub const SP_YUV_ORDER_YVYU: u32 = reg_field_prep(SP_YUV_ORDER_MASK, 2);

/// `SP_YUV_ORDER_VYUY` (intel_sprite_regs.h)
pub const SP_YUV_ORDER_VYUY: u32 = reg_field_prep(SP_YUV_ORDER_MASK, 3);

/// `SP_ROTATE_180` (intel_sprite_regs.h)
pub const SP_ROTATE_180: u32 = reg_bit(15);

/// `SP_TILED` (intel_sprite_regs.h)
pub const SP_TILED: u32 = reg_bit(10);

/// `SP_MIRROR` (intel_sprite_regs.h)
pub const SP_MIRROR: u32 = reg_bit(8);

/// `_SPALINOFF` (intel_sprite_regs.h)
pub const _SPALINOFF: u32 = ((VLV_DISPLAY_BASE + 0x72184));

/// `_SPBLINOFF` (intel_sprite_regs.h)
pub const _SPBLINOFF: u32 = ((VLV_DISPLAY_BASE + 0x72284));

/// `SPLINOFF(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPLINOFF(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPALINOFF, _SPBLINOFF)
}

/// `_SPASTRIDE` (intel_sprite_regs.h)
pub const _SPASTRIDE: u32 = ((VLV_DISPLAY_BASE + 0x72188));

/// `_SPBSTRIDE` (intel_sprite_regs.h)
pub const _SPBSTRIDE: u32 = ((VLV_DISPLAY_BASE + 0x72288));

/// `SPSTRIDE(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPSTRIDE(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPASTRIDE, _SPBSTRIDE)
}

/// `_SPAPOS` (intel_sprite_regs.h)
pub const _SPAPOS: u32 = ((VLV_DISPLAY_BASE + 0x7218c));

/// `_SPBPOS` (intel_sprite_regs.h)
pub const _SPBPOS: u32 = ((VLV_DISPLAY_BASE + 0x7228c));

/// `SPPOS(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPPOS(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPAPOS, _SPBPOS)
}

/// `SP_POS_Y_MASK` (intel_sprite_regs.h)
pub const SP_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `SP_POS_Y(y)` (intel_sprite_regs.h)
pub const fn SP_POS_Y(y: u32) -> u32 {
    reg_field_prep(SP_POS_Y_MASK, (y))
}

/// `SP_POS_X_MASK` (intel_sprite_regs.h)
pub const SP_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `SP_POS_X(x)` (intel_sprite_regs.h)
pub const fn SP_POS_X(x: u32) -> u32 {
    reg_field_prep(SP_POS_X_MASK, (x))
}

/// `_SPASIZE` (intel_sprite_regs.h)
pub const _SPASIZE: u32 = ((VLV_DISPLAY_BASE + 0x72190));

/// `_SPBSIZE` (intel_sprite_regs.h)
pub const _SPBSIZE: u32 = ((VLV_DISPLAY_BASE + 0x72290));

/// `SPSIZE(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPSIZE(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPASIZE, _SPBSIZE)
}

/// `SP_HEIGHT_MASK` (intel_sprite_regs.h)
pub const SP_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `SP_HEIGHT(h)` (intel_sprite_regs.h)
pub const fn SP_HEIGHT(h: u32) -> u32 {
    reg_field_prep(SP_HEIGHT_MASK, (h))
}

/// `SP_WIDTH_MASK` (intel_sprite_regs.h)
pub const SP_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `SP_WIDTH(w)` (intel_sprite_regs.h)
pub const fn SP_WIDTH(w: u32) -> u32 {
    reg_field_prep(SP_WIDTH_MASK, (w))
}

/// `_SPAKEYMINVAL` (intel_sprite_regs.h)
pub const _SPAKEYMINVAL: u32 = ((VLV_DISPLAY_BASE + 0x72194));

/// `_SPBKEYMINVAL` (intel_sprite_regs.h)
pub const _SPBKEYMINVAL: u32 = ((VLV_DISPLAY_BASE + 0x72294));

/// `SPKEYMINVAL(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPKEYMINVAL(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPAKEYMINVAL, _SPBKEYMINVAL)
}

/// `_SPAKEYMSK` (intel_sprite_regs.h)
pub const _SPAKEYMSK: u32 = ((VLV_DISPLAY_BASE + 0x72198));

/// `_SPBKEYMSK` (intel_sprite_regs.h)
pub const _SPBKEYMSK: u32 = ((VLV_DISPLAY_BASE + 0x72298));

/// `SPKEYMSK(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPKEYMSK(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPAKEYMSK, _SPBKEYMSK)
}

/// `_SPASURF` (intel_sprite_regs.h)
pub const _SPASURF: u32 = ((VLV_DISPLAY_BASE + 0x7219c));

/// `_SPBSURF` (intel_sprite_regs.h)
pub const _SPBSURF: u32 = ((VLV_DISPLAY_BASE + 0x7229c));

/// `SPSURF(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPSURF(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPASURF, _SPBSURF)
}

/// `SP_ADDR_MASK` (intel_sprite_regs.h)
pub const SP_ADDR_MASK: u32 = reg_genmask(31, 12);

/// `_SPAKEYMAXVAL` (intel_sprite_regs.h)
pub const _SPAKEYMAXVAL: u32 = ((VLV_DISPLAY_BASE + 0x721a0));

/// `_SPBKEYMAXVAL` (intel_sprite_regs.h)
pub const _SPBKEYMAXVAL: u32 = ((VLV_DISPLAY_BASE + 0x722a0));

/// `SPKEYMAXVAL(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPKEYMAXVAL(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPAKEYMAXVAL, _SPBKEYMAXVAL)
}

/// `_SPATILEOFF` (intel_sprite_regs.h)
pub const _SPATILEOFF: u32 = ((VLV_DISPLAY_BASE + 0x721a4));

/// `_SPBTILEOFF` (intel_sprite_regs.h)
pub const _SPBTILEOFF: u32 = ((VLV_DISPLAY_BASE + 0x722a4));

/// `SPTILEOFF(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPTILEOFF(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPATILEOFF, _SPBTILEOFF)
}

/// `SP_OFFSET_Y_MASK` (intel_sprite_regs.h)
pub const SP_OFFSET_Y_MASK: u32 = reg_genmask(31, 16);

/// `SP_OFFSET_Y(y)` (intel_sprite_regs.h)
pub const fn SP_OFFSET_Y(y: u32) -> u32 {
    reg_field_prep(SP_OFFSET_Y_MASK, (y))
}

/// `SP_OFFSET_X_MASK` (intel_sprite_regs.h)
pub const SP_OFFSET_X_MASK: u32 = reg_genmask(15, 0);

/// `SP_OFFSET_X(x)` (intel_sprite_regs.h)
pub const fn SP_OFFSET_X(x: u32) -> u32 {
    reg_field_prep(SP_OFFSET_X_MASK, (x))
}

/// `_SPACONSTALPHA` (intel_sprite_regs.h)
pub const _SPACONSTALPHA: u32 = ((VLV_DISPLAY_BASE + 0x721a8));

/// `_SPBCONSTALPHA` (intel_sprite_regs.h)
pub const _SPBCONSTALPHA: u32 = ((VLV_DISPLAY_BASE + 0x722a8));

/// `SPCONSTALPHA(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPCONSTALPHA(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPACONSTALPHA, _SPBCONSTALPHA)
}

/// `SP_CONST_ALPHA_ENABLE` (intel_sprite_regs.h)
pub const SP_CONST_ALPHA_ENABLE: u32 = reg_bit(31);

/// `SP_CONST_ALPHA_MASK` (intel_sprite_regs.h)
pub const SP_CONST_ALPHA_MASK: u32 = reg_genmask(7, 0);

/// `SP_CONST_ALPHA(alpha)` (intel_sprite_regs.h)
pub const fn SP_CONST_ALPHA(alpha: u32) -> u32 {
    reg_field_prep(SP_CONST_ALPHA_MASK, (alpha))
}

/// `_SPASURFLIVE` (intel_sprite_regs.h)
pub const _SPASURFLIVE: u32 = ((VLV_DISPLAY_BASE + 0x721ac));

/// `_SPBSURFLIVE` (intel_sprite_regs.h)
pub const _SPBSURFLIVE: u32 = ((VLV_DISPLAY_BASE + 0x722ac));

/// `SPSURFLIVE(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPSURFLIVE(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPASURFLIVE, _SPBSURFLIVE)
}

/// `_SPACLRC0` (intel_sprite_regs.h)
pub const _SPACLRC0: u32 = ((VLV_DISPLAY_BASE + 0x721d0));

/// `_SPBCLRC0` (intel_sprite_regs.h)
pub const _SPBCLRC0: u32 = ((VLV_DISPLAY_BASE + 0x722d0));

/// `SPCLRC0(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPCLRC0(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPACLRC0, _SPBCLRC0)
}

/// `SP_CONTRAST_MASK` (intel_sprite_regs.h)
pub const SP_CONTRAST_MASK: u32 = reg_genmask(26, 18);

/// `SP_CONTRAST(x)` (intel_sprite_regs.h)
pub const fn SP_CONTRAST(x: u32) -> u32 {
    reg_field_prep(SP_CONTRAST_MASK, (x))
}

/// `SP_BRIGHTNESS_MASK` (intel_sprite_regs.h)
pub const SP_BRIGHTNESS_MASK: u32 = reg_genmask(7, 0);

/// `SP_BRIGHTNESS(x)` (intel_sprite_regs.h)
pub const fn SP_BRIGHTNESS(x: u32) -> u32 {
    reg_field_prep(SP_BRIGHTNESS_MASK, (x))
}

/// `_SPACLRC1` (intel_sprite_regs.h)
pub const _SPACLRC1: u32 = ((VLV_DISPLAY_BASE + 0x721d4));

/// `_SPBCLRC1` (intel_sprite_regs.h)
pub const _SPBCLRC1: u32 = ((VLV_DISPLAY_BASE + 0x722d4));

/// `SPCLRC1(pipe, plane_id)` (intel_sprite_regs.h)
pub const fn SPCLRC1(pipe: u32, plane_id: u32) -> I915RegT {
    _MMIO_VLV_SPR((pipe), (plane_id), _SPACLRC1, _SPBCLRC1)
}

/// `SP_SH_SIN_MASK` (intel_sprite_regs.h)
pub const SP_SH_SIN_MASK: u32 = reg_genmask(26, 16);

/// `SP_SH_SIN(x)` (intel_sprite_regs.h)
pub const fn SP_SH_SIN(x: u32) -> u32 {
    reg_field_prep(SP_SH_SIN_MASK, (x))
}

/// `SP_SH_COS_MASK` (intel_sprite_regs.h)
pub const SP_SH_COS_MASK: u32 = reg_genmask(9, 0);

/// `SP_SH_COS(x)` (intel_sprite_regs.h)
pub const fn SP_SH_COS(x: u32) -> u32 {
    reg_field_prep(SP_SH_COS_MASK, (x))
}

/// `_SPAGAMC` (intel_sprite_regs.h)
pub const _SPAGAMC: u32 = ((VLV_DISPLAY_BASE + 0x721e0));

/// `_SPBGAMC` (intel_sprite_regs.h)
pub const _SPBGAMC: u32 = ((VLV_DISPLAY_BASE + 0x722e0));

/// `SPGAMC(pipe, plane_id, i)` (intel_sprite_regs.h)
pub const fn SPGAMC(pipe: u32, plane_id: u32, i: u32) -> I915RegT {
    mmio((_VLV_SPR((pipe), (plane_id), _SPAGAMC, _SPBGAMC) + (((5 - (i))) * 4)))
}

/// `_MMIO_CHV_SPCSC(plane_id, reg)` (intel_sprite_regs.h)
pub const fn _MMIO_CHV_SPCSC(plane_id: u32, reg: u32) -> I915RegT {
    mmio(((VLV_DISPLAY_BASE + ((((plane_id) - (PLANE_SPRITE0 as u32))) * 0x1000)) + (reg)))
}

/// `SPCSCYGOFF(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCYGOFF(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d900)
}

/// `SPCSCCBOFF(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCBOFF(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d904)
}

/// `SPCSCCROFF(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCROFF(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d908)
}

/// `SPCSC_OOFF_MASK` (intel_sprite_regs.h)
pub const SPCSC_OOFF_MASK: u32 = reg_genmask(26, 16);

/// `SPCSC_OOFF(x)` (intel_sprite_regs.h)
pub const fn SPCSC_OOFF(x: u32) -> u32 {
    reg_field_prep(SPCSC_OOFF_MASK, ((x) & 0x7ff))
}

/// `SPCSC_IOFF_MASK` (intel_sprite_regs.h)
pub const SPCSC_IOFF_MASK: u32 = reg_genmask(10, 0);

/// `SPCSC_IOFF(x)` (intel_sprite_regs.h)
pub const fn SPCSC_IOFF(x: u32) -> u32 {
    reg_field_prep(SPCSC_IOFF_MASK, ((x) & 0x7ff))
}

/// `SPCSCC01(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCC01(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d90c)
}

/// `SPCSCC23(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCC23(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d910)
}

/// `SPCSCC45(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCC45(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d914)
}

/// `SPCSCC67(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCC67(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d918)
}

/// `SPCSCC8(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCC8(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d91c)
}

/// `SPCSC_C1_MASK` (intel_sprite_regs.h)
pub const SPCSC_C1_MASK: u32 = reg_genmask(30, 16);

/// `SPCSC_C1(x)` (intel_sprite_regs.h)
pub const fn SPCSC_C1(x: u32) -> u32 {
    reg_field_prep(SPCSC_C1_MASK, ((x) & 0x7fff))
}

/// `SPCSC_C0_MASK` (intel_sprite_regs.h)
pub const SPCSC_C0_MASK: u32 = reg_genmask(14, 0);

/// `SPCSC_C0(x)` (intel_sprite_regs.h)
pub const fn SPCSC_C0(x: u32) -> u32 {
    reg_field_prep(SPCSC_C0_MASK, ((x) & 0x7fff))
}

/// `SPCSCYGICLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCYGICLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d920)
}

/// `SPCSCCBICLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCBICLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d924)
}

/// `SPCSCCRICLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCRICLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d928)
}

/// `SPCSC_IMAX_MASK` (intel_sprite_regs.h)
pub const SPCSC_IMAX_MASK: u32 = reg_genmask(26, 16);

/// `SPCSC_IMAX(x)` (intel_sprite_regs.h)
pub const fn SPCSC_IMAX(x: u32) -> u32 {
    reg_field_prep(SPCSC_IMAX_MASK, ((x) & 0x7ff))
}

/// `SPCSC_IMIN_MASK` (intel_sprite_regs.h)
pub const SPCSC_IMIN_MASK: u32 = reg_genmask(10, 0);

/// `SPCSC_IMIN(x)` (intel_sprite_regs.h)
pub const fn SPCSC_IMIN(x: u32) -> u32 {
    reg_field_prep(SPCSC_IMIN_MASK, ((x) & 0x7ff))
}

/// `SPCSCYGOCLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCYGOCLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d92c)
}

/// `SPCSCCBOCLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCBOCLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d930)
}

/// `SPCSCCROCLAMP(plane_id)` (intel_sprite_regs.h)
pub const fn SPCSCCROCLAMP(plane_id: u32) -> I915RegT {
    _MMIO_CHV_SPCSC(plane_id, 0x6d934)
}

/// `SPCSC_OMAX_MASK` (intel_sprite_regs.h)
pub const SPCSC_OMAX_MASK: u32 = reg_genmask(25, 16);

/// `SPCSC_OMAX(x)` (intel_sprite_regs.h)
pub const fn SPCSC_OMAX(x: u32) -> u32 {
    reg_field_prep(SPCSC_OMAX_MASK, (x))
}

/// `SPCSC_OMIN_MASK` (intel_sprite_regs.h)
pub const SPCSC_OMIN_MASK: u32 = reg_genmask(9, 0);

/// `SPCSC_OMIN(x)` (intel_sprite_regs.h)
pub const fn SPCSC_OMIN(x: u32) -> u32 {
    reg_field_prep(SPCSC_OMIN_MASK, (x))
}
