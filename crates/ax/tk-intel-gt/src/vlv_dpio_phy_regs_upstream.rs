// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/vlv_dpio_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_VLV_CMN(dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_CMN(dw: u32) -> u32 {
    ((0x8100 + ((dw) * 4)))
}

/// `_CHV_CMN(cl, dw)` (vlv_dpio_phy_regs.h)
pub const fn _CHV_CMN(cl: u32, dw: u32) -> u32 {
    (((0x8100 - ((cl) * 0x80)) + ((dw) * 4)))
}

/// `_VLV_PLL(ch, dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_PLL(ch: u32, dw: u32) -> u32 {
    (((0x8000 + ((ch) * 0x20)) + ((dw) * 4)))
}

/// `_CHV_PLL(ch, dw)` (vlv_dpio_phy_regs.h)
pub const fn _CHV_PLL(ch: u32, dw: u32) -> u32 {
    (((0x8000 + ((ch) * 0x180)) + ((dw) * 4)))
}

/// `_VLV_REF(dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_REF(dw: u32) -> u32 {
    ((0x80a0 + ((((dw) - 8)) * 4)))
}

/// `_VLV_PCS(ch, spline, dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_PCS(ch: u32, spline: u32, dw: u32) -> u32 {
    ((((0x200 + ((ch) * 0x2400)) + ((spline) * 0x200)) + ((dw) * 4)))
}

/// `_VLV_PCS_GRP(ch, dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_PCS_GRP(ch: u32, dw: u32) -> u32 {
    (((0x8200 + ((ch) * 0x200)) + ((dw) * 4)))
}

/// `_VLV_PCS_BCAST(dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_PCS_BCAST(dw: u32) -> u32 {
    ((0xc000 + ((dw) * 4)))
}

/// `_VLV_TX(ch, lane, dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_TX(ch: u32, lane: u32, dw: u32) -> u32 {
    ((((0x80 + ((ch) * 0x2400)) + ((lane) * 0x200)) + ((dw) * 4)))
}

/// `_VLV_TX_GRP(ch, dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_TX_GRP(ch: u32, dw: u32) -> u32 {
    (((0x8280 + ((ch) * 0x200)) + ((dw) * 4)))
}

/// `_VLV_TX_BCAST(dw)` (vlv_dpio_phy_regs.h)
pub const fn _VLV_TX_BCAST(dw: u32) -> u32 {
    ((0xc080 + ((dw) * 4)))
}

/// `VLV_PLL_DW3(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW3(ch: u32) -> u32 {
    _VLV_PLL((ch), 3)
}

/// `DPIO_S1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_S1_DIV_MASK: u32 = reg_genmask(30, 28);

/// `DPIO_S1_DIV(s1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_S1_DIV(s1: u32) -> u32 {
    reg_field_prep(DPIO_S1_DIV_MASK, (s1))
}

/// `DPIO_S1_DIV_DAC` (vlv_dpio_phy_regs.h)
pub const DPIO_S1_DIV_DAC: u32 = 0;

/// `DPIO_S1_DIV_HDMIDP` (vlv_dpio_phy_regs.h)
pub const DPIO_S1_DIV_HDMIDP: u32 = 1;

/// `DPIO_S1_DIV_LVDS1` (vlv_dpio_phy_regs.h)
pub const DPIO_S1_DIV_LVDS1: u32 = 2;

/// `DPIO_S1_DIV_LVDS2` (vlv_dpio_phy_regs.h)
pub const DPIO_S1_DIV_LVDS2: u32 = 3;

/// `DPIO_K_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_K_DIV_MASK: u32 = reg_genmask(27, 24);

/// `DPIO_K_DIV(k)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_K_DIV(k: u32) -> u32 {
    reg_field_prep(DPIO_K_DIV_MASK, (k))
}

/// `DPIO_P1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_P1_DIV_MASK: u32 = reg_genmask(23, 21);

/// `DPIO_P1_DIV(p1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_P1_DIV(p1: u32) -> u32 {
    reg_field_prep(DPIO_P1_DIV_MASK, (p1))
}

/// `DPIO_P2_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_P2_DIV_MASK: u32 = reg_genmask(20, 16);

/// `DPIO_P2_DIV(p2)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_P2_DIV(p2: u32) -> u32 {
    reg_field_prep(DPIO_P2_DIV_MASK, (p2))
}

/// `DPIO_N_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_N_DIV_MASK: u32 = reg_genmask(15, 12);

/// `DPIO_N_DIV(n)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_N_DIV(n: u32) -> u32 {
    reg_field_prep(DPIO_N_DIV_MASK, (n))
}

