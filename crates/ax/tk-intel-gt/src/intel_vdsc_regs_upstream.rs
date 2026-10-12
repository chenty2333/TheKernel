// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_vdsc_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `DSS_CTL1` (intel_vdsc_regs.h)
pub const DSS_CTL1: I915RegT = mmio(0x67400);

/// `SPLITTER_ENABLE` (intel_vdsc_regs.h)
pub const SPLITTER_ENABLE: u32 = ((1 << 31));

/// `JOINER_ENABLE` (intel_vdsc_regs.h)
pub const JOINER_ENABLE: u32 = ((1 << 30));

/// `DUAL_LINK_MODE_INTERLEAVE` (intel_vdsc_regs.h)
pub const DUAL_LINK_MODE_INTERLEAVE: u32 = ((1 << 24));

/// `DUAL_LINK_MODE_FRONTBACK` (intel_vdsc_regs.h)
pub const DUAL_LINK_MODE_FRONTBACK: u32 = ((0 << 24));

/// `OVERLAP_PIXELS_MASK` (intel_vdsc_regs.h)
pub const OVERLAP_PIXELS_MASK: u32 = ((0xf << 16));

/// `OVERLAP_PIXELS(pixels)` (intel_vdsc_regs.h)
pub const fn OVERLAP_PIXELS(pixels: u32) -> u32 {
    (((pixels) << 16))
}

/// `LEFT_DL_BUF_TARGET_DEPTH_MASK` (intel_vdsc_regs.h)
pub const LEFT_DL_BUF_TARGET_DEPTH_MASK: u32 = ((0xfff << 0));

/// `LEFT_DL_BUF_TARGET_DEPTH(pixels)` (intel_vdsc_regs.h)
pub const fn LEFT_DL_BUF_TARGET_DEPTH(pixels: u32) -> u32 {
    (((pixels) << 0))
}

/// `MAX_DL_BUFFER_TARGET_DEPTH` (intel_vdsc_regs.h)
pub const MAX_DL_BUFFER_TARGET_DEPTH: u32 = 0x5a0;

/// `DSS_CTL2` (intel_vdsc_regs.h)
pub const DSS_CTL2: I915RegT = mmio(0x67404);

/// `VDSC0_ENABLE` (intel_vdsc_regs.h)
pub const VDSC0_ENABLE: u32 = reg_bit(31);

/// `VDSC2_ENABLE` (intel_vdsc_regs.h)
pub const VDSC2_ENABLE: u32 = reg_bit(30);

/// `SMALL_JOINER_CONFIG_3_ENGINES` (intel_vdsc_regs.h)
pub const SMALL_JOINER_CONFIG_3_ENGINES: u32 = reg_bit(23);

/// `VDSC1_ENABLE` (intel_vdsc_regs.h)
pub const VDSC1_ENABLE: u32 = reg_bit(15);

/// `RIGHT_DL_BUF_TARGET_DEPTH_MASK` (intel_vdsc_regs.h)
pub const RIGHT_DL_BUF_TARGET_DEPTH_MASK: u32 = ((0xfff << 0));

/// `RIGHT_DL_BUF_TARGET_DEPTH(pixels)` (intel_vdsc_regs.h)
pub const fn RIGHT_DL_BUF_TARGET_DEPTH(pixels: u32) -> u32 {
    (((pixels) << 0))
}

/// `_ICL_PIPE_DSS_CTL1_PB` (intel_vdsc_regs.h)
pub const _ICL_PIPE_DSS_CTL1_PB: u32 = 0x78200;

/// `_ICL_PIPE_DSS_CTL1_PC` (intel_vdsc_regs.h)
pub const _ICL_PIPE_DSS_CTL1_PC: u32 = 0x78400;

/// `ICL_PIPE_DSS_CTL1(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_PIPE_DSS_CTL1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_PIPE_DSS_CTL1_PB, _ICL_PIPE_DSS_CTL1_PC)
}

/// `BIG_JOINER_ENABLE` (intel_vdsc_regs.h)
pub const BIG_JOINER_ENABLE: u32 = ((1 << 29));

/// `PRIMARY_BIG_JOINER_ENABLE` (intel_vdsc_regs.h)
pub const PRIMARY_BIG_JOINER_ENABLE: u32 = ((1 << 28));

/// `VGA_CENTERING_ENABLE` (intel_vdsc_regs.h)
pub const VGA_CENTERING_ENABLE: u32 = ((1 << 27));

/// `SPLITTER_CONFIGURATION_MASK` (intel_vdsc_regs.h)
pub const SPLITTER_CONFIGURATION_MASK: u32 = reg_genmask(26, 25);

/// `SPLITTER_CONFIGURATION_2_SEGMENT` (intel_vdsc_regs.h)
pub const SPLITTER_CONFIGURATION_2_SEGMENT: u32 = reg_field_prep(SPLITTER_CONFIGURATION_MASK, 0);

/// `SPLITTER_CONFIGURATION_4_SEGMENT` (intel_vdsc_regs.h)
pub const SPLITTER_CONFIGURATION_4_SEGMENT: u32 = reg_field_prep(SPLITTER_CONFIGURATION_MASK, 1);

