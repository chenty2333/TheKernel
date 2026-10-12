// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_dkl_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_DKL_PHY1_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY1_BASE: u32 = 0x168000;

/// `_DKL_PHY2_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY2_BASE: u32 = 0x169000;

/// `_DKL_PHY3_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY3_BASE: u32 = 0x16A000;

/// `_DKL_PHY4_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY4_BASE: u32 = 0x16B000;

/// `_DKL_PHY5_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY5_BASE: u32 = 0x16C000;

/// `_DKL_PHY6_BASE` (intel_dkl_phy_regs.h)
pub const _DKL_PHY6_BASE: u32 = 0x16D000;

/// `_DKL_REG_PHY_BASE(tc_port)` (intel_dkl_phy_regs.h)
pub const fn _DKL_REG_PHY_BASE(tc_port: u32) -> u32 {
    _PORT(tc_port, _DKL_PHY1_BASE, _DKL_PHY2_BASE)
}

/// `_DKL_BANK_SHIFT` (intel_dkl_phy_regs.h)
pub const _DKL_BANK_SHIFT: u32 = 12;

/// `_DKL_REG_BANK_OFFSET(phy_offset)` (intel_dkl_phy_regs.h)
pub const fn _DKL_REG_BANK_OFFSET(phy_offset: u32) -> u32 {
    (((phy_offset) & ((((1 << _DKL_BANK_SHIFT)) - 1))))
}

/// `_DKL_REG_BANK_IDX(phy_offset)` (intel_dkl_phy_regs.h)
pub const fn _DKL_REG_BANK_IDX(phy_offset: u32) -> u32 {
    (((((phy_offset) >> _DKL_BANK_SHIFT)) & 0xf))
}

/// `_DKL_PCS_DW5_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_PCS_DW5_LN0: u32 = 0x0014;

/// `_DKL_PCS_DW5_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_PCS_DW5_LN1: u32 = 0x1014;

/// `DKL_PCS_DW5_CORE_SOFTRESET` (intel_dkl_phy_regs.h)
pub const DKL_PCS_DW5_CORE_SOFTRESET: u32 = reg_bit(11);

/// `_DKL_PLL_DIV0` (intel_dkl_phy_regs.h)
pub const _DKL_PLL_DIV0: u32 = 0x2200;

/// `DKL_PLL_DIV0_AFC_STARTUP_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_AFC_STARTUP_MASK: u32 = reg_genmask(27, 25);

/// `DKL_PLL_DIV0_AFC_STARTUP(val)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV0_AFC_STARTUP(val: u32) -> u32 {
    reg_field_prep(DKL_PLL_DIV0_AFC_STARTUP_MASK, (val))
}

/// `DKL_PLL_DIV0_INTEG_COEFF(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV0_INTEG_COEFF(x: u32) -> u32 {
    (((x) << 16))
}

/// `DKL_PLL_DIV0_INTEG_COEFF_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_INTEG_COEFF_MASK: u32 = ((0x1F << 16));

/// `DKL_PLL_DIV0_PROP_COEFF(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV0_PROP_COEFF(x: u32) -> u32 {
    (((x) << 12))
}

/// `DKL_PLL_DIV0_PROP_COEFF_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_PROP_COEFF_MASK: u32 = ((0xF << 12));

/// `DKL_PLL_DIV0_FBPREDIV_SHIFT` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_FBPREDIV_SHIFT: u32 = (8);

/// `DKL_PLL_DIV0_FBPREDIV(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV0_FBPREDIV(x: u32) -> u32 {
    (((x) << DKL_PLL_DIV0_FBPREDIV_SHIFT))
}

/// `DKL_PLL_DIV0_FBPREDIV_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_FBPREDIV_MASK: u32 = ((0xF << DKL_PLL_DIV0_FBPREDIV_SHIFT));

/// `DKL_PLL_DIV0_FBDIV_INT(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV0_FBDIV_INT(x: u32) -> u32 {
    (((x) << 0))
}

/// `DKL_PLL_DIV0_FBDIV_INT_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_FBDIV_INT_MASK: u32 = ((0xFF << 0));

/// `DKL_PLL_DIV0_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV0_MASK: u32 = ((((DKL_PLL_DIV0_INTEG_COEFF_MASK | DKL_PLL_DIV0_PROP_COEFF_MASK) | DKL_PLL_DIV0_FBPREDIV_MASK) | DKL_PLL_DIV0_FBDIV_INT_MASK));

/// `_DKL_PLL_DIV1` (intel_dkl_phy_regs.h)
pub const _DKL_PLL_DIV1: u32 = 0x2204;

/// `DKL_PLL_DIV1_IREF_TRIM(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV1_IREF_TRIM(x: u32) -> u32 {
    (((x) << 16))
}