/// `DPIO_ENABLE_CALIBRATION` (vlv_dpio_phy_regs.h)
pub const DPIO_ENABLE_CALIBRATION: u32 = reg_bit(11);

/// `DPIO_M1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_M1_DIV_MASK: u32 = reg_genmask(10, 8);

/// `DPIO_M1_DIV(m1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_M1_DIV(m1: u32) -> u32 {
    reg_field_prep(DPIO_M1_DIV_MASK, (m1))
}

/// `DPIO_M2_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_M2_DIV_MASK: u32 = reg_genmask(7, 0);

/// `DPIO_M2_DIV(m2)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_M2_DIV(m2: u32) -> u32 {
    reg_field_prep(DPIO_M2_DIV_MASK, (m2))
}

/// `VLV_PLL_DW5(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW5(ch: u32) -> u32 {
    _VLV_PLL((ch), 5)
}

/// `DPIO_REFSEL_OVERRIDE` (vlv_dpio_phy_regs.h)
pub const DPIO_REFSEL_OVERRIDE: u32 = reg_bit(27);

/// `DPIO_PLL_MODESEL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PLL_MODESEL_MASK: u32 = reg_genmask(26, 24);

/// `DPIO_BIAS_CURRENT_CTL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_BIAS_CURRENT_CTL_MASK: u32 = reg_genmask(22, 20);

/// `DPIO_PLL_REFCLK_SEL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PLL_REFCLK_SEL_MASK: u32 = reg_genmask(17, 16);

/// `DPIO_DRIVER_CTL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_DRIVER_CTL_MASK: u32 = reg_genmask(15, 12);

/// `DPIO_CLK_BIAS_CTL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CLK_BIAS_CTL_MASK: u32 = reg_genmask(11, 8);

/// `VLV_PLL_DW7(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW7(ch: u32) -> u32 {
    _VLV_PLL((ch), 7)
}

/// `VLV_PLL_DW16(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW16(ch: u32) -> u32 {
    _VLV_PLL((ch), 16)
}

/// `VLV_PLL_DW17(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW17(ch: u32) -> u32 {
    _VLV_PLL((ch), 17)
}

/// `VLV_PLL_DW18(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW18(ch: u32) -> u32 {
    _VLV_PLL((ch), 18)
}

/// `VLV_PLL_DW19(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PLL_DW19(ch: u32) -> u32 {
    _VLV_PLL((ch), 19)
}

/// `VLV_REF_DW11` (vlv_dpio_phy_regs.h)
pub const VLV_REF_DW11: u32 = _VLV_REF(11);

/// `VLV_CMN_DW0` (vlv_dpio_phy_regs.h)
pub const VLV_CMN_DW0: u32 = _VLV_CMN(0);

/// `VLV_PCS_DW0_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW0_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 0)
}

/// `VLV_PCS01_DW0(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW0(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 0)
}

/// `VLV_PCS23_DW0(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW0(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 0)
}

/// `DPIO_PCS_TX_LANE2_RESET` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX_LANE2_RESET: u32 = reg_bit(16);

/// `DPIO_PCS_TX_LANE1_RESET` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX_LANE1_RESET: u32 = reg_bit(7);

/// `DPIO_LEFT_TXFIFO_RST_MASTER2` (vlv_dpio_phy_regs.h)
pub const DPIO_LEFT_TXFIFO_RST_MASTER2: u32 = reg_bit(4);

/// `DPIO_RIGHT_TXFIFO_RST_MASTER2` (vlv_dpio_phy_regs.h)
pub const DPIO_RIGHT_TXFIFO_RST_MASTER2: u32 = reg_bit(3);

/// `VLV_PCS_DW1_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW1_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 1)
}

/// `VLV_PCS01_DW1(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW1(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 1)
}

/// `VLV_PCS23_DW1(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW1(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 1)
}

/// `CHV_PCS_REQ_SOFTRESET_EN` (vlv_dpio_phy_regs.h)
pub const CHV_PCS_REQ_SOFTRESET_EN: u32 = reg_bit(23);

/// `DPIO_PCS_CLK_CRI_RXEB_EIOS_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_CRI_RXEB_EIOS_EN: u32 = reg_bit(22);

/// `DPIO_PCS_CLK_CRI_RXDIGFILTSG_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_CRI_RXDIGFILTSG_EN: u32 = reg_bit(21);

/// `DPIO_PCS_CLK_DATAWIDTH_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_DATAWIDTH_MASK: u32 = reg_genmask(7, 6);

