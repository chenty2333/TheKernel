// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_fdi_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `FDI_PLL_BIOS_0` (intel_fdi_regs.h)
pub const FDI_PLL_BIOS_0: I915RegT = mmio(0x46000);

/// `FDI_PLL_FB_CLOCK_MASK` (intel_fdi_regs.h)
pub const FDI_PLL_FB_CLOCK_MASK: u32 = 0xff;

/// `FDI_PLL_BIOS_1` (intel_fdi_regs.h)
pub const FDI_PLL_BIOS_1: I915RegT = mmio(0x46004);

/// `FDI_PLL_BIOS_2` (intel_fdi_regs.h)
pub const FDI_PLL_BIOS_2: I915RegT = mmio(0x46008);

/// `DISPLAY_PORT_PLL_BIOS_0` (intel_fdi_regs.h)
pub const DISPLAY_PORT_PLL_BIOS_0: I915RegT = mmio(0x4600c);

/// `DISPLAY_PORT_PLL_BIOS_1` (intel_fdi_regs.h)
pub const DISPLAY_PORT_PLL_BIOS_1: I915RegT = mmio(0x46010);

/// `DISPLAY_PORT_PLL_BIOS_2` (intel_fdi_regs.h)
pub const DISPLAY_PORT_PLL_BIOS_2: I915RegT = mmio(0x46014);

/// `FDI_PLL_FREQ_CTL` (intel_fdi_regs.h)
pub const FDI_PLL_FREQ_CTL: I915RegT = mmio(0x46030);

/// `FDI_PLL_FREQ_CHANGE_REQUEST` (intel_fdi_regs.h)
pub const FDI_PLL_FREQ_CHANGE_REQUEST: u32 = ((1 << 24));

/// `FDI_PLL_FREQ_LOCK_LIMIT_MASK` (intel_fdi_regs.h)
pub const FDI_PLL_FREQ_LOCK_LIMIT_MASK: u32 = 0xfff00;

/// `FDI_PLL_FREQ_DISABLE_COUNT_LIMIT_MASK` (intel_fdi_regs.h)
pub const FDI_PLL_FREQ_DISABLE_COUNT_LIMIT_MASK: u32 = 0xff;

/// `_FDI_RXA_CHICKEN` (intel_fdi_regs.h)
pub const _FDI_RXA_CHICKEN: u32 = 0xc200c;

/// `_FDI_RXB_CHICKEN` (intel_fdi_regs.h)
pub const _FDI_RXB_CHICKEN: u32 = 0xc2010;

/// `FDI_RX_PHASE_SYNC_POINTER_OVR` (intel_fdi_regs.h)
pub const FDI_RX_PHASE_SYNC_POINTER_OVR: u32 = ((1 << 1));

/// `FDI_RX_PHASE_SYNC_POINTER_EN` (intel_fdi_regs.h)
pub const FDI_RX_PHASE_SYNC_POINTER_EN: u32 = ((1 << 0));

/// `FDI_RX_CHICKEN(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_CHICKEN(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_CHICKEN, _FDI_RXB_CHICKEN)
}

/// `_FDI_TXA_CTL` (intel_fdi_regs.h)
pub const _FDI_TXA_CTL: u32 = 0x60100;

/// `_FDI_TXB_CTL` (intel_fdi_regs.h)
pub const _FDI_TXB_CTL: u32 = 0x61100;

/// `FDI_TX_CTL(pipe)` (intel_fdi_regs.h)
pub const fn FDI_TX_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_TXA_CTL, _FDI_TXB_CTL)
}

/// `FDI_TX_DISABLE` (intel_fdi_regs.h)
pub const FDI_TX_DISABLE: u32 = ((0 << 31));

/// `FDI_TX_ENABLE` (intel_fdi_regs.h)
pub const FDI_TX_ENABLE: u32 = ((1 << 31));

/// `FDI_LINK_TRAIN_PATTERN_1` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_1: u32 = ((0 << 28));