/// `DKL_PLL_DIV1_IREF_TRIM_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV1_IREF_TRIM_MASK: u32 = ((0x1F << 16));

/// `DKL_PLL_DIV1_TDC_TARGET_CNT(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_DIV1_TDC_TARGET_CNT(x: u32) -> u32 {
    (((x) << 0))
}

/// `DKL_PLL_DIV1_TDC_TARGET_CNT_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_DIV1_TDC_TARGET_CNT_MASK: u32 = ((0xFF << 0));

/// `_DKL_PLL_SSC` (intel_dkl_phy_regs.h)
pub const _DKL_PLL_SSC: u32 = 0x2210;

/// `DKL_PLL_SSC_IREF_NDIV_RATIO(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_SSC_IREF_NDIV_RATIO(x: u32) -> u32 {
    (((x) << 29))
}

/// `DKL_PLL_SSC_IREF_NDIV_RATIO_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_SSC_IREF_NDIV_RATIO_MASK: u32 = ((0x7 << 29));

/// `DKL_PLL_SSC_STEP_LEN(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_SSC_STEP_LEN(x: u32) -> u32 {
    (((x) << 16))
}

/// `DKL_PLL_SSC_STEP_LEN_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_SSC_STEP_LEN_MASK: u32 = ((0xFF << 16));

/// `DKL_PLL_SSC_STEP_NUM(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_SSC_STEP_NUM(x: u32) -> u32 {
    (((x) << 11))
}

/// `DKL_PLL_SSC_STEP_NUM_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_SSC_STEP_NUM_MASK: u32 = ((0x7 << 11));

/// `DKL_PLL_SSC_EN` (intel_dkl_phy_regs.h)
pub const DKL_PLL_SSC_EN: u32 = ((1 << 9));

/// `_DKL_PLL_BIAS` (intel_dkl_phy_regs.h)
pub const _DKL_PLL_BIAS: u32 = 0x2214;

/// `DKL_PLL_BIAS_FBDIV_SHIFT` (intel_dkl_phy_regs.h)
pub const DKL_PLL_BIAS_FBDIV_SHIFT: u32 = (8);

/// `DKL_PLL_BIAS_FBDIV_FRAC(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_BIAS_FBDIV_FRAC(x: u32) -> u32 {
    (((x) << DKL_PLL_BIAS_FBDIV_SHIFT))
}

/// `DKL_PLL_BIAS_FBDIV_FRAC_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_BIAS_FBDIV_FRAC_MASK: u32 = ((0x3FFFFF << DKL_PLL_BIAS_FBDIV_SHIFT));

/// `_DKL_PLL_TDC_COLDST_BIAS` (intel_dkl_phy_regs.h)
pub const _DKL_PLL_TDC_COLDST_BIAS: u32 = 0x2218;

/// `DKL_PLL_TDC_SSC_STEP_SIZE(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_TDC_SSC_STEP_SIZE(x: u32) -> u32 {
    (((x) << 8))
}

/// `DKL_PLL_TDC_SSC_STEP_SIZE_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_TDC_SSC_STEP_SIZE_MASK: u32 = ((0xFF << 8));

/// `DKL_PLL_TDC_FEED_FWD_GAIN(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_PLL_TDC_FEED_FWD_GAIN(x: u32) -> u32 {
    (((x) << 0))
}

/// `DKL_PLL_TDC_FEED_FWD_GAIN_MASK` (intel_dkl_phy_regs.h)
pub const DKL_PLL_TDC_FEED_FWD_GAIN_MASK: u32 = ((0xFF << 0));

/// `_DKL_REFCLKIN_CTL` (intel_dkl_phy_regs.h)
pub const _DKL_REFCLKIN_CTL: u32 = 0x212C;

/// `_DKL_CLKTOP2_HSCLKCTL` (intel_dkl_phy_regs.h)
pub const _DKL_CLKTOP2_HSCLKCTL: u32 = 0x20D4;

/// `_DKL_CLKTOP2_CORECLKCTL1` (intel_dkl_phy_regs.h)
pub const _DKL_CLKTOP2_CORECLKCTL1: u32 = 0x20D8;

/// `_DKL_TX_DPCNTL0_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL0_LN0: u32 = 0x02C0;

/// `_DKL_TX_DPCNTL0_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL0_LN1: u32 = 0x12C0;

/// `DKL_TX_PRESHOOT_COEFF(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_TX_PRESHOOT_COEFF(x: u32) -> u32 {
    (((x) << 13))
}

/// `DKL_TX_PRESHOOT_COEFF_MASK` (intel_dkl_phy_regs.h)
pub const DKL_TX_PRESHOOT_COEFF_MASK: u32 = ((0x1f << 13));

/// `DKL_TX_DE_EMPHASIS_COEFF(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_TX_DE_EMPHASIS_COEFF(x: u32) -> u32 {
    (((x) << 8))
}

