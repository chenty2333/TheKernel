// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_mg_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `MG_PHY_PORT_LN(ln, tc_port, ln0p1, ln0p2, ln1p1)` (intel_mg_phy_regs.h)
pub const fn MG_PHY_PORT_LN(ln: u32, tc_port: u32, ln0p1: u32, ln0p2: u32, ln1p1: u32) -> I915RegT {
    mmio((_PORT(tc_port, ln0p1, ln0p2) + ((ln) * (((ln1p1) - (ln0p1))))))
}

/// `MG_TX_LINK_PARAMS_TX1LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX1LN0_PORT1: u32 = 0x16812C;

/// `MG_TX_LINK_PARAMS_TX1LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX1LN1_PORT1: u32 = 0x16852C;

/// `MG_TX_LINK_PARAMS_TX1LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX1LN0_PORT2: u32 = 0x16912C;

/// `MG_TX_LINK_PARAMS_TX1LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX1LN1_PORT2: u32 = 0x16952C;

/// `MG_TX1_LINK_PARAMS(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX1_LINK_PARAMS(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_LINK_PARAMS_TX1LN0_PORT1, MG_TX_LINK_PARAMS_TX1LN0_PORT2, MG_TX_LINK_PARAMS_TX1LN1_PORT1)
}

/// `MG_TX_LINK_PARAMS_TX2LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX2LN0_PORT1: u32 = 0x1680AC;

/// `MG_TX_LINK_PARAMS_TX2LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX2LN1_PORT1: u32 = 0x1684AC;

/// `MG_TX_LINK_PARAMS_TX2LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX2LN0_PORT2: u32 = 0x1690AC;

/// `MG_TX_LINK_PARAMS_TX2LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_LINK_PARAMS_TX2LN1_PORT2: u32 = 0x1694AC;

/// `MG_TX2_LINK_PARAMS(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX2_LINK_PARAMS(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_LINK_PARAMS_TX2LN0_PORT1, MG_TX_LINK_PARAMS_TX2LN0_PORT2, MG_TX_LINK_PARAMS_TX2LN1_PORT1)
}

/// `CRI_USE_FS32` (intel_mg_phy_regs.h)
pub const CRI_USE_FS32: u32 = ((1 << 5));

/// `MG_TX_PISO_READLOAD_TX1LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX1LN0_PORT1: u32 = 0x16814C;

/// `MG_TX_PISO_READLOAD_TX1LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX1LN1_PORT1: u32 = 0x16854C;

/// `MG_TX_PISO_READLOAD_TX1LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX1LN0_PORT2: u32 = 0x16914C;

/// `MG_TX_PISO_READLOAD_TX1LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX1LN1_PORT2: u32 = 0x16954C;

/// `MG_TX1_PISO_READLOAD(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX1_PISO_READLOAD(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_PISO_READLOAD_TX1LN0_PORT1, MG_TX_PISO_READLOAD_TX1LN0_PORT2, MG_TX_PISO_READLOAD_TX1LN1_PORT1)
}

/// `MG_TX_PISO_READLOAD_TX2LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX2LN0_PORT1: u32 = 0x1680CC;

/// `MG_TX_PISO_READLOAD_TX2LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX2LN1_PORT1: u32 = 0x1684CC;

/// `MG_TX_PISO_READLOAD_TX2LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX2LN0_PORT2: u32 = 0x1690CC;

/// `MG_TX_PISO_READLOAD_TX2LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_PISO_READLOAD_TX2LN1_PORT2: u32 = 0x1694CC;

/// `MG_TX2_PISO_READLOAD(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX2_PISO_READLOAD(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_PISO_READLOAD_TX2LN0_PORT1, MG_TX_PISO_READLOAD_TX2LN0_PORT2, MG_TX_PISO_READLOAD_TX2LN1_PORT1)
}

/// `CRI_CALCINIT` (intel_mg_phy_regs.h)
pub const CRI_CALCINIT: u32 = ((1 << 1));

/// `MG_TX_SWINGCTRL_TX1LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX1LN0_PORT1: u32 = 0x168148;

/// `MG_TX_SWINGCTRL_TX1LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX1LN1_PORT1: u32 = 0x168548;

/// `MG_TX_SWINGCTRL_TX1LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX1LN0_PORT2: u32 = 0x169148;