/// `ULTRA_JOINER_ENABLE` (intel_vdsc_regs.h)
pub const ULTRA_JOINER_ENABLE: u32 = reg_bit(23);

/// `PRIMARY_ULTRA_JOINER_ENABLE` (intel_vdsc_regs.h)
pub const PRIMARY_ULTRA_JOINER_ENABLE: u32 = reg_bit(22);

/// `UNCOMPRESSED_JOINER_PRIMARY` (intel_vdsc_regs.h)
pub const UNCOMPRESSED_JOINER_PRIMARY: u32 = ((1 << 21));

/// `UNCOMPRESSED_JOINER_SECONDARY` (intel_vdsc_regs.h)
pub const UNCOMPRESSED_JOINER_SECONDARY: u32 = ((1 << 20));

/// `_ICL_PIPE_DSS_CTL2_PB` (intel_vdsc_regs.h)
pub const _ICL_PIPE_DSS_CTL2_PB: u32 = 0x78204;

/// `_ICL_PIPE_DSS_CTL2_PC` (intel_vdsc_regs.h)
pub const _ICL_PIPE_DSS_CTL2_PC: u32 = 0x78404;

/// `ICL_PIPE_DSS_CTL2(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_PIPE_DSS_CTL2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_PIPE_DSS_CTL2_PB, _ICL_PIPE_DSS_CTL2_PC)
}

/// `DSCA_PICTURE_PARAMETER_SET_0` (intel_vdsc_regs.h)
pub const DSCA_PICTURE_PARAMETER_SET_0: I915RegT = mmio(0x6B200);

/// `DSCC_PICTURE_PARAMETER_SET_0` (intel_vdsc_regs.h)
pub const DSCC_PICTURE_PARAMETER_SET_0: I915RegT = mmio(0x6BA00);

/// `_DSCA_PPS_0` (intel_vdsc_regs.h)
pub const _DSCA_PPS_0: u32 = 0x6B200;

/// `_DSCC_PPS_0` (intel_vdsc_regs.h)
pub const _DSCC_PPS_0: u32 = 0x6BA00;

/// `_ICL_DSC0_PICTURE_PARAMETER_SET_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_PICTURE_PARAMETER_SET_0_PB: u32 = 0x78270;

/// `_ICL_DSC1_PICTURE_PARAMETER_SET_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_PICTURE_PARAMETER_SET_0_PB: u32 = 0x78370;

/// `_BMG_DSC2_PICTURE_PARAMETER_SET_0_PB` (intel_vdsc_regs.h)
pub const _BMG_DSC2_PICTURE_PARAMETER_SET_0_PB: u32 = 0x78970;

/// `_ICL_DSC0_PICTURE_PARAMETER_SET_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_PICTURE_PARAMETER_SET_0_PC: u32 = 0x78470;

/// `_ICL_DSC1_PICTURE_PARAMETER_SET_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_PICTURE_PARAMETER_SET_0_PC: u32 = 0x78570;

/// `_BMG_DSC2_PICTURE_PARAMETER_SET_0_PC` (intel_vdsc_regs.h)
pub const _BMG_DSC2_PICTURE_PARAMETER_SET_0_PC: u32 = 0x78A70;

/// `ICL_DSC0_PICTURE_PARAMETER_SET_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_PICTURE_PARAMETER_SET_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_PICTURE_PARAMETER_SET_0_PB, _ICL_DSC0_PICTURE_PARAMETER_SET_0_PC)
}

/// `ICL_DSC1_PICTURE_PARAMETER_SET_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_PICTURE_PARAMETER_SET_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_PICTURE_PARAMETER_SET_0_PB, _ICL_DSC1_PICTURE_PARAMETER_SET_0_PC)
}

/// `_ICL_DSC0_PPS_0(pipe)` (intel_vdsc_regs.h)
pub const fn _ICL_DSC0_PPS_0(pipe: u32) -> u32 {
    pick_even(((pipe) - (PIPE_B as u32)), _ICL_DSC0_PICTURE_PARAMETER_SET_0_PB, _ICL_DSC0_PICTURE_PARAMETER_SET_0_PC)
}

/// `_ICL_DSC1_PPS_0(pipe)` (intel_vdsc_regs.h)
pub const fn _ICL_DSC1_PPS_0(pipe: u32) -> u32 {
    pick_even(((pipe) - (PIPE_B as u32)), _ICL_DSC1_PICTURE_PARAMETER_SET_0_PB, _ICL_DSC1_PICTURE_PARAMETER_SET_0_PC)
}

/// `_BMG_DSC2_PPS_0(pipe)` (intel_vdsc_regs.h)
pub const fn _BMG_DSC2_PPS_0(pipe: u32) -> u32 {
    pick_even(((pipe) - (PIPE_B as u32)), _BMG_DSC2_PICTURE_PARAMETER_SET_0_PB, _BMG_DSC2_PICTURE_PARAMETER_SET_0_PC)
}

