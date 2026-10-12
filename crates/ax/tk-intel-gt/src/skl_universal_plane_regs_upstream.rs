// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/skl_universal_plane_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `_SKL_PLANE(pipe, plane, reg_1_a, reg_1_b, reg_2_a, reg_2_b)` (skl_universal_plane_regs.h)
pub const fn _SKL_PLANE(pipe: u32, plane: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32) -> u32 {
    _PLANE((plane), _PIPE((pipe), (reg_1_a), (reg_1_b)), _PIPE((pipe), (reg_2_a), (reg_2_b)))
}

/// `_SKL_PLANE_DW(pipe, plane, dw, reg_1_a, reg_1_b, reg_2_a, reg_2_b)` (skl_universal_plane_regs.h)
pub const fn _SKL_PLANE_DW(pipe: u32, plane: u32, dw: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32) -> u32 {
    ((_SKL_PLANE((pipe), (plane), (reg_1_a), (reg_1_b), (reg_2_a), (reg_2_b)) + ((dw) * 4)))
}

/// `_MMIO_SKL_PLANE(pipe, plane, reg_1_a, reg_1_b, reg_2_a, reg_2_b)` (skl_universal_plane_regs.h)
pub const fn _MMIO_SKL_PLANE(pipe: u32, plane: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32) -> I915RegT {
    mmio(_SKL_PLANE((pipe), (plane), (reg_1_a), (reg_1_b), (reg_2_a), (reg_2_b)))
}

/// `_MMIO_SKL_PLANE_DW(pipe, plane, dw, reg_1_a, reg_1_b, reg_2_a, reg_2_b)` (skl_universal_plane_regs.h)
pub const fn _MMIO_SKL_PLANE_DW(pipe: u32, plane: u32, dw: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32) -> I915RegT {
    mmio(_SKL_PLANE_DW((pipe), (plane), (dw), (reg_1_a), (reg_1_b), (reg_2_a), (reg_2_b)))
}

/// `_SEL_FETCH(pipe, plane, reg_1_a, reg_1_b, reg_2_a, reg_2_b, reg_5_a, reg_5_b, reg_6_a, reg_6_b)` (skl_universal_plane_regs.h)
pub const fn _SEL_FETCH(pipe: u32, plane: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32, reg_5_a: u32, reg_5_b: u32, reg_6_a: u32, reg_6_b: u32) -> u32 {
    pick_even_2ranges((plane), (PLANE_5 as u32), _PIPE((pipe), (reg_1_a), (reg_1_b)), _PIPE((pipe), (reg_2_a), (reg_2_b)), _PIPE((pipe), (reg_5_a), (reg_5_b)), _PIPE((pipe), (reg_6_a), (reg_6_b)))
}

/// `_MMIO_SEL_FETCH(pipe, plane, reg_1_a, reg_1_b, reg_2_a, reg_2_b, reg_5_a, reg_5_b, reg_6_a, reg_6_b)` (skl_universal_plane_regs.h)
pub const fn _MMIO_SEL_FETCH(pipe: u32, plane: u32, reg_1_a: u32, reg_1_b: u32, reg_2_a: u32, reg_2_b: u32, reg_5_a: u32, reg_5_b: u32, reg_6_a: u32, reg_6_b: u32) -> I915RegT {
    mmio(_SEL_FETCH((pipe), (plane), (reg_1_a), (reg_1_b), (reg_2_a), (reg_2_b), (reg_5_a), (reg_5_b), (reg_6_a), (reg_6_b)))
}

/// `_PLANE_CTL_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CTL_1_A: u32 = 0x70180;

/// `_PLANE_CTL_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CTL_2_A: u32 = 0x70280;

/// `_PLANE_CTL_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CTL_1_B: u32 = 0x71180;

/// `_PLANE_CTL_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CTL_2_B: u32 = 0x71280;

/// `PLANE_CTL(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_CTL(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_CTL_1_A, _PLANE_CTL_1_B, _PLANE_CTL_2_A, _PLANE_CTL_2_B)
}

/// `PLANE_CTL_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ENABLE: u32 = reg_bit(31);

/// `PLANE_CTL_ARB_SLOTS_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ARB_SLOTS_MASK: u32 = reg_genmask(30, 28);

/// `PLANE_CTL_ARB_SLOTS(x)` (skl_universal_plane_regs.h)
pub const fn PLANE_CTL_ARB_SLOTS(x: u32) -> u32 {
    reg_field_prep(PLANE_CTL_ARB_SLOTS_MASK, (x))
}

/// `PLANE_CTL_PIPE_GAMMA_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV_RANGE_CORRECTION_DISABLE: u32 = reg_bit(28);

/// `PLANE_CTL_FORMAT_MASK_SKL` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_MASK_SKL: u32 = reg_genmask(27, 24);

/// `PLANE_CTL_FORMAT_MASK_ICL` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_MASK_ICL: u32 = reg_genmask(27, 23);

/// `PLANE_CTL_FORMAT_YUV422` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_YUV422: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 0);

/// `PLANE_CTL_FORMAT_NV12` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_NV12: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 1);

/// `PLANE_CTL_FORMAT_XRGB_2101010` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_XRGB_2101010: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 2);

/// `PLANE_CTL_FORMAT_P010` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_P010: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 3);

/// `PLANE_CTL_FORMAT_XRGB_8888` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_XRGB_8888: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 4);

/// `PLANE_CTL_FORMAT_P012` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_P012: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 5);

/// `PLANE_CTL_FORMAT_XRGB_16161616F` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_XRGB_16161616F: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 6);

/// `PLANE_CTL_FORMAT_P016` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_P016: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 7);

/// `PLANE_CTL_FORMAT_XYUV` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_XYUV: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 8);

/// `PLANE_CTL_FORMAT_INDEXED` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_INDEXED: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 12);

/// `PLANE_CTL_FORMAT_RGB_565` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_RGB_565: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_SKL, 14);

/// `PLANE_CTL_FORMAT_Y210` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y210: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 1);

/// `PLANE_CTL_FORMAT_Y212` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y212: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 3);

/// `PLANE_CTL_FORMAT_Y216` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y216: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 5);

/// `PLANE_CTL_FORMAT_Y410` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y410: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 7);

/// `PLANE_CTL_FORMAT_Y412` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y412: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 9);

/// `PLANE_CTL_FORMAT_Y416` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FORMAT_Y416: u32 = reg_field_prep(PLANE_CTL_FORMAT_MASK_ICL, 11);

/// `PLANE_CTL_PIPE_CSC_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_PIPE_CSC_ENABLE: u32 = reg_bit(23);

/// `PLANE_CTL_KEY_ENABLE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_KEY_ENABLE_MASK: u32 = reg_genmask(22, 21);

/// `PLANE_CTL_KEY_ENABLE_SOURCE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_KEY_ENABLE_SOURCE: u32 = reg_field_prep(PLANE_CTL_KEY_ENABLE_MASK, 1);

/// `PLANE_CTL_KEY_ENABLE_DESTINATION` (skl_universal_plane_regs.h)
pub const PLANE_CTL_KEY_ENABLE_DESTINATION: u32 = reg_field_prep(PLANE_CTL_KEY_ENABLE_MASK, 2);

/// `PLANE_CTL_ORDER_RGBX` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ORDER_RGBX: u32 = reg_bit(20);

/// `PLANE_CTL_YUV420_Y_PLANE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV420_Y_PLANE: u32 = reg_bit(19);

/// `PLANE_CTL_YUV_TO_RGB_CSC_FORMAT_BT709` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV_TO_RGB_CSC_FORMAT_BT709: u32 = reg_bit(18);

/// `PLANE_CTL_YUV422_ORDER_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV422_ORDER_MASK: u32 = reg_genmask(17, 16);

/// `PLANE_CTL_YUV422_ORDER_YUYV` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV422_ORDER_YUYV: u32 = reg_field_prep(PLANE_CTL_YUV422_ORDER_MASK, 0);