/// `MG_TX_SWINGCTRL_TX1LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX1LN1_PORT2: u32 = 0x169548;

/// `MG_TX1_SWINGCTRL(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX1_SWINGCTRL(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_SWINGCTRL_TX1LN0_PORT1, MG_TX_SWINGCTRL_TX1LN0_PORT2, MG_TX_SWINGCTRL_TX1LN1_PORT1)
}

/// `MG_TX_SWINGCTRL_TX2LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX2LN0_PORT1: u32 = 0x1680C8;

/// `MG_TX_SWINGCTRL_TX2LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX2LN1_PORT1: u32 = 0x1684C8;

/// `MG_TX_SWINGCTRL_TX2LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX2LN0_PORT2: u32 = 0x1690C8;

/// `MG_TX_SWINGCTRL_TX2LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_SWINGCTRL_TX2LN1_PORT2: u32 = 0x1694C8;

/// `MG_TX2_SWINGCTRL(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX2_SWINGCTRL(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_SWINGCTRL_TX2LN0_PORT1, MG_TX_SWINGCTRL_TX2LN0_PORT2, MG_TX_SWINGCTRL_TX2LN1_PORT1)
}

/// `CRI_TXDEEMPH_OVERRIDE_17_12(x)` (intel_mg_phy_regs.h)
pub const fn CRI_TXDEEMPH_OVERRIDE_17_12(x: u32) -> u32 {
    (((x) << 0))
}

/// `CRI_TXDEEMPH_OVERRIDE_17_12_MASK` (intel_mg_phy_regs.h)
pub const CRI_TXDEEMPH_OVERRIDE_17_12_MASK: u32 = ((0x3F << 0));

/// `MG_TX_DRVCTRL_TX1LN0_TXPORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN0_TXPORT1: u32 = 0x168144;

/// `MG_TX_DRVCTRL_TX1LN1_TXPORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN1_TXPORT1: u32 = 0x168544;

/// `MG_TX_DRVCTRL_TX1LN0_TXPORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN0_TXPORT2: u32 = 0x169144;

/// `MG_TX_DRVCTRL_TX1LN1_TXPORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN1_TXPORT2: u32 = 0x169544;

/// `MG_TX_DRVCTRL_TX1LN0_TXPORT3` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN0_TXPORT3: u32 = 0x16A144;

/// `MG_TX_DRVCTRL_TX1LN1_TXPORT3` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN1_TXPORT3: u32 = 0x16A544;

/// `MG_TX_DRVCTRL_TX1LN0_TXPORT4` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN0_TXPORT4: u32 = 0x16B144;

/// `MG_TX_DRVCTRL_TX1LN1_TXPORT4` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX1LN1_TXPORT4: u32 = 0x16B544;

/// `MG_TX1_DRVCTRL(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX1_DRVCTRL(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_DRVCTRL_TX1LN0_TXPORT1, MG_TX_DRVCTRL_TX1LN0_TXPORT2, MG_TX_DRVCTRL_TX1LN1_TXPORT1)
}

/// `MG_TX_DRVCTRL_TX2LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX2LN0_PORT1: u32 = 0x1680C4;

/// `MG_TX_DRVCTRL_TX2LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX2LN1_PORT1: u32 = 0x1684C4;

/// `MG_TX_DRVCTRL_TX2LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX2LN0_PORT2: u32 = 0x1690C4;

/// `MG_TX_DRVCTRL_TX2LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DRVCTRL_TX2LN1_PORT2: u32 = 0x1694C4;

/// `MG_TX2_DRVCTRL(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX2_DRVCTRL(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_DRVCTRL_TX2LN0_PORT1, MG_TX_DRVCTRL_TX2LN0_PORT2, MG_TX_DRVCTRL_TX2LN1_PORT1)
}

/// `CRI_TXDEEMPH_OVERRIDE_11_6(x)` (intel_mg_phy_regs.h)
pub const fn CRI_TXDEEMPH_OVERRIDE_11_6(x: u32) -> u32 {
    (((x) << 24))
}

/// `CRI_TXDEEMPH_OVERRIDE_11_6_MASK` (intel_mg_phy_regs.h)
pub const CRI_TXDEEMPH_OVERRIDE_11_6_MASK: u32 = ((0x3F << 24));