/// `FDI_LINK_TRAIN_PATTERN_2` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_2: u32 = ((1 << 28));

/// `FDI_LINK_TRAIN_PATTERN_IDLE` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_IDLE: u32 = ((2 << 28));

/// `FDI_LINK_TRAIN_NONE` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_NONE: u32 = ((3 << 28));

/// `FDI_LINK_TRAIN_VOLTAGE_0_4V` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_VOLTAGE_0_4V: u32 = ((0 << 25));

/// `FDI_LINK_TRAIN_VOLTAGE_0_6V` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_VOLTAGE_0_6V: u32 = ((1 << 25));

/// `FDI_LINK_TRAIN_VOLTAGE_0_8V` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_VOLTAGE_0_8V: u32 = ((2 << 25));

/// `FDI_LINK_TRAIN_VOLTAGE_1_2V` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_VOLTAGE_1_2V: u32 = ((3 << 25));

/// `FDI_LINK_TRAIN_PRE_EMPHASIS_NONE` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PRE_EMPHASIS_NONE: u32 = ((0 << 22));

/// `FDI_LINK_TRAIN_PRE_EMPHASIS_1_5X` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PRE_EMPHASIS_1_5X: u32 = ((1 << 22));

/// `FDI_LINK_TRAIN_PRE_EMPHASIS_2X` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PRE_EMPHASIS_2X: u32 = ((2 << 22));

/// `FDI_LINK_TRAIN_PRE_EMPHASIS_3X` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PRE_EMPHASIS_3X: u32 = ((3 << 22));

/// `FDI_LINK_TRAIN_400MV_0DB_SNB_A` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_400MV_0DB_SNB_A: u32 = ((0x38 << 22));

/// `FDI_LINK_TRAIN_400MV_6DB_SNB_A` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_400MV_6DB_SNB_A: u32 = ((0x02 << 22));

/// `FDI_LINK_TRAIN_600MV_3_5DB_SNB_A` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_600MV_3_5DB_SNB_A: u32 = ((0x01 << 22));

/// `FDI_LINK_TRAIN_800MV_0DB_SNB_A` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_800MV_0DB_SNB_A: u32 = ((0x0 << 22));

/// `FDI_LINK_TRAIN_400MV_0DB_SNB_B` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_400MV_0DB_SNB_B: u32 = ((0x0 << 22));

/// `FDI_LINK_TRAIN_400MV_6DB_SNB_B` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_400MV_6DB_SNB_B: u32 = ((0x3a << 22));

/// `FDI_LINK_TRAIN_600MV_3_5DB_SNB_B` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_600MV_3_5DB_SNB_B: u32 = ((0x39 << 22));

/// `FDI_LINK_TRAIN_800MV_0DB_SNB_B` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_800MV_0DB_SNB_B: u32 = ((0x38 << 22));

/// `FDI_LINK_TRAIN_VOL_EMP_MASK` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_VOL_EMP_MASK: u32 = ((0x3f << 22));

/// `FDI_DP_PORT_WIDTH_SHIFT` (intel_fdi_regs.h)
pub const FDI_DP_PORT_WIDTH_SHIFT: u32 = 19;

/// `FDI_DP_PORT_WIDTH_MASK` (intel_fdi_regs.h)
pub const FDI_DP_PORT_WIDTH_MASK: u32 = ((7 << FDI_DP_PORT_WIDTH_SHIFT));

/// `FDI_DP_PORT_WIDTH(width)` (intel_fdi_regs.h)
pub const fn FDI_DP_PORT_WIDTH(width: u32) -> u32 {
    (((((width) - 1)) << FDI_DP_PORT_WIDTH_SHIFT))
}

/// `FDI_TX_ENHANCE_FRAME_ENABLE` (intel_fdi_regs.h)
pub const FDI_TX_ENHANCE_FRAME_ENABLE: u32 = ((1 << 18));

