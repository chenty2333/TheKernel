// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_combo_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_ICL_COMBOPHY_A` (intel_combo_phy_regs.h)
pub const _ICL_COMBOPHY_A: u32 = 0x162000;

/// `_ICL_COMBOPHY_B` (intel_combo_phy_regs.h)
pub const _ICL_COMBOPHY_B: u32 = 0x6C000;

/// `_EHL_COMBOPHY_C` (intel_combo_phy_regs.h)
pub const _EHL_COMBOPHY_C: u32 = 0x160000;

/// `_RKL_COMBOPHY_D` (intel_combo_phy_regs.h)
pub const _RKL_COMBOPHY_D: u32 = 0x161000;

/// `_ADL_COMBOPHY_E` (intel_combo_phy_regs.h)
pub const _ADL_COMBOPHY_E: u32 = 0x16B000;

/// `_ICL_COMBOPHY(phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_COMBOPHY(phy: u32) -> u32 {
    pick(phy, &[_ICL_COMBOPHY_A, _ICL_COMBOPHY_B, _EHL_COMBOPHY_C, _RKL_COMBOPHY_D, _ADL_COMBOPHY_E])
}

/// `_ICL_PORT_CL_DW(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_CL_DW(dw: u32, phy: u32) -> u32 {
    ((_ICL_COMBOPHY(phy) + (4 * (dw))))
}

/// `ICL_PORT_CL_DW5(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_CL_DW5(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_CL_DW(5, phy))
}

/// `CL_POWER_DOWN_ENABLE` (intel_combo_phy_regs.h)
pub const CL_POWER_DOWN_ENABLE: u32 = reg_bit(4);

/// `SUS_CLOCK_CONFIG` (intel_combo_phy_regs.h)
pub const SUS_CLOCK_CONFIG: u32 = reg_genmask(1, 0);

/// `ICL_PORT_CL_DW10(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_CL_DW10(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_CL_DW(10, phy))
}

/// `PG_SEQ_DELAY_OVERRIDE_MASK` (intel_combo_phy_regs.h)
pub const PG_SEQ_DELAY_OVERRIDE_MASK: u32 = reg_genmask(26, 25);

/// `PG_SEQ_DELAY_OVERRIDE_ENABLE` (intel_combo_phy_regs.h)
pub const PG_SEQ_DELAY_OVERRIDE_ENABLE: u32 = reg_bit(24);

/// `PWR_DOWN_LN_MASK` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_MASK: u32 = reg_genmask(7, 4);

/// `PWR_UP_ALL_LANES` (intel_combo_phy_regs.h)
pub const PWR_UP_ALL_LANES: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0x0);

/// `PWR_DOWN_LN_3_2_1` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_3_2_1: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0xe);

/// `PWR_DOWN_LN_3_2` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_3_2: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0xc);

/// `PWR_DOWN_LN_3` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_3: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0x8);

/// `PWR_DOWN_LN_2_1_0` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_2_1_0: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0x7);

/// `PWR_DOWN_LN_1_0` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_1_0: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0x3);

/// `PWR_DOWN_LN_3_1` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_3_1: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0xa);

/// `PWR_DOWN_LN_3_1_0` (intel_combo_phy_regs.h)
pub const PWR_DOWN_LN_3_1_0: u32 = reg_field_prep(PWR_DOWN_LN_MASK, 0xb);

/// `EDP4K2K_MODE_OVRD_EN` (intel_combo_phy_regs.h)
pub const EDP4K2K_MODE_OVRD_EN: u32 = reg_bit(3);

/// `EDP4K2K_MODE_OVRD_OPTIMIZED` (intel_combo_phy_regs.h)
pub const EDP4K2K_MODE_OVRD_OPTIMIZED: u32 = reg_bit(2);

/// `ICL_PORT_CL_DW12(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_CL_DW12(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_CL_DW(12, phy))
}

/// `ICL_LANE_ENABLE_AUX` (intel_combo_phy_regs.h)
pub const ICL_LANE_ENABLE_AUX: u32 = reg_bit(0);

/// `_ICL_PORT_COMP` (intel_combo_phy_regs.h)
pub const _ICL_PORT_COMP: u32 = 0x100;

/// `_ICL_PORT_COMP_DW(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_COMP_DW(dw: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_COMP) + (4 * (dw))))
}

/// `ICL_PORT_COMP_DW0(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW0(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(0, phy))
}

/// `COMP_INIT` (intel_combo_phy_regs.h)
pub const COMP_INIT: u32 = reg_bit(31);

/// `ICL_PORT_COMP_DW1(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW1(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(1, phy))
}

/// `ICL_PORT_COMP_DW3(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW3(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(3, phy))
}