/// `CRI_TXDEEMPH_OVERRIDE_EN` (intel_mg_phy_regs.h)
pub const CRI_TXDEEMPH_OVERRIDE_EN: u32 = ((1 << 22));

/// `CRI_TXDEEMPH_OVERRIDE_5_0(x)` (intel_mg_phy_regs.h)
pub const fn CRI_TXDEEMPH_OVERRIDE_5_0(x: u32) -> u32 {
    (((x) << 16))
}

/// `CRI_TXDEEMPH_OVERRIDE_5_0_MASK` (intel_mg_phy_regs.h)
pub const CRI_TXDEEMPH_OVERRIDE_5_0_MASK: u32 = ((0x3F << 16));

/// `CRI_LOADGEN_SEL(x)` (intel_mg_phy_regs.h)
pub const fn CRI_LOADGEN_SEL(x: u32) -> u32 {
    (((x) << 12))
}

/// `CRI_LOADGEN_SEL_MASK` (intel_mg_phy_regs.h)
pub const CRI_LOADGEN_SEL_MASK: u32 = ((0x3 << 12));

/// `MG_CLKHUB_LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_CLKHUB_LN0_PORT1: u32 = 0x16839C;

/// `MG_CLKHUB_LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_CLKHUB_LN1_PORT1: u32 = 0x16879C;

/// `MG_CLKHUB_LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_CLKHUB_LN0_PORT2: u32 = 0x16939C;

/// `MG_CLKHUB_LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_CLKHUB_LN1_PORT2: u32 = 0x16979C;

/// `MG_CLKHUB(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_CLKHUB(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_CLKHUB_LN0_PORT1, MG_CLKHUB_LN0_PORT2, MG_CLKHUB_LN1_PORT1)
}

/// `CFG_LOW_RATE_LKREN_EN` (intel_mg_phy_regs.h)
pub const CFG_LOW_RATE_LKREN_EN: u32 = ((1 << 11));

/// `MG_TX_DCC_TX1LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX1LN0_PORT1: u32 = 0x168110;

/// `MG_TX_DCC_TX1LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX1LN1_PORT1: u32 = 0x168510;

/// `MG_TX_DCC_TX1LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX1LN0_PORT2: u32 = 0x169110;

/// `MG_TX_DCC_TX1LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX1LN1_PORT2: u32 = 0x169510;

/// `MG_TX1_DCC(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX1_DCC(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_DCC_TX1LN0_PORT1, MG_TX_DCC_TX1LN0_PORT2, MG_TX_DCC_TX1LN1_PORT1)
}

/// `MG_TX_DCC_TX2LN0_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX2LN0_PORT1: u32 = 0x168090;

/// `MG_TX_DCC_TX2LN1_PORT1` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX2LN1_PORT1: u32 = 0x168490;

/// `MG_TX_DCC_TX2LN0_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX2LN0_PORT2: u32 = 0x169090;

/// `MG_TX_DCC_TX2LN1_PORT2` (intel_mg_phy_regs.h)
pub const MG_TX_DCC_TX2LN1_PORT2: u32 = 0x169490;

/// `MG_TX2_DCC(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_TX2_DCC(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_TX_DCC_TX2LN0_PORT1, MG_TX_DCC_TX2LN0_PORT2, MG_TX_DCC_TX2LN1_PORT1)
}

/// `CFG_AMI_CK_DIV_OVERRIDE_VAL(x)` (intel_mg_phy_regs.h)
pub const fn CFG_AMI_CK_DIV_OVERRIDE_VAL(x: u32) -> u32 {
    (((x) << 25))
}

/// `CFG_AMI_CK_DIV_OVERRIDE_VAL_MASK` (intel_mg_phy_regs.h)
pub const CFG_AMI_CK_DIV_OVERRIDE_VAL_MASK: u32 = ((0x3 << 25));

/// `CFG_AMI_CK_DIV_OVERRIDE_EN` (intel_mg_phy_regs.h)
pub const CFG_AMI_CK_DIV_OVERRIDE_EN: u32 = ((1 << 24));

/// `MG_DP_MODE_LN0_ACU_PORT1` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_LN0_ACU_PORT1: u32 = 0x1683A0;