/// `FDI_TX_PLL_ENABLE` (intel_fdi_regs.h)
pub const FDI_TX_PLL_ENABLE: u32 = ((1 << 14));

/// `FDI_LINK_TRAIN_PATTERN_1_IVB` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_1_IVB: u32 = ((0 << 8));

/// `FDI_LINK_TRAIN_PATTERN_2_IVB` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_2_IVB: u32 = ((1 << 8));

/// `FDI_LINK_TRAIN_PATTERN_IDLE_IVB` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_IDLE_IVB: u32 = ((2 << 8));

/// `FDI_LINK_TRAIN_NONE_IVB` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_NONE_IVB: u32 = ((3 << 8));

/// `FDI_COMPOSITE_SYNC` (intel_fdi_regs.h)
pub const FDI_COMPOSITE_SYNC: u32 = ((1 << 11));

/// `FDI_LINK_TRAIN_AUTO` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_AUTO: u32 = ((1 << 10));

/// `FDI_SCRAMBLING_ENABLE` (intel_fdi_regs.h)
pub const FDI_SCRAMBLING_ENABLE: u32 = ((0 << 7));

/// `FDI_SCRAMBLING_DISABLE` (intel_fdi_regs.h)
pub const FDI_SCRAMBLING_DISABLE: u32 = ((1 << 7));

/// `_FDI_RXA_CTL` (intel_fdi_regs.h)
pub const _FDI_RXA_CTL: u32 = 0xf000c;

/// `_FDI_RXB_CTL` (intel_fdi_regs.h)
pub const _FDI_RXB_CTL: u32 = 0xf100c;

/// `FDI_RX_CTL(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_CTL, _FDI_RXB_CTL)
}

/// `FDI_RX_ENABLE` (intel_fdi_regs.h)
pub const FDI_RX_ENABLE: u32 = ((1 << 31));

/// `FDI_FS_ERRC_ENABLE` (intel_fdi_regs.h)
pub const FDI_FS_ERRC_ENABLE: u32 = ((1 << 27));

/// `FDI_FE_ERRC_ENABLE` (intel_fdi_regs.h)
pub const FDI_FE_ERRC_ENABLE: u32 = ((1 << 26));

/// `FDI_RX_POLARITY_REVERSED_LPT` (intel_fdi_regs.h)
pub const FDI_RX_POLARITY_REVERSED_LPT: u32 = ((1 << 16));

/// `FDI_8BPC` (intel_fdi_regs.h)
pub const FDI_8BPC: u32 = ((0 << 16));

/// `FDI_10BPC` (intel_fdi_regs.h)
pub const FDI_10BPC: u32 = ((1 << 16));

/// `FDI_6BPC` (intel_fdi_regs.h)
pub const FDI_6BPC: u32 = ((2 << 16));

/// `FDI_12BPC` (intel_fdi_regs.h)
pub const FDI_12BPC: u32 = ((3 << 16));

/// `FDI_RX_LINK_REVERSAL_OVERRIDE` (intel_fdi_regs.h)
pub const FDI_RX_LINK_REVERSAL_OVERRIDE: u32 = ((1 << 15));

/// `FDI_DMI_LINK_REVERSE_MASK` (intel_fdi_regs.h)
pub const FDI_DMI_LINK_REVERSE_MASK: u32 = ((1 << 14));

/// `FDI_RX_PLL_ENABLE` (intel_fdi_regs.h)
pub const FDI_RX_PLL_ENABLE: u32 = ((1 << 13));

/// `FDI_FS_ERR_CORRECT_ENABLE` (intel_fdi_regs.h)
pub const FDI_FS_ERR_CORRECT_ENABLE: u32 = ((1 << 11));

/// `FDI_FE_ERR_CORRECT_ENABLE` (intel_fdi_regs.h)
pub const FDI_FE_ERR_CORRECT_ENABLE: u32 = ((1 << 10));