/// `PLANE_CTL_YUV422_ORDER_UYVY` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV422_ORDER_UYVY: u32 = reg_field_prep(PLANE_CTL_YUV422_ORDER_MASK, 1);

/// `PLANE_CTL_YUV422_ORDER_YVYU` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV422_ORDER_YVYU: u32 = reg_field_prep(PLANE_CTL_YUV422_ORDER_MASK, 2);

/// `PLANE_CTL_YUV422_ORDER_VYUY` (skl_universal_plane_regs.h)
pub const PLANE_CTL_YUV422_ORDER_VYUY: u32 = reg_field_prep(PLANE_CTL_YUV422_ORDER_MASK, 3);

/// `PLANE_CTL_RENDER_DECOMPRESSION_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_RENDER_DECOMPRESSION_ENABLE: u32 = reg_bit(15);

/// `PLANE_CTL_TRICKLE_FEED_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TRICKLE_FEED_DISABLE: u32 = reg_bit(14);

/// `PLANE_CTL_CLEAR_COLOR_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_CLEAR_COLOR_DISABLE: u32 = reg_bit(13);

/// `PLANE_CTL_PLANE_GAMMA_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_PLANE_GAMMA_DISABLE: u32 = reg_bit(13);

/// `PLANE_CTL_TILED_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_MASK: u32 = reg_genmask(12, 10);

/// `PLANE_CTL_TILED_LINEAR` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_LINEAR: u32 = reg_field_prep(PLANE_CTL_TILED_MASK, 0);

/// `PLANE_CTL_TILED_X` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_X: u32 = reg_field_prep(PLANE_CTL_TILED_MASK, 1);

/// `PLANE_CTL_TILED_Y` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_Y: u32 = reg_field_prep(PLANE_CTL_TILED_MASK, 4);

/// `PLANE_CTL_TILED_YF` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_YF: u32 = reg_field_prep(PLANE_CTL_TILED_MASK, 5);

/// `PLANE_CTL_TILED_4` (skl_universal_plane_regs.h)
pub const PLANE_CTL_TILED_4: u32 = reg_field_prep(PLANE_CTL_TILED_MASK, 5);

/// `PLANE_CTL_ASYNC_FLIP` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ASYNC_FLIP: u32 = reg_bit(9);

/// `PLANE_CTL_FLIP_HORIZONTAL` (skl_universal_plane_regs.h)
pub const PLANE_CTL_FLIP_HORIZONTAL: u32 = reg_bit(8);

/// `PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_MEDIA_DECOMPRESSION_ENABLE: u32 = reg_bit(4);

/// `PLANE_CTL_ALPHA_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ALPHA_MASK: u32 = reg_genmask(5, 4);

/// `PLANE_CTL_ALPHA_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ALPHA_DISABLE: u32 = reg_field_prep(PLANE_CTL_ALPHA_MASK, 0);

/// `PLANE_CTL_ALPHA_SW_PREMULTIPLY` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ALPHA_SW_PREMULTIPLY: u32 = reg_field_prep(PLANE_CTL_ALPHA_MASK, 2);

/// `PLANE_CTL_ALPHA_HW_PREMULTIPLY` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ALPHA_HW_PREMULTIPLY: u32 = reg_field_prep(PLANE_CTL_ALPHA_MASK, 3);

/// `PLANE_CTL_ROTATE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ROTATE_MASK: u32 = reg_genmask(1, 0);

/// `PLANE_CTL_ROTATE_0` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ROTATE_0: u32 = reg_field_prep(PLANE_CTL_ROTATE_MASK, 0);

/// `PLANE_CTL_ROTATE_90` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ROTATE_90: u32 = reg_field_prep(PLANE_CTL_ROTATE_MASK, 1);

/// `PLANE_CTL_ROTATE_180` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ROTATE_180: u32 = reg_field_prep(PLANE_CTL_ROTATE_MASK, 2);

/// `PLANE_CTL_ROTATE_270` (skl_universal_plane_regs.h)
pub const PLANE_CTL_ROTATE_270: u32 = reg_field_prep(PLANE_CTL_ROTATE_MASK, 3);

/// `_PLANE_STRIDE_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_STRIDE_1_A: u32 = 0x70188;

/// `_PLANE_STRIDE_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_STRIDE_2_A: u32 = 0x70288;

/// `_PLANE_STRIDE_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_STRIDE_1_B: u32 = 0x71188;

/// `_PLANE_STRIDE_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_STRIDE_2_B: u32 = 0x71288;

/// `PLANE_STRIDE(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_STRIDE(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_STRIDE_1_A, _PLANE_STRIDE_1_B, _PLANE_STRIDE_2_A, _PLANE_STRIDE_2_B)
}

/// `PLANE_STRIDE__MASK` (skl_universal_plane_regs.h)
pub const PLANE_STRIDE__MASK: u32 = reg_genmask(11, 0);

/// `PLANE_STRIDE_(stride)` (skl_universal_plane_regs.h)
pub const fn PLANE_STRIDE_(stride: u32) -> u32 {
    reg_field_prep(PLANE_STRIDE__MASK, (stride))
}

/// `_PLANE_POS_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POS_1_A: u32 = 0x7018c;

/// `_PLANE_POS_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POS_2_A: u32 = 0x7028c;

/// `_PLANE_POS_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POS_1_B: u32 = 0x7118c;

/// `_PLANE_POS_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POS_2_B: u32 = 0x7128c;

/// `PLANE_POS(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_POS(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_POS_1_A, _PLANE_POS_1_B, _PLANE_POS_2_A, _PLANE_POS_2_B)
}

/// `PLANE_POS_Y_MASK` (skl_universal_plane_regs.h)
pub const PLANE_POS_Y_MASK: u32 = reg_genmask(31, 16);

/// `PLANE_POS_Y(y)` (skl_universal_plane_regs.h)
pub const fn PLANE_POS_Y(y: u32) -> u32 {
    reg_field_prep(PLANE_POS_Y_MASK, (y))
}

/// `PLANE_POS_X_MASK` (skl_universal_plane_regs.h)
pub const PLANE_POS_X_MASK: u32 = reg_genmask(15, 0);

/// `PLANE_POS_X(x)` (skl_universal_plane_regs.h)
pub const fn PLANE_POS_X(x: u32) -> u32 {
    reg_field_prep(PLANE_POS_X_MASK, (x))
}

/// `_PLANE_SIZE_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_SIZE_1_A: u32 = 0x70190;

/// `_PLANE_SIZE_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_SIZE_2_A: u32 = 0x70290;

/// `_PLANE_SIZE_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_SIZE_1_B: u32 = 0x71190;

/// `_PLANE_SIZE_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_SIZE_2_B: u32 = 0x71290;

/// `PLANE_SIZE(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_SIZE(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_SIZE_1_A, _PLANE_SIZE_1_B, _PLANE_SIZE_2_A, _PLANE_SIZE_2_B)
}

/// `PLANE_HEIGHT_MASK` (skl_universal_plane_regs.h)
pub const PLANE_HEIGHT_MASK: u32 = reg_genmask(31, 16);

/// `PLANE_HEIGHT(h)` (skl_universal_plane_regs.h)
pub const fn PLANE_HEIGHT(h: u32) -> u32 {
    reg_field_prep(PLANE_HEIGHT_MASK, (h))
}

/// `PLANE_WIDTH_MASK` (skl_universal_plane_regs.h)
pub const PLANE_WIDTH_MASK: u32 = reg_genmask(15, 0);

/// `PLANE_WIDTH(w)` (skl_universal_plane_regs.h)
pub const fn PLANE_WIDTH(w: u32) -> u32 {
    reg_field_prep(PLANE_WIDTH_MASK, (w))
}

/// `_PLANE_KEYVAL_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYVAL_1_A: u32 = 0x70194;

/// `_PLANE_KEYVAL_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYVAL_2_A: u32 = 0x70294;