/// `MG_DP_MODE_LN1_ACU_PORT1` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_LN1_ACU_PORT1: u32 = 0x1687A0;

/// `MG_DP_MODE_LN0_ACU_PORT2` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_LN0_ACU_PORT2: u32 = 0x1693A0;

/// `MG_DP_MODE_LN1_ACU_PORT2` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_LN1_ACU_PORT2: u32 = 0x1697A0;

/// `MG_DP_MODE(ln, tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_DP_MODE(ln: u32, tc_port: u32) -> I915RegT {
    MG_PHY_PORT_LN(ln, tc_port, MG_DP_MODE_LN0_ACU_PORT1, MG_DP_MODE_LN0_ACU_PORT2, MG_DP_MODE_LN1_ACU_PORT1)
}

/// `MG_DP_MODE_CFG_DP_X2_MODE` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_CFG_DP_X2_MODE: u32 = ((1 << 7));

/// `MG_DP_MODE_CFG_DP_X1_MODE` (intel_mg_phy_regs.h)
pub const MG_DP_MODE_CFG_DP_X1_MODE: u32 = ((1 << 6));

/// `FIA1_BASE` (intel_mg_phy_regs.h)
pub const FIA1_BASE: u32 = 0x163000;

/// `FIA2_BASE` (intel_mg_phy_regs.h)
pub const FIA2_BASE: u32 = 0x16E000;

/// `FIA3_BASE` (intel_mg_phy_regs.h)
pub const FIA3_BASE: u32 = 0x16F000;

/// `_FIA(fia)` (intel_mg_phy_regs.h)
pub const fn _FIA(fia: u32) -> u32 {
    pick_even_2ranges((fia), 1, FIA1_BASE, FIA1_BASE, FIA2_BASE, FIA3_BASE)
}

/// `_MMIO_FIA(fia, off)` (intel_mg_phy_regs.h)
pub const fn _MMIO_FIA(fia: u32, off: u32) -> I915RegT {
    mmio((_FIA(fia) + (off)))
}

/// `PORT_TX_DFLEXDPMLE1(fia)` (intel_mg_phy_regs.h)
pub const fn PORT_TX_DFLEXDPMLE1(fia: u32) -> I915RegT {
    _MMIO_FIA((fia), 0x008C0)
}

/// `DFLEXDPMLE1_DPMLETC_MASK(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_MASK(idx: u32) -> u32 {
    ((0xf << ((4 * (idx)))))
}

/// `DFLEXDPMLE1_DPMLETC_ML0(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_ML0(idx: u32) -> u32 {
    ((1 << ((4 * (idx)))))
}

/// `DFLEXDPMLE1_DPMLETC_ML1_0(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_ML1_0(idx: u32) -> u32 {
    ((3 << ((4 * (idx)))))
}

/// `DFLEXDPMLE1_DPMLETC_ML3(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_ML3(idx: u32) -> u32 {
    ((8 << ((4 * (idx)))))
}

/// `DFLEXDPMLE1_DPMLETC_ML3_2(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_ML3_2(idx: u32) -> u32 {
    ((12 << ((4 * (idx)))))
}

/// `DFLEXDPMLE1_DPMLETC_ML3_0(idx)` (intel_mg_phy_regs.h)
pub const fn DFLEXDPMLE1_DPMLETC_ML3_0(idx: u32) -> u32 {
    ((15 << ((4 * (idx)))))
}

/// `_MG_REFCLKIN_CTL_PORT1` (intel_mg_phy_regs.h)
pub const _MG_REFCLKIN_CTL_PORT1: u32 = 0x16892C;

/// `_MG_REFCLKIN_CTL_PORT2` (intel_mg_phy_regs.h)
pub const _MG_REFCLKIN_CTL_PORT2: u32 = 0x16992C;