/// `FDI_FS_ERR_REPORT_ENABLE` (intel_fdi_regs.h)
pub const FDI_FS_ERR_REPORT_ENABLE: u32 = ((1 << 9));

/// `FDI_FE_ERR_REPORT_ENABLE` (intel_fdi_regs.h)
pub const FDI_FE_ERR_REPORT_ENABLE: u32 = ((1 << 8));

/// `FDI_RX_ENHANCE_FRAME_ENABLE` (intel_fdi_regs.h)
pub const FDI_RX_ENHANCE_FRAME_ENABLE: u32 = ((1 << 6));

/// `FDI_PCDCLK` (intel_fdi_regs.h)
pub const FDI_PCDCLK: u32 = ((1 << 4));

/// `FDI_AUTO_TRAINING` (intel_fdi_regs.h)
pub const FDI_AUTO_TRAINING: u32 = ((1 << 10));

/// `FDI_LINK_TRAIN_PATTERN_1_CPT` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_1_CPT: u32 = ((0 << 8));

/// `FDI_LINK_TRAIN_PATTERN_2_CPT` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_2_CPT: u32 = ((1 << 8));

/// `FDI_LINK_TRAIN_PATTERN_IDLE_CPT` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_IDLE_CPT: u32 = ((2 << 8));

/// `FDI_LINK_TRAIN_NORMAL_CPT` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_NORMAL_CPT: u32 = ((3 << 8));

/// `FDI_LINK_TRAIN_PATTERN_MASK_CPT` (intel_fdi_regs.h)
pub const FDI_LINK_TRAIN_PATTERN_MASK_CPT: u32 = ((3 << 8));

/// `_FDI_RXA_MISC` (intel_fdi_regs.h)
pub const _FDI_RXA_MISC: u32 = 0xf0010;

/// `_FDI_RXB_MISC` (intel_fdi_regs.h)
pub const _FDI_RXB_MISC: u32 = 0xf1010;

/// `FDI_RX_PWRDN_LANE1_MASK` (intel_fdi_regs.h)
pub const FDI_RX_PWRDN_LANE1_MASK: u32 = ((3 << 26));

/// `FDI_RX_PWRDN_LANE1_VAL(x)` (intel_fdi_regs.h)
pub const fn FDI_RX_PWRDN_LANE1_VAL(x: u32) -> u32 {
    (((x) << 26))
}

/// `FDI_RX_PWRDN_LANE0_MASK` (intel_fdi_regs.h)
pub const FDI_RX_PWRDN_LANE0_MASK: u32 = ((3 << 24));

/// `FDI_RX_PWRDN_LANE0_VAL(x)` (intel_fdi_regs.h)
pub const fn FDI_RX_PWRDN_LANE0_VAL(x: u32) -> u32 {
    (((x) << 24))
}

/// `FDI_RX_TP1_TO_TP2_48` (intel_fdi_regs.h)
pub const FDI_RX_TP1_TO_TP2_48: u32 = ((2 << 20));

/// `FDI_RX_TP1_TO_TP2_64` (intel_fdi_regs.h)
pub const FDI_RX_TP1_TO_TP2_64: u32 = ((3 << 20));

/// `FDI_RX_FDI_DELAY_90` (intel_fdi_regs.h)
pub const FDI_RX_FDI_DELAY_90: u32 = ((0x90 << 0));

/// `FDI_RX_MISC(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_MISC(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_MISC, _FDI_RXB_MISC)
}

/// `_FDI_RXA_TUSIZE1` (intel_fdi_regs.h)
pub const _FDI_RXA_TUSIZE1: u32 = 0xf0030;

/// `_FDI_RXA_TUSIZE2` (intel_fdi_regs.h)
pub const _FDI_RXA_TUSIZE2: u32 = 0xf0038;

/// `_FDI_RXB_TUSIZE1` (intel_fdi_regs.h)
pub const _FDI_RXB_TUSIZE1: u32 = 0xf1030;