/// `_PLANE_KEYVAL_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYVAL_1_B: u32 = 0x71194;

/// `_PLANE_KEYVAL_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYVAL_2_B: u32 = 0x71294;

/// `PLANE_KEYVAL(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_KEYVAL(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_KEYVAL_1_A, _PLANE_KEYVAL_1_B, _PLANE_KEYVAL_2_A, _PLANE_KEYVAL_2_B)
}

/// `_PLANE_KEYMSK_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMSK_1_A: u32 = 0x70198;

/// `_PLANE_KEYMSK_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMSK_2_A: u32 = 0x70298;

/// `_PLANE_KEYMSK_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMSK_1_B: u32 = 0x71198;

/// `_PLANE_KEYMSK_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMSK_2_B: u32 = 0x71298;

/// `PLANE_KEYMSK(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_KEYMSK(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_KEYMSK_1_A, _PLANE_KEYMSK_1_B, _PLANE_KEYMSK_2_A, _PLANE_KEYMSK_2_B)
}

/// `PLANE_KEYMSK_ALPHA_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_KEYMSK_ALPHA_ENABLE: u32 = reg_bit(31);

/// `_PLANE_SURF_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_SURF_1_A: u32 = 0x7019c;

/// `_PLANE_SURF_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_SURF_2_A: u32 = 0x7029c;

/// `_PLANE_SURF_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_SURF_1_B: u32 = 0x7119c;

/// `_PLANE_SURF_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_SURF_2_B: u32 = 0x7129c;

/// `PLANE_SURF(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_SURF(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_SURF_1_A, _PLANE_SURF_1_B, _PLANE_SURF_2_A, _PLANE_SURF_2_B)
}

/// `PLANE_SURF_ADDR_MASK` (skl_universal_plane_regs.h)
pub const PLANE_SURF_ADDR_MASK: u32 = reg_genmask(31, 12);

/// `PLANE_SURF_DECRYPT` (skl_universal_plane_regs.h)
pub const PLANE_SURF_DECRYPT: u32 = reg_bit(2);

/// `PLANE_SURF_ASYNC_UPDATE` (skl_universal_plane_regs.h)
pub const PLANE_SURF_ASYNC_UPDATE: u32 = reg_bit(0);

/// `_PLANE_KEYMAX_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMAX_1_A: u32 = 0x701a0;

/// `_PLANE_KEYMAX_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMAX_2_A: u32 = 0x702a0;

/// `_PLANE_KEYMAX_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMAX_1_B: u32 = 0x711a0;

/// `_PLANE_KEYMAX_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_KEYMAX_2_B: u32 = 0x712a0;

/// `PLANE_KEYMAX(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_KEYMAX(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_KEYMAX_1_A, _PLANE_KEYMAX_1_B, _PLANE_KEYMAX_2_A, _PLANE_KEYMAX_2_B)
}

/// `PLANE_KEYMAX_ALPHA_MASK` (skl_universal_plane_regs.h)
pub const PLANE_KEYMAX_ALPHA_MASK: u32 = reg_genmask(31, 24);

/// `PLANE_KEYMAX_ALPHA(a)` (skl_universal_plane_regs.h)
pub const fn PLANE_KEYMAX_ALPHA(a: u32) -> u32 {
    reg_field_prep(PLANE_KEYMAX_ALPHA_MASK, (a))
}

/// `_PLANE_OFFSET_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_OFFSET_1_A: u32 = 0x701a4;

/// `_PLANE_OFFSET_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_OFFSET_2_A: u32 = 0x702a4;

/// `_PLANE_OFFSET_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_OFFSET_1_B: u32 = 0x711a4;

/// `_PLANE_OFFSET_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_OFFSET_2_B: u32 = 0x712a4;

/// `PLANE_OFFSET(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_OFFSET(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_OFFSET_1_A, _PLANE_OFFSET_1_B, _PLANE_OFFSET_2_A, _PLANE_OFFSET_2_B)
}

/// `PLANE_OFFSET_Y_MASK` (skl_universal_plane_regs.h)
pub const PLANE_OFFSET_Y_MASK: u32 = reg_genmask(31, 16);

/// `PLANE_OFFSET_Y(y)` (skl_universal_plane_regs.h)
pub const fn PLANE_OFFSET_Y(y: u32) -> u32 {
    reg_field_prep(PLANE_OFFSET_Y_MASK, (y))
}

/// `PLANE_OFFSET_X_MASK` (skl_universal_plane_regs.h)
pub const PLANE_OFFSET_X_MASK: u32 = reg_genmask(15, 0);

/// `PLANE_OFFSET_X(x)` (skl_universal_plane_regs.h)
pub const fn PLANE_OFFSET_X(x: u32) -> u32 {
    reg_field_prep(PLANE_OFFSET_X_MASK, (x))
}

/// `_PLANE_SURFLIVE_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_SURFLIVE_1_A: u32 = 0x701ac;

/// `_PLANE_SURFLIVE_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_SURFLIVE_2_A: u32 = 0x702ac;

/// `_PLANE_SURFLIVE_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_SURFLIVE_1_B: u32 = 0x711ac;

/// `_PLANE_SURFLIVE_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_SURFLIVE_2_B: u32 = 0x712ac;

/// `PLANE_SURFLIVE(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_SURFLIVE(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_SURFLIVE_1_A, _PLANE_SURFLIVE_1_B, _PLANE_SURFLIVE_2_A, _PLANE_SURFLIVE_2_B)
}

/// `_PLANE_CC_VAL_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CC_VAL_1_A: u32 = 0x701b4;

/// `_PLANE_CC_VAL_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CC_VAL_2_A: u32 = 0x702b4;

/// `_PLANE_CC_VAL_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CC_VAL_1_B: u32 = 0x711b4;

/// `_PLANE_CC_VAL_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CC_VAL_2_B: u32 = 0x712b4;

/// `PLANE_CC_VAL(pipe, plane, dw)` (skl_universal_plane_regs.h)
pub const fn PLANE_CC_VAL(pipe: u32, plane: u32, dw: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (dw), _PLANE_CC_VAL_1_A, _PLANE_CC_VAL_1_B, _PLANE_CC_VAL_2_A, _PLANE_CC_VAL_2_B)
}

/// `_PLANE_AUX_DIST_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_DIST_1_A: u32 = 0x701c0;

/// `_PLANE_AUX_DIST_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_DIST_2_A: u32 = 0x702c0;

/// `_PLANE_AUX_DIST_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_DIST_1_B: u32 = 0x711c0;

/// `_PLANE_AUX_DIST_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_DIST_2_B: u32 = 0x712c0;

/// `PLANE_AUX_DIST(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_AUX_DIST(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_AUX_DIST_1_A, _PLANE_AUX_DIST_1_B, _PLANE_AUX_DIST_2_A, _PLANE_AUX_DIST_2_B)
}

/// `PLANE_AUX_DISTANCE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_AUX_DISTANCE_MASK: u32 = reg_genmask(31, 12);

/// `PLANE_AUX_STRIDE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_AUX_STRIDE_MASK: u32 = reg_genmask(11, 0);

/// `PLANE_AUX_STRIDE(stride)` (skl_universal_plane_regs.h)
pub const fn PLANE_AUX_STRIDE(stride: u32) -> u32 {
    reg_field_prep(PLANE_AUX_STRIDE_MASK, (stride))
}

/// `_PLANE_AUX_OFFSET_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_OFFSET_1_A: u32 = 0x701c4;

/// `_PLANE_AUX_OFFSET_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_OFFSET_2_A: u32 = 0x702c4;

/// `_PLANE_AUX_OFFSET_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_OFFSET_1_B: u32 = 0x711c4;

/// `_PLANE_AUX_OFFSET_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_AUX_OFFSET_2_B: u32 = 0x712c4;

