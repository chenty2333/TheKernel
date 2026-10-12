// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_audio_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `G4X_AUD_CNTL_ST` (intel_audio_regs.h)
pub const G4X_AUD_CNTL_ST: I915RegT = mmio(0x620B4);

/// `G4X_ELD_VALID` (intel_audio_regs.h)
pub const G4X_ELD_VALID: u32 = reg_bit(14);

/// `G4X_ELD_BUFFER_SIZE_MASK` (intel_audio_regs.h)
pub const G4X_ELD_BUFFER_SIZE_MASK: u32 = reg_genmask(13, 9);

/// `G4X_ELD_ADDRESS_MASK` (intel_audio_regs.h)
pub const G4X_ELD_ADDRESS_MASK: u32 = reg_genmask(8, 5);

/// `G4X_ELD_ACK` (intel_audio_regs.h)
pub const G4X_ELD_ACK: u32 = reg_bit(4);

/// `G4X_HDMIW_HDMIEDID` (intel_audio_regs.h)
pub const G4X_HDMIW_HDMIEDID: I915RegT = mmio(0x6210C);

/// `_IBX_HDMIW_HDMIEDID_A` (intel_audio_regs.h)
pub const _IBX_HDMIW_HDMIEDID_A: u32 = 0xE2050;

/// `_IBX_HDMIW_HDMIEDID_B` (intel_audio_regs.h)
pub const _IBX_HDMIW_HDMIEDID_B: u32 = 0xE2150;

/// `IBX_HDMIW_HDMIEDID(pipe)` (intel_audio_regs.h)
pub const fn IBX_HDMIW_HDMIEDID(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _IBX_HDMIW_HDMIEDID_A, _IBX_HDMIW_HDMIEDID_B)
}

/// `_IBX_AUD_CNTL_ST_A` (intel_audio_regs.h)
pub const _IBX_AUD_CNTL_ST_A: u32 = 0xE20B4;

/// `_IBX_AUD_CNTL_ST_B` (intel_audio_regs.h)
pub const _IBX_AUD_CNTL_ST_B: u32 = 0xE21B4;

/// `IBX_AUD_CNTL_ST(pipe)` (intel_audio_regs.h)
pub const fn IBX_AUD_CNTL_ST(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _IBX_AUD_CNTL_ST_A, _IBX_AUD_CNTL_ST_B)
}

/// `IBX_ELD_BUFFER_SIZE_MASK` (intel_audio_regs.h)
pub const IBX_ELD_BUFFER_SIZE_MASK: u32 = reg_genmask(14, 10);

/// `IBX_ELD_ADDRESS_MASK` (intel_audio_regs.h)
pub const IBX_ELD_ADDRESS_MASK: u32 = reg_genmask(9, 5);

/// `IBX_ELD_ACK` (intel_audio_regs.h)
pub const IBX_ELD_ACK: u32 = reg_bit(4);

/// `IBX_AUD_CNTL_ST2` (intel_audio_regs.h)
pub const IBX_AUD_CNTL_ST2: I915RegT = mmio(0xE20C0);

/// `IBX_CP_READY(port)` (intel_audio_regs.h)
pub const fn IBX_CP_READY(port: u32) -> u32 {
    reg_bit((((((port) - 1)) * 4) + 1))
}

/// `IBX_ELD_VALID(port)` (intel_audio_regs.h)
pub const fn IBX_ELD_VALID(port: u32) -> u32 {
    reg_bit((((((port) - 1)) * 4) + 0))
}

/// `_CPT_HDMIW_HDMIEDID_A` (intel_audio_regs.h)
pub const _CPT_HDMIW_HDMIEDID_A: u32 = 0xE5050;

/// `_CPT_HDMIW_HDMIEDID_B` (intel_audio_regs.h)
pub const _CPT_HDMIW_HDMIEDID_B: u32 = 0xE5150;