/// `PROCESS_INFO_MASK` (intel_combo_phy_regs.h)
pub const PROCESS_INFO_MASK: u32 = reg_genmask(28, 26);

/// `PROCESS_INFO_DOT_0` (intel_combo_phy_regs.h)
pub const PROCESS_INFO_DOT_0: u32 = reg_field_prep(PROCESS_INFO_MASK, 0);

/// `PROCESS_INFO_DOT_1` (intel_combo_phy_regs.h)
pub const PROCESS_INFO_DOT_1: u32 = reg_field_prep(PROCESS_INFO_MASK, 1);

/// `PROCESS_INFO_DOT_4` (intel_combo_phy_regs.h)
pub const PROCESS_INFO_DOT_4: u32 = reg_field_prep(PROCESS_INFO_MASK, 2);

/// `VOLTAGE_INFO_MASK` (intel_combo_phy_regs.h)
pub const VOLTAGE_INFO_MASK: u32 = reg_genmask(25, 24);

/// `VOLTAGE_INFO_0_85V` (intel_combo_phy_regs.h)
pub const VOLTAGE_INFO_0_85V: u32 = reg_field_prep(VOLTAGE_INFO_MASK, 0);

/// `VOLTAGE_INFO_0_95V` (intel_combo_phy_regs.h)
pub const VOLTAGE_INFO_0_95V: u32 = reg_field_prep(VOLTAGE_INFO_MASK, 1);

/// `VOLTAGE_INFO_1_05V` (intel_combo_phy_regs.h)
pub const VOLTAGE_INFO_1_05V: u32 = reg_field_prep(VOLTAGE_INFO_MASK, 2);

/// `ICL_PORT_COMP_DW8(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW8(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(8, phy))
}

/// `IREFGEN` (intel_combo_phy_regs.h)
pub const IREFGEN: u32 = reg_bit(24);

/// `ICL_PORT_COMP_DW9(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW9(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(9, phy))
}

/// `ICL_PORT_COMP_DW10(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_COMP_DW10(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_COMP_DW(10, phy))
}

/// `_ICL_PORT_PCS_AUX` (intel_combo_phy_regs.h)
pub const _ICL_PORT_PCS_AUX: u32 = 0x300;

/// `_ICL_PORT_PCS_GRP` (intel_combo_phy_regs.h)
pub const _ICL_PORT_PCS_GRP: u32 = 0x600;

/// `_ICL_PORT_PCS_LN(ln)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_PCS_LN(ln: u32) -> u32 {
    ((0x800 + ((ln) * 0x100)))
}

/// `_ICL_PORT_PCS_DW_AUX(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_PCS_DW_AUX(dw: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_PCS_AUX) + (4 * (dw))))
}

/// `_ICL_PORT_PCS_DW_GRP(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_PCS_DW_GRP(dw: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_PCS_GRP) + (4 * (dw))))
}

/// `_ICL_PORT_PCS_DW_LN(dw, ln, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_PCS_DW_LN(dw: u32, ln: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_PCS_LN(ln)) + (4 * (dw))))
}

/// `ICL_PORT_PCS_DW1_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_PCS_DW1_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_PCS_DW_AUX(1, phy))
}

/// `ICL_PORT_PCS_DW1_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_PCS_DW1_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_PCS_DW_GRP(1, phy))
}

/// `ICL_PORT_PCS_DW1_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_PCS_DW1_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_PCS_DW_LN(1, ln, phy))
}

/// `DCC_MODE_SELECT_MASK` (intel_combo_phy_regs.h)
pub const DCC_MODE_SELECT_MASK: u32 = reg_genmask(21, 20);

/// `RUN_DCC_ONCE` (intel_combo_phy_regs.h)
pub const RUN_DCC_ONCE: u32 = reg_field_prep(DCC_MODE_SELECT_MASK, 0);

/// `COMMON_KEEPER_EN` (intel_combo_phy_regs.h)
pub const COMMON_KEEPER_EN: u32 = reg_bit(26);

/// `LATENCY_OPTIM_MASK` (intel_combo_phy_regs.h)
pub const LATENCY_OPTIM_MASK: u32 = reg_genmask(3, 2);

/// `LATENCY_OPTIM_VAL(x)` (intel_combo_phy_regs.h)
pub const fn LATENCY_OPTIM_VAL(x: u32) -> u32 {
    reg_field_prep(LATENCY_OPTIM_MASK, (x))
}

/// `_ICL_PORT_TX_AUX` (intel_combo_phy_regs.h)
pub const _ICL_PORT_TX_AUX: u32 = 0x380;

/// `_ICL_PORT_TX_GRP` (intel_combo_phy_regs.h)
pub const _ICL_PORT_TX_GRP: u32 = 0x680;