/// `MG_REFCLKIN_CTL_OD_2_MUX(x)` (intel_mg_phy_regs.h)
pub const fn MG_REFCLKIN_CTL_OD_2_MUX(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_REFCLKIN_CTL_OD_2_MUX_MASK` (intel_mg_phy_regs.h)
pub const MG_REFCLKIN_CTL_OD_2_MUX_MASK: u32 = ((0x7 << 8));

/// `MG_REFCLKIN_CTL(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_REFCLKIN_CTL(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_REFCLKIN_CTL_PORT1, _MG_REFCLKIN_CTL_PORT2)
}

/// `_MG_CLKTOP2_CORECLKCTL1_PORT1` (intel_mg_phy_regs.h)
pub const _MG_CLKTOP2_CORECLKCTL1_PORT1: u32 = 0x1688D8;

/// `_MG_CLKTOP2_CORECLKCTL1_PORT2` (intel_mg_phy_regs.h)
pub const _MG_CLKTOP2_CORECLKCTL1_PORT2: u32 = 0x1698D8;

/// `MG_CLKTOP2_CORECLKCTL1_B_DIVRATIO(x)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_CORECLKCTL1_B_DIVRATIO(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_CLKTOP2_CORECLKCTL1_B_DIVRATIO_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_CORECLKCTL1_B_DIVRATIO_MASK: u32 = ((0xff << 16));

/// `MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO(x)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK: u32 = ((0xff << 8));

/// `MG_CLKTOP2_CORECLKCTL1(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_CORECLKCTL1(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_CLKTOP2_CORECLKCTL1_PORT1, _MG_CLKTOP2_CORECLKCTL1_PORT2)
}

/// `_MG_CLKTOP2_HSCLKCTL_PORT1` (intel_mg_phy_regs.h)
pub const _MG_CLKTOP2_HSCLKCTL_PORT1: u32 = 0x1688D4;

/// `_MG_CLKTOP2_HSCLKCTL_PORT2` (intel_mg_phy_regs.h)
pub const _MG_CLKTOP2_HSCLKCTL_PORT2: u32 = 0x1698D4;

/// `MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL(x)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK: u32 = ((0x1 << 16));

/// `MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL(x)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL(x: u32) -> u32 {
    (((x) << 14))
}

/// `MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK: u32 = ((0x3 << 14));

/// `MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK: u32 = ((0x3 << 12));

/// `MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2: u32 = ((0 << 12));

/// `MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3: u32 = ((1 << 12));

/// `MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5: u32 = ((2 << 12));

/// `MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7: u32 = ((3 << 12));

/// `MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO(x)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_SHIFT` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_SHIFT: u32 = 8;

/// `MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK` (intel_mg_phy_regs.h)
pub const MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK: u32 = ((0xf << 8));

/// `MG_CLKTOP2_HSCLKCTL(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_CLKTOP2_HSCLKCTL(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_CLKTOP2_HSCLKCTL_PORT1, _MG_CLKTOP2_HSCLKCTL_PORT2)
}

/// `_MG_PLL_DIV0_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_DIV0_PORT1: u32 = 0x168A00;

/// `_MG_PLL_DIV0_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_DIV0_PORT2: u32 = 0x169A00;

/// `MG_PLL_DIV0_FBDIV_FRAC_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV0_FBDIV_FRAC_MASK: u32 = ((0x3fffff << 8));

/// `MG_PLL_DIV0_FBDIV_FRAC_SHIFT` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV0_FBDIV_FRAC_SHIFT: u32 = 8;

/// `MG_PLL_DIV0_FBDIV_FRAC(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV0_FBDIV_FRAC(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_PLL_DIV0_FBDIV_INT_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV0_FBDIV_INT_MASK: u32 = ((0xff << 0));

/// `MG_PLL_DIV0_FBDIV_INT(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV0_FBDIV_INT(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_DIV0(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV0(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_DIV0_PORT1, _MG_PLL_DIV0_PORT2)
}

/// `_MG_PLL_DIV1_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_DIV1_PORT1: u32 = 0x168A04;

/// `_MG_PLL_DIV1_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_DIV1_PORT2: u32 = 0x169A04;

/// `MG_PLL_DIV1_IREF_NDIVRATIO(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV1_IREF_NDIVRATIO(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_PLL_DIV1_DITHER_DIV_1` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV1_DITHER_DIV_1: u32 = ((0 << 12));

/// `MG_PLL_DIV1_DITHER_DIV_2` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV1_DITHER_DIV_2: u32 = ((1 << 12));

/// `MG_PLL_DIV1_DITHER_DIV_4` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV1_DITHER_DIV_4: u32 = ((2 << 12));