/// `CPT_HDMIW_HDMIEDID(pipe)` (intel_audio_regs.h)
pub const fn CPT_HDMIW_HDMIEDID(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CPT_HDMIW_HDMIEDID_A, _CPT_HDMIW_HDMIEDID_B)
}

/// `_CPT_AUD_CNTL_ST_A` (intel_audio_regs.h)
pub const _CPT_AUD_CNTL_ST_A: u32 = 0xE50B4;

/// `_CPT_AUD_CNTL_ST_B` (intel_audio_regs.h)
pub const _CPT_AUD_CNTL_ST_B: u32 = 0xE51B4;

/// `CPT_AUD_CNTL_ST(pipe)` (intel_audio_regs.h)
pub const fn CPT_AUD_CNTL_ST(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CPT_AUD_CNTL_ST_A, _CPT_AUD_CNTL_ST_B)
}

/// `CPT_AUD_CNTRL_ST2` (intel_audio_regs.h)
pub const CPT_AUD_CNTRL_ST2: I915RegT = mmio(0xE50C0);

/// `_VLV_HDMIW_HDMIEDID_A` (intel_audio_regs.h)
pub const _VLV_HDMIW_HDMIEDID_A: u32 = ((VLV_DISPLAY_BASE + 0x62050));

/// `_VLV_HDMIW_HDMIEDID_B` (intel_audio_regs.h)
pub const _VLV_HDMIW_HDMIEDID_B: u32 = ((VLV_DISPLAY_BASE + 0x62150));

/// `VLV_HDMIW_HDMIEDID(pipe)` (intel_audio_regs.h)
pub const fn VLV_HDMIW_HDMIEDID(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_HDMIW_HDMIEDID_A, _VLV_HDMIW_HDMIEDID_B)
}

/// `_VLV_AUD_CNTL_ST_A` (intel_audio_regs.h)
pub const _VLV_AUD_CNTL_ST_A: u32 = ((VLV_DISPLAY_BASE + 0x620B4));

/// `_VLV_AUD_CNTL_ST_B` (intel_audio_regs.h)
pub const _VLV_AUD_CNTL_ST_B: u32 = ((VLV_DISPLAY_BASE + 0x621B4));

/// `VLV_AUD_CNTL_ST(pipe)` (intel_audio_regs.h)
pub const fn VLV_AUD_CNTL_ST(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_AUD_CNTL_ST_A, _VLV_AUD_CNTL_ST_B)
}

/// `VLV_AUD_CNTL_ST2` (intel_audio_regs.h)
pub const VLV_AUD_CNTL_ST2: I915RegT = mmio((VLV_DISPLAY_BASE + 0x620C0));

/// `_IBX_AUD_CONFIG_A` (intel_audio_regs.h)
pub const _IBX_AUD_CONFIG_A: u32 = 0xe2000;

/// `_IBX_AUD_CONFIG_B` (intel_audio_regs.h)
pub const _IBX_AUD_CONFIG_B: u32 = 0xe2100;

/// `IBX_AUD_CFG(pipe)` (intel_audio_regs.h)
pub const fn IBX_AUD_CFG(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _IBX_AUD_CONFIG_A, _IBX_AUD_CONFIG_B)
}

/// `_CPT_AUD_CONFIG_A` (intel_audio_regs.h)
pub const _CPT_AUD_CONFIG_A: u32 = 0xe5000;

/// `_CPT_AUD_CONFIG_B` (intel_audio_regs.h)
pub const _CPT_AUD_CONFIG_B: u32 = 0xe5100;

/// `CPT_AUD_CFG(pipe)` (intel_audio_regs.h)
pub const fn CPT_AUD_CFG(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CPT_AUD_CONFIG_A, _CPT_AUD_CONFIG_B)
}

/// `_VLV_AUD_CONFIG_A` (intel_audio_regs.h)
pub const _VLV_AUD_CONFIG_A: u32 = ((VLV_DISPLAY_BASE + 0x62000));

