// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/bxt_dpio_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_BXT_PHY0_BASE` (bxt_dpio_phy_regs.h)
pub const _BXT_PHY0_BASE: u32 = 0x6C000;

/// `_BXT_PHY1_BASE` (bxt_dpio_phy_regs.h)
pub const _BXT_PHY1_BASE: u32 = 0x162000;

/// `_BXT_PHY2_BASE` (bxt_dpio_phy_regs.h)
pub const _BXT_PHY2_BASE: u32 = 0x163000;

/// `BXT_PHY_BASE(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PHY_BASE(phy: u32) -> u32 {
    pick_even_2ranges(phy, 1, _BXT_PHY0_BASE, _BXT_PHY0_BASE, _BXT_PHY1_BASE, _BXT_PHY2_BASE)
}

/// `_BXT_PHY(phy, reg)` (bxt_dpio_phy_regs.h)
pub const fn _BXT_PHY(phy: u32, reg: u32) -> I915RegT {
    mmio(((BXT_PHY_BASE(phy) - _BXT_PHY0_BASE) + (reg)))
}

/// `_BXT_PHY_CH(phy, ch, reg_ch0, reg_ch1)` (bxt_dpio_phy_regs.h)
pub const fn _BXT_PHY_CH(phy: u32, ch: u32, reg_ch0: u32, reg_ch1: u32) -> u32 {
    ((BXT_PHY_BASE(phy) + _PIPE((ch), ((reg_ch0) - _BXT_PHY0_BASE), ((reg_ch1) - _BXT_PHY0_BASE))))
}

/// `_MMIO_BXT_PHY_CH(phy, ch, reg_ch0, reg_ch1)` (bxt_dpio_phy_regs.h)
pub const fn _MMIO_BXT_PHY_CH(phy: u32, ch: u32, reg_ch0: u32, reg_ch1: u32) -> I915RegT {
    mmio(_BXT_PHY_CH(phy, ch, reg_ch0, reg_ch1))
}

/// `_BXT_LANE_OFFSET(lane)` (bxt_dpio_phy_regs.h)
pub const fn _BXT_LANE_OFFSET(lane: u32) -> u32 {
    ((((((lane) >> 1)) * 0x200) + ((((lane) & 1)) * 0x80)))
}

/// `_MMIO_BXT_PHY_CH_LN(phy, ch, lane, reg_ch0, reg_ch1)` (bxt_dpio_phy_regs.h)
pub const fn _MMIO_BXT_PHY_CH_LN(phy: u32, ch: u32, lane: u32, reg_ch0: u32, reg_ch1: u32) -> I915RegT {
    mmio((_BXT_PHY_CH(phy, ch, reg_ch0, reg_ch1) + _BXT_LANE_OFFSET(lane)))
}

/// `_PORT_PLL_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_A: u32 = 0x46074;

/// `_PORT_PLL_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_B: u32 = 0x46078;

/// `_PORT_PLL_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_C: u32 = 0x4607c;

/// `PORT_PLL_ENABLE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_ENABLE: u32 = reg_bit(31);

/// `PORT_PLL_LOCK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_LOCK: u32 = reg_bit(30);

/// `PORT_PLL_REF_SEL` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_REF_SEL: u32 = reg_bit(27);

/// `PORT_PLL_POWER_ENABLE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_POWER_ENABLE: u32 = reg_bit(26);

/// `PORT_PLL_POWER_STATE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_POWER_STATE: u32 = reg_bit(25);

/// `BXT_PORT_PLL_ENABLE(port)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PLL_ENABLE(port: u32) -> I915RegT {
    _MMIO_PORT(port, _PORT_PLL_A, _PORT_PLL_B)
}

/// `_PORT_PLL_EBB_0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_0_A: u32 = 0x162034;

/// `_PORT_PLL_EBB_0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_0_B: u32 = 0x6C034;

/// `_PORT_PLL_EBB_0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_0_C: u32 = 0x6C340;

/// `PORT_PLL_P1_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_P1_MASK: u32 = reg_genmask(15, 13);

/// `PORT_PLL_P1(p1)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_P1(p1: u32) -> u32 {
    reg_field_prep(PORT_PLL_P1_MASK, (p1))
}

/// `PORT_PLL_P2_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_P2_MASK: u32 = reg_genmask(12, 8);