/// `ICL_DSC0_PPS(pipe, pps)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_PPS(pipe: u32, pps: u32) -> I915RegT {
    mmio((_ICL_DSC0_PPS_0(pipe) + (((pps) * 4))))
}

/// `ICL_DSC1_PPS(pipe, pps)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_PPS(pipe: u32, pps: u32) -> I915RegT {
    mmio((_ICL_DSC1_PPS_0(pipe) + (((pps) * 4))))
}

/// `BMG_DSC2_PPS(pipe, pps)` (intel_vdsc_regs.h)
pub const fn BMG_DSC2_PPS(pipe: u32, pps: u32) -> I915RegT {
    mmio((_BMG_DSC2_PPS_0(pipe) + (((pps) * 4))))
}

/// `DSC_PPS0_NATIVE_422_ENABLE` (intel_vdsc_regs.h)
pub const DSC_PPS0_NATIVE_422_ENABLE: u32 = reg_bit(23);

/// `DSC_PPS0_NATIVE_420_ENABLE` (intel_vdsc_regs.h)
pub const DSC_PPS0_NATIVE_420_ENABLE: u32 = reg_bit(22);

/// `DSC_PPS0_ALT_ICH_SEL` (intel_vdsc_regs.h)
pub const DSC_PPS0_ALT_ICH_SEL: u32 = reg_bit(20);

/// `DSC_PPS0_VBR_ENABLE` (intel_vdsc_regs.h)
pub const DSC_PPS0_VBR_ENABLE: u32 = reg_bit(19);

/// `DSC_PPS0_422_ENABLE` (intel_vdsc_regs.h)
pub const DSC_PPS0_422_ENABLE: u32 = reg_bit(18);

/// `DSC_PPS0_COLOR_SPACE_CONVERSION` (intel_vdsc_regs.h)
pub const DSC_PPS0_COLOR_SPACE_CONVERSION: u32 = reg_bit(17);

/// `DSC_PPS0_BLOCK_PREDICTION` (intel_vdsc_regs.h)
pub const DSC_PPS0_BLOCK_PREDICTION: u32 = reg_bit(16);

/// `DSC_PPS0_LINE_BUF_DEPTH_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS0_LINE_BUF_DEPTH_MASK: u32 = reg_genmask(15, 12);

/// `DSC_PPS0_LINE_BUF_DEPTH(depth)` (intel_vdsc_regs.h)
pub const fn DSC_PPS0_LINE_BUF_DEPTH(depth: u32) -> u32 {
    reg_field_prep(DSC_PPS0_LINE_BUF_DEPTH_MASK, depth)
}

/// `DSC_PPS0_BPC_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS0_BPC_MASK: u32 = reg_genmask(11, 8);

/// `DSC_PPS0_BPC(bpc)` (intel_vdsc_regs.h)
pub const fn DSC_PPS0_BPC(bpc: u32) -> u32 {
    reg_field_prep(DSC_PPS0_BPC_MASK, bpc)
}

/// `DSC_PPS0_VER_MINOR_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS0_VER_MINOR_MASK: u32 = reg_genmask(7, 4);

/// `DSC_PPS0_VER_MINOR(minor)` (intel_vdsc_regs.h)
pub const fn DSC_PPS0_VER_MINOR(minor: u32) -> u32 {
    reg_field_prep(DSC_PPS0_VER_MINOR_MASK, minor)
}

/// `DSC_PPS0_VER_MAJOR_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS0_VER_MAJOR_MASK: u32 = reg_genmask(3, 0);

/// `DSC_PPS0_VER_MAJOR(major)` (intel_vdsc_regs.h)
pub const fn DSC_PPS0_VER_MAJOR(major: u32) -> u32 {
    reg_field_prep(DSC_PPS0_VER_MAJOR_MASK, major)
}

/// `DSC_PPS1_BPP_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS1_BPP_MASK: u32 = reg_genmask(9, 0);

/// `DSC_PPS1_BPP(bpp)` (intel_vdsc_regs.h)
pub const fn DSC_PPS1_BPP(bpp: u32) -> u32 {
    reg_field_prep(DSC_PPS1_BPP_MASK, bpp)
}

/// `DSC_PPS2_PIC_WIDTH_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS2_PIC_WIDTH_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS2_PIC_HEIGHT_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS2_PIC_HEIGHT_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS2_PIC_WIDTH(pic_width)` (intel_vdsc_regs.h)
pub const fn DSC_PPS2_PIC_WIDTH(pic_width: u32) -> u32 {
    reg_field_prep(DSC_PPS2_PIC_WIDTH_MASK, pic_width)
}

/// `DSC_PPS2_PIC_HEIGHT(pic_height)` (intel_vdsc_regs.h)
pub const fn DSC_PPS2_PIC_HEIGHT(pic_height: u32) -> u32 {
    reg_field_prep(DSC_PPS2_PIC_HEIGHT_MASK, pic_height)
}