/// `DPIO_PCS_CLK_DATAWIDTH_8_10` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_DATAWIDTH_8_10: u32 = reg_field_prep(DPIO_PCS_CLK_DATAWIDTH_MASK, 1);

/// `DPIO_PCS_CLK_DATAWIDTH_16_20` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_DATAWIDTH_16_20: u32 = reg_field_prep(DPIO_PCS_CLK_DATAWIDTH_MASK, 2);

/// `DPIO_PCS_CLK_DATAWIDTH_32_40` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_DATAWIDTH_32_40: u32 = reg_field_prep(DPIO_PCS_CLK_DATAWIDTH_MASK, 3);

/// `DPIO_PCS_CLK_SOFT_RESET` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_CLK_SOFT_RESET: u32 = reg_bit(5);

/// `VLV_PCS_DW8_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW8_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 8)
}

/// `VLV_PCS01_DW8(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW8(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 8)
}

/// `VLV_PCS23_DW8(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW8(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 8)
}

/// `DPIO_PCS_USEDCLKCHANNEL` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_USEDCLKCHANNEL: u32 = reg_bit(21);

/// `DPIO_PCS_USEDCLKCHANNEL_OVRRIDE` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_USEDCLKCHANNEL_OVRRIDE: u32 = reg_bit(20);

/// `VLV_PCS_DW9_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW9_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 9)
}

/// `VLV_PCS01_DW9(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW9(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 9)
}

/// `VLV_PCS23_DW9(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW9(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 9)
}

/// `DPIO_PCS_TX2MARGIN_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2MARGIN_MASK: u32 = reg_genmask(15, 13);

/// `DPIO_PCS_TX2MARGIN_000` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2MARGIN_000: u32 = reg_field_prep(DPIO_PCS_TX2MARGIN_MASK, 0);

/// `DPIO_PCS_TX2MARGIN_101` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2MARGIN_101: u32 = reg_field_prep(DPIO_PCS_TX2MARGIN_MASK, 1);

/// `DPIO_PCS_TX1MARGIN_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1MARGIN_MASK: u32 = reg_genmask(12, 10);

/// `DPIO_PCS_TX1MARGIN_000` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1MARGIN_000: u32 = reg_field_prep(DPIO_PCS_TX1MARGIN_MASK, 0);

/// `DPIO_PCS_TX1MARGIN_101` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1MARGIN_101: u32 = reg_field_prep(DPIO_PCS_TX1MARGIN_MASK, 1);

/// `VLV_PCS_DW10_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW10_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 10)
}

/// `VLV_PCS01_DW10(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW10(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 10)
}

/// `VLV_PCS23_DW10(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW10(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 10)
}

/// `DPIO_PCS_SWING_CALC_TX1_TX3` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_SWING_CALC_TX1_TX3: u32 = reg_bit(31);

/// `DPIO_PCS_SWING_CALC_TX0_TX2` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_SWING_CALC_TX0_TX2: u32 = reg_bit(30);

/// `DPIO_PCS_TX2DEEMP_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2DEEMP_MASK: u32 = reg_genmask(27, 24);

/// `DPIO_PCS_TX2DEEMP_9P5` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2DEEMP_9P5: u32 = reg_field_prep(DPIO_PCS_TX2DEEMP_MASK, 0);

/// `DPIO_PCS_TX2DEEMP_6P0` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX2DEEMP_6P0: u32 = reg_field_prep(DPIO_PCS_TX2DEEMP_MASK, 2);

/// `DPIO_PCS_TX1DEEMP_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1DEEMP_MASK: u32 = reg_genmask(19, 16);

/// `DPIO_PCS_TX1DEEMP_9P5` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1DEEMP_9P5: u32 = reg_field_prep(DPIO_PCS_TX1DEEMP_MASK, 0);

/// `DPIO_PCS_TX1DEEMP_6P0` (vlv_dpio_phy_regs.h)
pub const DPIO_PCS_TX1DEEMP_6P0: u32 = reg_field_prep(DPIO_PCS_TX1DEEMP_MASK, 2);

/// `VLV_PCS_DW11_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW11_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 11)
}

/// `VLV_PCS01_DW11(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW11(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 11)
}

/// `VLV_PCS23_DW11(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW11(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 11)
}

/// `DPIO_TX2_STAGGER_MASK_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_TX2_STAGGER_MASK_MASK: u32 = reg_genmask(28, 24);

/// `DPIO_TX2_STAGGER_MASK(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_TX2_STAGGER_MASK(x: u32) -> u32 {
    reg_field_prep(DPIO_TX2_STAGGER_MASK_MASK, (x))
}