/// `PORT_PLL_P2(p2)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_P2(p2: u32) -> u32 {
    reg_field_prep(PORT_PLL_P2_MASK, (p2))
}

/// `BXT_PORT_PLL_EBB_0(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PLL_EBB_0(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PLL_EBB_0_B, _PORT_PLL_EBB_0_C)
}

/// `_PORT_PLL_EBB_4_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_4_A: u32 = 0x162038;

/// `_PORT_PLL_EBB_4_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_4_B: u32 = 0x6C038;

/// `_PORT_PLL_EBB_4_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_EBB_4_C: u32 = 0x6C344;

/// `PORT_PLL_RECALIBRATE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_RECALIBRATE: u32 = reg_bit(14);

/// `PORT_PLL_10BIT_CLK_ENABLE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_10BIT_CLK_ENABLE: u32 = reg_bit(13);

/// `BXT_PORT_PLL_EBB_4(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PLL_EBB_4(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PLL_EBB_4_B, _PORT_PLL_EBB_4_C)
}

/// `_PORT_PLL_0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_0_A: u32 = 0x162100;

/// `_PORT_PLL_0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_0_B: u32 = 0x6C100;

/// `_PORT_PLL_0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PLL_0_C: u32 = 0x6C380;

/// `PORT_PLL_M2_INT_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_M2_INT_MASK: u32 = reg_genmask(7, 0);

/// `PORT_PLL_M2_INT(m2_int)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_M2_INT(m2_int: u32) -> u32 {
    reg_field_prep(PORT_PLL_M2_INT_MASK, (m2_int))
}

/// `PORT_PLL_N_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_N_MASK: u32 = reg_genmask(11, 8);

/// `PORT_PLL_N(n)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_N(n: u32) -> u32 {
    reg_field_prep(PORT_PLL_N_MASK, (n))
}

/// `PORT_PLL_M2_FRAC_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_M2_FRAC_MASK: u32 = reg_genmask(21, 0);

/// `PORT_PLL_M2_FRAC(m2_frac)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_M2_FRAC(m2_frac: u32) -> u32 {
    reg_field_prep(PORT_PLL_M2_FRAC_MASK, (m2_frac))
}

/// `PORT_PLL_M2_FRAC_ENABLE` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_M2_FRAC_ENABLE: u32 = reg_bit(16);

/// `PORT_PLL_GAIN_CTL_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_GAIN_CTL_MASK: u32 = reg_genmask(18, 16);

/// `PORT_PLL_GAIN_CTL(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_GAIN_CTL(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_GAIN_CTL_MASK, (x))
}

/// `PORT_PLL_INT_COEFF_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_INT_COEFF_MASK: u32 = reg_genmask(12, 8);

/// `PORT_PLL_INT_COEFF(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_INT_COEFF(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_INT_COEFF_MASK, (x))
}

/// `PORT_PLL_PROP_COEFF_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_PROP_COEFF_MASK: u32 = reg_genmask(3, 0);

/// `PORT_PLL_PROP_COEFF(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_PROP_COEFF(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_PROP_COEFF_MASK, (x))
}

/// `PORT_PLL_TARGET_CNT_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_TARGET_CNT_MASK: u32 = reg_genmask(9, 0);

/// `PORT_PLL_TARGET_CNT(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_TARGET_CNT(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_TARGET_CNT_MASK, (x))
}

/// `PORT_PLL_LOCK_THRESHOLD_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_LOCK_THRESHOLD_MASK: u32 = reg_genmask(3, 1);

/// `PORT_PLL_LOCK_THRESHOLD(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_LOCK_THRESHOLD(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_LOCK_THRESHOLD_MASK, (x))
}

/// `PORT_PLL_DCO_AMP_MASK` (bxt_dpio_phy_regs.h)
pub const PORT_PLL_DCO_AMP_MASK: u32 = reg_genmask(13, 10);

/// `PORT_PLL_DCO_AMP(x)` (bxt_dpio_phy_regs.h)
pub const fn PORT_PLL_DCO_AMP(x: u32) -> u32 {
    reg_field_prep(PORT_PLL_DCO_AMP_MASK, (x))
}

/// `_PORT_PLL_BASE(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn _PORT_PLL_BASE(phy: u32, ch: u32) -> u32 {
    _BXT_PHY_CH(phy, ch, _PORT_PLL_0_B, _PORT_PLL_0_C)
}