/// `PLANE_AUX_OFFSET(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_AUX_OFFSET(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_AUX_OFFSET_1_A, _PLANE_AUX_OFFSET_1_B, _PLANE_AUX_OFFSET_2_A, _PLANE_AUX_OFFSET_2_B)
}

/// `_PLANE_CUS_CTL_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CUS_CTL_1_A: u32 = 0x701c8;

/// `_PLANE_CUS_CTL_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CUS_CTL_2_A: u32 = 0x702c8;

/// `_PLANE_CUS_CTL_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CUS_CTL_1_B: u32 = 0x711c8;

/// `_PLANE_CUS_CTL_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CUS_CTL_2_B: u32 = 0x712c8;

/// `PLANE_CUS_CTL(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_CUS_CTL(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_CUS_CTL_1_A, _PLANE_CUS_CTL_1_B, _PLANE_CUS_CTL_2_A, _PLANE_CUS_CTL_2_B)
}

/// `PLANE_CUS_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_CUS_ENABLE: u32 = reg_bit(31);

/// `PLANE_CUS_Y_PLANE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CUS_Y_PLANE_MASK: u32 = reg_bit(30);

/// `PLANE_CUS_Y_PLANE_4_RKL` (skl_universal_plane_regs.h)
pub const PLANE_CUS_Y_PLANE_4_RKL: u32 = reg_field_prep(PLANE_CUS_Y_PLANE_MASK, 0);

/// `PLANE_CUS_Y_PLANE_5_RKL` (skl_universal_plane_regs.h)
pub const PLANE_CUS_Y_PLANE_5_RKL: u32 = reg_field_prep(PLANE_CUS_Y_PLANE_MASK, 1);

/// `PLANE_CUS_Y_PLANE_6_ICL` (skl_universal_plane_regs.h)
pub const PLANE_CUS_Y_PLANE_6_ICL: u32 = reg_field_prep(PLANE_CUS_Y_PLANE_MASK, 0);

/// `PLANE_CUS_Y_PLANE_7_ICL` (skl_universal_plane_regs.h)
pub const PLANE_CUS_Y_PLANE_7_ICL: u32 = reg_field_prep(PLANE_CUS_Y_PLANE_MASK, 1);

/// `PLANE_CUS_HPHASE_SIGN_NEGATIVE` (skl_universal_plane_regs.h)
pub const PLANE_CUS_HPHASE_SIGN_NEGATIVE: u32 = reg_bit(19);

/// `PLANE_CUS_HPHASE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CUS_HPHASE_MASK: u32 = reg_genmask(17, 16);

/// `PLANE_CUS_HPHASE_0` (skl_universal_plane_regs.h)
pub const PLANE_CUS_HPHASE_0: u32 = reg_field_prep(PLANE_CUS_HPHASE_MASK, 0);

/// `PLANE_CUS_HPHASE_0_25` (skl_universal_plane_regs.h)
pub const PLANE_CUS_HPHASE_0_25: u32 = reg_field_prep(PLANE_CUS_HPHASE_MASK, 1);

/// `PLANE_CUS_HPHASE_0_5` (skl_universal_plane_regs.h)
pub const PLANE_CUS_HPHASE_0_5: u32 = reg_field_prep(PLANE_CUS_HPHASE_MASK, 2);

/// `PLANE_CUS_VPHASE_SIGN_NEGATIVE` (skl_universal_plane_regs.h)
pub const PLANE_CUS_VPHASE_SIGN_NEGATIVE: u32 = reg_bit(15);

/// `PLANE_CUS_VPHASE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_CUS_VPHASE_MASK: u32 = reg_genmask(13, 12);

/// `PLANE_CUS_VPHASE_0` (skl_universal_plane_regs.h)
pub const PLANE_CUS_VPHASE_0: u32 = reg_field_prep(PLANE_CUS_VPHASE_MASK, 0);

/// `PLANE_CUS_VPHASE_0_25` (skl_universal_plane_regs.h)
pub const PLANE_CUS_VPHASE_0_25: u32 = reg_field_prep(PLANE_CUS_VPHASE_MASK, 1);

/// `PLANE_CUS_VPHASE_0_5` (skl_universal_plane_regs.h)
pub const PLANE_CUS_VPHASE_0_5: u32 = reg_field_prep(PLANE_CUS_VPHASE_MASK, 2);

/// `_PLANE_COLOR_CTL_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_COLOR_CTL_1_A: u32 = 0x701cc;

/// `_PLANE_COLOR_CTL_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_COLOR_CTL_2_A: u32 = 0x702cc;

/// `_PLANE_COLOR_CTL_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_COLOR_CTL_1_B: u32 = 0x711cc;

/// `_PLANE_COLOR_CTL_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_COLOR_CTL_2_B: u32 = 0x712cc;

/// `PLANE_COLOR_CTL(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_COLOR_CTL(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_COLOR_CTL_1_A, _PLANE_COLOR_CTL_1_B, _PLANE_COLOR_CTL_2_A, _PLANE_COLOR_CTL_2_B)
}

/// `PLANE_COLOR_PIPE_GAMMA_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_PIPE_GAMMA_ENABLE: u32 = reg_bit(30);

/// `PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_YUV_RANGE_CORRECTION_DISABLE: u32 = reg_bit(28);

/// `PLANE_COLOR_PIPE_CSC_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_PIPE_CSC_ENABLE: u32 = reg_bit(23);

/// `PLANE_COLOR_PLANE_CSC_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_PLANE_CSC_ENABLE: u32 = reg_bit(21);

/// `PLANE_COLOR_INPUT_CSC_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_INPUT_CSC_ENABLE: u32 = reg_bit(20);

/// `PLANE_COLOR_POST_CSC_GAMMA_MULTSEG_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_POST_CSC_GAMMA_MULTSEG_ENABLE: u32 = reg_bit(15);

/// `PLANE_COLOR_PRE_CSC_GAMMA_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_PRE_CSC_GAMMA_ENABLE: u32 = reg_bit(14);

/// `PLANE_COLOR_CSC_MODE_MASK` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_MASK: u32 = reg_genmask(19, 17);

/// `PLANE_COLOR_CSC_MODE_BYPASS` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_BYPASS: u32 = reg_field_prep(PLANE_COLOR_CSC_MODE_MASK, 0);

/// `PLANE_COLOR_CSC_MODE_YUV601_TO_RGB601` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_YUV601_TO_RGB601: u32 = reg_field_prep(PLANE_COLOR_CSC_MODE_MASK, 1);

/// `PLANE_COLOR_CSC_MODE_YUV709_TO_RGB709` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_YUV709_TO_RGB709: u32 = reg_field_prep(PLANE_COLOR_CSC_MODE_MASK, 2);

/// `PLANE_COLOR_CSC_MODE_YUV2020_TO_RGB2020` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_YUV2020_TO_RGB2020: u32 = reg_field_prep(PLANE_COLOR_CSC_MODE_MASK, 3);

/// `PLANE_COLOR_CSC_MODE_RGB709_TO_RGB2020` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_CSC_MODE_RGB709_TO_RGB2020: u32 = reg_field_prep(PLANE_COLOR_CSC_MODE_MASK, 4);

/// `PLANE_COLOR_PLANE_GAMMA_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_PLANE_GAMMA_DISABLE: u32 = reg_bit(13);

/// `PLANE_COLOR_ALPHA_MASK` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_ALPHA_MASK: u32 = reg_genmask(5, 4);

/// `PLANE_COLOR_ALPHA_DISABLE` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_ALPHA_DISABLE: u32 = reg_field_prep(PLANE_COLOR_ALPHA_MASK, 0);

/// `PLANE_COLOR_ALPHA_SW_PREMULTIPLY` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_ALPHA_SW_PREMULTIPLY: u32 = reg_field_prep(PLANE_COLOR_ALPHA_MASK, 2);