/// `DPIO_LANEDESKEW_STRAP_OVRD` (vlv_dpio_phy_regs.h)
pub const DPIO_LANEDESKEW_STRAP_OVRD: u32 = reg_bit(3);

/// `DPIO_LEFT_TXFIFO_RST_MASTER` (vlv_dpio_phy_regs.h)
pub const DPIO_LEFT_TXFIFO_RST_MASTER: u32 = reg_bit(1);

/// `DPIO_RIGHT_TXFIFO_RST_MASTER` (vlv_dpio_phy_regs.h)
pub const DPIO_RIGHT_TXFIFO_RST_MASTER: u32 = reg_bit(0);

/// `VLV_PCS_DW12_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW12_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 12)
}

/// `VLV_PCS01_DW12(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW12(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 12)
}

/// `VLV_PCS23_DW12(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW12(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 12)
}

/// `DPIO_TX2_STAGGER_MULT_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_TX2_STAGGER_MULT_MASK: u32 = reg_genmask(22, 20);

/// `DPIO_TX2_STAGGER_MULT(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_TX2_STAGGER_MULT(x: u32) -> u32 {
    reg_field_prep(DPIO_TX2_STAGGER_MULT_MASK, (x))
}

/// `DPIO_TX1_STAGGER_MULT_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_TX1_STAGGER_MULT_MASK: u32 = reg_genmask(20, 16);

/// `DPIO_TX1_STAGGER_MULT(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_TX1_STAGGER_MULT(x: u32) -> u32 {
    reg_field_prep(DPIO_TX1_STAGGER_MULT_MASK, (x))
}

/// `DPIO_TX1_STAGGER_MASK_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_TX1_STAGGER_MASK_MASK: u32 = reg_genmask(12, 8);

/// `DPIO_TX1_STAGGER_MASK(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_TX1_STAGGER_MASK(x: u32) -> u32 {
    reg_field_prep(DPIO_TX1_STAGGER_MASK_MASK, (x))
}

/// `DPIO_LANESTAGGER_STRAP_OVRD` (vlv_dpio_phy_regs.h)
pub const DPIO_LANESTAGGER_STRAP_OVRD: u32 = reg_bit(6);

/// `DPIO_LANESTAGGER_STRAP_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_LANESTAGGER_STRAP_MASK: u32 = reg_genmask(4, 0);

/// `DPIO_LANESTAGGER_STRAP(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_LANESTAGGER_STRAP(x: u32) -> u32 {
    reg_field_prep(DPIO_LANESTAGGER_STRAP_MASK, (x))
}

/// `VLV_PCS_DW14_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW14_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 14)
}

/// `VLV_PCS01_DW14(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW14(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 14)
}

/// `VLV_PCS23_DW14(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW14(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 14)
}

/// `VLV_PCS_DW17_BCAST` (vlv_dpio_phy_regs.h)
pub const VLV_PCS_DW17_BCAST: u32 = _VLV_PCS_BCAST(17);

/// `VLV_PCS_DW17_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW17_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 17)
}

/// `VLV_PCS01_DW17(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW17(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 17)
}

/// `VLV_PCS23_DW17(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW17(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 17)
}

/// `VLV_PCS_DW23_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS_DW23_GRP(ch: u32) -> u32 {
    _VLV_PCS_GRP((ch), 23)
}

/// `VLV_PCS01_DW23(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS01_DW23(ch: u32) -> u32 {
    _VLV_PCS((ch), 0, 23)
}

/// `VLV_PCS23_DW23(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_PCS23_DW23(ch: u32) -> u32 {
    _VLV_PCS((ch), 1, 23)
}

/// `VLV_TX_DW2_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW2_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 2)
}

/// `VLV_TX_DW2(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW2(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 2)
}

/// `DPIO_SWING_MARGIN000_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_SWING_MARGIN000_MASK: u32 = reg_genmask(23, 16);

/// `DPIO_SWING_MARGIN000(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_SWING_MARGIN000(x: u32) -> u32 {
    reg_field_prep(DPIO_SWING_MARGIN000_MASK, (x))
}

/// `DPIO_UNIQ_TRANS_SCALE_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_UNIQ_TRANS_SCALE_MASK: u32 = reg_genmask(15, 8);

/// `DPIO_UNIQ_TRANS_SCALE(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_UNIQ_TRANS_SCALE(x: u32) -> u32 {
    reg_field_prep(DPIO_UNIQ_TRANS_SCALE_MASK, (x))
}