/// `MG_PLL_DIV1_DITHER_DIV_8` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV1_DITHER_DIV_8: u32 = ((3 << 12));

/// `MG_PLL_DIV1_NDIVRATIO(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV1_NDIVRATIO(x: u32) -> u32 {
    (((x) << 4))
}

/// `MG_PLL_DIV1_FBPREDIV_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_DIV1_FBPREDIV_MASK: u32 = ((0xf << 0));

/// `MG_PLL_DIV1_FBPREDIV(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV1_FBPREDIV(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_DIV1(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_DIV1(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_DIV1_PORT1, _MG_PLL_DIV1_PORT2)
}

/// `_MG_PLL_LF_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_LF_PORT1: u32 = 0x168A08;

/// `_MG_PLL_LF_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_LF_PORT2: u32 = 0x169A08;

/// `MG_PLL_LF_TDCTARGETCNT(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_LF_TDCTARGETCNT(x: u32) -> u32 {
    (((x) << 24))
}

/// `MG_PLL_LF_AFCCNTSEL_256` (intel_mg_phy_regs.h)
pub const MG_PLL_LF_AFCCNTSEL_256: u32 = ((0 << 20));

/// `MG_PLL_LF_AFCCNTSEL_512` (intel_mg_phy_regs.h)
pub const MG_PLL_LF_AFCCNTSEL_512: u32 = ((1 << 20));

/// `MG_PLL_LF_GAINCTRL(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_LF_GAINCTRL(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_PLL_LF_INT_COEFF(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_LF_INT_COEFF(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_PLL_LF_PROP_COEFF(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_LF_PROP_COEFF(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_LF(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_LF(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_LF_PORT1, _MG_PLL_LF_PORT2)
}

/// `_MG_PLL_FRAC_LOCK_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_FRAC_LOCK_PORT1: u32 = 0x168A0C;

/// `_MG_PLL_FRAC_LOCK_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_FRAC_LOCK_PORT2: u32 = 0x169A0C;

/// `MG_PLL_FRAC_LOCK_TRUELOCK_CRIT_32` (intel_mg_phy_regs.h)
pub const MG_PLL_FRAC_LOCK_TRUELOCK_CRIT_32: u32 = ((1 << 18));

/// `MG_PLL_FRAC_LOCK_EARLYLOCK_CRIT_32` (intel_mg_phy_regs.h)
pub const MG_PLL_FRAC_LOCK_EARLYLOCK_CRIT_32: u32 = ((1 << 16));

/// `MG_PLL_FRAC_LOCK_LOCKTHRESH(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_FRAC_LOCK_LOCKTHRESH(x: u32) -> u32 {
    (((x) << 11))
}

/// `MG_PLL_FRAC_LOCK_DCODITHEREN` (intel_mg_phy_regs.h)
pub const MG_PLL_FRAC_LOCK_DCODITHEREN: u32 = ((1 << 10));

/// `MG_PLL_FRAC_LOCK_FEEDFWRDCAL_EN` (intel_mg_phy_regs.h)
pub const MG_PLL_FRAC_LOCK_FEEDFWRDCAL_EN: u32 = ((1 << 8));

/// `MG_PLL_FRAC_LOCK_FEEDFWRDGAIN(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_FRAC_LOCK_FEEDFWRDGAIN(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_FRAC_LOCK(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_FRAC_LOCK(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_FRAC_LOCK_PORT1, _MG_PLL_FRAC_LOCK_PORT2)
}

/// `_MG_PLL_SSC_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_SSC_PORT1: u32 = 0x168A10;

/// `_MG_PLL_SSC_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_SSC_PORT2: u32 = 0x169A10;

/// `MG_PLL_SSC_EN` (intel_mg_phy_regs.h)
pub const MG_PLL_SSC_EN: u32 = ((1 << 28));

/// `MG_PLL_SSC_TYPE(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_SSC_TYPE(x: u32) -> u32 {
    (((x) << 26))
}

/// `MG_PLL_SSC_STEPLENGTH(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_SSC_STEPLENGTH(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_PLL_SSC_STEPNUM(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_SSC_STEPNUM(x: u32) -> u32 {
    (((x) << 10))
}