/// `DSC_PPS3_SLICE_WIDTH_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS3_SLICE_WIDTH_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS3_SLICE_HEIGHT_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS3_SLICE_HEIGHT_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS3_SLICE_WIDTH(slice_width)` (intel_vdsc_regs.h)
pub const fn DSC_PPS3_SLICE_WIDTH(slice_width: u32) -> u32 {
    reg_field_prep(DSC_PPS3_SLICE_WIDTH_MASK, slice_width)
}

/// `DSC_PPS3_SLICE_HEIGHT(slice_height)` (intel_vdsc_regs.h)
pub const fn DSC_PPS3_SLICE_HEIGHT(slice_height: u32) -> u32 {
    reg_field_prep(DSC_PPS3_SLICE_HEIGHT_MASK, slice_height)
}

/// `DSC_PPS4_INITIAL_DEC_DELAY_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS4_INITIAL_DEC_DELAY_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS4_INITIAL_XMIT_DELAY_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS4_INITIAL_XMIT_DELAY_MASK: u32 = reg_genmask(9, 0);

/// `DSC_PPS4_INITIAL_DEC_DELAY(dec_delay)` (intel_vdsc_regs.h)
pub const fn DSC_PPS4_INITIAL_DEC_DELAY(dec_delay: u32) -> u32 {
    reg_field_prep(DSC_PPS4_INITIAL_DEC_DELAY_MASK, dec_delay)
}

/// `DSC_PPS4_INITIAL_XMIT_DELAY(xmit_delay)` (intel_vdsc_regs.h)
pub const fn DSC_PPS4_INITIAL_XMIT_DELAY(xmit_delay: u32) -> u32 {
    reg_field_prep(DSC_PPS4_INITIAL_XMIT_DELAY_MASK, xmit_delay)
}

/// `DSC_PPS5_SCALE_DEC_INT_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS5_SCALE_DEC_INT_MASK: u32 = reg_genmask(27, 16);

/// `DSC_PPS5_SCALE_INC_INT_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS5_SCALE_INC_INT_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS5_SCALE_DEC_INT(scale_dec)` (intel_vdsc_regs.h)
pub const fn DSC_PPS5_SCALE_DEC_INT(scale_dec: u32) -> u32 {
    reg_field_prep(DSC_PPS5_SCALE_DEC_INT_MASK, scale_dec)
}

/// `DSC_PPS5_SCALE_INC_INT(scale_inc)` (intel_vdsc_regs.h)
pub const fn DSC_PPS5_SCALE_INC_INT(scale_inc: u32) -> u32 {
    reg_field_prep(DSC_PPS5_SCALE_INC_INT_MASK, scale_inc)
}

/// `DSC_PPS6_FLATNESS_MAX_QP_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS6_FLATNESS_MAX_QP_MASK: u32 = reg_genmask(28, 24);

/// `DSC_PPS6_FLATNESS_MIN_QP_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS6_FLATNESS_MIN_QP_MASK: u32 = reg_genmask(20, 16);

/// `DSC_PPS6_FIRST_LINE_BPG_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS6_FIRST_LINE_BPG_OFFSET_MASK: u32 = reg_genmask(12, 8);

/// `DSC_PPS6_INITIAL_SCALE_VALUE_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS6_INITIAL_SCALE_VALUE_MASK: u32 = reg_genmask(5, 0);

/// `DSC_PPS6_FLATNESS_MAX_QP(max_qp)` (intel_vdsc_regs.h)
pub const fn DSC_PPS6_FLATNESS_MAX_QP(max_qp: u32) -> u32 {
    reg_field_prep(DSC_PPS6_FLATNESS_MAX_QP_MASK, max_qp)
}

/// `DSC_PPS6_FLATNESS_MIN_QP(min_qp)` (intel_vdsc_regs.h)
pub const fn DSC_PPS6_FLATNESS_MIN_QP(min_qp: u32) -> u32 {
    reg_field_prep(DSC_PPS6_FLATNESS_MIN_QP_MASK, min_qp)
}

/// `DSC_PPS6_FIRST_LINE_BPG_OFFSET(offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS6_FIRST_LINE_BPG_OFFSET(offset: u32) -> u32 {
    reg_field_prep(DSC_PPS6_FIRST_LINE_BPG_OFFSET_MASK, offset)
}

/// `DSC_PPS6_INITIAL_SCALE_VALUE(value)` (intel_vdsc_regs.h)
pub const fn DSC_PPS6_INITIAL_SCALE_VALUE(value: u32) -> u32 {
    reg_field_prep(DSC_PPS6_INITIAL_SCALE_VALUE_MASK, value)
}

/// `DSC_PPS7_NFL_BPG_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS7_NFL_BPG_OFFSET_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS7_SLICE_BPG_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS7_SLICE_BPG_OFFSET_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS7_NFL_BPG_OFFSET(bpg_offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS7_NFL_BPG_OFFSET(bpg_offset: u32) -> u32 {
    reg_field_prep(DSC_PPS7_NFL_BPG_OFFSET_MASK, bpg_offset)
}

/// `DSC_PPS7_SLICE_BPG_OFFSET(bpg_offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS7_SLICE_BPG_OFFSET(bpg_offset: u32) -> u32 {
    reg_field_prep(DSC_PPS7_SLICE_BPG_OFFSET_MASK, bpg_offset)
}