/// `VLV_TX_DW3_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW3_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 3)
}

/// `VLV_TX_DW3(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW3(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 3)
}

/// `DPIO_TX_UNIQ_TRANS_SCALE_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_TX_UNIQ_TRANS_SCALE_EN: u32 = reg_bit(27);

/// `DPIO_SWING_MARGIN101_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_SWING_MARGIN101_MASK: u32 = reg_genmask(23, 16);

/// `DPIO_SWING_MARGIN101(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_SWING_MARGIN101(x: u32) -> u32 {
    reg_field_prep(DPIO_SWING_MARGIN101_MASK, (x))
}

/// `VLV_TX_DW4_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW4_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 4)
}

/// `VLV_TX_DW4(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW4(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 4)
}

/// `DPIO_SWING_DEEMPH9P5_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_SWING_DEEMPH9P5_MASK: u32 = reg_genmask(31, 24);

/// `DPIO_SWING_DEEMPH9P5(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_SWING_DEEMPH9P5(x: u32) -> u32 {
    reg_field_prep(DPIO_SWING_DEEMPH9P5_MASK, (x))
}

/// `DPIO_SWING_DEEMPH6P0_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_SWING_DEEMPH6P0_MASK: u32 = reg_genmask(23, 16);

/// `VLV_TX_DW5_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW5_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 5)
}

/// `VLV_TX_DW5(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW5(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 5)
}

/// `DPIO_TX_OCALINIT_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_TX_OCALINIT_EN: u32 = reg_bit(31);

/// `VLV_TX_DW11_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW11_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 11)
}

/// `VLV_TX_DW11(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW11(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 11)
}

/// `VLV_TX_DW14_GRP(ch)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW14_GRP(ch: u32) -> u32 {
    _VLV_TX_GRP((ch), 14)
}

/// `VLV_TX_DW14(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn VLV_TX_DW14(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 14)
}

/// `CHV_PLL_DW0(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW0(ch: u32) -> u32 {
    _CHV_PLL((ch), 0)
}

/// `DPIO_CHV_M2_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_M2_DIV_MASK: u32 = reg_genmask(7, 0);

/// `DPIO_CHV_M2_DIV(m2)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_M2_DIV(m2: u32) -> u32 {
    reg_field_prep(DPIO_CHV_M2_DIV_MASK, (m2))
}

/// `CHV_PLL_DW1(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW1(ch: u32) -> u32 {
    _CHV_PLL((ch), 1)
}

/// `DPIO_CHV_N_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_N_DIV_MASK: u32 = reg_genmask(11, 8);

/// `DPIO_CHV_N_DIV(n)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_N_DIV(n: u32) -> u32 {
    reg_field_prep(DPIO_CHV_N_DIV_MASK, (n))
}

/// `DPIO_CHV_M1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_M1_DIV_MASK: u32 = reg_genmask(2, 0);

/// `DPIO_CHV_M1_DIV(m1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_M1_DIV(m1: u32) -> u32 {
    reg_field_prep(DPIO_CHV_M1_DIV_MASK, (m1))
}

/// `DPIO_CHV_M1_DIV_BY_2` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_M1_DIV_BY_2: u32 = 0;

/// `CHV_PLL_DW2(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW2(ch: u32) -> u32 {
    _CHV_PLL((ch), 2)
}

/// `DPIO_CHV_M2_FRAC_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_M2_FRAC_DIV_MASK: u32 = reg_genmask(21, 0);

/// `DPIO_CHV_M2_FRAC_DIV(m2_frac)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_M2_FRAC_DIV(m2_frac: u32) -> u32 {
    reg_field_prep(DPIO_CHV_M2_FRAC_DIV_MASK, (m2_frac))
}

/// `CHV_PLL_DW3(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW3(ch: u32) -> u32 {
    _CHV_PLL((ch), 3)
}

/// `DPIO_CHV_FRAC_DIV_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_FRAC_DIV_EN: u32 = reg_bit(16);

/// `DPIO_CHV_SECOND_MOD` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_SECOND_MOD: u32 = reg_bit(8);

/// `DPIO_CHV_FEEDFWD_GAIN_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_FEEDFWD_GAIN_MASK: u32 = reg_genmask(3, 0);

/// `DPIO_CHV_FEEDFWD_GAIN(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_FEEDFWD_GAIN(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_FEEDFWD_GAIN_MASK, (x))
}

/// `CHV_PLL_DW6(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW6(ch: u32) -> u32 {
    _CHV_PLL((ch), 6)
}