/// `_ICL_PORT_TX_LN(ln)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_TX_LN(ln: u32) -> u32 {
    ((0x880 + ((ln) * 0x100)))
}

/// `_ICL_PORT_TX_DW_AUX(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_TX_DW_AUX(dw: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_TX_AUX) + (4 * (dw))))
}

/// `_ICL_PORT_TX_DW_GRP(dw, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_TX_DW_GRP(dw: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_TX_GRP) + (4 * (dw))))
}

/// `_ICL_PORT_TX_DW_LN(dw, ln, phy)` (intel_combo_phy_regs.h)
pub const fn _ICL_PORT_TX_DW_LN(dw: u32, ln: u32, phy: u32) -> u32 {
    (((_ICL_COMBOPHY(phy) + _ICL_PORT_TX_LN(ln)) + (4 * (dw))))
}

/// `ICL_PORT_TX_DW2_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW2_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(2, phy))
}

/// `ICL_PORT_TX_DW2_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW2_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(2, phy))
}

/// `ICL_PORT_TX_DW2_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW2_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(2, ln, phy))
}

/// `SWING_SEL_UPPER_MASK` (intel_combo_phy_regs.h)
pub const SWING_SEL_UPPER_MASK: u32 = reg_bit(15);

/// `SWING_SEL_UPPER(x)` (intel_combo_phy_regs.h)
pub const fn SWING_SEL_UPPER(x: u32) -> u32 {
    reg_field_prep(SWING_SEL_UPPER_MASK, ((x) >> 3))
}

/// `SWING_SEL_LOWER_MASK` (intel_combo_phy_regs.h)
pub const SWING_SEL_LOWER_MASK: u32 = reg_genmask(13, 11);

/// `SWING_SEL_LOWER(x)` (intel_combo_phy_regs.h)
pub const fn SWING_SEL_LOWER(x: u32) -> u32 {
    reg_field_prep(SWING_SEL_LOWER_MASK, ((x) & 0x7))
}

/// `FRC_LATENCY_OPTIM_MASK` (intel_combo_phy_regs.h)
pub const FRC_LATENCY_OPTIM_MASK: u32 = reg_genmask(10, 8);

/// `FRC_LATENCY_OPTIM_VAL(x)` (intel_combo_phy_regs.h)
pub const fn FRC_LATENCY_OPTIM_VAL(x: u32) -> u32 {
    reg_field_prep(FRC_LATENCY_OPTIM_MASK, (x))
}

/// `RCOMP_SCALAR_MASK` (intel_combo_phy_regs.h)
pub const RCOMP_SCALAR_MASK: u32 = reg_genmask(7, 0);

/// `RCOMP_SCALAR(x)` (intel_combo_phy_regs.h)
pub const fn RCOMP_SCALAR(x: u32) -> u32 {
    reg_field_prep(RCOMP_SCALAR_MASK, (x))
}

/// `ICL_PORT_TX_DW4_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW4_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(4, phy))
}

/// `ICL_PORT_TX_DW4_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW4_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(4, phy))
}

/// `ICL_PORT_TX_DW4_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW4_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(4, ln, phy))
}

/// `LOADGEN_SELECT` (intel_combo_phy_regs.h)
pub const LOADGEN_SELECT: u32 = reg_bit(31);

/// `POST_CURSOR_1_MASK` (intel_combo_phy_regs.h)
pub const POST_CURSOR_1_MASK: u32 = reg_genmask(17, 12);

/// `POST_CURSOR_1(x)` (intel_combo_phy_regs.h)
pub const fn POST_CURSOR_1(x: u32) -> u32 {
    reg_field_prep(POST_CURSOR_1_MASK, (x))
}

/// `POST_CURSOR_2_MASK` (intel_combo_phy_regs.h)
pub const POST_CURSOR_2_MASK: u32 = reg_genmask(11, 6);

/// `POST_CURSOR_2(x)` (intel_combo_phy_regs.h)
pub const fn POST_CURSOR_2(x: u32) -> u32 {
    reg_field_prep(POST_CURSOR_2_MASK, (x))
}

/// `CURSOR_COEFF_MASK` (intel_combo_phy_regs.h)
pub const CURSOR_COEFF_MASK: u32 = reg_genmask(5, 0);

/// `CURSOR_COEFF(x)` (intel_combo_phy_regs.h)
pub const fn CURSOR_COEFF(x: u32) -> u32 {
    reg_field_prep(CURSOR_COEFF_MASK, (x))
}

/// `ICL_PORT_TX_DW5_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW5_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(5, phy))
}

/// `ICL_PORT_TX_DW5_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW5_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(5, phy))
}

/// `ICL_PORT_TX_DW5_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW5_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(5, ln, phy))
}