/// `DKL_TX_DE_EMPAHSIS_COEFF_MASK` (intel_dkl_phy_regs.h)
pub const DKL_TX_DE_EMPAHSIS_COEFF_MASK: u32 = ((0x1f << 8));

/// `DKL_TX_VSWING_CONTROL(x)` (intel_dkl_phy_regs.h)
pub const fn DKL_TX_VSWING_CONTROL(x: u32) -> u32 {
    (((x) << 0))
}

/// `DKL_TX_VSWING_CONTROL_MASK` (intel_dkl_phy_regs.h)
pub const DKL_TX_VSWING_CONTROL_MASK: u32 = ((0x7 << 0));

/// `_DKL_TX_DPCNTL1_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL1_LN0: u32 = 0x02C4;

/// `_DKL_TX_DPCNTL1_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL1_LN1: u32 = 0x12C4;

/// `_DKL_TX_DPCNTL2_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL2_LN0: u32 = 0x02C8;

/// `_DKL_TX_DPCNTL2_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DPCNTL2_LN1: u32 = 0x12C8;

/// `DKL_TX_DP20BITMODE` (intel_dkl_phy_regs.h)
pub const DKL_TX_DP20BITMODE: u32 = reg_bit(2);

/// `DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1_MASK` (intel_dkl_phy_regs.h)
pub const DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1_MASK: u32 = reg_genmask(4, 3);

/// `DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1(val)` (intel_dkl_phy_regs.h)
pub const fn DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1(val: u32) -> u32 {
    reg_field_prep(DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX1_MASK, (val))
}

/// `DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2_MASK` (intel_dkl_phy_regs.h)
pub const DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2_MASK: u32 = reg_genmask(6, 5);

/// `DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2(val)` (intel_dkl_phy_regs.h)
pub const fn DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2(val: u32) -> u32 {
    reg_field_prep(DKL_TX_DPCNTL2_CFG_LOADGENSELECT_TX2_MASK, (val))
}

/// `LOADGEN_SHARING_PMD_DISABLE` (intel_dkl_phy_regs.h)
pub const LOADGEN_SHARING_PMD_DISABLE: u32 = reg_bit(12);

/// `_DKL_TX_FW_CALIB_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_FW_CALIB_LN0: u32 = 0x02F8;

/// `_DKL_TX_FW_CALIB_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_FW_CALIB_LN1: u32 = 0x12F8;

/// `DKL_TX_CFG_DISABLE_WAIT_INIT` (intel_dkl_phy_regs.h)
pub const DKL_TX_CFG_DISABLE_WAIT_INIT: u32 = ((1 << 7));

/// `_DKL_TX_PMD_LANE_SUS_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_PMD_LANE_SUS_LN0: u32 = 0x0D00;

/// `_DKL_TX_PMD_LANE_SUS_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_PMD_LANE_SUS_LN1: u32 = 0x1D00;

/// `_DKL_TX_DW17_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DW17_LN0: u32 = 0x0DC4;

/// `_DKL_TX_DW17_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DW17_LN1: u32 = 0x1DC4;

/// `_DKL_TX_DW18_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DW18_LN0: u32 = 0x0DC8;

/// `_DKL_TX_DW18_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_TX_DW18_LN1: u32 = 0x1DC8;

/// `_DKL_DP_MODE_LN0` (intel_dkl_phy_regs.h)
pub const _DKL_DP_MODE_LN0: u32 = 0x00A0;

/// `_DKL_DP_MODE_LN1` (intel_dkl_phy_regs.h)
pub const _DKL_DP_MODE_LN1: u32 = 0x10A0;

/// `_DKL_CMN_UC_DW27` (intel_dkl_phy_regs.h)
pub const _DKL_CMN_UC_DW27: u32 = 0x236C;

/// `DKL_CMN_UC_DW27_UC_HEALTH` (intel_dkl_phy_regs.h)
pub const DKL_CMN_UC_DW27_UC_HEALTH: u32 = ((0x1 << 15));

/// `_HIP_INDEX_REG0` (intel_dkl_phy_regs.h)
pub const _HIP_INDEX_REG0: u32 = 0x1010A0;

/// `_HIP_INDEX_REG1` (intel_dkl_phy_regs.h)
pub const _HIP_INDEX_REG1: u32 = 0x1010A4;

/// `_HIP_INDEX_SHIFT(tc_port)` (intel_dkl_phy_regs.h)
pub const fn _HIP_INDEX_SHIFT(tc_port: u32) -> u32 {
    ((8 * (((tc_port) % 4))))
}

/// `HIP_INDEX_VAL(tc_port, val)` (intel_dkl_phy_regs.h)
pub const fn HIP_INDEX_VAL(tc_port: u32, val: u32) -> u32 {
    (((val) << _HIP_INDEX_SHIFT(tc_port)))
}