/// `BXT_PORT_PLL(phy, ch, idx)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PLL(phy: u32, ch: u32, idx: u32) -> I915RegT {
    mmio((_PORT_PLL_BASE(phy, ch) + ((idx) * 4)))
}

/// `_PORT_CL1CM_DW0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW0_A: u32 = 0x162000;

/// `_PORT_CL1CM_DW0_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW0_BC: u32 = 0x6C000;

/// `PHY_POWER_GOOD` (bxt_dpio_phy_regs.h)
pub const PHY_POWER_GOOD: u32 = reg_bit(16);

/// `PHY_RESERVED` (bxt_dpio_phy_regs.h)
pub const PHY_RESERVED: u32 = reg_bit(7);

/// `BXT_PORT_CL1CM_DW0(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL1CM_DW0(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL1CM_DW0_BC)
}

/// `_PORT_CL1CM_DW9_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW9_A: u32 = 0x162024;

/// `_PORT_CL1CM_DW9_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW9_BC: u32 = 0x6C024;

/// `IREF0RC_OFFSET_MASK` (bxt_dpio_phy_regs.h)
pub const IREF0RC_OFFSET_MASK: u32 = reg_genmask(15, 8);

/// `IREF0RC_OFFSET(x)` (bxt_dpio_phy_regs.h)
pub const fn IREF0RC_OFFSET(x: u32) -> u32 {
    reg_field_prep(IREF0RC_OFFSET_MASK, (x))
}

/// `BXT_PORT_CL1CM_DW9(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL1CM_DW9(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL1CM_DW9_BC)
}

/// `_PORT_CL1CM_DW10_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW10_A: u32 = 0x162028;

/// `_PORT_CL1CM_DW10_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW10_BC: u32 = 0x6C028;

/// `IREF1RC_OFFSET_MASK` (bxt_dpio_phy_regs.h)
pub const IREF1RC_OFFSET_MASK: u32 = reg_genmask(15, 8);

/// `IREF1RC_OFFSET(x)` (bxt_dpio_phy_regs.h)
pub const fn IREF1RC_OFFSET(x: u32) -> u32 {
    reg_field_prep(IREF1RC_OFFSET_MASK, (x))
}

/// `BXT_PORT_CL1CM_DW10(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL1CM_DW10(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL1CM_DW10_BC)
}

/// `_PORT_CL1CM_DW28_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW28_A: u32 = 0x162070;

/// `_PORT_CL1CM_DW28_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW28_BC: u32 = 0x6C070;

/// `OCL1_POWER_DOWN_EN` (bxt_dpio_phy_regs.h)
pub const OCL1_POWER_DOWN_EN: u32 = reg_bit(23);

/// `DW28_OLDO_DYN_PWR_DOWN_EN` (bxt_dpio_phy_regs.h)
pub const DW28_OLDO_DYN_PWR_DOWN_EN: u32 = reg_bit(22);

/// `SUS_CLK_CONFIG` (bxt_dpio_phy_regs.h)
pub const SUS_CLK_CONFIG: u32 = reg_genmask(1, 0);

/// `BXT_PORT_CL1CM_DW28(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL1CM_DW28(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL1CM_DW28_BC)
}

/// `_PORT_CL1CM_DW30_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW30_A: u32 = 0x162078;

/// `_PORT_CL1CM_DW30_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL1CM_DW30_BC: u32 = 0x6C078;

/// `OCL2_LDOFUSE_PWR_DIS` (bxt_dpio_phy_regs.h)
pub const OCL2_LDOFUSE_PWR_DIS: u32 = reg_bit(6);

/// `BXT_PORT_CL1CM_DW30(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL1CM_DW30(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL1CM_DW30_BC)
}

/// `_PORT_CL2CM_DW6_A` (bxt_dpio_phy_regs.h)
pub const _PORT_CL2CM_DW6_A: u32 = 0x162358;

/// `_PORT_CL2CM_DW6_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_CL2CM_DW6_BC: u32 = 0x6C358;

/// `BXT_PORT_CL2CM_DW6(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_CL2CM_DW6(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_CL2CM_DW6_BC)
}

/// `DW6_OLDO_DYN_PWR_DOWN_EN` (bxt_dpio_phy_regs.h)
pub const DW6_OLDO_DYN_PWR_DOWN_EN: u32 = reg_bit(28);

/// `_PORT_REF_DW3_A` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW3_A: u32 = 0x16218C;