/// `DPIO_CHV_GAIN_CTRL_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_GAIN_CTRL_MASK: u32 = reg_genmask(18, 16);

/// `DPIO_CHV_GAIN_CTRL(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_GAIN_CTRL(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_GAIN_CTRL_MASK, (x))
}

/// `DPIO_CHV_INT_COEFF_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_INT_COEFF_MASK: u32 = reg_genmask(12, 8);

/// `DPIO_CHV_INT_COEFF(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_INT_COEFF(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_INT_COEFF_MASK, (x))
}

/// `DPIO_CHV_PROP_COEFF_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_PROP_COEFF_MASK: u32 = reg_genmask(3, 0);

/// `DPIO_CHV_PROP_COEFF(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_PROP_COEFF(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_PROP_COEFF_MASK, (x))
}

/// `CHV_PLL_DW8(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW8(ch: u32) -> u32 {
    _CHV_PLL((ch), 8)
}

/// `DPIO_CHV_TDC_TARGET_CNT_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_TDC_TARGET_CNT_MASK: u32 = reg_genmask(9, 0);

/// `DPIO_CHV_TDC_TARGET_CNT(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_TDC_TARGET_CNT(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_TDC_TARGET_CNT_MASK, (x))
}

/// `CHV_PLL_DW9(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_PLL_DW9(ch: u32) -> u32 {
    _CHV_PLL((ch), 9)
}

/// `DPIO_CHV_INT_LOCK_THRESHOLD_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_INT_LOCK_THRESHOLD_MASK: u32 = reg_genmask(3, 1);

/// `DPIO_CHV_INT_LOCK_THRESHOLD(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_INT_LOCK_THRESHOLD(x: u32) -> u32 {
    reg_field_prep(DPIO_CHV_INT_LOCK_THRESHOLD_MASK, (x))
}

/// `DPIO_CHV_INT_LOCK_THRESHOLD_SEL_COARSE` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_INT_LOCK_THRESHOLD_SEL_COARSE: u32 = reg_bit(0);

/// `CHV_CMN_DW0_CH0` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW0_CH0: u32 = _CHV_CMN(0, 0);

/// `DPIO_ALLDL_POWERDOWN_CH0` (vlv_dpio_phy_regs.h)
pub const DPIO_ALLDL_POWERDOWN_CH0: u32 = reg_bit(19);

/// `DPIO_ANYDL_POWERDOWN_CH0` (vlv_dpio_phy_regs.h)
pub const DPIO_ANYDL_POWERDOWN_CH0: u32 = reg_bit(18);

/// `DPIO_ALLDL_POWERDOWN` (vlv_dpio_phy_regs.h)
pub const DPIO_ALLDL_POWERDOWN: u32 = reg_bit(1);

/// `DPIO_ANYDL_POWERDOWN` (vlv_dpio_phy_regs.h)
pub const DPIO_ANYDL_POWERDOWN: u32 = reg_bit(0);

/// `CHV_CMN_DW5_CH0` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW5_CH0: u32 = _CHV_CMN(0, 5);

/// `CHV_BUFRIGHTENA1_MASK` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA1_MASK: u32 = reg_genmask(21, 20);

/// `CHV_BUFRIGHTENA1_DISABLE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA1_DISABLE: u32 = reg_field_prep(CHV_BUFRIGHTENA1_MASK, 0);

/// `CHV_BUFRIGHTENA1_NORMAL` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA1_NORMAL: u32 = reg_field_prep(CHV_BUFRIGHTENA1_MASK, 1);

/// `CHV_BUFRIGHTENA1_FORCE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA1_FORCE: u32 = reg_field_prep(CHV_BUFRIGHTENA1_MASK, 3);

/// `CHV_BUFLEFTENA1_MASK` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA1_MASK: u32 = reg_genmask(23, 22);

/// `CHV_BUFLEFTENA1_DISABLE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA1_DISABLE: u32 = reg_field_prep(CHV_BUFLEFTENA1_MASK, 0);

/// `CHV_BUFLEFTENA1_NORMAL` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA1_NORMAL: u32 = reg_field_prep(CHV_BUFLEFTENA1_MASK, 1);

/// `CHV_BUFLEFTENA1_FORCE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA1_FORCE: u32 = reg_field_prep(CHV_BUFLEFTENA1_MASK, 3);

/// `CHV_CMN_DW13_CH0` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW13_CH0: u32 = _CHV_CMN(0, 13);

/// `CHV_CMN_DW0_CH1` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW0_CH1: u32 = _CHV_CMN(1, 0);