/// `PLANE_COLOR_ALPHA_HW_PREMULTIPLY` (skl_universal_plane_regs.h)
pub const PLANE_COLOR_ALPHA_HW_PREMULTIPLY: u32 = reg_field_prep(PLANE_COLOR_ALPHA_MASK, 3);

/// `_PLANE_INPUT_CSC_RY_GY_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_RY_GY_1_A: u32 = 0x701e0;

/// `_PLANE_INPUT_CSC_RY_GY_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_RY_GY_2_A: u32 = 0x702e0;

/// `_PLANE_INPUT_CSC_RY_GY_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_RY_GY_1_B: u32 = 0x711e0;

/// `_PLANE_INPUT_CSC_RY_GY_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_RY_GY_2_B: u32 = 0x712e0;

/// `PLANE_INPUT_CSC_COEFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_INPUT_CSC_COEFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_INPUT_CSC_RY_GY_1_A, _PLANE_INPUT_CSC_RY_GY_1_B, _PLANE_INPUT_CSC_RY_GY_2_A, _PLANE_INPUT_CSC_RY_GY_2_B)
}

/// `_PLANE_INPUT_CSC_PREOFF_HI_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_PREOFF_HI_1_A: u32 = 0x701f8;

/// `_PLANE_INPUT_CSC_PREOFF_HI_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_PREOFF_HI_2_A: u32 = 0x702f8;

/// `_PLANE_INPUT_CSC_PREOFF_HI_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_PREOFF_HI_1_B: u32 = 0x711f8;

/// `_PLANE_INPUT_CSC_PREOFF_HI_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_PREOFF_HI_2_B: u32 = 0x712f8;

/// `PLANE_INPUT_CSC_PREOFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_INPUT_CSC_PREOFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_INPUT_CSC_PREOFF_HI_1_A, _PLANE_INPUT_CSC_PREOFF_HI_1_B, _PLANE_INPUT_CSC_PREOFF_HI_2_A, _PLANE_INPUT_CSC_PREOFF_HI_2_B)
}

/// `_PLANE_INPUT_CSC_POSTOFF_HI_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_POSTOFF_HI_1_A: u32 = 0x70204;

/// `_PLANE_INPUT_CSC_POSTOFF_HI_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_POSTOFF_HI_2_A: u32 = 0x70304;

/// `_PLANE_INPUT_CSC_POSTOFF_HI_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_POSTOFF_HI_1_B: u32 = 0x71204;

/// `_PLANE_INPUT_CSC_POSTOFF_HI_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_INPUT_CSC_POSTOFF_HI_2_B: u32 = 0x71304;

/// `PLANE_INPUT_CSC_POSTOFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_INPUT_CSC_POSTOFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_INPUT_CSC_POSTOFF_HI_1_A, _PLANE_INPUT_CSC_POSTOFF_HI_1_B, _PLANE_INPUT_CSC_POSTOFF_HI_2_A, _PLANE_INPUT_CSC_POSTOFF_HI_2_B)
}

/// `_MMIO_PLANE_GAMC(plane, i, a, b)` (skl_universal_plane_regs.h)
pub const fn _MMIO_PLANE_GAMC(plane: u32, i: u32, a: u32, b: u32) -> I915RegT {
    mmio((_PIPE(plane, a, b) + ((i) * 4)))
}

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_A: u32 = 0x70160;

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_B: u32 = 0x71160;

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_A: u32 = 0x70260;

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_B: u32 = 0x71260;

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_A, _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1_B)
}

/// `_PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_A, _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2_B)
}

/// `PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_1(pipe), _PLANE_POST_CSC_GAMC_SEG0_INDEX_ENH_2(pipe))
}

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_A: u32 = 0x70164;

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_B: u32 = 0x71164;

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_A: u32 = 0x70264;

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_B: u32 = 0x71264;

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_A, _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1_B)
}

/// `_PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_A, _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2_B)
}

/// `PLANE_POST_CSC_GAMC_SEG0_DATA_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_SEG0_DATA_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_1(pipe), _PLANE_POST_CSC_GAMC_SEG0_DATA_ENH_2(pipe))
}

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_ENH_1_A: u32 = 0x701d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_ENH_1_B: u32 = 0x711d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_ENH_2_A: u32 = 0x702d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_ENH_2_B: u32 = 0x712d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_INDEX_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_INDEX_ENH_1_A, _PLANE_POST_CSC_GAMC_INDEX_ENH_1_B)
}

/// `_PLANE_POST_CSC_GAMC_INDEX_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_INDEX_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_INDEX_ENH_2_A, _PLANE_POST_CSC_GAMC_INDEX_ENH_2_B)
}

/// `PLANE_POST_CSC_GAMC_INDEX_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_INDEX_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_INDEX_ENH_1(pipe), _PLANE_POST_CSC_GAMC_INDEX_ENH_2(pipe))
}

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_ENH_1_A: u32 = 0x701dc;

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_ENH_1_B: u32 = 0x711dc;

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_ENH_2_A: u32 = 0x702dc;

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_ENH_2_B: u32 = 0x712dc;

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_DATA_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_DATA_ENH_1_A, _PLANE_POST_CSC_GAMC_DATA_ENH_1_B)
}

/// `_PLANE_POST_CSC_GAMC_DATA_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_DATA_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_DATA_ENH_2_A, _PLANE_POST_CSC_GAMC_DATA_ENH_2_B)
}

/// `PLANE_POST_CSC_GAMC_DATA_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_DATA_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_DATA_ENH_1(pipe), _PLANE_POST_CSC_GAMC_DATA_ENH_2(pipe))
}

/// `_PLANE_POST_CSC_GAMC_INDEX_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_1_A: u32 = 0x704d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_1_B: u32 = 0x714d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_2_A: u32 = 0x705d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_INDEX_2_B: u32 = 0x715d8;

/// `_PLANE_POST_CSC_GAMC_INDEX_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_INDEX_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_INDEX_1_A, _PLANE_POST_CSC_GAMC_INDEX_1_B)
}

/// `_PLANE_POST_CSC_GAMC_INDEX_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_INDEX_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_INDEX_2_A, _PLANE_POST_CSC_GAMC_INDEX_2_B)
}

/// `PLANE_POST_CSC_GAMC_INDEX(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_INDEX(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_INDEX_1(pipe), _PLANE_POST_CSC_GAMC_INDEX_2(pipe))
}

/// `_PLANE_POST_CSC_GAMC_DATA_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_1_A: u32 = 0x704dc;

/// `_PLANE_POST_CSC_GAMC_DATA_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_1_B: u32 = 0x714dc;

/// `_PLANE_POST_CSC_GAMC_DATA_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_2_A: u32 = 0x705dc;

/// `_PLANE_POST_CSC_GAMC_DATA_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_POST_CSC_GAMC_DATA_2_B: u32 = 0x715dc;

/// `_PLANE_POST_CSC_GAMC_DATA_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_DATA_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_DATA_1_A, _PLANE_POST_CSC_GAMC_DATA_1_B)
}

/// `_PLANE_POST_CSC_GAMC_DATA_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_POST_CSC_GAMC_DATA_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_POST_CSC_GAMC_DATA_2_A, _PLANE_POST_CSC_GAMC_DATA_2_B)
}

/// `PLANE_POST_CSC_GAMC_DATA(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_POST_CSC_GAMC_DATA(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_POST_CSC_GAMC_DATA_1(pipe), _PLANE_POST_CSC_GAMC_DATA_2(pipe))
}

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_ENH_1_A: u32 = 0x701d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_ENH_1_B: u32 = 0x711d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_ENH_2_A: u32 = 0x702d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_ENH_2_B: u32 = 0x712d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_INDEX_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_INDEX_ENH_1_A, _PLANE_PRE_CSC_GAMC_INDEX_ENH_1_B)
}

/// `_PLANE_PRE_CSC_GAMC_INDEX_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_INDEX_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_INDEX_ENH_2_A, _PLANE_PRE_CSC_GAMC_INDEX_ENH_2_B)
}