/// `_VLV_AUD_CONFIG_B` (intel_audio_regs.h)
pub const _VLV_AUD_CONFIG_B: u32 = ((VLV_DISPLAY_BASE + 0x62100));

/// `VLV_AUD_CFG(pipe)` (intel_audio_regs.h)
pub const fn VLV_AUD_CFG(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VLV_AUD_CONFIG_A, _VLV_AUD_CONFIG_B)
}

/// `AUD_CONFIG_N_VALUE_INDEX` (intel_audio_regs.h)
pub const AUD_CONFIG_N_VALUE_INDEX: u32 = reg_bit(29);

/// `AUD_CONFIG_N_PROG_ENABLE` (intel_audio_regs.h)
pub const AUD_CONFIG_N_PROG_ENABLE: u32 = reg_bit(28);

/// `AUD_CONFIG_UPPER_N_MASK` (intel_audio_regs.h)
pub const AUD_CONFIG_UPPER_N_MASK: u32 = reg_genmask(27, 20);

/// `AUD_CONFIG_LOWER_N_MASK` (intel_audio_regs.h)
pub const AUD_CONFIG_LOWER_N_MASK: u32 = reg_genmask(15, 4);

/// `AUD_CONFIG_N_MASK` (intel_audio_regs.h)
pub const AUD_CONFIG_N_MASK: u32 = ((AUD_CONFIG_UPPER_N_MASK | AUD_CONFIG_LOWER_N_MASK));