/// `DPIO_CHV_S1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_S1_DIV_MASK: u32 = reg_genmask(23, 21);

/// `DPIO_CHV_S1_DIV(s1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_S1_DIV(s1: u32) -> u32 {
    reg_field_prep(DPIO_CHV_S1_DIV_MASK, (s1))
}

/// `DPIO_CHV_P1_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_P1_DIV_MASK: u32 = reg_genmask(15, 13);

/// `DPIO_CHV_P1_DIV(p1)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_P1_DIV(p1: u32) -> u32 {
    reg_field_prep(DPIO_CHV_P1_DIV_MASK, (p1))
}

/// `DPIO_CHV_P2_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_P2_DIV_MASK: u32 = reg_genmask(12, 8);

/// `DPIO_CHV_P2_DIV(p2)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_P2_DIV(p2: u32) -> u32 {
    reg_field_prep(DPIO_CHV_P2_DIV_MASK, (p2))
}

/// `DPIO_CHV_K_DIV_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_CHV_K_DIV_MASK: u32 = reg_genmask(7, 4);

/// `DPIO_CHV_K_DIV(k)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_CHV_K_DIV(k: u32) -> u32 {
    reg_field_prep(DPIO_CHV_K_DIV_MASK, (k))
}

/// `DPIO_PLL_FREQLOCK` (vlv_dpio_phy_regs.h)
pub const DPIO_PLL_FREQLOCK: u32 = reg_bit(1);

/// `DPIO_PLL_LOCK` (vlv_dpio_phy_regs.h)
pub const DPIO_PLL_LOCK: u32 = reg_bit(0);

/// `CHV_CMN_DW13(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_CMN_DW13(ch: u32) -> u32 {
    _PIPE(ch, CHV_CMN_DW13_CH0, CHV_CMN_DW0_CH1)
}

/// `CHV_CMN_DW14_CH0` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW14_CH0: u32 = _CHV_CMN(0, 14);

/// `CHV_CMN_DW1_CH1` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW1_CH1: u32 = _CHV_CMN(1, 1);

/// `DPIO_AFC_RECAL` (vlv_dpio_phy_regs.h)
pub const DPIO_AFC_RECAL: u32 = reg_bit(14);

/// `DPIO_DCLKP_EN` (vlv_dpio_phy_regs.h)
pub const DPIO_DCLKP_EN: u32 = reg_bit(13);

/// `CHV_BUFLEFTENA2_MASK` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA2_MASK: u32 = reg_genmask(18, 17);

/// `CHV_BUFLEFTENA2_DISABLE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA2_DISABLE: u32 = reg_field_prep(CHV_BUFLEFTENA2_MASK, 0);

/// `CHV_BUFLEFTENA2_NORMAL` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA2_NORMAL: u32 = reg_field_prep(CHV_BUFLEFTENA2_MASK, 1);

/// `CHV_BUFLEFTENA2_FORCE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFLEFTENA2_FORCE: u32 = reg_field_prep(CHV_BUFLEFTENA2_MASK, 3);

/// `CHV_BUFRIGHTENA2_MASK` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA2_MASK: u32 = reg_genmask(20, 19);

/// `CHV_BUFRIGHTENA2_DISABLE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA2_DISABLE: u32 = reg_field_prep(CHV_BUFRIGHTENA2_MASK, 0);

/// `CHV_BUFRIGHTENA2_NORMAL` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA2_NORMAL: u32 = reg_field_prep(CHV_BUFRIGHTENA2_MASK, 1);

/// `CHV_BUFRIGHTENA2_FORCE` (vlv_dpio_phy_regs.h)
pub const CHV_BUFRIGHTENA2_FORCE: u32 = reg_field_prep(CHV_BUFRIGHTENA2_MASK, 3);

/// `CHV_CMN_DW14(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_CMN_DW14(ch: u32) -> u32 {
    _PIPE(ch, CHV_CMN_DW14_CH0, CHV_CMN_DW1_CH1)
}

/// `CHV_CMN_DW19_CH0` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW19_CH0: u32 = _CHV_CMN(0, 19);

/// `CHV_CMN_DW6_CH1` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW6_CH1: u32 = _CHV_CMN(1, 6);

/// `DPIO_ALLDL_POWERDOWN_CH1` (vlv_dpio_phy_regs.h)
pub const DPIO_ALLDL_POWERDOWN_CH1: u32 = reg_bit(30);

/// `DPIO_ANYDL_POWERDOWN_CH1` (vlv_dpio_phy_regs.h)
pub const DPIO_ANYDL_POWERDOWN_CH1: u32 = reg_bit(29);