/// `PLANE_PRE_CSC_GAMC_INDEX_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_PRE_CSC_GAMC_INDEX_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_PRE_CSC_GAMC_INDEX_ENH_1(pipe), _PLANE_PRE_CSC_GAMC_INDEX_ENH_2(pipe))
}

/// `PLANE_PAL_PREC_AUTO_INCREMENT` (skl_universal_plane_regs.h)
pub const PLANE_PAL_PREC_AUTO_INCREMENT: u32 = reg_bit(10);

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_ENH_1_A: u32 = 0x701d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_ENH_1_B: u32 = 0x711d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_ENH_2_A: u32 = 0x702d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_ENH_2_B: u32 = 0x712d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_DATA_ENH_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_DATA_ENH_1_A, _PLANE_PRE_CSC_GAMC_DATA_ENH_1_B)
}

/// `_PLANE_PRE_CSC_GAMC_DATA_ENH_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_DATA_ENH_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_DATA_ENH_2_A, _PLANE_PRE_CSC_GAMC_DATA_ENH_2_B)
}

/// `PLANE_PRE_CSC_GAMC_DATA_ENH(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_PRE_CSC_GAMC_DATA_ENH(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_PRE_CSC_GAMC_DATA_ENH_1(pipe), _PLANE_PRE_CSC_GAMC_DATA_ENH_2(pipe))
}

/// `_PLANE_PRE_CSC_GAMC_INDEX_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_1_A: u32 = 0x704d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_1_B: u32 = 0x714d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_2_A: u32 = 0x705d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_INDEX_2_B: u32 = 0x715d0;

/// `_PLANE_PRE_CSC_GAMC_INDEX_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_INDEX_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_INDEX_1_A, _PLANE_PRE_CSC_GAMC_INDEX_1_B)
}

/// `_PLANE_PRE_CSC_GAMC_INDEX_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_INDEX_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_INDEX_2_A, _PLANE_PRE_CSC_GAMC_INDEX_2_B)
}

/// `PLANE_PRE_CSC_GAMC_INDEX(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_PRE_CSC_GAMC_INDEX(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_PRE_CSC_GAMC_INDEX_1(pipe), _PLANE_PRE_CSC_GAMC_INDEX_2(pipe))
}

/// `_PLANE_PRE_CSC_GAMC_DATA_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_1_A: u32 = 0x704d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_1_B: u32 = 0x714d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_2_A: u32 = 0x705d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_PRE_CSC_GAMC_DATA_2_B: u32 = 0x715d4;

/// `_PLANE_PRE_CSC_GAMC_DATA_1(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_DATA_1(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_DATA_1_A, _PLANE_PRE_CSC_GAMC_DATA_1_B)
}

/// `_PLANE_PRE_CSC_GAMC_DATA_2(pipe)` (skl_universal_plane_regs.h)
pub const fn _PLANE_PRE_CSC_GAMC_DATA_2(pipe: u32) -> u32 {
    _PIPE(pipe, _PLANE_PRE_CSC_GAMC_DATA_2_A, _PLANE_PRE_CSC_GAMC_DATA_2_B)
}

/// `PLANE_PRE_CSC_GAMC_DATA(pipe, plane, i)` (skl_universal_plane_regs.h)
pub const fn PLANE_PRE_CSC_GAMC_DATA(pipe: u32, plane: u32, i: u32) -> I915RegT {
    _MMIO_PLANE_GAMC(plane, i, _PLANE_PRE_CSC_GAMC_DATA_1(pipe), _PLANE_PRE_CSC_GAMC_DATA_2(pipe))
}

/// `_PLANE_CSC_RY_GY_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_RY_GY_1_A: u32 = 0x70210;

/// `_PLANE_CSC_RY_GY_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_RY_GY_2_A: u32 = 0x70310;

/// `_PLANE_CSC_RY_GY_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_RY_GY_1_B: u32 = 0x71210;

/// `_PLANE_CSC_RY_GY_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_RY_GY_2_B: u32 = 0x71310;

/// `PLANE_CSC_COEFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_CSC_COEFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_CSC_RY_GY_1_A, _PLANE_CSC_RY_GY_1_B, _PLANE_CSC_RY_GY_2_A, _PLANE_CSC_RY_GY_2_B)
}

/// `_PLANE_CSC_PREOFF_HI_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_PREOFF_HI_1_A: u32 = 0x70228;

/// `_PLANE_CSC_PREOFF_HI_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_PREOFF_HI_2_A: u32 = 0x70328;

/// `_PLANE_CSC_PREOFF_HI_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_PREOFF_HI_1_B: u32 = 0x71228;

/// `_PLANE_CSC_PREOFF_HI_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_PREOFF_HI_2_B: u32 = 0x71328;

/// `PLANE_CSC_PREOFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_CSC_PREOFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_CSC_PREOFF_HI_1_A, _PLANE_CSC_PREOFF_HI_1_B, _PLANE_CSC_PREOFF_HI_2_A, _PLANE_CSC_PREOFF_HI_2_B)
}

/// `_PLANE_CSC_POSTOFF_HI_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_POSTOFF_HI_1_A: u32 = 0x70234;

/// `_PLANE_CSC_POSTOFF_HI_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_POSTOFF_HI_2_A: u32 = 0x70334;

/// `_PLANE_CSC_POSTOFF_HI_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_POSTOFF_HI_1_B: u32 = 0x71234;

/// `_PLANE_CSC_POSTOFF_HI_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CSC_POSTOFF_HI_2_B: u32 = 0x71334;

/// `PLANE_CSC_POSTOFF(pipe, plane, index)` (skl_universal_plane_regs.h)
pub const fn PLANE_CSC_POSTOFF(pipe: u32, plane: u32, index: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (index), _PLANE_CSC_POSTOFF_HI_1_A, _PLANE_CSC_POSTOFF_HI_1_B, _PLANE_CSC_POSTOFF_HI_2_A, _PLANE_CSC_POSTOFF_HI_2_B)
}

/// `_PLANE_WM_1_A_0` (skl_universal_plane_regs.h)
pub const _PLANE_WM_1_A_0: u32 = 0x70240;

/// `_PLANE_WM_1_B_0` (skl_universal_plane_regs.h)
pub const _PLANE_WM_1_B_0: u32 = 0x71240;

/// `_PLANE_WM_2_A_0` (skl_universal_plane_regs.h)
pub const _PLANE_WM_2_A_0: u32 = 0x70340;

/// `_PLANE_WM_2_B_0` (skl_universal_plane_regs.h)
pub const _PLANE_WM_2_B_0: u32 = 0x71340;

/// `PLANE_WM(pipe, plane, level)` (skl_universal_plane_regs.h)
pub const fn PLANE_WM(pipe: u32, plane: u32, level: u32) -> I915RegT {
    _MMIO_SKL_PLANE_DW((pipe), (plane), (level), _PLANE_WM_1_A_0, _PLANE_WM_1_B_0, _PLANE_WM_2_A_0, _PLANE_WM_2_B_0)
}

/// `PLANE_WM_EN` (skl_universal_plane_regs.h)
pub const PLANE_WM_EN: u32 = reg_bit(31);

/// `PLANE_WM_IGNORE_LINES` (skl_universal_plane_regs.h)
pub const PLANE_WM_IGNORE_LINES: u32 = reg_bit(30);

/// `PLANE_WM_AUTO_MIN_ALLOC_EN` (skl_universal_plane_regs.h)
pub const PLANE_WM_AUTO_MIN_ALLOC_EN: u32 = reg_bit(29);

/// `PLANE_WM_LINES_MASK` (skl_universal_plane_regs.h)
pub const PLANE_WM_LINES_MASK: u32 = reg_genmask(26, 14);

/// `PLANE_WM_BLOCKS_MASK` (skl_universal_plane_regs.h)
pub const PLANE_WM_BLOCKS_MASK: u32 = reg_genmask(12, 0);