/// `AUD_CONFIG_N(n)` (intel_audio_regs.h)
pub const fn AUD_CONFIG_N(n: u32) -> u32 {
    ((reg_field_prep(AUD_CONFIG_UPPER_N_MASK, ((n) >> 12)) | reg_field_prep(AUD_CONFIG_LOWER_N_MASK, ((n) & 0xfff))))
}

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK: u32 = reg_genmask(19, 16);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_25175` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_25175: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 0);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_25200` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_25200: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 1);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_27000` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_27000: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 2);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_27027` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_27027: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 3);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_54000` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_54000: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 4);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_54054` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_54054: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 5);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_74176` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_74176: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 6);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_74250` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_74250: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 7);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_148352` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_148352: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 8);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_148500` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_148500: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 9);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_296703` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_296703: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 10);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_297000` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_297000: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 11);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_593407` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_593407: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 12);

/// `AUD_CONFIG_PIXEL_CLOCK_HDMI_594000` (intel_audio_regs.h)
pub const AUD_CONFIG_PIXEL_CLOCK_HDMI_594000: u32 = reg_field_prep(AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK, 13);

/// `AUD_CONFIG_DISABLE_NCTS` (intel_audio_regs.h)
pub const AUD_CONFIG_DISABLE_NCTS: u32 = reg_bit(3);

/// `_HSW_AUD_CONFIG_A` (intel_audio_regs.h)
pub const _HSW_AUD_CONFIG_A: u32 = 0x65000;

/// `_HSW_AUD_CONFIG_B` (intel_audio_regs.h)
pub const _HSW_AUD_CONFIG_B: u32 = 0x65100;

/// `HSW_AUD_CFG(trans)` (intel_audio_regs.h)
pub const fn HSW_AUD_CFG(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_CONFIG_A, _HSW_AUD_CONFIG_B)
}

/// `_HSW_AUD_MISC_CTRL_A` (intel_audio_regs.h)
pub const _HSW_AUD_MISC_CTRL_A: u32 = 0x65010;

/// `_HSW_AUD_MISC_CTRL_B` (intel_audio_regs.h)
pub const _HSW_AUD_MISC_CTRL_B: u32 = 0x65110;

/// `HSW_AUD_MISC_CTRL(trans)` (intel_audio_regs.h)
pub const fn HSW_AUD_MISC_CTRL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_MISC_CTRL_A, _HSW_AUD_MISC_CTRL_B)
}

/// `_HSW_AUD_M_CTS_ENABLE_A` (intel_audio_regs.h)
pub const _HSW_AUD_M_CTS_ENABLE_A: u32 = 0x65028;

/// `_HSW_AUD_M_CTS_ENABLE_B` (intel_audio_regs.h)
pub const _HSW_AUD_M_CTS_ENABLE_B: u32 = 0x65128;

/// `HSW_AUD_M_CTS_ENABLE(trans)` (intel_audio_regs.h)
pub const fn HSW_AUD_M_CTS_ENABLE(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_M_CTS_ENABLE_A, _HSW_AUD_M_CTS_ENABLE_B)
}

/// `AUD_M_CTS_M_VALUE_INDEX` (intel_audio_regs.h)
pub const AUD_M_CTS_M_VALUE_INDEX: u32 = reg_bit(21);

/// `AUD_M_CTS_M_PROG_ENABLE` (intel_audio_regs.h)
pub const AUD_M_CTS_M_PROG_ENABLE: u32 = reg_bit(20);

/// `AUD_CONFIG_M_MASK` (intel_audio_regs.h)
pub const AUD_CONFIG_M_MASK: u32 = reg_genmask(19, 0);

/// `_HSW_AUD_DIP_ELD_CTRL_ST_A` (intel_audio_regs.h)
pub const _HSW_AUD_DIP_ELD_CTRL_ST_A: u32 = 0x650b4;

/// `_HSW_AUD_DIP_ELD_CTRL_ST_B` (intel_audio_regs.h)
pub const _HSW_AUD_DIP_ELD_CTRL_ST_B: u32 = 0x651b4;

/// `HSW_AUD_DIP_ELD_CTRL(trans)` (intel_audio_regs.h)
pub const fn HSW_AUD_DIP_ELD_CTRL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_DIP_ELD_CTRL_ST_A, _HSW_AUD_DIP_ELD_CTRL_ST_B)
}

/// `_HSW_AUD_DIG_CNVT_1` (intel_audio_regs.h)
pub const _HSW_AUD_DIG_CNVT_1: u32 = 0x65080;

/// `_HSW_AUD_DIG_CNVT_2` (intel_audio_regs.h)
pub const _HSW_AUD_DIG_CNVT_2: u32 = 0x65180;

/// `AUD_DIG_CNVT(trans)` (intel_audio_regs.h)
pub const fn AUD_DIG_CNVT(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_DIG_CNVT_1, _HSW_AUD_DIG_CNVT_2)
}

/// `DIP_PORT_SEL_MASK` (intel_audio_regs.h)
pub const DIP_PORT_SEL_MASK: u32 = 0x3;

/// `_HSW_AUD_EDID_DATA_A` (intel_audio_regs.h)
pub const _HSW_AUD_EDID_DATA_A: u32 = 0x65050;

/// `_HSW_AUD_EDID_DATA_B` (intel_audio_regs.h)
pub const _HSW_AUD_EDID_DATA_B: u32 = 0x65150;

/// `HSW_AUD_EDID_DATA(trans)` (intel_audio_regs.h)
pub const fn HSW_AUD_EDID_DATA(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _HSW_AUD_EDID_DATA_A, _HSW_AUD_EDID_DATA_B)
}

/// `HSW_AUD_PIPE_CONV_CFG` (intel_audio_regs.h)
pub const HSW_AUD_PIPE_CONV_CFG: I915RegT = mmio(0x6507c);

/// `HSW_AUD_PIN_ELD_CP_VLD` (intel_audio_regs.h)
pub const HSW_AUD_PIN_ELD_CP_VLD: I915RegT = mmio(0x650c0);

/// `AUDIO_INACTIVE(trans)` (intel_audio_regs.h)
pub const fn AUDIO_INACTIVE(trans: u32) -> u32 {
    ((((1 << 3)) << (((trans) * 4))))
}

/// `AUDIO_OUTPUT_ENABLE(trans)` (intel_audio_regs.h)
pub const fn AUDIO_OUTPUT_ENABLE(trans: u32) -> u32 {
    ((((1 << 2)) << (((trans) * 4))))
}

/// `AUDIO_CP_READY(trans)` (intel_audio_regs.h)
pub const fn AUDIO_CP_READY(trans: u32) -> u32 {
    ((((1 << 1)) << (((trans) * 4))))
}

/// `AUDIO_ELD_VALID(trans)` (intel_audio_regs.h)
pub const fn AUDIO_ELD_VALID(trans: u32) -> u32 {
    ((((1 << 0)) << (((trans) * 4))))
}

/// `_AUD_TCA_DP_2DOT0_CTRL` (intel_audio_regs.h)
pub const _AUD_TCA_DP_2DOT0_CTRL: u32 = 0x650bc;

/// `_AUD_TCB_DP_2DOT0_CTRL` (intel_audio_regs.h)
pub const _AUD_TCB_DP_2DOT0_CTRL: u32 = 0x651bc;

/// `AUD_DP_2DOT0_CTRL(trans)` (intel_audio_regs.h)
pub const fn AUD_DP_2DOT0_CTRL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _AUD_TCA_DP_2DOT0_CTRL, _AUD_TCB_DP_2DOT0_CTRL)
}

/// `AUD_ENABLE_SDP_SPLIT` (intel_audio_regs.h)
pub const AUD_ENABLE_SDP_SPLIT: u32 = reg_bit(31);

/// `HSW_AUD_CHICKENBIT` (intel_audio_regs.h)
pub const HSW_AUD_CHICKENBIT: I915RegT = mmio(0x65f10);

/// `SKL_AUD_CODEC_WAKE_SIGNAL` (intel_audio_regs.h)
pub const SKL_AUD_CODEC_WAKE_SIGNAL: u32 = reg_bit(15);

/// `AUD_FREQ_CNTRL` (intel_audio_regs.h)
pub const AUD_FREQ_CNTRL: I915RegT = mmio(0x65900);

/// `AUD_PIN_BUF_CTL` (intel_audio_regs.h)
pub const AUD_PIN_BUF_CTL: I915RegT = mmio(0x48414);

/// `AUD_PIN_BUF_ENABLE` (intel_audio_regs.h)
pub const AUD_PIN_BUF_ENABLE: u32 = reg_bit(31);

/// `AUD_TS_CDCLK_M` (intel_audio_regs.h)
pub const AUD_TS_CDCLK_M: I915RegT = mmio(0x65ea0);

/// `AUD_TS_CDCLK_M_EN` (intel_audio_regs.h)
pub const AUD_TS_CDCLK_M_EN: u32 = reg_bit(31);

/// `AUD_TS_CDCLK_N` (intel_audio_regs.h)
pub const AUD_TS_CDCLK_N: I915RegT = mmio(0x65ea4);

/// `AUD_CONFIG_BE` (intel_audio_regs.h)
pub const AUD_CONFIG_BE: I915RegT = mmio(0x65ef0);

/// `HBLANK_EARLY_ENABLE_ICL(pipe)` (intel_audio_regs.h)
pub const fn HBLANK_EARLY_ENABLE_ICL(pipe: u32) -> u32 {
    ((0x1 << ((20 - (pipe)))))
}

/// `HBLANK_EARLY_ENABLE_TGL(pipe)` (intel_audio_regs.h)
pub const fn HBLANK_EARLY_ENABLE_TGL(pipe: u32) -> u32 {
    ((0x1 << ((24 + (pipe)))))
}

/// `HBLANK_START_COUNT_MASK(pipe)` (intel_audio_regs.h)
pub const fn HBLANK_START_COUNT_MASK(pipe: u32) -> u32 {
    ((0x7 << ((3 + (((pipe) * 6))))))
}

/// `HBLANK_START_COUNT(pipe, val)` (intel_audio_regs.h)
pub const fn HBLANK_START_COUNT(pipe: u32, val: u32) -> u32 {
    (((((val) & 0x7)) << ((3 + (((pipe)) * 6)))))
}

/// `NUMBER_SAMPLES_PER_LINE_MASK(pipe)` (intel_audio_regs.h)
pub const fn NUMBER_SAMPLES_PER_LINE_MASK(pipe: u32) -> u32 {
    ((0x3 << (((pipe) * 6))))
}

/// `NUMBER_SAMPLES_PER_LINE(pipe, val)` (intel_audio_regs.h)
pub const fn NUMBER_SAMPLES_PER_LINE(pipe: u32, val: u32) -> u32 {
    (((((val) & 0x3)) << (((pipe) * 6))))
}

/// `HBLANK_START_COUNT_8` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_8: u32 = 0;

/// `HBLANK_START_COUNT_16` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_16: u32 = 1;

/// `HBLANK_START_COUNT_32` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_32: u32 = 2;

/// `HBLANK_START_COUNT_64` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_64: u32 = 3;

/// `HBLANK_START_COUNT_96` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_96: u32 = 4;

/// `HBLANK_START_COUNT_128` (intel_audio_regs.h)
pub const HBLANK_START_COUNT_128: u32 = 5;

/// `I915_HDMI_LPE_AUDIO_BASE` (intel_audio_regs.h)
pub const I915_HDMI_LPE_AUDIO_BASE: u32 = ((VLV_DISPLAY_BASE + 0x65000));

/// `I915_HDMI_LPE_AUDIO_SIZE` (intel_audio_regs.h)
pub const I915_HDMI_LPE_AUDIO_SIZE: u32 = 0x1000;

/// `VLV_AUD_CHICKEN_BIT_REG` (intel_audio_regs.h)
pub const VLV_AUD_CHICKEN_BIT_REG: I915RegT = mmio((VLV_DISPLAY_BASE + 0x62F38));

/// `VLV_CHICKEN_BIT_DBG_ENABLE` (intel_audio_regs.h)
pub const VLV_CHICKEN_BIT_DBG_ENABLE: u32 = ((1 << 0));

/// `_VLV_AUD_PORT_EN_B_DBG` (intel_audio_regs.h)
pub const _VLV_AUD_PORT_EN_B_DBG: u32 = 0x62F20;

/// `_VLV_AUD_PORT_EN_C_DBG` (intel_audio_regs.h)
pub const _VLV_AUD_PORT_EN_C_DBG: u32 = 0x62F30;

/// `_VLV_AUD_PORT_EN_D_DBG` (intel_audio_regs.h)
pub const _VLV_AUD_PORT_EN_D_DBG: u32 = 0x62F34;

/// `VLV_AUD_PORT_EN_DBG(port)` (intel_audio_regs.h)
pub const fn VLV_AUD_PORT_EN_DBG(port: u32) -> I915RegT {
    _MMIO_BASE_PORT3(VLV_DISPLAY_BASE, ((port) - (PORT_B as u32)), _VLV_AUD_PORT_EN_B_DBG, _VLV_AUD_PORT_EN_C_DBG, _VLV_AUD_PORT_EN_D_DBG)
}

/// `VLV_AMP_MUTE` (intel_audio_regs.h)
pub const VLV_AMP_MUTE: u32 = ((1 << 1));

/// `AUD_CHICKENBIT_REG3` (intel_audio_regs.h)
pub const AUD_CHICKENBIT_REG3: I915RegT = mmio(0x65F1C);

/// `DACBE_DISABLE_MIN_HBLANK_FIX` (intel_audio_regs.h)
pub const DACBE_DISABLE_MIN_HBLANK_FIX: u32 = reg_bit(18);