/// `MG_PLL_SSC_FLLEN` (intel_mg_phy_regs.h)
pub const MG_PLL_SSC_FLLEN: u32 = ((1 << 9));

/// `MG_PLL_SSC_STEPSIZE(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_SSC_STEPSIZE(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_SSC(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_SSC(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_SSC_PORT1, _MG_PLL_SSC_PORT2)
}

/// `_MG_PLL_BIAS_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_BIAS_PORT1: u32 = 0x168A14;

/// `_MG_PLL_BIAS_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_BIAS_PORT2: u32 = 0x169A14;

/// `MG_PLL_BIAS_BIAS_GB_SEL(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_BIAS_GB_SEL(x: u32) -> u32 {
    (((x) << 30))
}

/// `MG_PLL_BIAS_BIAS_GB_SEL_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_BIAS_GB_SEL_MASK: u32 = ((0x3 << 30));

/// `MG_PLL_BIAS_INIT_DCOAMP(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_INIT_DCOAMP(x: u32) -> u32 {
    (((x) << 24))
}

/// `MG_PLL_BIAS_INIT_DCOAMP_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_INIT_DCOAMP_MASK: u32 = ((0x3f << 24));

/// `MG_PLL_BIAS_BIAS_BONUS(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_BIAS_BONUS(x: u32) -> u32 {
    (((x) << 16))
}

/// `MG_PLL_BIAS_BIAS_BONUS_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_BIAS_BONUS_MASK: u32 = ((0xff << 16));

/// `MG_PLL_BIAS_BIASCAL_EN` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_BIASCAL_EN: u32 = ((1 << 15));

/// `MG_PLL_BIAS_CTRIM(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_CTRIM(x: u32) -> u32 {
    (((x) << 8))
}

/// `MG_PLL_BIAS_CTRIM_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_CTRIM_MASK: u32 = ((0x1f << 8));

/// `MG_PLL_BIAS_VREF_RDAC(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_VREF_RDAC(x: u32) -> u32 {
    (((x) << 5))
}

/// `MG_PLL_BIAS_VREF_RDAC_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_VREF_RDAC_MASK: u32 = ((0x7 << 5));

/// `MG_PLL_BIAS_IREFTRIM(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS_IREFTRIM(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_BIAS_IREFTRIM_MASK` (intel_mg_phy_regs.h)
pub const MG_PLL_BIAS_IREFTRIM_MASK: u32 = ((0x1f << 0));

/// `MG_PLL_BIAS(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_BIAS(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_BIAS_PORT1, _MG_PLL_BIAS_PORT2)
}

/// `_MG_PLL_TDC_COLDST_BIAS_PORT1` (intel_mg_phy_regs.h)
pub const _MG_PLL_TDC_COLDST_BIAS_PORT1: u32 = 0x168A18;

/// `_MG_PLL_TDC_COLDST_BIAS_PORT2` (intel_mg_phy_regs.h)
pub const _MG_PLL_TDC_COLDST_BIAS_PORT2: u32 = 0x169A18;

/// `MG_PLL_TDC_COLDST_IREFINT_EN` (intel_mg_phy_regs.h)
pub const MG_PLL_TDC_COLDST_IREFINT_EN: u32 = ((1 << 27));

/// `MG_PLL_TDC_COLDST_REFBIAS_START_PULSE_W(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_TDC_COLDST_REFBIAS_START_PULSE_W(x: u32) -> u32 {
    (((x) << 17))
}

/// `MG_PLL_TDC_COLDST_COLDSTART` (intel_mg_phy_regs.h)
pub const MG_PLL_TDC_COLDST_COLDSTART: u32 = ((1 << 16));

/// `MG_PLL_TDC_TDCOVCCORR_EN` (intel_mg_phy_regs.h)
pub const MG_PLL_TDC_TDCOVCCORR_EN: u32 = ((1 << 2));

/// `MG_PLL_TDC_TDCSEL(x)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_TDC_TDCSEL(x: u32) -> u32 {
    (((x) << 0))
}

/// `MG_PLL_TDC_COLDST_BIAS(tc_port)` (intel_mg_phy_regs.h)
pub const fn MG_PLL_TDC_COLDST_BIAS(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL_TDC_COLDST_BIAS_PORT1, _MG_PLL_TDC_COLDST_BIAS_PORT2)
}