/// `TX_TRAINING_EN` (intel_combo_phy_regs.h)
pub const TX_TRAINING_EN: u32 = reg_bit(31);

/// `TAP2_DISABLE` (intel_combo_phy_regs.h)
pub const TAP2_DISABLE: u32 = reg_bit(30);

/// `TAP3_DISABLE` (intel_combo_phy_regs.h)
pub const TAP3_DISABLE: u32 = reg_bit(29);

/// `CURSOR_PROGRAM` (intel_combo_phy_regs.h)
pub const CURSOR_PROGRAM: u32 = reg_bit(26);

/// `COEFF_POLARITY` (intel_combo_phy_regs.h)
pub const COEFF_POLARITY: u32 = reg_bit(25);

/// `SCALING_MODE_SEL_MASK` (intel_combo_phy_regs.h)
pub const SCALING_MODE_SEL_MASK: u32 = reg_genmask(20, 18);

/// `SCALING_MODE_SEL(x)` (intel_combo_phy_regs.h)
pub const fn SCALING_MODE_SEL(x: u32) -> u32 {
    reg_field_prep(SCALING_MODE_SEL_MASK, (x))
}

/// `RTERM_SELECT_MASK` (intel_combo_phy_regs.h)
pub const RTERM_SELECT_MASK: u32 = reg_genmask(5, 3);

/// `RTERM_SELECT(x)` (intel_combo_phy_regs.h)
pub const fn RTERM_SELECT(x: u32) -> u32 {
    reg_field_prep(RTERM_SELECT_MASK, (x))
}

/// `ICL_PORT_TX_DW6_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW6_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(6, phy))
}

/// `ICL_PORT_TX_DW6_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW6_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(6, phy))
}

/// `ICL_PORT_TX_DW6_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW6_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(6, ln, phy))
}

/// `O_FUNC_OVRD_EN` (intel_combo_phy_regs.h)
pub const O_FUNC_OVRD_EN: u32 = reg_bit(7);

/// `O_LDO_REF_SEL_CRI` (intel_combo_phy_regs.h)
pub const O_LDO_REF_SEL_CRI: u32 = reg_genmask(6, 1);

/// `O_LDO_BYPASS_CRI` (intel_combo_phy_regs.h)
pub const O_LDO_BYPASS_CRI: u32 = reg_bit(0);

/// `ICL_PORT_TX_DW7_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW7_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(7, phy))
}

/// `ICL_PORT_TX_DW7_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW7_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(7, phy))
}

/// `ICL_PORT_TX_DW7_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW7_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(7, ln, phy))
}

/// `N_SCALAR_MASK` (intel_combo_phy_regs.h)
pub const N_SCALAR_MASK: u32 = reg_genmask(30, 24);

/// `N_SCALAR(x)` (intel_combo_phy_regs.h)
pub const fn N_SCALAR(x: u32) -> u32 {
    reg_field_prep(N_SCALAR_MASK, (x))
}

/// `ICL_PORT_TX_DW8_AUX(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW8_AUX(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_AUX(8, phy))
}

/// `ICL_PORT_TX_DW8_GRP(phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW8_GRP(phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_GRP(8, phy))
}

/// `ICL_PORT_TX_DW8_LN(ln, phy)` (intel_combo_phy_regs.h)
pub const fn ICL_PORT_TX_DW8_LN(ln: u32, phy: u32) -> I915RegT {
    mmio(_ICL_PORT_TX_DW_LN(8, ln, phy))
}

/// `ICL_PORT_TX_DW8_ODCC_CLK_SEL` (intel_combo_phy_regs.h)
pub const ICL_PORT_TX_DW8_ODCC_CLK_SEL: u32 = reg_bit(31);

/// `ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_MASK` (intel_combo_phy_regs.h)
pub const ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_MASK: u32 = reg_genmask(30, 29);

/// `ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_DIV2` (intel_combo_phy_regs.h)
pub const ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_DIV2: u32 = reg_field_prep(ICL_PORT_TX_DW8_ODCC_CLK_DIV_SEL_MASK, 0x1);

/// `_ICL_DPHY_CHKN_REG` (intel_combo_phy_regs.h)
pub const _ICL_DPHY_CHKN_REG: u32 = 0x194;

/// `ICL_DPHY_CHKN(port)` (intel_combo_phy_regs.h)
pub const fn ICL_DPHY_CHKN(port: u32) -> I915RegT {
    mmio((_ICL_COMBOPHY(port) + _ICL_DPHY_CHKN_REG))
}

/// `ICL_DPHY_CHKN_AFE_OVER_PPI_STRAP` (intel_combo_phy_regs.h)
pub const ICL_DPHY_CHKN_AFE_OVER_PPI_STRAP: u32 = reg_bit(7);