/// `_PLANE_WM_SAGV_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_1_A: u32 = 0x70258;

/// `_PLANE_WM_SAGV_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_1_B: u32 = 0x71258;

/// `_PLANE_WM_SAGV_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_2_A: u32 = 0x70358;

/// `_PLANE_WM_SAGV_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_2_B: u32 = 0x71358;

/// `PLANE_WM_SAGV(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_WM_SAGV(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_WM_SAGV_1_A, _PLANE_WM_SAGV_1_B, _PLANE_WM_SAGV_2_A, _PLANE_WM_SAGV_2_B)
}

/// `_PLANE_WM_SAGV_TRANS_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_TRANS_1_A: u32 = 0x7025c;

/// `_PLANE_WM_SAGV_TRANS_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_TRANS_1_B: u32 = 0x7125c;

/// `_PLANE_WM_SAGV_TRANS_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_TRANS_2_A: u32 = 0x7035c;

/// `_PLANE_WM_SAGV_TRANS_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_SAGV_TRANS_2_B: u32 = 0x7135c;

/// `PLANE_WM_SAGV_TRANS(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_WM_SAGV_TRANS(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_WM_SAGV_TRANS_1_A, _PLANE_WM_SAGV_TRANS_1_B, _PLANE_WM_SAGV_TRANS_2_A, _PLANE_WM_SAGV_TRANS_2_B)
}

/// `_PLANE_WM_TRANS_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_TRANS_1_A: u32 = 0x70268;

/// `_PLANE_WM_TRANS_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_TRANS_1_B: u32 = 0x71268;

/// `_PLANE_WM_TRANS_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_WM_TRANS_2_A: u32 = 0x70368;

/// `_PLANE_WM_TRANS_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_WM_TRANS_2_B: u32 = 0x71368;

/// `PLANE_WM_TRANS(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_WM_TRANS(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_WM_TRANS_1_A, _PLANE_WM_TRANS_1_B, _PLANE_WM_TRANS_2_A, _PLANE_WM_TRANS_2_B)
}

/// `_PLANE_CHICKEN_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_CHICKEN_1_A: u32 = 0x7026c;

/// `_PLANE_CHICKEN_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_CHICKEN_2_A: u32 = 0x7036c;

/// `_PLANE_CHICKEN_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_CHICKEN_1_B: u32 = 0x7126c;

/// `_PLANE_CHICKEN_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_CHICKEN_2_B: u32 = 0x7136c;

/// `PLANE_CHICKEN(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_CHICKEN(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_CHICKEN_1_A, _PLANE_CHICKEN_1_B, _PLANE_CHICKEN_2_A, _PLANE_CHICKEN_2_B)
}

/// `PLANE_CHICKEN_DISABLE_DPT` (skl_universal_plane_regs.h)
pub const PLANE_CHICKEN_DISABLE_DPT: u32 = reg_bit(19);

/// `_PLANE_NV12_BUF_CFG_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_NV12_BUF_CFG_1_A: u32 = 0x70278;

/// `_PLANE_NV12_BUF_CFG_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_NV12_BUF_CFG_2_A: u32 = 0x70378;

/// `_PLANE_NV12_BUF_CFG_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_NV12_BUF_CFG_1_B: u32 = 0x71278;

/// `_PLANE_NV12_BUF_CFG_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_NV12_BUF_CFG_2_B: u32 = 0x71378;

/// `PLANE_NV12_BUF_CFG(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_NV12_BUF_CFG(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_NV12_BUF_CFG_1_A, _PLANE_NV12_BUF_CFG_1_B, _PLANE_NV12_BUF_CFG_2_A, _PLANE_NV12_BUF_CFG_2_B)
}

/// `_PLANE_BUF_CFG_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_BUF_CFG_1_A: u32 = 0x7027c;

/// `_PLANE_BUF_CFG_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_BUF_CFG_2_A: u32 = 0x7037c;

/// `_PLANE_BUF_CFG_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_BUF_CFG_1_B: u32 = 0x7127c;

/// `_PLANE_BUF_CFG_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_BUF_CFG_2_B: u32 = 0x7137c;

/// `PLANE_BUF_CFG(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_BUF_CFG(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_BUF_CFG_1_A, _PLANE_BUF_CFG_1_B, _PLANE_BUF_CFG_2_A, _PLANE_BUF_CFG_2_B)
}

/// `PLANE_BUF_END_MASK` (skl_universal_plane_regs.h)
pub const PLANE_BUF_END_MASK: u32 = reg_genmask(28, 16);

/// `PLANE_BUF_END(end)` (skl_universal_plane_regs.h)
pub const fn PLANE_BUF_END(end: u32) -> u32 {
    reg_field_prep(PLANE_BUF_END_MASK, (end))
}

/// `PLANE_BUF_START_MASK` (skl_universal_plane_regs.h)
pub const PLANE_BUF_START_MASK: u32 = reg_genmask(12, 0);

/// `PLANE_BUF_START(start)` (skl_universal_plane_regs.h)
pub const fn PLANE_BUF_START(start: u32) -> u32 {
    reg_field_prep(PLANE_BUF_START_MASK, (start))
}

/// `_PLANE_MIN_BUF_CFG_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_MIN_BUF_CFG_1_A: u32 = 0x70274;

/// `_PLANE_MIN_BUF_CFG_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_MIN_BUF_CFG_2_A: u32 = 0x70374;

/// `_PLANE_MIN_BUF_CFG_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_MIN_BUF_CFG_1_B: u32 = 0x71274;

/// `_PLANE_MIN_BUF_CFG_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_MIN_BUF_CFG_2_B: u32 = 0x71374;

/// `PLANE_MIN_BUF_CFG(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_MIN_BUF_CFG(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_MIN_BUF_CFG_1_A, _PLANE_MIN_BUF_CFG_1_B, _PLANE_MIN_BUF_CFG_2_A, _PLANE_MIN_BUF_CFG_2_B)
}

/// `PLANE_AUTO_MIN_DBUF_EN` (skl_universal_plane_regs.h)
pub const PLANE_AUTO_MIN_DBUF_EN: u32 = reg_bit(31);

/// `PLANE_MIN_DBUF_BLOCKS_MASK` (skl_universal_plane_regs.h)
pub const PLANE_MIN_DBUF_BLOCKS_MASK: u32 = reg_genmask(28, 16);

/// `PLANE_MIN_DBUF_BLOCKS(val)` (skl_universal_plane_regs.h)
pub const fn PLANE_MIN_DBUF_BLOCKS(val: u32) -> u32 {
    reg_field_prep(PLANE_MIN_DBUF_BLOCKS_MASK, (val))
}

/// `PLANE_INTERIM_DBUF_BLOCKS_MASK` (skl_universal_plane_regs.h)
pub const PLANE_INTERIM_DBUF_BLOCKS_MASK: u32 = reg_genmask(12, 0);

/// `PLANE_INTERIM_DBUF_BLOCKS(val)` (skl_universal_plane_regs.h)
pub const fn PLANE_INTERIM_DBUF_BLOCKS(val: u32) -> u32 {
    reg_field_prep(PLANE_INTERIM_DBUF_BLOCKS_MASK, (val))
}

/// `_SEL_FETCH_PLANE_CTL_1_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_1_A: u32 = 0x70890;

/// `_SEL_FETCH_PLANE_CTL_2_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_2_A: u32 = 0x708b0;

/// `_SEL_FETCH_PLANE_CTL_5_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_5_A: u32 = 0x70920;

/// `_SEL_FETCH_PLANE_CTL_6_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_6_A: u32 = 0x70940;

/// `_SEL_FETCH_PLANE_CTL_1_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_1_B: u32 = 0x71890;

/// `_SEL_FETCH_PLANE_CTL_2_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_2_B: u32 = 0x718b0;

/// `_SEL_FETCH_PLANE_CTL_5_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_5_B: u32 = 0x71920;