/// `DSC_PPS8_INITIAL_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS8_INITIAL_OFFSET_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS8_FINAL_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS8_FINAL_OFFSET_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS8_INITIAL_OFFSET(initial_offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS8_INITIAL_OFFSET(initial_offset: u32) -> u32 {
    reg_field_prep(DSC_PPS8_INITIAL_OFFSET_MASK, initial_offset)
}

/// `DSC_PPS8_FINAL_OFFSET(final_offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS8_FINAL_OFFSET(final_offset: u32) -> u32 {
    reg_field_prep(DSC_PPS8_FINAL_OFFSET_MASK, final_offset)
}

/// `DSC_PPS9_RC_EDGE_FACTOR_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS9_RC_EDGE_FACTOR_MASK: u32 = reg_genmask(19, 16);

/// `DSC_PPS9_RC_MODEL_SIZE_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS9_RC_MODEL_SIZE_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS9_RC_EDGE_FACTOR(rc_edge_fact)` (intel_vdsc_regs.h)
pub const fn DSC_PPS9_RC_EDGE_FACTOR(rc_edge_fact: u32) -> u32 {
    reg_field_prep(DSC_PPS9_RC_EDGE_FACTOR_MASK, rc_edge_fact)
}

/// `DSC_PPS9_RC_MODEL_SIZE(rc_model_size)` (intel_vdsc_regs.h)
pub const fn DSC_PPS9_RC_MODEL_SIZE(rc_model_size: u32) -> u32 {
    reg_field_prep(DSC_PPS9_RC_MODEL_SIZE_MASK, rc_model_size)
}

/// `DSC_PPS10_RC_TGT_OFF_LOW_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS10_RC_TGT_OFF_LOW_MASK: u32 = reg_genmask(23, 20);

/// `DSC_PPS10_RC_TGT_OFF_HIGH_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS10_RC_TGT_OFF_HIGH_MASK: u32 = reg_genmask(19, 16);

/// `DSC_PPS10_RC_QUANT_INC_LIMIT1_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS10_RC_QUANT_INC_LIMIT1_MASK: u32 = reg_genmask(12, 8);

/// `DSC_PPS10_RC_QUANT_INC_LIMIT0_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS10_RC_QUANT_INC_LIMIT0_MASK: u32 = reg_genmask(4, 0);

/// `DSC_PPS10_RC_TARGET_OFF_LOW(rc_tgt_off_low)` (intel_vdsc_regs.h)
pub const fn DSC_PPS10_RC_TARGET_OFF_LOW(rc_tgt_off_low: u32) -> u32 {
    reg_field_prep(DSC_PPS10_RC_TGT_OFF_LOW_MASK, rc_tgt_off_low)
}

/// `DSC_PPS10_RC_TARGET_OFF_HIGH(rc_tgt_off_high)` (intel_vdsc_regs.h)
pub const fn DSC_PPS10_RC_TARGET_OFF_HIGH(rc_tgt_off_high: u32) -> u32 {
    reg_field_prep(DSC_PPS10_RC_TGT_OFF_HIGH_MASK, rc_tgt_off_high)
}

/// `DSC_PPS10_RC_QUANT_INC_LIMIT1(lim)` (intel_vdsc_regs.h)
pub const fn DSC_PPS10_RC_QUANT_INC_LIMIT1(lim: u32) -> u32 {
    reg_field_prep(DSC_PPS10_RC_QUANT_INC_LIMIT1_MASK, lim)
}

/// `DSC_PPS10_RC_QUANT_INC_LIMIT0(lim)` (intel_vdsc_regs.h)
pub const fn DSC_PPS10_RC_QUANT_INC_LIMIT0(lim: u32) -> u32 {
    reg_field_prep(DSC_PPS10_RC_QUANT_INC_LIMIT0_MASK, lim)
}

/// `DSC_PPS16_SLICE_ROW_PR_FRME_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS16_SLICE_ROW_PR_FRME_MASK: u32 = reg_genmask(31, 20);

/// `DSC_PPS16_SLICE_PER_LINE_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS16_SLICE_PER_LINE_MASK: u32 = reg_genmask(18, 16);

/// `DSC_PPS16_SLICE_CHUNK_SIZE_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS16_SLICE_CHUNK_SIZE_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS16_SLICE_ROW_PER_FRAME(slice_row_per_frame)` (intel_vdsc_regs.h)
pub const fn DSC_PPS16_SLICE_ROW_PER_FRAME(slice_row_per_frame: u32) -> u32 {
    reg_field_prep(DSC_PPS16_SLICE_ROW_PR_FRME_MASK, slice_row_per_frame)
}

/// `DSC_PPS16_SLICE_PER_LINE(slice_per_line)` (intel_vdsc_regs.h)
pub const fn DSC_PPS16_SLICE_PER_LINE(slice_per_line: u32) -> u32 {
    reg_field_prep(DSC_PPS16_SLICE_PER_LINE_MASK, slice_per_line)
}