/// `_FDI_RXB_TUSIZE2` (intel_fdi_regs.h)
pub const _FDI_RXB_TUSIZE2: u32 = 0xf1038;

/// `FDI_RX_TUSIZE1(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_TUSIZE1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_TUSIZE1, _FDI_RXB_TUSIZE1)
}

/// `FDI_RX_TUSIZE2(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_TUSIZE2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_TUSIZE2, _FDI_RXB_TUSIZE2)
}

/// `FDI_RX_INTER_LANE_ALIGN` (intel_fdi_regs.h)
pub const FDI_RX_INTER_LANE_ALIGN: u32 = ((1 << 10));

/// `FDI_RX_SYMBOL_LOCK` (intel_fdi_regs.h)
pub const FDI_RX_SYMBOL_LOCK: u32 = ((1 << 9));

/// `FDI_RX_BIT_LOCK` (intel_fdi_regs.h)
pub const FDI_RX_BIT_LOCK: u32 = ((1 << 8));

/// `FDI_RX_TRAIN_PATTERN_2_FAIL` (intel_fdi_regs.h)
pub const FDI_RX_TRAIN_PATTERN_2_FAIL: u32 = ((1 << 7));

/// `FDI_RX_FS_CODE_ERR` (intel_fdi_regs.h)
pub const FDI_RX_FS_CODE_ERR: u32 = ((1 << 6));

/// `FDI_RX_FE_CODE_ERR` (intel_fdi_regs.h)
pub const FDI_RX_FE_CODE_ERR: u32 = ((1 << 5));

/// `FDI_RX_SYMBOL_ERR_RATE_ABOVE` (intel_fdi_regs.h)
pub const FDI_RX_SYMBOL_ERR_RATE_ABOVE: u32 = ((1 << 4));

/// `FDI_RX_HDCP_LINK_FAIL` (intel_fdi_regs.h)
pub const FDI_RX_HDCP_LINK_FAIL: u32 = ((1 << 3));

/// `FDI_RX_PIXEL_FIFO_OVERFLOW` (intel_fdi_regs.h)
pub const FDI_RX_PIXEL_FIFO_OVERFLOW: u32 = ((1 << 2));

/// `FDI_RX_CROSS_CLOCK_OVERFLOW` (intel_fdi_regs.h)
pub const FDI_RX_CROSS_CLOCK_OVERFLOW: u32 = ((1 << 1));

/// `FDI_RX_SYMBOL_QUEUE_OVERFLOW` (intel_fdi_regs.h)
pub const FDI_RX_SYMBOL_QUEUE_OVERFLOW: u32 = ((1 << 0));

/// `_FDI_RXA_IIR` (intel_fdi_regs.h)
pub const _FDI_RXA_IIR: u32 = 0xf0014;

/// `_FDI_RXA_IMR` (intel_fdi_regs.h)
pub const _FDI_RXA_IMR: u32 = 0xf0018;

/// `_FDI_RXB_IIR` (intel_fdi_regs.h)
pub const _FDI_RXB_IIR: u32 = 0xf1014;

/// `_FDI_RXB_IMR` (intel_fdi_regs.h)
pub const _FDI_RXB_IMR: u32 = 0xf1018;

/// `FDI_RX_IIR(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_IIR(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_IIR, _FDI_RXB_IIR)
}

/// `FDI_RX_IMR(pipe)` (intel_fdi_regs.h)
pub const fn FDI_RX_IMR(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FDI_RXA_IMR, _FDI_RXB_IMR)
}

/// `FDI_PLL_CTL_1` (intel_fdi_regs.h)
pub const FDI_PLL_CTL_1: I915RegT = mmio(0xfe000);

/// `FDI_PLL_CTL_2` (intel_fdi_regs.h)
pub const FDI_PLL_CTL_2: I915RegT = mmio(0xfe004);