/// `_SEL_FETCH_PLANE_CTL_6_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_CTL_6_B: u32 = 0x71940;

/// `SEL_FETCH_PLANE_CTL(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn SEL_FETCH_PLANE_CTL(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SEL_FETCH((pipe), (plane), _SEL_FETCH_PLANE_CTL_1_A, _SEL_FETCH_PLANE_CTL_1_B, _SEL_FETCH_PLANE_CTL_2_A, _SEL_FETCH_PLANE_CTL_2_B, _SEL_FETCH_PLANE_CTL_5_A, _SEL_FETCH_PLANE_CTL_5_B, _SEL_FETCH_PLANE_CTL_6_A, _SEL_FETCH_PLANE_CTL_6_B)
}

/// `SEL_FETCH_PLANE_CTL_ENABLE` (skl_universal_plane_regs.h)
pub const SEL_FETCH_PLANE_CTL_ENABLE: u32 = reg_bit(31);

/// `_SEL_FETCH_PLANE_POS_1_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_1_A: u32 = 0x70894;

/// `_SEL_FETCH_PLANE_POS_2_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_2_A: u32 = 0x708b4;

/// `_SEL_FETCH_PLANE_POS_5_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_5_A: u32 = 0x70924;

/// `_SEL_FETCH_PLANE_POS_6_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_6_A: u32 = 0x70944;

/// `_SEL_FETCH_PLANE_POS_1_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_1_B: u32 = 0x71894;

/// `_SEL_FETCH_PLANE_POS_2_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_2_B: u32 = 0x718b4;

/// `_SEL_FETCH_PLANE_POS_5_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_5_B: u32 = 0x71924;

/// `_SEL_FETCH_PLANE_POS_6_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_POS_6_B: u32 = 0x71944;

/// `SEL_FETCH_PLANE_POS(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn SEL_FETCH_PLANE_POS(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SEL_FETCH((pipe), (plane), _SEL_FETCH_PLANE_POS_1_A, _SEL_FETCH_PLANE_POS_1_B, _SEL_FETCH_PLANE_POS_2_A, _SEL_FETCH_PLANE_POS_2_B, _SEL_FETCH_PLANE_POS_5_A, _SEL_FETCH_PLANE_POS_5_B, _SEL_FETCH_PLANE_POS_6_A, _SEL_FETCH_PLANE_POS_6_B)
}

/// `_SEL_FETCH_PLANE_SIZE_1_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_1_A: u32 = 0x70898;

/// `_SEL_FETCH_PLANE_SIZE_2_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_2_A: u32 = 0x708b8;

/// `_SEL_FETCH_PLANE_SIZE_5_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_5_A: u32 = 0x70928;

/// `_SEL_FETCH_PLANE_SIZE_6_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_6_A: u32 = 0x70948;

/// `_SEL_FETCH_PLANE_SIZE_1_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_1_B: u32 = 0x71898;

/// `_SEL_FETCH_PLANE_SIZE_2_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_2_B: u32 = 0x718b8;

/// `_SEL_FETCH_PLANE_SIZE_5_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_5_B: u32 = 0x71928;

/// `_SEL_FETCH_PLANE_SIZE_6_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_SIZE_6_B: u32 = 0x71948;

/// `SEL_FETCH_PLANE_SIZE(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn SEL_FETCH_PLANE_SIZE(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SEL_FETCH((pipe), (plane), _SEL_FETCH_PLANE_SIZE_1_A, _SEL_FETCH_PLANE_SIZE_1_B, _SEL_FETCH_PLANE_SIZE_2_A, _SEL_FETCH_PLANE_SIZE_2_B, _SEL_FETCH_PLANE_SIZE_5_A, _SEL_FETCH_PLANE_SIZE_5_B, _SEL_FETCH_PLANE_SIZE_6_A, _SEL_FETCH_PLANE_SIZE_6_B)
}

/// `_SEL_FETCH_PLANE_OFFSET_1_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_1_A: u32 = 0x7089c;

/// `_SEL_FETCH_PLANE_OFFSET_2_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_2_A: u32 = 0x708bc;

/// `_SEL_FETCH_PLANE_OFFSET_5_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_5_A: u32 = 0x7092c;

/// `_SEL_FETCH_PLANE_OFFSET_6_A` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_6_A: u32 = 0x7094c;

/// `_SEL_FETCH_PLANE_OFFSET_1_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_1_B: u32 = 0x7189c;

/// `_SEL_FETCH_PLANE_OFFSET_2_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_2_B: u32 = 0x718bc;

/// `_SEL_FETCH_PLANE_OFFSET_5_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_5_B: u32 = 0x7192c;

/// `_SEL_FETCH_PLANE_OFFSET_6_B` (skl_universal_plane_regs.h)
pub const _SEL_FETCH_PLANE_OFFSET_6_B: u32 = 0x7194c;

/// `SEL_FETCH_PLANE_OFFSET(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn SEL_FETCH_PLANE_OFFSET(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SEL_FETCH((pipe), (plane), _SEL_FETCH_PLANE_OFFSET_1_A, _SEL_FETCH_PLANE_OFFSET_1_B, _SEL_FETCH_PLANE_OFFSET_2_A, _SEL_FETCH_PLANE_OFFSET_2_B, _SEL_FETCH_PLANE_OFFSET_5_A, _SEL_FETCH_PLANE_OFFSET_5_B, _SEL_FETCH_PLANE_OFFSET_6_A, _SEL_FETCH_PLANE_OFFSET_6_B)
}

/// `_PLANE_PIXEL_NORMALIZE_1_A` (skl_universal_plane_regs.h)
pub const _PLANE_PIXEL_NORMALIZE_1_A: u32 = 0x701a8;

/// `_PLANE_PIXEL_NORMALIZE_2_A` (skl_universal_plane_regs.h)
pub const _PLANE_PIXEL_NORMALIZE_2_A: u32 = 0x702a8;

/// `_PLANE_PIXEL_NORMALIZE_1_B` (skl_universal_plane_regs.h)
pub const _PLANE_PIXEL_NORMALIZE_1_B: u32 = 0x711a8;

/// `_PLANE_PIXEL_NORMALIZE_2_B` (skl_universal_plane_regs.h)
pub const _PLANE_PIXEL_NORMALIZE_2_B: u32 = 0x712a8;

/// `PLANE_PIXEL_NORMALIZE(pipe, plane)` (skl_universal_plane_regs.h)
pub const fn PLANE_PIXEL_NORMALIZE(pipe: u32, plane: u32) -> I915RegT {
    _MMIO_SKL_PLANE((pipe), (plane), _PLANE_PIXEL_NORMALIZE_1_A, _PLANE_PIXEL_NORMALIZE_1_B, _PLANE_PIXEL_NORMALIZE_2_A, _PLANE_PIXEL_NORMALIZE_2_B)
}

/// `PLANE_PIXEL_NORMALIZE_ENABLE` (skl_universal_plane_regs.h)
pub const PLANE_PIXEL_NORMALIZE_ENABLE: u32 = reg_bit(31);

/// `PLANE_PIXEL_NORMALIZE_NORM_FACTOR_MASK` (skl_universal_plane_regs.h)
pub const PLANE_PIXEL_NORMALIZE_NORM_FACTOR_MASK: u32 = reg_genmask(15, 0);

/// `PLANE_PIXEL_NORMALIZE_NORM_FACTOR(val)` (skl_universal_plane_regs.h)
pub const fn PLANE_PIXEL_NORMALIZE_NORM_FACTOR(val: u32) -> u32 {
    reg_field_prep(PLANE_PIXEL_NORMALIZE_NORM_FACTOR_MASK, (val))
}

/// `PLANE_PIXEL_NORMALIZE_NORM_FACTOR_1_0` (skl_universal_plane_regs.h)
pub const PLANE_PIXEL_NORMALIZE_NORM_FACTOR_1_0: u32 = 0x3c00;