/// `DSC_PPS16_SLICE_CHUNK_SIZE(slice_chunk_size)` (intel_vdsc_regs.h)
pub const fn DSC_PPS16_SLICE_CHUNK_SIZE(slice_chunk_size: u32) -> u32 {
    reg_field_prep(DSC_PPS16_SLICE_CHUNK_SIZE_MASK, slice_chunk_size)
}

/// `DSC_PPS17_SL_BPG_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS17_SL_BPG_OFFSET_MASK: u32 = reg_genmask(31, 27);

/// `DSC_PPS17_SL_BPG_OFFSET(offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS17_SL_BPG_OFFSET(offset: u32) -> u32 {
    reg_field_prep(DSC_PPS17_SL_BPG_OFFSET_MASK, offset)
}

/// `DSC_PPS18_NSL_BPG_OFFSET_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS18_NSL_BPG_OFFSET_MASK: u32 = reg_genmask(31, 16);

/// `DSC_PPS18_SL_OFFSET_ADJ_MASK` (intel_vdsc_regs.h)
pub const DSC_PPS18_SL_OFFSET_ADJ_MASK: u32 = reg_genmask(15, 0);

/// `DSC_PPS18_NSL_BPG_OFFSET(offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS18_NSL_BPG_OFFSET(offset: u32) -> u32 {
    reg_field_prep(DSC_PPS18_NSL_BPG_OFFSET_MASK, offset)
}

/// `DSC_PPS18_SL_OFFSET_ADJ(offset)` (intel_vdsc_regs.h)
pub const fn DSC_PPS18_SL_OFFSET_ADJ(offset: u32) -> u32 {
    reg_field_prep(DSC_PPS18_SL_OFFSET_ADJ_MASK, offset)
}

/// `_LNL_DSC0_SU_PARAMETER_SET_0_PA` (intel_vdsc_regs.h)
pub const _LNL_DSC0_SU_PARAMETER_SET_0_PA: u32 = 0x78064;

/// `_LNL_DSC1_SU_PARAMETER_SET_0_PA` (intel_vdsc_regs.h)
pub const _LNL_DSC1_SU_PARAMETER_SET_0_PA: u32 = 0x78164;

/// `_LNL_DSC0_SU_PARAMETER_SET_0_PB` (intel_vdsc_regs.h)
pub const _LNL_DSC0_SU_PARAMETER_SET_0_PB: u32 = 0x78264;

/// `_LNL_DSC1_SU_PARAMETER_SET_0_PB` (intel_vdsc_regs.h)
pub const _LNL_DSC1_SU_PARAMETER_SET_0_PB: u32 = 0x78364;

/// `LNL_DSC0_SU_PARAMETER_SET_0(pipe)` (intel_vdsc_regs.h)
pub const fn LNL_DSC0_SU_PARAMETER_SET_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _LNL_DSC0_SU_PARAMETER_SET_0_PA, _LNL_DSC0_SU_PARAMETER_SET_0_PB)
}

/// `LNL_DSC1_SU_PARAMETER_SET_0(pipe)` (intel_vdsc_regs.h)
pub const fn LNL_DSC1_SU_PARAMETER_SET_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _LNL_DSC1_SU_PARAMETER_SET_0_PA, _LNL_DSC1_SU_PARAMETER_SET_0_PB)
}

/// `DSC_SUPS0_SU_SLICE_ROW_PER_FRAME_MASK` (intel_vdsc_regs.h)
pub const DSC_SUPS0_SU_SLICE_ROW_PER_FRAME_MASK: u32 = reg_genmask(31, 20);

/// `DSC_SUPS0_SU_SLICE_ROW_PER_FRAME(rows)` (intel_vdsc_regs.h)
pub const fn DSC_SUPS0_SU_SLICE_ROW_PER_FRAME(rows: u32) -> u32 {
    reg_field_prep(DSC_SUPS0_SU_SLICE_ROW_PER_FRAME_MASK, (rows))
}

/// `DSC_SUPS0_SU_PIC_HEIGHT_MASK` (intel_vdsc_regs.h)
pub const DSC_SUPS0_SU_PIC_HEIGHT_MASK: u32 = reg_genmask(15, 0);

/// `DSC_SUPS0_SU_PIC_HEIGHT(h)` (intel_vdsc_regs.h)
pub const fn DSC_SUPS0_SU_PIC_HEIGHT(h: u32) -> u32 {
    reg_field_prep(DSC_SUPS0_SU_PIC_HEIGHT_MASK, (h))
}

/// `DSCA_RC_BUF_THRESH_0` (intel_vdsc_regs.h)
pub const DSCA_RC_BUF_THRESH_0: I915RegT = mmio(0x6B230);

/// `DSCA_RC_BUF_THRESH_0_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_BUF_THRESH_0_UDW: I915RegT = mmio((0x6B230 + 4));

/// `DSCC_RC_BUF_THRESH_0` (intel_vdsc_regs.h)
pub const DSCC_RC_BUF_THRESH_0: I915RegT = mmio(0x6BA30);