/// `_PORT_REF_DW3_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW3_BC: u32 = 0x6C18C;

/// `GRC_DONE` (bxt_dpio_phy_regs.h)
pub const GRC_DONE: u32 = reg_bit(22);

/// `BXT_PORT_REF_DW3(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_REF_DW3(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_REF_DW3_BC)
}

/// `_PORT_REF_DW6_A` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW6_A: u32 = 0x162198;

/// `_PORT_REF_DW6_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW6_BC: u32 = 0x6C198;

/// `GRC_CODE_MASK` (bxt_dpio_phy_regs.h)
pub const GRC_CODE_MASK: u32 = reg_genmask(31, 24);

/// `GRC_CODE(x)` (bxt_dpio_phy_regs.h)
pub const fn GRC_CODE(x: u32) -> u32 {
    reg_field_prep(GRC_CODE_MASK, (x))
}

/// `GRC_CODE_FAST_MASK` (bxt_dpio_phy_regs.h)
pub const GRC_CODE_FAST_MASK: u32 = reg_genmask(23, 16);

/// `GRC_CODE_FAST(x)` (bxt_dpio_phy_regs.h)
pub const fn GRC_CODE_FAST(x: u32) -> u32 {
    reg_field_prep(GRC_CODE_FAST_MASK, (x))
}

/// `GRC_CODE_SLOW_MASK` (bxt_dpio_phy_regs.h)
pub const GRC_CODE_SLOW_MASK: u32 = reg_genmask(15, 8);

/// `GRC_CODE_SLOW(x)` (bxt_dpio_phy_regs.h)
pub const fn GRC_CODE_SLOW(x: u32) -> u32 {
    reg_field_prep(GRC_CODE_SLOW_MASK, (x))
}

/// `GRC_CODE_NOM_MASK` (bxt_dpio_phy_regs.h)
pub const GRC_CODE_NOM_MASK: u32 = reg_genmask(7, 0);

/// `GRC_CODE_NOM(x)` (bxt_dpio_phy_regs.h)
pub const fn GRC_CODE_NOM(x: u32) -> u32 {
    reg_field_prep(GRC_CODE_NOM_MASK, (x))
}

/// `BXT_PORT_REF_DW6(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_REF_DW6(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_REF_DW6_BC)
}

/// `_PORT_REF_DW8_A` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW8_A: u32 = 0x1621A0;

/// `_PORT_REF_DW8_BC` (bxt_dpio_phy_regs.h)
pub const _PORT_REF_DW8_BC: u32 = 0x6C1A0;

/// `GRC_DIS` (bxt_dpio_phy_regs.h)
pub const GRC_DIS: u32 = reg_bit(15);

/// `GRC_RDY_OVRD` (bxt_dpio_phy_regs.h)
pub const GRC_RDY_OVRD: u32 = reg_bit(1);

/// `BXT_PORT_REF_DW8(phy)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_REF_DW8(phy: u32) -> I915RegT {
    _BXT_PHY((phy), _PORT_REF_DW8_BC)
}

/// `_PORT_PCS_DW10_LN01_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_LN01_A: u32 = 0x162428;

/// `_PORT_PCS_DW10_LN01_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_LN01_B: u32 = 0x6C428;

/// `_PORT_PCS_DW10_LN01_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_LN01_C: u32 = 0x6C828;

/// `_PORT_PCS_DW10_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_GRP_A: u32 = 0x162C28;

/// `_PORT_PCS_DW10_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_GRP_B: u32 = 0x6CC28;

/// `_PORT_PCS_DW10_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW10_GRP_C: u32 = 0x6CE28;

/// `BXT_PORT_PCS_DW10_LN01(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PCS_DW10_LN01(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PCS_DW10_LN01_B, _PORT_PCS_DW10_LN01_C)
}

/// `BXT_PORT_PCS_DW10_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PCS_DW10_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PCS_DW10_GRP_B, _PORT_PCS_DW10_GRP_C)
}

/// `TX2_SWING_CALC_INIT` (bxt_dpio_phy_regs.h)
pub const TX2_SWING_CALC_INIT: u32 = reg_bit(31);

/// `TX1_SWING_CALC_INIT` (bxt_dpio_phy_regs.h)
pub const TX1_SWING_CALC_INIT: u32 = reg_bit(30);