/// `DPIO_DYNPWRDOWNEN_CH1` (vlv_dpio_phy_regs.h)
pub const DPIO_DYNPWRDOWNEN_CH1: u32 = reg_bit(28);

/// `CHV_CMN_USEDCLKCHANNEL` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_USEDCLKCHANNEL: u32 = reg_bit(13);

/// `CHV_CMN_DW19(ch)` (vlv_dpio_phy_regs.h)
pub const fn CHV_CMN_DW19(ch: u32) -> u32 {
    _PIPE(ch, CHV_CMN_DW19_CH0, CHV_CMN_DW6_CH1)
}

/// `CHV_CMN_DW28` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW28: u32 = _CHV_CMN(0, 28);

/// `DPIO_CL1POWERDOWNEN` (vlv_dpio_phy_regs.h)
pub const DPIO_CL1POWERDOWNEN: u32 = reg_bit(23);

/// `DPIO_DYNPWRDOWNEN_CH0` (vlv_dpio_phy_regs.h)
pub const DPIO_DYNPWRDOWNEN_CH0: u32 = reg_bit(22);

/// `DPIO_SUS_CLK_CONFIG_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_SUS_CLK_CONFIG_MASK: u32 = reg_genmask(1, 0);

/// `DPIO_SUS_CLK_CONFIG_ON` (vlv_dpio_phy_regs.h)
pub const DPIO_SUS_CLK_CONFIG_ON: u32 = reg_field_prep(DPIO_SUS_CLK_CONFIG_MASK, 0);

/// `DPIO_SUS_CLK_CONFIG_CLKREQ` (vlv_dpio_phy_regs.h)
pub const DPIO_SUS_CLK_CONFIG_CLKREQ: u32 = reg_field_prep(DPIO_SUS_CLK_CONFIG_MASK, 1);

/// `DPIO_SUS_CLK_CONFIG_GATE` (vlv_dpio_phy_regs.h)
pub const DPIO_SUS_CLK_CONFIG_GATE: u32 = reg_field_prep(DPIO_SUS_CLK_CONFIG_MASK, 2);

/// `DPIO_SUS_CLK_CONFIG_GATE_CLKREQ` (vlv_dpio_phy_regs.h)
pub const DPIO_SUS_CLK_CONFIG_GATE_CLKREQ: u32 = reg_field_prep(DPIO_SUS_CLK_CONFIG_MASK, 3);

/// `CHV_CMN_DW30` (vlv_dpio_phy_regs.h)
pub const CHV_CMN_DW30: u32 = _CHV_CMN(0, 30);

/// `DPIO_CL2_LDOFUSE_PWRENB` (vlv_dpio_phy_regs.h)
pub const DPIO_CL2_LDOFUSE_PWRENB: u32 = reg_bit(6);

/// `DPIO_LRC_BYPASS` (vlv_dpio_phy_regs.h)
pub const DPIO_LRC_BYPASS: u32 = reg_bit(3);

/// `CHV_TX_DW0(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW0(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 0)
}

/// `CHV_TX_DW1(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW1(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 1)
}

/// `CHV_TX_DW2(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW2(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 2)
}

/// `CHV_TX_DW3(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW3(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 3)
}

/// `CHV_TX_DW4(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW4(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 4)
}

/// `CHV_TX_DW5(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW5(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 5)
}

/// `CHV_TX_DW6(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW6(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 6)
}

/// `CHV_TX_DW7(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW7(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 7)
}

/// `CHV_TX_DW8(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW8(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 8)
}

/// `CHV_TX_DW9(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW9(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 9)
}

/// `CHV_TX_DW10(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW10(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 10)
}

/// `CHV_TX_DW11(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW11(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 11)
}

/// `DPIO_FRC_LATENCY_MASK` (vlv_dpio_phy_regs.h)
pub const DPIO_FRC_LATENCY_MASK: u32 = reg_genmask(10, 8);

/// `DPIO_FRC_LATENCY(x)` (vlv_dpio_phy_regs.h)
pub const fn DPIO_FRC_LATENCY(x: u32) -> u32 {
    reg_field_prep(DPIO_FRC_LATENCY_MASK, (x))
}

/// `CHV_TX_DW14(ch, lane)` (vlv_dpio_phy_regs.h)
pub const fn CHV_TX_DW14(ch: u32, lane: u32) -> u32 {
    _VLV_TX((ch), (lane), 14)
}

/// `DPIO_UPAR` (vlv_dpio_phy_regs.h)
pub const DPIO_UPAR: u32 = reg_bit(30);