/// `DSCC_RC_BUF_THRESH_0_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_BUF_THRESH_0_UDW: I915RegT = mmio((0x6BA30 + 4));

/// `_ICL_DSC0_RC_BUF_THRESH_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_0_PB: u32 = (0x78254);

/// `_ICL_DSC0_RC_BUF_THRESH_0_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_0_UDW_PB: u32 = ((0x78254 + 4));

/// `_ICL_DSC1_RC_BUF_THRESH_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_0_PB: u32 = (0x78354);

/// `_ICL_DSC1_RC_BUF_THRESH_0_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_0_UDW_PB: u32 = ((0x78354 + 4));

/// `_ICL_DSC0_RC_BUF_THRESH_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_0_PC: u32 = (0x78454);

/// `_ICL_DSC0_RC_BUF_THRESH_0_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_0_UDW_PC: u32 = ((0x78454 + 4));

/// `_ICL_DSC1_RC_BUF_THRESH_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_0_PC: u32 = (0x78554);

/// `_ICL_DSC1_RC_BUF_THRESH_0_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_0_UDW_PC: u32 = ((0x78554 + 4));

/// `ICL_DSC0_RC_BUF_THRESH_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_BUF_THRESH_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_BUF_THRESH_0_PB, _ICL_DSC0_RC_BUF_THRESH_0_PC)
}

/// `ICL_DSC0_RC_BUF_THRESH_0_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_BUF_THRESH_0_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_BUF_THRESH_0_UDW_PB, _ICL_DSC0_RC_BUF_THRESH_0_UDW_PC)
}

/// `ICL_DSC1_RC_BUF_THRESH_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_BUF_THRESH_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_BUF_THRESH_0_PB, _ICL_DSC1_RC_BUF_THRESH_0_PC)
}

/// `ICL_DSC1_RC_BUF_THRESH_0_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_BUF_THRESH_0_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_BUF_THRESH_0_UDW_PB, _ICL_DSC1_RC_BUF_THRESH_0_UDW_PC)
}

/// `DSCA_RC_BUF_THRESH_1` (intel_vdsc_regs.h)
pub const DSCA_RC_BUF_THRESH_1: I915RegT = mmio(0x6B238);

/// `DSCA_RC_BUF_THRESH_1_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_BUF_THRESH_1_UDW: I915RegT = mmio((0x6B238 + 4));

/// `DSCC_RC_BUF_THRESH_1` (intel_vdsc_regs.h)
pub const DSCC_RC_BUF_THRESH_1: I915RegT = mmio(0x6BA38);

/// `DSCC_RC_BUF_THRESH_1_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_BUF_THRESH_1_UDW: I915RegT = mmio((0x6BA38 + 4));

/// `_ICL_DSC0_RC_BUF_THRESH_1_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_1_PB: u32 = (0x7825C);

/// `_ICL_DSC0_RC_BUF_THRESH_1_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_1_UDW_PB: u32 = ((0x7825C + 4));

/// `_ICL_DSC1_RC_BUF_THRESH_1_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_1_PB: u32 = (0x7835C);

/// `_ICL_DSC1_RC_BUF_THRESH_1_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_1_UDW_PB: u32 = ((0x7835C + 4));

/// `_ICL_DSC0_RC_BUF_THRESH_1_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_1_PC: u32 = (0x7845C);

/// `_ICL_DSC0_RC_BUF_THRESH_1_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_BUF_THRESH_1_UDW_PC: u32 = ((0x7845C + 4));

/// `_ICL_DSC1_RC_BUF_THRESH_1_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_1_PC: u32 = (0x7855C);

/// `_ICL_DSC1_RC_BUF_THRESH_1_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_BUF_THRESH_1_UDW_PC: u32 = ((0x7855C + 4));

/// `ICL_DSC0_RC_BUF_THRESH_1(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_BUF_THRESH_1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_BUF_THRESH_1_PB, _ICL_DSC0_RC_BUF_THRESH_1_PC)
}

/// `ICL_DSC0_RC_BUF_THRESH_1_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_BUF_THRESH_1_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_BUF_THRESH_1_UDW_PB, _ICL_DSC0_RC_BUF_THRESH_1_UDW_PC)
}

/// `ICL_DSC1_RC_BUF_THRESH_1(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_BUF_THRESH_1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_BUF_THRESH_1_PB, _ICL_DSC1_RC_BUF_THRESH_1_PC)
}

/// `ICL_DSC1_RC_BUF_THRESH_1_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_BUF_THRESH_1_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_BUF_THRESH_1_UDW_PB, _ICL_DSC1_RC_BUF_THRESH_1_UDW_PC)
}

/// `DSCA_RC_RANGE_PARAMETERS_0` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_0: I915RegT = mmio(0x6B240);

/// `DSCA_RC_RANGE_PARAMETERS_0_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_0_UDW: I915RegT = mmio((0x6B240 + 4));

/// `DSCC_RC_RANGE_PARAMETERS_0` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_0: I915RegT = mmio(0x6BA40);