/// `_PORT_PCS_DW12_LN01_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN01_A: u32 = 0x162430;

/// `_PORT_PCS_DW12_LN01_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN01_B: u32 = 0x6C430;

/// `_PORT_PCS_DW12_LN01_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN01_C: u32 = 0x6C830;

/// `_PORT_PCS_DW12_LN23_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN23_A: u32 = 0x162630;

/// `_PORT_PCS_DW12_LN23_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN23_B: u32 = 0x6C630;

/// `_PORT_PCS_DW12_LN23_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_LN23_C: u32 = 0x6CA30;

/// `_PORT_PCS_DW12_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_GRP_A: u32 = 0x162c30;

/// `_PORT_PCS_DW12_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_GRP_B: u32 = 0x6CC30;

/// `_PORT_PCS_DW12_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_PCS_DW12_GRP_C: u32 = 0x6CE30;

/// `LANESTAGGER_STRAP_OVRD` (bxt_dpio_phy_regs.h)
pub const LANESTAGGER_STRAP_OVRD: u32 = reg_bit(6);

/// `LANE_STAGGER_MASK` (bxt_dpio_phy_regs.h)
pub const LANE_STAGGER_MASK: u32 = reg_genmask(4, 0);

/// `BXT_PORT_PCS_DW12_LN01(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PCS_DW12_LN01(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PCS_DW12_LN01_B, _PORT_PCS_DW12_LN01_C)
}

/// `BXT_PORT_PCS_DW12_LN23(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PCS_DW12_LN23(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PCS_DW12_LN23_B, _PORT_PCS_DW12_LN23_C)
}

/// `BXT_PORT_PCS_DW12_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_PCS_DW12_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_PCS_DW12_GRP_B, _PORT_PCS_DW12_GRP_C)
}

/// `_PORT_TX_DW2_LN0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_LN0_A: u32 = 0x162508;

/// `_PORT_TX_DW2_LN0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_LN0_B: u32 = 0x6C508;

/// `_PORT_TX_DW2_LN0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_LN0_C: u32 = 0x6C908;

/// `_PORT_TX_DW2_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_GRP_A: u32 = 0x162D08;

/// `_PORT_TX_DW2_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_GRP_B: u32 = 0x6CD08;

/// `_PORT_TX_DW2_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW2_GRP_C: u32 = 0x6CF08;

/// `BXT_PORT_TX_DW2_LN(phy, ch, lane)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW2_LN(phy: u32, ch: u32, lane: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH_LN(phy, ch, lane, _PORT_TX_DW2_LN0_B, _PORT_TX_DW2_LN0_C)
}

/// `BXT_PORT_TX_DW2_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW2_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_TX_DW2_GRP_B, _PORT_TX_DW2_GRP_C)
}

/// `MARGIN_000_MASK` (bxt_dpio_phy_regs.h)
pub const MARGIN_000_MASK: u32 = reg_genmask(23, 16);

/// `MARGIN_000(x)` (bxt_dpio_phy_regs.h)
pub const fn MARGIN_000(x: u32) -> u32 {
    reg_field_prep(MARGIN_000_MASK, (x))
}

/// `UNIQ_TRANS_SCALE_MASK` (bxt_dpio_phy_regs.h)
pub const UNIQ_TRANS_SCALE_MASK: u32 = reg_genmask(15, 8);

/// `UNIQ_TRANS_SCALE(x)` (bxt_dpio_phy_regs.h)
pub const fn UNIQ_TRANS_SCALE(x: u32) -> u32 {
    reg_field_prep(UNIQ_TRANS_SCALE_MASK, (x))
}

/// `_PORT_TX_DW3_LN0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_LN0_A: u32 = 0x16250C;

/// `_PORT_TX_DW3_LN0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_LN0_B: u32 = 0x6C50C;

/// `_PORT_TX_DW3_LN0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_LN0_C: u32 = 0x6C90C;

/// `_PORT_TX_DW3_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_GRP_A: u32 = 0x162D0C;

/// `_PORT_TX_DW3_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_GRP_B: u32 = 0x6CD0C;

/// `_PORT_TX_DW3_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW3_GRP_C: u32 = 0x6CF0C;

/// `BXT_PORT_TX_DW3_LN(phy, ch, lane)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW3_LN(phy: u32, ch: u32, lane: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH_LN(phy, ch, lane, _PORT_TX_DW3_LN0_B, _PORT_TX_DW3_LN0_C)
}

/// `BXT_PORT_TX_DW3_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW3_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_TX_DW3_GRP_B, _PORT_TX_DW3_GRP_C)
}

/// `SCALE_DCOMP_METHOD` (bxt_dpio_phy_regs.h)
pub const SCALE_DCOMP_METHOD: u32 = reg_bit(26);

/// `UNIQUE_TRANGE_EN_METHOD` (bxt_dpio_phy_regs.h)
pub const UNIQUE_TRANGE_EN_METHOD: u32 = reg_bit(27);

/// `_PORT_TX_DW4_LN0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_LN0_A: u32 = 0x162510;

/// `_PORT_TX_DW4_LN0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_LN0_B: u32 = 0x6C510;

/// `_PORT_TX_DW4_LN0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_LN0_C: u32 = 0x6C910;

/// `_PORT_TX_DW4_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_GRP_A: u32 = 0x162D10;

/// `_PORT_TX_DW4_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_GRP_B: u32 = 0x6CD10;

/// `_PORT_TX_DW4_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW4_GRP_C: u32 = 0x6CF10;

/// `BXT_PORT_TX_DW4_LN(phy, ch, lane)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW4_LN(phy: u32, ch: u32, lane: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH_LN(phy, ch, lane, _PORT_TX_DW4_LN0_B, _PORT_TX_DW4_LN0_C)
}

/// `BXT_PORT_TX_DW4_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW4_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_TX_DW4_GRP_B, _PORT_TX_DW4_GRP_C)
}

/// `DE_EMPHASIS_MASK` (bxt_dpio_phy_regs.h)
pub const DE_EMPHASIS_MASK: u32 = reg_genmask(31, 24);

/// `DE_EMPHASIS(x)` (bxt_dpio_phy_regs.h)
pub const fn DE_EMPHASIS(x: u32) -> u32 {
    reg_field_prep(DE_EMPHASIS_MASK, (x))
}

/// `_PORT_TX_DW5_LN0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_LN0_A: u32 = 0x162514;

/// `_PORT_TX_DW5_LN0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_LN0_B: u32 = 0x6C514;

/// `_PORT_TX_DW5_LN0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_LN0_C: u32 = 0x6C914;

/// `_PORT_TX_DW5_GRP_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_GRP_A: u32 = 0x162D14;

/// `_PORT_TX_DW5_GRP_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_GRP_B: u32 = 0x6CD14;

/// `_PORT_TX_DW5_GRP_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW5_GRP_C: u32 = 0x6CF14;

/// `BXT_PORT_TX_DW5_LN(phy, ch, lane)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW5_LN(phy: u32, ch: u32, lane: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH_LN(phy, ch, lane, _PORT_TX_DW5_LN0_B, _PORT_TX_DW5_LN0_C)
}

/// `BXT_PORT_TX_DW5_GRP(phy, ch)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW5_GRP(phy: u32, ch: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH(phy, ch, _PORT_TX_DW5_GRP_B, _PORT_TX_DW5_GRP_C)
}

/// `DCC_DELAY_RANGE_1` (bxt_dpio_phy_regs.h)
pub const DCC_DELAY_RANGE_1: u32 = reg_bit(9);

/// `DCC_DELAY_RANGE_2` (bxt_dpio_phy_regs.h)
pub const DCC_DELAY_RANGE_2: u32 = reg_bit(8);

/// `_PORT_TX_DW14_LN0_A` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW14_LN0_A: u32 = 0x162538;

/// `_PORT_TX_DW14_LN0_B` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW14_LN0_B: u32 = 0x6C538;

/// `_PORT_TX_DW14_LN0_C` (bxt_dpio_phy_regs.h)
pub const _PORT_TX_DW14_LN0_C: u32 = 0x6C938;

/// `LATENCY_OPTIM` (bxt_dpio_phy_regs.h)
pub const LATENCY_OPTIM: u32 = reg_bit(30);

/// `BXT_PORT_TX_DW14_LN(phy, ch, lane)` (bxt_dpio_phy_regs.h)
pub const fn BXT_PORT_TX_DW14_LN(phy: u32, ch: u32, lane: u32) -> I915RegT {
    _MMIO_BXT_PHY_CH_LN(phy, ch, lane, _PORT_TX_DW14_LN0_B, _PORT_TX_DW14_LN0_C)
}