/// `DSCC_RC_RANGE_PARAMETERS_0_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_0_UDW: I915RegT = mmio((0x6BA40 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_0_PB: u32 = (0x78208);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PB: u32 = ((0x78208 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_0_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_0_PB: u32 = (0x78308);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PB: u32 = ((0x78308 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_0_PC: u32 = (0x78408);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PC: u32 = ((0x78408 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_0_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_0_PC: u32 = (0x78508);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PC: u32 = ((0x78508 + 4));

/// `ICL_DSC0_RC_RANGE_PARAMETERS_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_0_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_0_PC)
}

/// `ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_0_UDW_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_0(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_0_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_0_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_0_UDW_PC)
}

/// `RC_BPG_OFFSET_SHIFT` (intel_vdsc_regs.h)
pub const RC_BPG_OFFSET_SHIFT: u32 = 10;

/// `RC_MAX_QP_SHIFT` (intel_vdsc_regs.h)
pub const RC_MAX_QP_SHIFT: u32 = 5;

/// `RC_MIN_QP_SHIFT` (intel_vdsc_regs.h)
pub const RC_MIN_QP_SHIFT: u32 = 0;

/// `DSCA_RC_RANGE_PARAMETERS_1` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_1: I915RegT = mmio(0x6B248);

/// `DSCA_RC_RANGE_PARAMETERS_1_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_1_UDW: I915RegT = mmio((0x6B248 + 4));

/// `DSCC_RC_RANGE_PARAMETERS_1` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_1: I915RegT = mmio(0x6BA48);

/// `DSCC_RC_RANGE_PARAMETERS_1_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_1_UDW: I915RegT = mmio((0x6BA48 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_1_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_1_PB: u32 = (0x78210);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PB: u32 = ((0x78210 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_1_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_1_PB: u32 = (0x78310);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PB: u32 = ((0x78310 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_1_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_1_PC: u32 = (0x78410);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PC: u32 = ((0x78410 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_1_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_1_PC: u32 = (0x78510);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PC: u32 = ((0x78510 + 4));

/// `ICL_DSC0_RC_RANGE_PARAMETERS_1(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_1_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_1_PC)
}

/// `ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_1_UDW_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_1(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_1_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_1_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_1_UDW_PC)
}

/// `DSCA_RC_RANGE_PARAMETERS_2` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_2: I915RegT = mmio(0x6B250);

/// `DSCA_RC_RANGE_PARAMETERS_2_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_2_UDW: I915RegT = mmio((0x6B250 + 4));

/// `DSCC_RC_RANGE_PARAMETERS_2` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_2: I915RegT = mmio(0x6BA50);

/// `DSCC_RC_RANGE_PARAMETERS_2_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_2_UDW: I915RegT = mmio((0x6BA50 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_2_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_2_PB: u32 = (0x78218);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PB: u32 = ((0x78218 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_2_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_2_PB: u32 = (0x78318);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PB: u32 = ((0x78318 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_2_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_2_PC: u32 = (0x78418);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PC: u32 = ((0x78418 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_2_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_2_PC: u32 = (0x78518);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PC: u32 = ((0x78518 + 4));

/// `ICL_DSC0_RC_RANGE_PARAMETERS_2(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_2_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_2_PC)
}

/// `ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_2_UDW_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_2(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_2_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_2_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_2_UDW_PC)
}

/// `DSCA_RC_RANGE_PARAMETERS_3` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_3: I915RegT = mmio(0x6B258);

/// `DSCA_RC_RANGE_PARAMETERS_3_UDW` (intel_vdsc_regs.h)
pub const DSCA_RC_RANGE_PARAMETERS_3_UDW: I915RegT = mmio((0x6B258 + 4));

/// `DSCC_RC_RANGE_PARAMETERS_3` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_3: I915RegT = mmio(0x6BA58);

/// `DSCC_RC_RANGE_PARAMETERS_3_UDW` (intel_vdsc_regs.h)
pub const DSCC_RC_RANGE_PARAMETERS_3_UDW: I915RegT = mmio((0x6BA58 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_3_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_3_PB: u32 = (0x78220);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PB: u32 = ((0x78220 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_3_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_3_PB: u32 = (0x78320);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PB` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PB: u32 = ((0x78320 + 4));

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_3_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_3_PC: u32 = (0x78420);

/// `_ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PC: u32 = ((0x78420 + 4));

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_3_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_3_PC: u32 = (0x78520);

/// `_ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PC` (intel_vdsc_regs.h)
pub const _ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PC: u32 = ((0x78520 + 4));

/// `ICL_DSC0_RC_RANGE_PARAMETERS_3(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_3(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_3_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_3_PC)
}

/// `ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PB, _ICL_DSC0_RC_RANGE_PARAMETERS_3_UDW_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_3(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_3(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_3_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_3_PC)
}

/// `ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW(pipe)` (intel_vdsc_regs.h)
pub const fn ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW(pipe: u32) -> I915RegT {
    _MMIO_PIPE(((pipe) - (PIPE_B as u32)), _ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PB, _ICL_DSC1_RC_RANGE_PARAMETERS_3_UDW_PC)
}
