// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_display_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;
use crate::intel_mg_phy_regs_upstream::*;

/// `GU_CNTL_PROTECTED` (intel_display_regs.h)
pub const GU_CNTL_PROTECTED: I915RegT = mmio(0x10100C);

/// `DEPRESENT` (intel_display_regs.h)
pub const DEPRESENT: u32 = reg_bit(9);

/// `_GEN7_PIPEA_DE_LOAD_SL` (intel_display_regs.h)
pub const _GEN7_PIPEA_DE_LOAD_SL: u32 = 0x70068;

/// `_GEN7_PIPEB_DE_LOAD_SL` (intel_display_regs.h)
pub const _GEN7_PIPEB_DE_LOAD_SL: u32 = 0x71068;

/// `GEN7_PIPE_DE_LOAD_SL(pipe)` (intel_display_regs.h)
pub const fn GEN7_PIPE_DE_LOAD_SL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _GEN7_PIPEA_DE_LOAD_SL, _GEN7_PIPEB_DE_LOAD_SL)
}

/// `DPIO_CTL` (intel_display_regs.h)
pub const DPIO_CTL: I915RegT = mmio((VLV_DISPLAY_BASE + 0x2110));

/// `DPIO_MODSEL1` (intel_display_regs.h)
pub const DPIO_MODSEL1: u32 = ((1 << 3));

/// `DPIO_MODSEL0` (intel_display_regs.h)
pub const DPIO_MODSEL0: u32 = ((1 << 2));

/// `DPIO_SFR_BYPASS` (intel_display_regs.h)
pub const DPIO_SFR_BYPASS: u32 = ((1 << 1));

/// `DPIO_CMNRST` (intel_display_regs.h)
pub const DPIO_CMNRST: u32 = ((1 << 0));

/// `BXT_P_CR_GT_DISP_PWRON` (intel_display_regs.h)
pub const BXT_P_CR_GT_DISP_PWRON: I915RegT = mmio(0x138090);

/// `MIPIO_RST_CTRL` (intel_display_regs.h)
pub const MIPIO_RST_CTRL: u32 = ((1 << 2));

/// `_BXT_PHY_CTL_DDI_A` (intel_display_regs.h)
pub const _BXT_PHY_CTL_DDI_A: u32 = 0x64C00;

/// `_BXT_PHY_CTL_DDI_B` (intel_display_regs.h)
pub const _BXT_PHY_CTL_DDI_B: u32 = 0x64C10;

/// `_BXT_PHY_CTL_DDI_C` (intel_display_regs.h)
pub const _BXT_PHY_CTL_DDI_C: u32 = 0x64C20;

/// `BXT_PHY_CMNLANE_POWERDOWN_ACK` (intel_display_regs.h)
pub const BXT_PHY_CMNLANE_POWERDOWN_ACK: u32 = ((1 << 10));

/// `BXT_PHY_LANE_POWERDOWN_ACK` (intel_display_regs.h)
pub const BXT_PHY_LANE_POWERDOWN_ACK: u32 = ((1 << 9));

/// `BXT_PHY_LANE_ENABLED` (intel_display_regs.h)
pub const BXT_PHY_LANE_ENABLED: u32 = ((1 << 8));

/// `BXT_PHY_CTL(port)` (intel_display_regs.h)
pub const fn BXT_PHY_CTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _BXT_PHY_CTL_DDI_A, _BXT_PHY_CTL_DDI_B)
}

/// `_PHY_CTL_FAMILY_DDI` (intel_display_regs.h)
pub const _PHY_CTL_FAMILY_DDI: u32 = 0x64C90;

/// `_PHY_CTL_FAMILY_EDP` (intel_display_regs.h)
pub const _PHY_CTL_FAMILY_EDP: u32 = 0x64C80;

/// `_PHY_CTL_FAMILY_DDI_C` (intel_display_regs.h)
pub const _PHY_CTL_FAMILY_DDI_C: u32 = 0x64CA0;

/// `COMMON_RESET_DIS` (intel_display_regs.h)
pub const COMMON_RESET_DIS: u32 = ((1 << 31));

/// `BXT_PHY_CTL_FAMILY(phy)` (intel_display_regs.h)
pub const fn BXT_PHY_CTL_FAMILY(phy: u32) -> I915RegT {
    mmio(pick_even_2ranges(phy, 1, _PHY_CTL_FAMILY_DDI, _PHY_CTL_FAMILY_DDI, _PHY_CTL_FAMILY_EDP, _PHY_CTL_FAMILY_DDI_C))
}

/// `UAIMI_SPR1` (intel_display_regs.h)
pub const UAIMI_SPR1: I915RegT = mmio(0x4F074);

/// `SKL_VCCIO_MASK` (intel_display_regs.h)
pub const SKL_VCCIO_MASK: u32 = 0x1;

/// `DISPIO_CR_TX_BMU_CR0` (intel_display_regs.h)
pub const DISPIO_CR_TX_BMU_CR0: I915RegT = mmio(0x6C00C);

/// `BALANCE_LEG_SHIFT(port)` (intel_display_regs.h)
pub const fn BALANCE_LEG_SHIFT(port: u32) -> u32 {
    ((8 + (3 * (port))))
}

/// `BALANCE_LEG_MASK(port)` (intel_display_regs.h)
pub const fn BALANCE_LEG_MASK(port: u32) -> u32 {
    ((7 << ((8 + (3 * (port))))))
}

/// `BALANCE_LEG_DISABLE_SHIFT` (intel_display_regs.h)
pub const BALANCE_LEG_DISABLE_SHIFT: u32 = 23;

/// `BALANCE_LEG_DISABLE(port)` (intel_display_regs.h)
pub const fn BALANCE_LEG_DISABLE(port: u32) -> u32 {
    ((1 << ((23 + (port)))))
}

/// `ILK_GTT_FAULT` (intel_display_regs.h)
pub const ILK_GTT_FAULT: I915RegT = mmio(0x44040);

/// `GTT_FAULT_INVALID_GTT_PTE` (intel_display_regs.h)
pub const GTT_FAULT_INVALID_GTT_PTE: u32 = ((1 << 7));

/// `GTT_FAULT_INVALID_PTE_DATA` (intel_display_regs.h)
pub const GTT_FAULT_INVALID_PTE_DATA: u32 = ((1 << 6));

/// `GTT_FAULT_CURSOR_B_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_CURSOR_B_FAULT: u32 = ((1 << 5));

/// `GTT_FAULT_CURSOR_A_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_CURSOR_A_FAULT: u32 = ((1 << 4));

/// `GTT_FAULT_SPRITE_B_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_SPRITE_B_FAULT: u32 = ((1 << 3));

/// `GTT_FAULT_SPRITE_A_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_SPRITE_A_FAULT: u32 = ((1 << 2));

/// `GTT_FAULT_PRIMARY_B_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_PRIMARY_B_FAULT: u32 = ((1 << 1));

/// `GTT_FAULT_PRIMARY_A_FAULT` (intel_display_regs.h)
pub const GTT_FAULT_PRIMARY_A_FAULT: u32 = ((1 << 0));

/// `DERRMR` (intel_display_regs.h)
pub const DERRMR: I915RegT = mmio(0x44050);

/// `DERRMR_PIPEA_SCANLINE` (intel_display_regs.h)
pub const DERRMR_PIPEA_SCANLINE: u32 = ((1 << 0));

/// `DERRMR_PIPEA_PRI_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEA_PRI_FLIP_DONE: u32 = ((1 << 1));

/// `DERRMR_PIPEA_SPR_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEA_SPR_FLIP_DONE: u32 = ((1 << 2));

/// `DERRMR_PIPEA_VBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEA_VBLANK: u32 = ((1 << 3));

/// `DERRMR_PIPEA_HBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEA_HBLANK: u32 = ((1 << 5));

/// `DERRMR_PIPEB_SCANLINE` (intel_display_regs.h)
pub const DERRMR_PIPEB_SCANLINE: u32 = ((1 << 8));

/// `DERRMR_PIPEB_PRI_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEB_PRI_FLIP_DONE: u32 = ((1 << 9));

/// `DERRMR_PIPEB_SPR_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEB_SPR_FLIP_DONE: u32 = ((1 << 10));

/// `DERRMR_PIPEB_VBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEB_VBLANK: u32 = ((1 << 11));

/// `DERRMR_PIPEB_HBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEB_HBLANK: u32 = ((1 << 13));

/// `DERRMR_PIPEC_SCANLINE` (intel_display_regs.h)
pub const DERRMR_PIPEC_SCANLINE: u32 = ((1 << 14));

/// `DERRMR_PIPEC_PRI_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEC_PRI_FLIP_DONE: u32 = ((1 << 15));

/// `DERRMR_PIPEC_SPR_FLIP_DONE` (intel_display_regs.h)
pub const DERRMR_PIPEC_SPR_FLIP_DONE: u32 = ((1 << 20));

/// `DERRMR_PIPEC_VBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEC_VBLANK: u32 = ((1 << 21));

/// `DERRMR_PIPEC_HBLANK` (intel_display_regs.h)
pub const DERRMR_PIPEC_HBLANK: u32 = ((1 << 22));

/// `GEN7_ERR_INT` (intel_display_regs.h)
pub const GEN7_ERR_INT: I915RegT = mmio(0x44040);

/// `ERR_INT_POISON` (intel_display_regs.h)
pub const ERR_INT_POISON: u32 = ((1 << 31));

/// `ERR_INT_INVALID_GTT_PTE` (intel_display_regs.h)
pub const ERR_INT_INVALID_GTT_PTE: u32 = ((1 << 29));

/// `ERR_INT_INVALID_PTE_DATA` (intel_display_regs.h)
pub const ERR_INT_INVALID_PTE_DATA: u32 = ((1 << 28));

/// `ERR_INT_SPRITE_C_FAULT` (intel_display_regs.h)
pub const ERR_INT_SPRITE_C_FAULT: u32 = ((1 << 23));

/// `ERR_INT_PRIMARY_C_FAULT` (intel_display_regs.h)
pub const ERR_INT_PRIMARY_C_FAULT: u32 = ((1 << 22));

/// `ERR_INT_CURSOR_C_FAULT` (intel_display_regs.h)
pub const ERR_INT_CURSOR_C_FAULT: u32 = ((1 << 21));

/// `ERR_INT_SPRITE_B_FAULT` (intel_display_regs.h)
pub const ERR_INT_SPRITE_B_FAULT: u32 = ((1 << 20));

/// `ERR_INT_PRIMARY_B_FAULT` (intel_display_regs.h)
pub const ERR_INT_PRIMARY_B_FAULT: u32 = ((1 << 19));

/// `ERR_INT_CURSOR_B_FAULT` (intel_display_regs.h)
pub const ERR_INT_CURSOR_B_FAULT: u32 = ((1 << 18));

/// `ERR_INT_SPRITE_A_FAULT` (intel_display_regs.h)
pub const ERR_INT_SPRITE_A_FAULT: u32 = ((1 << 17));

/// `ERR_INT_PRIMARY_A_FAULT` (intel_display_regs.h)
pub const ERR_INT_PRIMARY_A_FAULT: u32 = ((1 << 16));

/// `ERR_INT_CURSOR_A_FAULT` (intel_display_regs.h)
pub const ERR_INT_CURSOR_A_FAULT: u32 = ((1 << 15));

/// `ERR_INT_MMIO_UNCLAIMED` (intel_display_regs.h)
pub const ERR_INT_MMIO_UNCLAIMED: u32 = ((1 << 13));

/// `ERR_INT_PIPE_CRC_DONE_C` (intel_display_regs.h)
pub const ERR_INT_PIPE_CRC_DONE_C: u32 = ((1 << 8));

/// `ERR_INT_FIFO_UNDERRUN_C` (intel_display_regs.h)
pub const ERR_INT_FIFO_UNDERRUN_C: u32 = ((1 << 6));

/// `ERR_INT_PIPE_CRC_DONE_B` (intel_display_regs.h)
pub const ERR_INT_PIPE_CRC_DONE_B: u32 = ((1 << 5));

/// `ERR_INT_FIFO_UNDERRUN_B` (intel_display_regs.h)
pub const ERR_INT_FIFO_UNDERRUN_B: u32 = ((1 << 3));

/// `ERR_INT_PIPE_CRC_DONE_A` (intel_display_regs.h)
pub const ERR_INT_PIPE_CRC_DONE_A: u32 = ((1 << 2));

/// `ERR_INT_PIPE_CRC_DONE(pipe)` (intel_display_regs.h)
pub const fn ERR_INT_PIPE_CRC_DONE(pipe: u32) -> u32 {
    ((1 << ((2 + ((pipe) * 3)))))
}

/// `ERR_INT_FIFO_UNDERRUN_A` (intel_display_regs.h)
pub const ERR_INT_FIFO_UNDERRUN_A: u32 = ((1 << 0));

/// `ERR_INT_FIFO_UNDERRUN(pipe)` (intel_display_regs.h)
pub const fn ERR_INT_FIFO_UNDERRUN(pipe: u32) -> u32 {
    ((1 << (((pipe) * 3))))
}

/// `VLV_EIR` (intel_display_regs.h)
pub const VLV_EIR: I915RegT = mmio((VLV_DISPLAY_BASE + 0x20b0));

/// `VLV_EMR` (intel_display_regs.h)
pub const VLV_EMR: I915RegT = mmio((VLV_DISPLAY_BASE + 0x20b4));

/// `VLV_ESR` (intel_display_regs.h)
pub const VLV_ESR: I915RegT = mmio((VLV_DISPLAY_BASE + 0x20b8));

/// `VLV_ERROR_GUNIT_TLB_DATA` (intel_display_regs.h)
pub const VLV_ERROR_GUNIT_TLB_DATA: u32 = ((1 << 6));

/// `VLV_ERROR_GUNIT_TLB_PTE` (intel_display_regs.h)
pub const VLV_ERROR_GUNIT_TLB_PTE: u32 = ((1 << 5));

/// `VLV_ERROR_PAGE_TABLE` (intel_display_regs.h)
pub const VLV_ERROR_PAGE_TABLE: u32 = ((1 << 4));

/// `VLV_ERROR_CLAIM` (intel_display_regs.h)
pub const VLV_ERROR_CLAIM: u32 = ((1 << 0));

/// `_MBUS_ABOX0_CTL` (intel_display_regs.h)
pub const _MBUS_ABOX0_CTL: u32 = 0x45038;

/// `_MBUS_ABOX1_CTL` (intel_display_regs.h)
pub const _MBUS_ABOX1_CTL: u32 = 0x45048;

/// `_MBUS_ABOX2_CTL` (intel_display_regs.h)
pub const _MBUS_ABOX2_CTL: u32 = 0x4504C;

/// `MBUS_ABOX_CTL(x)` (intel_display_regs.h)
pub const fn MBUS_ABOX_CTL(x: u32) -> I915RegT {
    mmio(pick_even_2ranges(x, 2, _MBUS_ABOX0_CTL, _MBUS_ABOX1_CTL, _MBUS_ABOX2_CTL, _MBUS_ABOX2_CTL))
}

/// `MBUS_ABOX_BW_CREDIT_MASK` (intel_display_regs.h)
pub const MBUS_ABOX_BW_CREDIT_MASK: u32 = ((3 << 20));

/// `MBUS_ABOX_BW_CREDIT(x)` (intel_display_regs.h)
pub const fn MBUS_ABOX_BW_CREDIT(x: u32) -> u32 {
    (((x) << 20))
}

/// `MBUS_ABOX_B_CREDIT_MASK` (intel_display_regs.h)
pub const MBUS_ABOX_B_CREDIT_MASK: u32 = ((0xF << 16));

/// `MBUS_ABOX_B_CREDIT(x)` (intel_display_regs.h)
pub const fn MBUS_ABOX_B_CREDIT(x: u32) -> u32 {
    (((x) << 16))
}

/// `MBUS_ABOX_BT_CREDIT_POOL2_MASK` (intel_display_regs.h)
pub const MBUS_ABOX_BT_CREDIT_POOL2_MASK: u32 = ((0x1F << 8));

/// `MBUS_ABOX_BT_CREDIT_POOL2(x)` (intel_display_regs.h)
pub const fn MBUS_ABOX_BT_CREDIT_POOL2(x: u32) -> u32 {
    (((x) << 8))
}

/// `MBUS_ABOX_BT_CREDIT_POOL1_MASK` (intel_display_regs.h)
pub const MBUS_ABOX_BT_CREDIT_POOL1_MASK: u32 = ((0x1F << 0));

/// `MBUS_ABOX_BT_CREDIT_POOL1(x)` (intel_display_regs.h)
pub const fn MBUS_ABOX_BT_CREDIT_POOL1(x: u32) -> u32 {
    (((x) << 0))
}

/// `IPS_CTL` (intel_display_regs.h)
pub const IPS_CTL: I915RegT = mmio(0x43408);

/// `IPS_ENABLE` (intel_display_regs.h)
pub const IPS_ENABLE: u32 = reg_bit(31);

/// `IPS_FALSE_COLOR` (intel_display_regs.h)
pub const IPS_FALSE_COLOR: u32 = reg_bit(4);

/// `_DPLL_A` (intel_display_regs.h)
pub const _DPLL_A: u32 = 0x6014;

/// `_DPLL_B` (intel_display_regs.h)
pub const _DPLL_B: u32 = 0x6018;

/// `_CHV_DPLL_C` (intel_display_regs.h)
pub const _CHV_DPLL_C: u32 = 0x6030;

/// `VGA0` (intel_display_regs.h)
pub const VGA0: I915RegT = mmio(0x6000);

/// `VGA1` (intel_display_regs.h)
pub const VGA1: I915RegT = mmio(0x6004);

/// `VGA_PD` (intel_display_regs.h)
pub const VGA_PD: I915RegT = mmio(0x6010);

/// `VGA0_PD_P2_DIV_4` (intel_display_regs.h)
pub const VGA0_PD_P2_DIV_4: u32 = ((1 << 7));

/// `VGA0_PD_P1_DIV_2` (intel_display_regs.h)
pub const VGA0_PD_P1_DIV_2: u32 = ((1 << 5));

/// `VGA0_PD_P1_SHIFT` (intel_display_regs.h)
pub const VGA0_PD_P1_SHIFT: u32 = 0;

/// `VGA0_PD_P1_MASK` (intel_display_regs.h)
pub const VGA0_PD_P1_MASK: u32 = ((0x1f << 0));

/// `VGA1_PD_P2_DIV_4` (intel_display_regs.h)
pub const VGA1_PD_P2_DIV_4: u32 = ((1 << 15));

/// `VGA1_PD_P1_DIV_2` (intel_display_regs.h)
pub const VGA1_PD_P1_DIV_2: u32 = ((1 << 13));

/// `VGA1_PD_P1_SHIFT` (intel_display_regs.h)
pub const VGA1_PD_P1_SHIFT: u32 = 8;

/// `VGA1_PD_P1_MASK` (intel_display_regs.h)
pub const VGA1_PD_P1_MASK: u32 = ((0x1f << 8));

/// `DPLL_VCO_ENABLE` (intel_display_regs.h)
pub const DPLL_VCO_ENABLE: u32 = ((1 << 31));

/// `DPLL_SDVO_HIGH_SPEED` (intel_display_regs.h)
pub const DPLL_SDVO_HIGH_SPEED: u32 = ((1 << 30));

/// `DPLL_DVO_2X_MODE` (intel_display_regs.h)
pub const DPLL_DVO_2X_MODE: u32 = ((1 << 30));

/// `DPLL_EXT_BUFFER_ENABLE_VLV` (intel_display_regs.h)
pub const DPLL_EXT_BUFFER_ENABLE_VLV: u32 = ((1 << 30));

/// `DPLL_SYNCLOCK_ENABLE` (intel_display_regs.h)
pub const DPLL_SYNCLOCK_ENABLE: u32 = ((1 << 29));

/// `DPLL_REF_CLK_ENABLE_VLV` (intel_display_regs.h)
pub const DPLL_REF_CLK_ENABLE_VLV: u32 = ((1 << 29));

/// `DPLL_VGA_MODE_DIS` (intel_display_regs.h)
pub const DPLL_VGA_MODE_DIS: u32 = ((1 << 28));

/// `DPLLB_MODE_DAC_SERIAL` (intel_display_regs.h)
pub const DPLLB_MODE_DAC_SERIAL: u32 = ((1 << 26));

/// `DPLLB_MODE_LVDS` (intel_display_regs.h)
pub const DPLLB_MODE_LVDS: u32 = ((2 << 26));

/// `DPLL_MODE_MASK` (intel_display_regs.h)
pub const DPLL_MODE_MASK: u32 = ((3 << 26));

/// `DPLL_DAC_SERIAL_P2_CLOCK_DIV_10` (intel_display_regs.h)
pub const DPLL_DAC_SERIAL_P2_CLOCK_DIV_10: u32 = ((0 << 24));

/// `DPLL_DAC_SERIAL_P2_CLOCK_DIV_5` (intel_display_regs.h)
pub const DPLL_DAC_SERIAL_P2_CLOCK_DIV_5: u32 = ((1 << 24));

/// `DPLLB_LVDS_P2_CLOCK_DIV_14` (intel_display_regs.h)
pub const DPLLB_LVDS_P2_CLOCK_DIV_14: u32 = ((0 << 24));

/// `DPLLB_LVDS_P2_CLOCK_DIV_7` (intel_display_regs.h)
pub const DPLLB_LVDS_P2_CLOCK_DIV_7: u32 = ((1 << 24));

/// `DPLL_P2_CLOCK_DIV_MASK` (intel_display_regs.h)
pub const DPLL_P2_CLOCK_DIV_MASK: u32 = 0x03000000;

/// `DPLL_FPA01_P1_POST_DIV_MASK` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_MASK: u32 = 0x00ff0000;

/// `DPLL_FPA01_P1_POST_DIV_MASK_PINEVIEW` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_MASK_PINEVIEW: u32 = 0x00ff8000;

/// `DPLL_LOCK_VLV` (intel_display_regs.h)
pub const DPLL_LOCK_VLV: u32 = ((1 << 15));

/// `DPLL_INTEGRATED_CRI_CLK_VLV` (intel_display_regs.h)
pub const DPLL_INTEGRATED_CRI_CLK_VLV: u32 = ((1 << 14));

/// `DPLL_INTEGRATED_REF_CLK_VLV` (intel_display_regs.h)
pub const DPLL_INTEGRATED_REF_CLK_VLV: u32 = ((1 << 13));

/// `DPLL_SSC_REF_CLK_CHV` (intel_display_regs.h)
pub const DPLL_SSC_REF_CLK_CHV: u32 = ((1 << 13));

/// `DPLL_PORTC_READY_MASK` (intel_display_regs.h)
pub const DPLL_PORTC_READY_MASK: u32 = ((0xf << 4));

/// `DPLL_PORTB_READY_MASK` (intel_display_regs.h)
pub const DPLL_PORTB_READY_MASK: u32 = (0xf);

/// `DPLL_FPA01_P1_POST_DIV_MASK_I830` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_MASK_I830: u32 = 0x001f0000;

/// `DSPCLK_GATE_D` (intel_display_regs.h)
pub const DSPCLK_GATE_D: I915RegT = mmio(0x6200);

/// `VLV_DSPCLK_GATE_D` (intel_display_regs.h)
pub const VLV_DSPCLK_GATE_D: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6200));

/// `DPUNIT_B_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPUNIT_B_CLOCK_GATE_DISABLE: u32 = ((1 << 30));

/// `VSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const VSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 29));

/// `VRHUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const VRHUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 28));

/// `VRDUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const VRDUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 27));

/// `AUDUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const AUDUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 26));

/// `DPUNIT_A_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPUNIT_A_CLOCK_GATE_DISABLE: u32 = ((1 << 25));

/// `DPCUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPCUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 24));

/// `PNV_GMBUSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PNV_GMBUSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 24));

/// `TVRUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const TVRUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 23));

/// `TVCUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const TVCUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 22));

/// `TVFUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const TVFUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 21));

/// `TVEUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const TVEUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 20));

/// `DVSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DVSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 19));

/// `DSSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DSSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 18));

/// `DDBUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DDBUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 17));

/// `DPRUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPRUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 16));

/// `DPFUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPFUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 15));

/// `DPBMUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPBMUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 14));

/// `DPLSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPLSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 13));

/// `DPLUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPLUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 12));

/// `DPOUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPOUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 11));

/// `DPBUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPBUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 10));

/// `DCUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DCUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 9));

/// `DPUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 8));

/// `VRUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const VRUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 7));

/// `OVHUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVHUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 6));

/// `DPIOUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const DPIOUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 6));

/// `OVFUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVFUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 5));

/// `OVBUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVBUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 4));

/// `ILK_DISPLAY_CHICKEN1` (intel_display_regs.h)
pub const ILK_DISPLAY_CHICKEN1: I915RegT = mmio(0x42000);

/// `ILK_FBCQ_DIS` (intel_display_regs.h)
pub const ILK_FBCQ_DIS: u32 = reg_bit(22);

/// `ILK_PABSTRETCH_DIS` (intel_display_regs.h)
pub const ILK_PABSTRETCH_DIS: u32 = reg_bit(21);

/// `ILK_SABSTRETCH_DIS` (intel_display_regs.h)
pub const ILK_SABSTRETCH_DIS: u32 = reg_bit(20);

/// `IVB_PRI_STRETCH_MAX_MASK` (intel_display_regs.h)
pub const IVB_PRI_STRETCH_MAX_MASK: u32 = reg_genmask(21, 20);

/// `IVB_PRI_STRETCH_MAX_X8` (intel_display_regs.h)
pub const IVB_PRI_STRETCH_MAX_X8: u32 = reg_field_prep(IVB_PRI_STRETCH_MAX_MASK, 0);

/// `IVB_PRI_STRETCH_MAX_X4` (intel_display_regs.h)
pub const IVB_PRI_STRETCH_MAX_X4: u32 = reg_field_prep(IVB_PRI_STRETCH_MAX_MASK, 1);

/// `IVB_PRI_STRETCH_MAX_X2` (intel_display_regs.h)
pub const IVB_PRI_STRETCH_MAX_X2: u32 = reg_field_prep(IVB_PRI_STRETCH_MAX_MASK, 2);

/// `IVB_PRI_STRETCH_MAX_X1` (intel_display_regs.h)
pub const IVB_PRI_STRETCH_MAX_X1: u32 = reg_field_prep(IVB_PRI_STRETCH_MAX_MASK, 3);

/// `IVB_SPR_STRETCH_MAX_MASK` (intel_display_regs.h)
pub const IVB_SPR_STRETCH_MAX_MASK: u32 = reg_genmask(19, 18);

/// `IVB_SPR_STRETCH_MAX_X8` (intel_display_regs.h)
pub const IVB_SPR_STRETCH_MAX_X8: u32 = reg_field_prep(IVB_SPR_STRETCH_MAX_MASK, 0);

/// `IVB_SPR_STRETCH_MAX_X4` (intel_display_regs.h)
pub const IVB_SPR_STRETCH_MAX_X4: u32 = reg_field_prep(IVB_SPR_STRETCH_MAX_MASK, 1);

/// `IVB_SPR_STRETCH_MAX_X2` (intel_display_regs.h)
pub const IVB_SPR_STRETCH_MAX_X2: u32 = reg_field_prep(IVB_SPR_STRETCH_MAX_MASK, 2);

/// `IVB_SPR_STRETCH_MAX_X1` (intel_display_regs.h)
pub const IVB_SPR_STRETCH_MAX_X1: u32 = reg_field_prep(IVB_SPR_STRETCH_MAX_MASK, 3);

/// `ILK_DISPLAY_CHICKEN2` (intel_display_regs.h)
pub const ILK_DISPLAY_CHICKEN2: I915RegT = mmio(0x42004);

/// `ILK_ELPIN_409_SELECT` (intel_display_regs.h)
pub const ILK_ELPIN_409_SELECT: u32 = reg_bit(25);

/// `ILK_DPARB_GATE` (intel_display_regs.h)
pub const ILK_DPARB_GATE: u32 = reg_bit(22);

/// `ILK_VSDPFD_FULL` (intel_display_regs.h)
pub const ILK_VSDPFD_FULL: u32 = reg_bit(21);

/// `ILK_DSPCLK_GATE_D` (intel_display_regs.h)
pub const ILK_DSPCLK_GATE_D: I915RegT = mmio(0x42020);

/// `ILK_VRHUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const ILK_VRHUNIT_CLOCK_GATE_DISABLE: u32 = reg_bit(28);

/// `ILK_DPFCUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const ILK_DPFCUNIT_CLOCK_GATE_DISABLE: u32 = reg_bit(9);

/// `ILK_DPFCRUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const ILK_DPFCRUNIT_CLOCK_GATE_DISABLE: u32 = reg_bit(8);

/// `ILK_DPFDUNIT_CLOCK_GATE_ENABLE` (intel_display_regs.h)
pub const ILK_DPFDUNIT_CLOCK_GATE_ENABLE: u32 = reg_bit(7);

/// `ILK_DPARBUNIT_CLOCK_GATE_ENABLE` (intel_display_regs.h)
pub const ILK_DPARBUNIT_CLOCK_GATE_ENABLE: u32 = reg_bit(5);

/// `OVRUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVRUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 3));

/// `OVCUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVCUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 2));

/// `OVUUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVUUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 1));

/// `ZVUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const ZVUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 0));

/// `OVLUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const OVLUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 0));

/// `DPIO_PHY_STATUS` (intel_display_regs.h)
pub const DPIO_PHY_STATUS: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6240));

/// `DPLL_PORTD_READY_MASK` (intel_display_regs.h)
pub const DPLL_PORTD_READY_MASK: u32 = (0xf);

/// `DISPLAY_PHY_CONTROL` (intel_display_regs.h)
pub const DISPLAY_PHY_CONTROL: I915RegT = mmio((VLV_DISPLAY_BASE + 0x60100));

/// `PHY_CH_POWER_DOWN_OVRD_EN(phy, ch)` (intel_display_regs.h)
pub const fn PHY_CH_POWER_DOWN_OVRD_EN(phy: u32, ch: u32) -> u32 {
    ((1 << ((((2 * (phy)) + (ch)) + 27))))
}

/// `PHY_LDO_DELAY_0NS` (intel_display_regs.h)
pub const PHY_LDO_DELAY_0NS: u32 = 0x0;

/// `PHY_LDO_DELAY_200NS` (intel_display_regs.h)
pub const PHY_LDO_DELAY_200NS: u32 = 0x1;

/// `PHY_LDO_DELAY_600NS` (intel_display_regs.h)
pub const PHY_LDO_DELAY_600NS: u32 = 0x2;

/// `PHY_LDO_SEQ_DELAY(delay, phy)` (intel_display_regs.h)
pub const fn PHY_LDO_SEQ_DELAY(delay: u32, phy: u32) -> u32 {
    (((delay) << (((2 * (phy)) + 23))))
}

/// `PHY_CH_POWER_DOWN_OVRD(mask, phy, ch)` (intel_display_regs.h)
pub const fn PHY_CH_POWER_DOWN_OVRD(mask: u32, phy: u32, ch: u32) -> u32 {
    (((mask) << ((((8 * (phy)) + (4 * (ch))) + 11))))
}

/// `PHY_CH_SU_PSR` (intel_display_regs.h)
pub const PHY_CH_SU_PSR: u32 = 0x1;

/// `PHY_CH_DEEP_PSR` (intel_display_regs.h)
pub const PHY_CH_DEEP_PSR: u32 = 0x7;

/// `PHY_CH_POWER_MODE(mode, phy, ch)` (intel_display_regs.h)
pub const fn PHY_CH_POWER_MODE(mode: u32, phy: u32, ch: u32) -> u32 {
    (((mode) << ((((6 * (phy)) + (3 * (ch))) + 2))))
}

/// `PHY_COM_LANE_RESET_DEASSERT(phy)` (intel_display_regs.h)
pub const fn PHY_COM_LANE_RESET_DEASSERT(phy: u32) -> u32 {
    ((1 << (phy)))
}

/// `DISPLAY_PHY_STATUS` (intel_display_regs.h)
pub const DISPLAY_PHY_STATUS: I915RegT = mmio((VLV_DISPLAY_BASE + 0x60104));

/// `PHY_STATUS_CMN_LDO(phy, ch)` (intel_display_regs.h)
pub const fn PHY_STATUS_CMN_LDO(phy: u32, ch: u32) -> u32 {
    ((1 << ((6 - (((6 * (phy)) + (3 * (ch))))))))
}

/// `PHY_STATUS_SPLINE_LDO(phy, ch, spline)` (intel_display_regs.h)
pub const fn PHY_STATUS_SPLINE_LDO(phy: u32, ch: u32, spline: u32) -> u32 {
    ((1 << ((8 - ((((6 * (phy)) + (3 * (ch))) + (spline)))))))
}

/// `DPLL_FPA01_P1_POST_DIV_MASK_I830_LVDS` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_MASK_I830_LVDS: u32 = 0x003f0000;

/// `DPLL_FPA01_P1_POST_DIV_SHIFT` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_SHIFT: u32 = 16;

/// `DPLL_FPA01_P1_POST_DIV_SHIFT_PINEVIEW` (intel_display_regs.h)
pub const DPLL_FPA01_P1_POST_DIV_SHIFT_PINEVIEW: u32 = 15;

/// `PLL_P2_DIVIDE_BY_4` (intel_display_regs.h)
pub const PLL_P2_DIVIDE_BY_4: u32 = ((1 << 23));

/// `PLL_P1_DIVIDE_BY_TWO` (intel_display_regs.h)
pub const PLL_P1_DIVIDE_BY_TWO: u32 = ((1 << 21));

/// `PLL_REF_INPUT_DREFCLK` (intel_display_regs.h)
pub const PLL_REF_INPUT_DREFCLK: u32 = ((0 << 13));

/// `PLL_REF_INPUT_TVCLKINA` (intel_display_regs.h)
pub const PLL_REF_INPUT_TVCLKINA: u32 = ((1 << 13));

/// `PLL_REF_INPUT_TVCLKINBC` (intel_display_regs.h)
pub const PLL_REF_INPUT_TVCLKINBC: u32 = ((2 << 13));

/// `PLLB_REF_INPUT_SPREADSPECTRUMIN` (intel_display_regs.h)
pub const PLLB_REF_INPUT_SPREADSPECTRUMIN: u32 = ((3 << 13));

/// `PLL_REF_INPUT_MASK` (intel_display_regs.h)
pub const PLL_REF_INPUT_MASK: u32 = ((3 << 13));

/// `PLL_LOAD_PULSE_PHASE_SHIFT` (intel_display_regs.h)
pub const PLL_LOAD_PULSE_PHASE_SHIFT: u32 = 9;

/// `PLL_REF_SDVO_HDMI_MULTIPLIER_SHIFT` (intel_display_regs.h)
pub const PLL_REF_SDVO_HDMI_MULTIPLIER_SHIFT: u32 = 9;

/// `PLL_REF_SDVO_HDMI_MULTIPLIER_MASK` (intel_display_regs.h)
pub const PLL_REF_SDVO_HDMI_MULTIPLIER_MASK: u32 = ((7 << 9));

/// `PLL_REF_SDVO_HDMI_MULTIPLIER(x)` (intel_display_regs.h)
pub const fn PLL_REF_SDVO_HDMI_MULTIPLIER(x: u32) -> u32 {
    (((((x) - 1)) << 9))
}

/// `DPLL_FPA1_P1_POST_DIV_SHIFT` (intel_display_regs.h)
pub const DPLL_FPA1_P1_POST_DIV_SHIFT: u32 = 0;

/// `DPLL_FPA1_P1_POST_DIV_MASK` (intel_display_regs.h)
pub const DPLL_FPA1_P1_POST_DIV_MASK: u32 = 0xff;

/// `PLL_LOAD_PULSE_PHASE_MASK` (intel_display_regs.h)
pub const PLL_LOAD_PULSE_PHASE_MASK: u32 = ((0xf << PLL_LOAD_PULSE_PHASE_SHIFT));

/// `DISPLAY_RATE_SELECT_FPA1` (intel_display_regs.h)
pub const DISPLAY_RATE_SELECT_FPA1: u32 = ((1 << 8));

/// `SDVO_MULTIPLIER_MASK` (intel_display_regs.h)
pub const SDVO_MULTIPLIER_MASK: u32 = 0x000000ff;

/// `SDVO_MULTIPLIER_SHIFT_HIRES` (intel_display_regs.h)
pub const SDVO_MULTIPLIER_SHIFT_HIRES: u32 = 4;

/// `SDVO_MULTIPLIER_SHIFT_VGA` (intel_display_regs.h)
pub const SDVO_MULTIPLIER_SHIFT_VGA: u32 = 0;

/// `_DPLL_A_MD` (intel_display_regs.h)
pub const _DPLL_A_MD: u32 = 0x601c;

/// `_DPLL_B_MD` (intel_display_regs.h)
pub const _DPLL_B_MD: u32 = 0x6020;

/// `_CHV_DPLL_C_MD` (intel_display_regs.h)
pub const _CHV_DPLL_C_MD: u32 = 0x603c;

/// `DPLL_MD_UDI_DIVIDER_MASK` (intel_display_regs.h)
pub const DPLL_MD_UDI_DIVIDER_MASK: u32 = 0x3f000000;

/// `DPLL_MD_UDI_DIVIDER_SHIFT` (intel_display_regs.h)
pub const DPLL_MD_UDI_DIVIDER_SHIFT: u32 = 24;

/// `DPLL_MD_VGA_UDI_DIVIDER_MASK` (intel_display_regs.h)
pub const DPLL_MD_VGA_UDI_DIVIDER_MASK: u32 = 0x003f0000;

/// `DPLL_MD_VGA_UDI_DIVIDER_SHIFT` (intel_display_regs.h)
pub const DPLL_MD_VGA_UDI_DIVIDER_SHIFT: u32 = 16;

/// `DPLL_MD_UDI_MULTIPLIER_MASK` (intel_display_regs.h)
pub const DPLL_MD_UDI_MULTIPLIER_MASK: u32 = 0x00003f00;

/// `DPLL_MD_UDI_MULTIPLIER_SHIFT` (intel_display_regs.h)
pub const DPLL_MD_UDI_MULTIPLIER_SHIFT: u32 = 8;

/// `DPLL_MD_VGA_UDI_MULTIPLIER_MASK` (intel_display_regs.h)
pub const DPLL_MD_VGA_UDI_MULTIPLIER_MASK: u32 = 0x0000003f;

/// `DPLL_MD_VGA_UDI_MULTIPLIER_SHIFT` (intel_display_regs.h)
pub const DPLL_MD_VGA_UDI_MULTIPLIER_SHIFT: u32 = 0;

/// `RAWCLK_FREQ_VLV` (intel_display_regs.h)
pub const RAWCLK_FREQ_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6024));

/// `_FPA0` (intel_display_regs.h)
pub const _FPA0: u32 = 0x6040;

/// `_FPA1` (intel_display_regs.h)
pub const _FPA1: u32 = 0x6044;

/// `_FPB0` (intel_display_regs.h)
pub const _FPB0: u32 = 0x6048;

/// `_FPB1` (intel_display_regs.h)
pub const _FPB1: u32 = 0x604c;

/// `FP0(pipe)` (intel_display_regs.h)
pub const fn FP0(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FPA0, _FPB0)
}

/// `FP1(pipe)` (intel_display_regs.h)
pub const fn FP1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _FPA1, _FPB1)
}

/// `FP_N_DIV_MASK` (intel_display_regs.h)
pub const FP_N_DIV_MASK: u32 = 0x003f0000;

/// `FP_N_PINEVIEW_DIV_MASK` (intel_display_regs.h)
pub const FP_N_PINEVIEW_DIV_MASK: u32 = 0x00ff0000;

/// `FP_N_DIV_SHIFT` (intel_display_regs.h)
pub const FP_N_DIV_SHIFT: u32 = 16;

/// `FP_M1_DIV_MASK` (intel_display_regs.h)
pub const FP_M1_DIV_MASK: u32 = 0x00003f00;

/// `FP_M1_DIV_SHIFT` (intel_display_regs.h)
pub const FP_M1_DIV_SHIFT: u32 = 8;

/// `FP_M2_DIV_MASK` (intel_display_regs.h)
pub const FP_M2_DIV_MASK: u32 = 0x0000003f;

/// `FP_M2_PINEVIEW_DIV_MASK` (intel_display_regs.h)
pub const FP_M2_PINEVIEW_DIV_MASK: u32 = 0x000000ff;

/// `FP_M2_DIV_SHIFT` (intel_display_regs.h)
pub const FP_M2_DIV_SHIFT: u32 = 0;

/// `FW_BLC_SELF_VLV` (intel_display_regs.h)
pub const FW_BLC_SELF_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6500));

/// `FW_CSPWRDWNEN` (intel_display_regs.h)
pub const FW_CSPWRDWNEN: u32 = ((1 << 15));

/// `MI_ARB_VLV` (intel_display_regs.h)
pub const MI_ARB_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6504));

/// `MI_ARB_DISPLAY_TRICKLE_FEED_DISABLE_VLV` (intel_display_regs.h)
pub const MI_ARB_DISPLAY_TRICKLE_FEED_DISABLE_VLV: u32 = ((1 << 2));

/// `CZCLK_CDCLK_FREQ_RATIO` (intel_display_regs.h)
pub const CZCLK_CDCLK_FREQ_RATIO: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6508));

/// `CDCLK_FREQ_SHIFT` (intel_display_regs.h)
pub const CDCLK_FREQ_SHIFT: u32 = 4;

/// `CDCLK_FREQ_MASK` (intel_display_regs.h)
pub const CDCLK_FREQ_MASK: u32 = ((0x1f << CDCLK_FREQ_SHIFT));

/// `CZCLK_FREQ_MASK` (intel_display_regs.h)
pub const CZCLK_FREQ_MASK: u32 = 0xf;

/// `GCI_CONTROL` (intel_display_regs.h)
pub const GCI_CONTROL: I915RegT = mmio((VLV_DISPLAY_BASE + 0x650C));

/// `PFI_CREDIT_63` (intel_display_regs.h)
pub const PFI_CREDIT_63: u32 = ((9 << 28));

/// `PFI_CREDIT_31` (intel_display_regs.h)
pub const PFI_CREDIT_31: u32 = ((8 << 28));

/// `PFI_CREDIT(x)` (intel_display_regs.h)
pub const fn PFI_CREDIT(x: u32) -> u32 {
    (((((x) - 8)) << 28))
}

/// `PFI_CREDIT_RESEND` (intel_display_regs.h)
pub const PFI_CREDIT_RESEND: u32 = ((1 << 27));

/// `VGA_FAST_MODE_DISABLE` (intel_display_regs.h)
pub const VGA_FAST_MODE_DISABLE: u32 = ((1 << 14));

/// `GMBUSFREQ_VLV` (intel_display_regs.h)
pub const GMBUSFREQ_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6510));

/// `PEG_BAND_GAP_DATA` (intel_display_regs.h)
pub const PEG_BAND_GAP_DATA: I915RegT = mmio(0x14d68);

/// `OVADD` (intel_display_regs.h)
pub const OVADD: I915RegT = mmio(0x30000);

/// `DOVSTA` (intel_display_regs.h)
pub const DOVSTA: I915RegT = mmio(0x30008);

/// `OC_BUF` (intel_display_regs.h)
pub const OC_BUF: u32 = ((0x3 << 20));

/// `OGAMC5` (intel_display_regs.h)
pub const OGAMC5: I915RegT = mmio(0x30010);

/// `OGAMC4` (intel_display_regs.h)
pub const OGAMC4: I915RegT = mmio(0x30014);

/// `OGAMC3` (intel_display_regs.h)
pub const OGAMC3: I915RegT = mmio(0x30018);

/// `OGAMC2` (intel_display_regs.h)
pub const OGAMC2: I915RegT = mmio(0x3001c);

/// `OGAMC1` (intel_display_regs.h)
pub const OGAMC1: I915RegT = mmio(0x30020);

/// `OGAMC0` (intel_display_regs.h)
pub const OGAMC0: I915RegT = mmio(0x30024);

/// `GEN8_CHICKEN_DCPR_1` (intel_display_regs.h)
pub const GEN8_CHICKEN_DCPR_1: I915RegT = mmio(0x46430);

/// `_LATENCY_REPORTING_REMOVED_PIPE_D` (intel_display_regs.h)
pub const _LATENCY_REPORTING_REMOVED_PIPE_D: u32 = reg_bit(31);

/// `SKL_SELECT_ALTERNATE_DC_EXIT` (intel_display_regs.h)
pub const SKL_SELECT_ALTERNATE_DC_EXIT: u32 = reg_bit(30);

/// `_LATENCY_REPORTING_REMOVED_PIPE_C` (intel_display_regs.h)
pub const _LATENCY_REPORTING_REMOVED_PIPE_C: u32 = reg_bit(25);

/// `_LATENCY_REPORTING_REMOVED_PIPE_B` (intel_display_regs.h)
pub const _LATENCY_REPORTING_REMOVED_PIPE_B: u32 = reg_bit(24);

/// `_LATENCY_REPORTING_REMOVED_PIPE_A` (intel_display_regs.h)
pub const _LATENCY_REPORTING_REMOVED_PIPE_A: u32 = reg_bit(23);

/// `LATENCY_REPORTING_REMOVED(pipe)` (intel_display_regs.h)
pub const fn LATENCY_REPORTING_REMOVED(pipe: u32) -> u32 {
    pick((pipe), &[_LATENCY_REPORTING_REMOVED_PIPE_A, _LATENCY_REPORTING_REMOVED_PIPE_B, _LATENCY_REPORTING_REMOVED_PIPE_C, _LATENCY_REPORTING_REMOVED_PIPE_D])
}

/// `ICL_DELAY_PMRSP` (intel_display_regs.h)
pub const ICL_DELAY_PMRSP: u32 = reg_bit(22);

/// `DISABLE_FLR_SRC` (intel_display_regs.h)
pub const DISABLE_FLR_SRC: u32 = reg_bit(15);

/// `MASK_WAKEMEM` (intel_display_regs.h)
pub const MASK_WAKEMEM: u32 = reg_bit(13);

/// `DDI_CLOCK_REG_ACCESS` (intel_display_regs.h)
pub const DDI_CLOCK_REG_ACCESS: u32 = reg_bit(7);

/// `CHICKEN_PAR1_1` (intel_display_regs.h)
pub const CHICKEN_PAR1_1: I915RegT = mmio(0x42080);

/// `IGNORE_KVMR_PIPE_A` (intel_display_regs.h)
pub const IGNORE_KVMR_PIPE_A: u32 = reg_bit(23);

/// `KBL_ARB_FILL_SPARE_22` (intel_display_regs.h)
pub const KBL_ARB_FILL_SPARE_22: u32 = reg_bit(22);

/// `DIS_RAM_BYPASS_PSR2_MAN_TRACK` (intel_display_regs.h)
pub const DIS_RAM_BYPASS_PSR2_MAN_TRACK: u32 = reg_bit(16);

/// `SKL_DE_COMPRESSED_HASH_MODE` (intel_display_regs.h)
pub const SKL_DE_COMPRESSED_HASH_MODE: u32 = reg_bit(15);

/// `HSW_MASK_VBL_TO_PIPE_IN_SRD` (intel_display_regs.h)
pub const HSW_MASK_VBL_TO_PIPE_IN_SRD: u32 = reg_bit(15);

/// `FORCE_ARB_IDLE_PLANES` (intel_display_regs.h)
pub const FORCE_ARB_IDLE_PLANES: u32 = reg_bit(14);

/// `SKL_EDP_PSR_FIX_RDWRAP` (intel_display_regs.h)
pub const SKL_EDP_PSR_FIX_RDWRAP: u32 = reg_bit(3);

/// `IGNORE_PSR2_HW_TRACKING` (intel_display_regs.h)
pub const IGNORE_PSR2_HW_TRACKING: u32 = reg_bit(1);

/// `CHICKEN_PAR2_1` (intel_display_regs.h)
pub const CHICKEN_PAR2_1: I915RegT = mmio(0x42090);

/// `KVM_CONFIG_CHANGE_NOTIFICATION_SELECT` (intel_display_regs.h)
pub const KVM_CONFIG_CHANGE_NOTIFICATION_SELECT: u32 = reg_bit(14);

/// `GEN9_CLKGATE_DIS_0` (intel_display_regs.h)
pub const GEN9_CLKGATE_DIS_0: I915RegT = mmio(0x46530);

/// `DARBF_GATING_DIS` (intel_display_regs.h)
pub const DARBF_GATING_DIS: u32 = reg_bit(27);

/// `DMG_GATING_DIS` (intel_display_regs.h)
pub const DMG_GATING_DIS: u32 = reg_bit(21);

/// `MTL_PIPEDMC_GATING_DIS(pipe)` (intel_display_regs.h)
pub const fn MTL_PIPEDMC_GATING_DIS(pipe: u32) -> u32 {
    reg_bit((15 - (pipe)))
}

/// `PWM2_GATING_DIS` (intel_display_regs.h)
pub const PWM2_GATING_DIS: u32 = reg_bit(14);

/// `PWM1_GATING_DIS` (intel_display_regs.h)
pub const PWM1_GATING_DIS: u32 = reg_bit(13);

/// `GEN9_CLKGATE_DIS_3` (intel_display_regs.h)
pub const GEN9_CLKGATE_DIS_3: I915RegT = mmio(0x46538);

/// `TGL_VRH_GATING_DIS` (intel_display_regs.h)
pub const TGL_VRH_GATING_DIS: u32 = reg_bit(31);

/// `DPT_GATING_DIS` (intel_display_regs.h)
pub const DPT_GATING_DIS: u32 = reg_bit(22);

/// `GEN9_CLKGATE_DIS_4` (intel_display_regs.h)
pub const GEN9_CLKGATE_DIS_4: I915RegT = mmio(0x4653C);

/// `BXT_GMBUS_GATING_DIS` (intel_display_regs.h)
pub const BXT_GMBUS_GATING_DIS: u32 = ((1 << 14));

/// `DG2_DPFC_GATING_DIS` (intel_display_regs.h)
pub const DG2_DPFC_GATING_DIS: u32 = reg_bit(31);

/// `GEN9_CLKGATE_DIS_5` (intel_display_regs.h)
pub const GEN9_CLKGATE_DIS_5: I915RegT = mmio(0x46540);

/// `DPCE_GATING_DIS` (intel_display_regs.h)
pub const DPCE_GATING_DIS: u32 = reg_bit(17);

/// `_CLKGATE_DIS_PSL_A` (intel_display_regs.h)
pub const _CLKGATE_DIS_PSL_A: u32 = 0x46520;

/// `_CLKGATE_DIS_PSL_B` (intel_display_regs.h)
pub const _CLKGATE_DIS_PSL_B: u32 = 0x46524;

/// `_CLKGATE_DIS_PSL_C` (intel_display_regs.h)
pub const _CLKGATE_DIS_PSL_C: u32 = 0x46528;

/// `DUPS1_GATING_DIS` (intel_display_regs.h)
pub const DUPS1_GATING_DIS: u32 = ((1 << 15));

/// `DUPS2_GATING_DIS` (intel_display_regs.h)
pub const DUPS2_GATING_DIS: u32 = ((1 << 19));

/// `DUPS3_GATING_DIS` (intel_display_regs.h)
pub const DUPS3_GATING_DIS: u32 = ((1 << 23));

/// `CURSOR_GATING_DIS` (intel_display_regs.h)
pub const CURSOR_GATING_DIS: u32 = reg_bit(28);

/// `DPF_GATING_DIS` (intel_display_regs.h)
pub const DPF_GATING_DIS: u32 = ((1 << 10));

/// `DPF_RAM_GATING_DIS` (intel_display_regs.h)
pub const DPF_RAM_GATING_DIS: u32 = ((1 << 9));

/// `DPFR_GATING_DIS` (intel_display_regs.h)
pub const DPFR_GATING_DIS: u32 = ((1 << 8));

/// `CLKGATE_DIS_PSL(pipe)` (intel_display_regs.h)
pub const fn CLKGATE_DIS_PSL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CLKGATE_DIS_PSL_A, _CLKGATE_DIS_PSL_B)
}

/// `_CLKGATE_DIS_PSL_EXT_A` (intel_display_regs.h)
pub const _CLKGATE_DIS_PSL_EXT_A: u32 = 0x4654C;

/// `_CLKGATE_DIS_PSL_EXT_B` (intel_display_regs.h)
pub const _CLKGATE_DIS_PSL_EXT_B: u32 = 0x46550;

/// `PIPEDMC_GATING_DIS` (intel_display_regs.h)
pub const PIPEDMC_GATING_DIS: u32 = reg_bit(12);

/// `CLKGATE_DIS_PSL_EXT(pipe)` (intel_display_regs.h)
pub const fn CLKGATE_DIS_PSL_EXT(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CLKGATE_DIS_PSL_EXT_A, _CLKGATE_DIS_PSL_EXT_B)
}

/// `_TRANS_HTOTAL_A` (intel_display_regs.h)
pub const _TRANS_HTOTAL_A: u32 = 0x60000;

/// `_TRANS_HTOTAL_B` (intel_display_regs.h)
pub const _TRANS_HTOTAL_B: u32 = 0x61000;

/// `HTOTAL_MASK` (intel_display_regs.h)
pub const HTOTAL_MASK: u32 = reg_genmask(31, 16);

/// `HTOTAL(htotal)` (intel_display_regs.h)
pub const fn HTOTAL(htotal: u32) -> u32 {
    reg_field_prep(HTOTAL_MASK, (htotal))
}

/// `HACTIVE_MASK` (intel_display_regs.h)
pub const HACTIVE_MASK: u32 = reg_genmask(15, 0);

/// `HACTIVE(hdisplay)` (intel_display_regs.h)
pub const fn HACTIVE(hdisplay: u32) -> u32 {
    reg_field_prep(HACTIVE_MASK, (hdisplay))
}

/// `_TRANS_HBLANK_A` (intel_display_regs.h)
pub const _TRANS_HBLANK_A: u32 = 0x60004;

/// `_TRANS_HBLANK_B` (intel_display_regs.h)
pub const _TRANS_HBLANK_B: u32 = 0x61004;

/// `HBLANK_END_MASK` (intel_display_regs.h)
pub const HBLANK_END_MASK: u32 = reg_genmask(31, 16);

/// `HBLANK_END(hblank_end)` (intel_display_regs.h)
pub const fn HBLANK_END(hblank_end: u32) -> u32 {
    reg_field_prep(HBLANK_END_MASK, (hblank_end))
}

/// `HBLANK_START_MASK` (intel_display_regs.h)
pub const HBLANK_START_MASK: u32 = reg_genmask(15, 0);

/// `HBLANK_START(hblank_start)` (intel_display_regs.h)
pub const fn HBLANK_START(hblank_start: u32) -> u32 {
    reg_field_prep(HBLANK_START_MASK, (hblank_start))
}

/// `_TRANS_HSYNC_A` (intel_display_regs.h)
pub const _TRANS_HSYNC_A: u32 = 0x60008;

/// `_TRANS_HSYNC_B` (intel_display_regs.h)
pub const _TRANS_HSYNC_B: u32 = 0x61008;

/// `HSYNC_END_MASK` (intel_display_regs.h)
pub const HSYNC_END_MASK: u32 = reg_genmask(31, 16);

/// `HSYNC_END(hsync_end)` (intel_display_regs.h)
pub const fn HSYNC_END(hsync_end: u32) -> u32 {
    reg_field_prep(HSYNC_END_MASK, (hsync_end))
}

/// `HSYNC_START_MASK` (intel_display_regs.h)
pub const HSYNC_START_MASK: u32 = reg_genmask(15, 0);

/// `HSYNC_START(hsync_start)` (intel_display_regs.h)
pub const fn HSYNC_START(hsync_start: u32) -> u32 {
    reg_field_prep(HSYNC_START_MASK, (hsync_start))
}

/// `_TRANS_VTOTAL_A` (intel_display_regs.h)
pub const _TRANS_VTOTAL_A: u32 = 0x6000c;

/// `_TRANS_VTOTAL_B` (intel_display_regs.h)
pub const _TRANS_VTOTAL_B: u32 = 0x6100c;

/// `VTOTAL_MASK` (intel_display_regs.h)
pub const VTOTAL_MASK: u32 = reg_genmask(31, 16);

/// `VTOTAL(vtotal)` (intel_display_regs.h)
pub const fn VTOTAL(vtotal: u32) -> u32 {
    reg_field_prep(VTOTAL_MASK, (vtotal))
}

/// `VACTIVE_MASK` (intel_display_regs.h)
pub const VACTIVE_MASK: u32 = reg_genmask(15, 0);

/// `VACTIVE(vdisplay)` (intel_display_regs.h)
pub const fn VACTIVE(vdisplay: u32) -> u32 {
    reg_field_prep(VACTIVE_MASK, (vdisplay))
}

/// `_TRANS_VBLANK_A` (intel_display_regs.h)
pub const _TRANS_VBLANK_A: u32 = 0x60010;

/// `_TRANS_VBLANK_B` (intel_display_regs.h)
pub const _TRANS_VBLANK_B: u32 = 0x61010;

/// `VBLANK_END_MASK` (intel_display_regs.h)
pub const VBLANK_END_MASK: u32 = reg_genmask(31, 16);

/// `VBLANK_END(vblank_end)` (intel_display_regs.h)
pub const fn VBLANK_END(vblank_end: u32) -> u32 {
    reg_field_prep(VBLANK_END_MASK, (vblank_end))
}

/// `VBLANK_START_MASK` (intel_display_regs.h)
pub const VBLANK_START_MASK: u32 = reg_genmask(15, 0);

/// `VBLANK_START(vblank_start)` (intel_display_regs.h)
pub const fn VBLANK_START(vblank_start: u32) -> u32 {
    reg_field_prep(VBLANK_START_MASK, (vblank_start))
}

/// `_TRANS_VSYNC_A` (intel_display_regs.h)
pub const _TRANS_VSYNC_A: u32 = 0x60014;

/// `_TRANS_VSYNC_B` (intel_display_regs.h)
pub const _TRANS_VSYNC_B: u32 = 0x61014;

/// `VSYNC_END_MASK` (intel_display_regs.h)
pub const VSYNC_END_MASK: u32 = reg_genmask(31, 16);

/// `VSYNC_END(vsync_end)` (intel_display_regs.h)
pub const fn VSYNC_END(vsync_end: u32) -> u32 {
    reg_field_prep(VSYNC_END_MASK, (vsync_end))
}

/// `VSYNC_START_MASK` (intel_display_regs.h)
pub const VSYNC_START_MASK: u32 = reg_genmask(15, 0);

/// `VSYNC_START(vsync_start)` (intel_display_regs.h)
pub const fn VSYNC_START(vsync_start: u32) -> u32 {
    reg_field_prep(VSYNC_START_MASK, (vsync_start))
}

/// `_PIPEASRC` (intel_display_regs.h)
pub const _PIPEASRC: u32 = 0x6001c;

/// `_PIPEBSRC` (intel_display_regs.h)
pub const _PIPEBSRC: u32 = 0x6101c;

/// `PIPESRC_WIDTH_MASK` (intel_display_regs.h)
pub const PIPESRC_WIDTH_MASK: u32 = reg_genmask(31, 16);

/// `PIPESRC_WIDTH(w)` (intel_display_regs.h)
pub const fn PIPESRC_WIDTH(w: u32) -> u32 {
    reg_field_prep(PIPESRC_WIDTH_MASK, (w))
}

/// `PIPESRC_HEIGHT_MASK` (intel_display_regs.h)
pub const PIPESRC_HEIGHT_MASK: u32 = reg_genmask(15, 0);

/// `PIPESRC_HEIGHT(h)` (intel_display_regs.h)
pub const fn PIPESRC_HEIGHT(h: u32) -> u32 {
    reg_field_prep(PIPESRC_HEIGHT_MASK, (h))
}

/// `_BCLRPAT_A` (intel_display_regs.h)
pub const _BCLRPAT_A: u32 = 0x60020;

/// `_BCLRPAT_B` (intel_display_regs.h)
pub const _BCLRPAT_B: u32 = 0x61020;

/// `_TRANS_VSYNCSHIFT_A` (intel_display_regs.h)
pub const _TRANS_VSYNCSHIFT_A: u32 = 0x60028;

/// `_TRANS_VSYNCSHIFT_B` (intel_display_regs.h)
pub const _TRANS_VSYNCSHIFT_B: u32 = 0x61028;

/// `_TRANS_MULT_A` (intel_display_regs.h)
pub const _TRANS_MULT_A: u32 = 0x6002c;

/// `_TRANS_MULT_B` (intel_display_regs.h)
pub const _TRANS_MULT_B: u32 = 0x6102c;

/// `PORTB_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const PORTB_HOTPLUG_INT_EN: u32 = ((1 << 29));

/// `PORTC_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const PORTC_HOTPLUG_INT_EN: u32 = ((1 << 28));

/// `PORTD_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const PORTD_HOTPLUG_INT_EN: u32 = ((1 << 27));

/// `SDVOB_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const SDVOB_HOTPLUG_INT_EN: u32 = ((1 << 26));

/// `SDVOC_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const SDVOC_HOTPLUG_INT_EN: u32 = ((1 << 25));

/// `TV_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const TV_HOTPLUG_INT_EN: u32 = ((1 << 18));

/// `CRT_HOTPLUG_INT_EN` (intel_display_regs.h)
pub const CRT_HOTPLUG_INT_EN: u32 = ((1 << 9));

/// `HOTPLUG_INT_EN_MASK` (intel_display_regs.h)
pub const HOTPLUG_INT_EN_MASK: u32 = ((((((PORTB_HOTPLUG_INT_EN | PORTC_HOTPLUG_INT_EN) | PORTD_HOTPLUG_INT_EN) | SDVOC_HOTPLUG_INT_EN) | SDVOB_HOTPLUG_INT_EN) | CRT_HOTPLUG_INT_EN));

/// `CRT_HOTPLUG_FORCE_DETECT` (intel_display_regs.h)
pub const CRT_HOTPLUG_FORCE_DETECT: u32 = ((1 << 3));

/// `CRT_HOTPLUG_ACTIVATION_PERIOD_32` (intel_display_regs.h)
pub const CRT_HOTPLUG_ACTIVATION_PERIOD_32: u32 = ((0 << 8));

/// `CRT_HOTPLUG_ACTIVATION_PERIOD_64` (intel_display_regs.h)
pub const CRT_HOTPLUG_ACTIVATION_PERIOD_64: u32 = ((1 << 8));

/// `CRT_HOTPLUG_DAC_ON_TIME_2M` (intel_display_regs.h)
pub const CRT_HOTPLUG_DAC_ON_TIME_2M: u32 = ((0 << 7));

/// `CRT_HOTPLUG_DAC_ON_TIME_4M` (intel_display_regs.h)
pub const CRT_HOTPLUG_DAC_ON_TIME_4M: u32 = ((1 << 7));

/// `CRT_HOTPLUG_VOLTAGE_COMPARE_40` (intel_display_regs.h)
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_40: u32 = ((0 << 5));

/// `CRT_HOTPLUG_VOLTAGE_COMPARE_50` (intel_display_regs.h)
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_50: u32 = ((1 << 5));

/// `CRT_HOTPLUG_VOLTAGE_COMPARE_60` (intel_display_regs.h)
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_60: u32 = ((2 << 5));

/// `CRT_HOTPLUG_VOLTAGE_COMPARE_70` (intel_display_regs.h)
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_70: u32 = ((3 << 5));

/// `CRT_HOTPLUG_VOLTAGE_COMPARE_MASK` (intel_display_regs.h)
pub const CRT_HOTPLUG_VOLTAGE_COMPARE_MASK: u32 = ((3 << 5));

/// `CRT_HOTPLUG_DETECT_DELAY_1G` (intel_display_regs.h)
pub const CRT_HOTPLUG_DETECT_DELAY_1G: u32 = ((0 << 4));

/// `CRT_HOTPLUG_DETECT_DELAY_2G` (intel_display_regs.h)
pub const CRT_HOTPLUG_DETECT_DELAY_2G: u32 = ((1 << 4));

/// `CRT_HOTPLUG_DETECT_VOLTAGE_325MV` (intel_display_regs.h)
pub const CRT_HOTPLUG_DETECT_VOLTAGE_325MV: u32 = ((0 << 2));

/// `CRT_HOTPLUG_DETECT_VOLTAGE_475MV` (intel_display_regs.h)
pub const CRT_HOTPLUG_DETECT_VOLTAGE_475MV: u32 = ((1 << 2));

/// `PORTD_HOTPLUG_LIVE_STATUS_G4X` (intel_display_regs.h)
pub const PORTD_HOTPLUG_LIVE_STATUS_G4X: u32 = ((1 << 27));

/// `PORTC_HOTPLUG_LIVE_STATUS_G4X` (intel_display_regs.h)
pub const PORTC_HOTPLUG_LIVE_STATUS_G4X: u32 = ((1 << 28));

/// `PORTB_HOTPLUG_LIVE_STATUS_G4X` (intel_display_regs.h)
pub const PORTB_HOTPLUG_LIVE_STATUS_G4X: u32 = ((1 << 29));

/// `PORTD_HOTPLUG_INT_STATUS` (intel_display_regs.h)
pub const PORTD_HOTPLUG_INT_STATUS: u32 = ((3 << 21));

/// `PORTD_HOTPLUG_INT_LONG_PULSE` (intel_display_regs.h)
pub const PORTD_HOTPLUG_INT_LONG_PULSE: u32 = ((2 << 21));

/// `PORTD_HOTPLUG_INT_SHORT_PULSE` (intel_display_regs.h)
pub const PORTD_HOTPLUG_INT_SHORT_PULSE: u32 = ((1 << 21));

/// `PORTC_HOTPLUG_INT_STATUS` (intel_display_regs.h)
pub const PORTC_HOTPLUG_INT_STATUS: u32 = ((3 << 19));

/// `PORTC_HOTPLUG_INT_LONG_PULSE` (intel_display_regs.h)
pub const PORTC_HOTPLUG_INT_LONG_PULSE: u32 = ((2 << 19));

/// `PORTC_HOTPLUG_INT_SHORT_PULSE` (intel_display_regs.h)
pub const PORTC_HOTPLUG_INT_SHORT_PULSE: u32 = ((1 << 19));

/// `PORTB_HOTPLUG_INT_STATUS` (intel_display_regs.h)
pub const PORTB_HOTPLUG_INT_STATUS: u32 = ((3 << 17));

/// `PORTB_HOTPLUG_INT_LONG_PULSE` (intel_display_regs.h)
pub const PORTB_HOTPLUG_INT_LONG_PULSE: u32 = ((2 << 17));

/// `PORTB_HOTPLUG_INT_SHORT_PLUSE` (intel_display_regs.h)
pub const PORTB_HOTPLUG_INT_SHORT_PLUSE: u32 = ((1 << 17));

/// `CRT_HOTPLUG_INT_STATUS` (intel_display_regs.h)
pub const CRT_HOTPLUG_INT_STATUS: u32 = ((1 << 11));

/// `TV_HOTPLUG_INT_STATUS` (intel_display_regs.h)
pub const TV_HOTPLUG_INT_STATUS: u32 = ((1 << 10));

/// `CRT_HOTPLUG_MONITOR_MASK` (intel_display_regs.h)
pub const CRT_HOTPLUG_MONITOR_MASK: u32 = ((3 << 8));

/// `CRT_HOTPLUG_MONITOR_COLOR` (intel_display_regs.h)
pub const CRT_HOTPLUG_MONITOR_COLOR: u32 = ((3 << 8));

/// `CRT_HOTPLUG_MONITOR_MONO` (intel_display_regs.h)
pub const CRT_HOTPLUG_MONITOR_MONO: u32 = ((2 << 8));

/// `CRT_HOTPLUG_MONITOR_NONE` (intel_display_regs.h)
pub const CRT_HOTPLUG_MONITOR_NONE: u32 = ((0 << 8));

/// `DP_AUX_CHANNEL_D_INT_STATUS_G4X` (intel_display_regs.h)
pub const DP_AUX_CHANNEL_D_INT_STATUS_G4X: u32 = ((1 << 6));

/// `DP_AUX_CHANNEL_C_INT_STATUS_G4X` (intel_display_regs.h)
pub const DP_AUX_CHANNEL_C_INT_STATUS_G4X: u32 = ((1 << 5));

/// `DP_AUX_CHANNEL_B_INT_STATUS_G4X` (intel_display_regs.h)
pub const DP_AUX_CHANNEL_B_INT_STATUS_G4X: u32 = ((1 << 4));

/// `DP_AUX_CHANNEL_MASK_INT_STATUS_G4X` (intel_display_regs.h)
pub const DP_AUX_CHANNEL_MASK_INT_STATUS_G4X: u32 = ((7 << 4));

/// `SDVOC_HOTPLUG_INT_STATUS_G4X` (intel_display_regs.h)
pub const SDVOC_HOTPLUG_INT_STATUS_G4X: u32 = ((1 << 3));

/// `SDVOB_HOTPLUG_INT_STATUS_G4X` (intel_display_regs.h)
pub const SDVOB_HOTPLUG_INT_STATUS_G4X: u32 = ((1 << 2));

/// `SDVOC_HOTPLUG_INT_STATUS_I965` (intel_display_regs.h)
pub const SDVOC_HOTPLUG_INT_STATUS_I965: u32 = ((3 << 4));

/// `SDVOB_HOTPLUG_INT_STATUS_I965` (intel_display_regs.h)
pub const SDVOB_HOTPLUG_INT_STATUS_I965: u32 = ((3 << 2));

/// `SDVOC_HOTPLUG_INT_STATUS_I915` (intel_display_regs.h)
pub const SDVOC_HOTPLUG_INT_STATUS_I915: u32 = ((1 << 7));

/// `SDVOB_HOTPLUG_INT_STATUS_I915` (intel_display_regs.h)
pub const SDVOB_HOTPLUG_INT_STATUS_I915: u32 = ((1 << 6));

/// `HOTPLUG_INT_STATUS_G4X` (intel_display_regs.h)
pub const HOTPLUG_INT_STATUS_G4X: u32 = ((((((CRT_HOTPLUG_INT_STATUS | SDVOB_HOTPLUG_INT_STATUS_G4X) | SDVOC_HOTPLUG_INT_STATUS_G4X) | PORTB_HOTPLUG_INT_STATUS) | PORTC_HOTPLUG_INT_STATUS) | PORTD_HOTPLUG_INT_STATUS));

/// `HOTPLUG_INT_STATUS_I915` (intel_display_regs.h)
pub const HOTPLUG_INT_STATUS_I915: u32 = ((((((CRT_HOTPLUG_INT_STATUS | SDVOB_HOTPLUG_INT_STATUS_I915) | SDVOC_HOTPLUG_INT_STATUS_I915) | PORTB_HOTPLUG_INT_STATUS) | PORTC_HOTPLUG_INT_STATUS) | PORTD_HOTPLUG_INT_STATUS));

/// `_GEN3_SDVOB` (intel_display_regs.h)
pub const _GEN3_SDVOB: u32 = 0x61140;

/// `_GEN3_SDVOC` (intel_display_regs.h)
pub const _GEN3_SDVOC: u32 = 0x61160;

/// `GEN3_SDVOB` (intel_display_regs.h)
pub const GEN3_SDVOB: I915RegT = mmio(_GEN3_SDVOB);

/// `GEN3_SDVOC` (intel_display_regs.h)
pub const GEN3_SDVOC: I915RegT = mmio(_GEN3_SDVOC);

/// `GEN4_HDMIB` (intel_display_regs.h)
pub const GEN4_HDMIB: I915RegT = GEN3_SDVOB;

/// `GEN4_HDMIC` (intel_display_regs.h)
pub const GEN4_HDMIC: I915RegT = GEN3_SDVOC;

/// `VLV_HDMIB` (intel_display_regs.h)
pub const VLV_HDMIB: I915RegT = mmio((VLV_DISPLAY_BASE + 0x61140));

/// `VLV_HDMIC` (intel_display_regs.h)
pub const VLV_HDMIC: I915RegT = mmio((VLV_DISPLAY_BASE + 0x61160));

/// `CHV_HDMID` (intel_display_regs.h)
pub const CHV_HDMID: I915RegT = mmio((VLV_DISPLAY_BASE + 0x6116C));

/// `PCH_SDVOB` (intel_display_regs.h)
pub const PCH_SDVOB: I915RegT = mmio(0xe1140);

/// `PCH_HDMIB` (intel_display_regs.h)
pub const PCH_HDMIB: I915RegT = PCH_SDVOB;

/// `PCH_HDMIC` (intel_display_regs.h)
pub const PCH_HDMIC: I915RegT = mmio(0xe1150);

/// `PCH_HDMID` (intel_display_regs.h)
pub const PCH_HDMID: I915RegT = mmio(0xe1160);

/// `PORT_DFT_I9XX` (intel_display_regs.h)
pub const PORT_DFT_I9XX: I915RegT = mmio(0x61150);

/// `DC_BALANCE_RESET` (intel_display_regs.h)
pub const DC_BALANCE_RESET: u32 = ((1 << 25));

/// `DC_BALANCE_RESET_VLV` (intel_display_regs.h)
pub const DC_BALANCE_RESET_VLV: u32 = ((1 << 31));

/// `PIPE_SCRAMBLE_RESET_MASK` (intel_display_regs.h)
pub const PIPE_SCRAMBLE_RESET_MASK: u32 = ((((1 << 14)) | ((0x3 << 0))));

/// `PIPE_C_SCRAMBLE_RESET` (intel_display_regs.h)
pub const PIPE_C_SCRAMBLE_RESET: u32 = reg_bit(14);

/// `PIPE_B_SCRAMBLE_RESET` (intel_display_regs.h)
pub const PIPE_B_SCRAMBLE_RESET: u32 = reg_bit(1);

/// `PIPE_A_SCRAMBLE_RESET` (intel_display_regs.h)
pub const PIPE_A_SCRAMBLE_RESET: u32 = reg_bit(0);

/// `SDVO_ENABLE` (intel_display_regs.h)
pub const SDVO_ENABLE: u32 = ((1 << 31));

/// `SDVO_PIPE_SEL_SHIFT` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_SHIFT: u32 = 30;

/// `SDVO_PIPE_SEL_MASK` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_MASK: u32 = ((1 << 30));

/// `SDVO_PIPE_SEL(pipe)` (intel_display_regs.h)
pub const fn SDVO_PIPE_SEL(pipe: u32) -> u32 {
    (((pipe) << 30))
}

/// `SDVO_STALL_SELECT` (intel_display_regs.h)
pub const SDVO_STALL_SELECT: u32 = ((1 << 29));

/// `SDVO_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const SDVO_INTERRUPT_ENABLE: u32 = ((1 << 26));

/// `SDVO_PORT_MULTIPLY_MASK` (intel_display_regs.h)
pub const SDVO_PORT_MULTIPLY_MASK: u32 = ((7 << 23));

/// `SDVO_PORT_MULTIPLY_SHIFT` (intel_display_regs.h)
pub const SDVO_PORT_MULTIPLY_SHIFT: u32 = 23;

/// `SDVO_PHASE_SELECT_MASK` (intel_display_regs.h)
pub const SDVO_PHASE_SELECT_MASK: u32 = ((15 << 19));

/// `SDVO_PHASE_SELECT_DEFAULT` (intel_display_regs.h)
pub const SDVO_PHASE_SELECT_DEFAULT: u32 = ((6 << 19));

/// `SDVO_CLOCK_OUTPUT_INVERT` (intel_display_regs.h)
pub const SDVO_CLOCK_OUTPUT_INVERT: u32 = ((1 << 18));

/// `SDVOC_GANG_MODE` (intel_display_regs.h)
pub const SDVOC_GANG_MODE: u32 = ((1 << 16));

/// `SDVO_BORDER_ENABLE` (intel_display_regs.h)
pub const SDVO_BORDER_ENABLE: u32 = ((1 << 7));

/// `SDVOB_PCIE_CONCURRENCY` (intel_display_regs.h)
pub const SDVOB_PCIE_CONCURRENCY: u32 = ((1 << 3));

/// `SDVO_DETECTED` (intel_display_regs.h)
pub const SDVO_DETECTED: u32 = ((1 << 2));

/// `SDVOB_PRESERVE_MASK` (intel_display_regs.h)
pub const SDVOB_PRESERVE_MASK: u32 = ((((((1 << 17)) | ((1 << 16))) | ((1 << 14))) | SDVO_INTERRUPT_ENABLE));

/// `SDVOC_PRESERVE_MASK` (intel_display_regs.h)
pub const SDVOC_PRESERVE_MASK: u32 = ((((1 << 17)) | SDVO_INTERRUPT_ENABLE));

/// `SDVO_COLOR_FORMAT_8bpc` (intel_display_regs.h)
pub const SDVO_COLOR_FORMAT_8bpc: u32 = ((0 << 26));

/// `SDVO_COLOR_FORMAT_MASK` (intel_display_regs.h)
pub const SDVO_COLOR_FORMAT_MASK: u32 = ((7 << 26));

/// `SDVO_ENCODING_SDVO` (intel_display_regs.h)
pub const SDVO_ENCODING_SDVO: u32 = ((0 << 10));

/// `SDVO_ENCODING_HDMI` (intel_display_regs.h)
pub const SDVO_ENCODING_HDMI: u32 = ((2 << 10));

/// `HDMI_MODE_SELECT_HDMI` (intel_display_regs.h)
pub const HDMI_MODE_SELECT_HDMI: u32 = ((1 << 9));

/// `HDMI_MODE_SELECT_DVI` (intel_display_regs.h)
pub const HDMI_MODE_SELECT_DVI: u32 = ((0 << 9));

/// `HDMI_COLOR_RANGE_16_235` (intel_display_regs.h)
pub const HDMI_COLOR_RANGE_16_235: u32 = ((1 << 8));

/// `HDMI_AUDIO_ENABLE` (intel_display_regs.h)
pub const HDMI_AUDIO_ENABLE: u32 = ((1 << 6));

/// `SDVO_VSYNC_ACTIVE_HIGH` (intel_display_regs.h)
pub const SDVO_VSYNC_ACTIVE_HIGH: u32 = ((1 << 4));

/// `SDVO_HSYNC_ACTIVE_HIGH` (intel_display_regs.h)
pub const SDVO_HSYNC_ACTIVE_HIGH: u32 = ((1 << 3));

/// `HDMI_COLOR_FORMAT_12bpc` (intel_display_regs.h)
pub const HDMI_COLOR_FORMAT_12bpc: u32 = ((3 << 26));

/// `SDVOB_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const SDVOB_HOTPLUG_ENABLE: u32 = ((1 << 23));

/// `SDVO_PIPE_SEL_SHIFT_CPT` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_SHIFT_CPT: u32 = 29;

/// `SDVO_PIPE_SEL_MASK_CPT` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_MASK_CPT: u32 = ((3 << 29));

/// `SDVO_PIPE_SEL_CPT(pipe)` (intel_display_regs.h)
pub const fn SDVO_PIPE_SEL_CPT(pipe: u32) -> u32 {
    (((pipe) << 29))
}

/// `SDVO_PIPE_SEL_SHIFT_CHV` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_SHIFT_CHV: u32 = 24;

/// `SDVO_PIPE_SEL_MASK_CHV` (intel_display_regs.h)
pub const SDVO_PIPE_SEL_MASK_CHV: u32 = ((3 << 24));

/// `SDVO_PIPE_SEL_CHV(pipe)` (intel_display_regs.h)
pub const fn SDVO_PIPE_SEL_CHV(pipe: u32) -> u32 {
    (((pipe) << 24))
}

/// `VIDEO_DIP_DATA` (intel_display_regs.h)
pub const VIDEO_DIP_DATA: I915RegT = mmio(0x61178);

/// `VIDEO_DIP_DATA_SIZE` (intel_display_regs.h)
pub const VIDEO_DIP_DATA_SIZE: u32 = 32;

/// `VIDEO_DIP_ASYNC_DATA_SIZE` (intel_display_regs.h)
pub const VIDEO_DIP_ASYNC_DATA_SIZE: u32 = 36;

/// `VIDEO_DIP_GMP_DATA_SIZE` (intel_display_regs.h)
pub const VIDEO_DIP_GMP_DATA_SIZE: u32 = 36;

/// `VIDEO_DIP_VSC_DATA_SIZE` (intel_display_regs.h)
pub const VIDEO_DIP_VSC_DATA_SIZE: u32 = 36;

/// `VIDEO_DIP_PPS_DATA_SIZE` (intel_display_regs.h)
pub const VIDEO_DIP_PPS_DATA_SIZE: u32 = 132;

/// `VIDEO_DIP_CTL` (intel_display_regs.h)
pub const VIDEO_DIP_CTL: I915RegT = mmio(0x61170);

/// `VIDEO_DIP_ENABLE` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE: u32 = ((1 << 31));

/// `VIDEO_DIP_PORT(port)` (intel_display_regs.h)
pub const fn VIDEO_DIP_PORT(port: u32) -> u32 {
    (((port) << 29))
}

/// `VIDEO_DIP_PORT_MASK` (intel_display_regs.h)
pub const VIDEO_DIP_PORT_MASK: u32 = ((3 << 29));

/// `VIDEO_DIP_ENABLE_GCP` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_GCP: u32 = ((1 << 25));

/// `VIDEO_DIP_ENABLE_AVI` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_AVI: u32 = ((1 << 21));

/// `VIDEO_DIP_ENABLE_VENDOR` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_VENDOR: u32 = ((2 << 21));

/// `VIDEO_DIP_ENABLE_GAMUT` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_GAMUT: u32 = ((4 << 21));

/// `VIDEO_DIP_ENABLE_SPD` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_SPD: u32 = ((8 << 21));

/// `VIDEO_DIP_SELECT_AVI` (intel_display_regs.h)
pub const VIDEO_DIP_SELECT_AVI: u32 = ((0 << 19));

/// `VIDEO_DIP_SELECT_VENDOR` (intel_display_regs.h)
pub const VIDEO_DIP_SELECT_VENDOR: u32 = ((1 << 19));

/// `VIDEO_DIP_SELECT_GAMUT` (intel_display_regs.h)
pub const VIDEO_DIP_SELECT_GAMUT: u32 = ((2 << 19));

/// `VIDEO_DIP_SELECT_SPD` (intel_display_regs.h)
pub const VIDEO_DIP_SELECT_SPD: u32 = ((3 << 19));

/// `VIDEO_DIP_SELECT_MASK` (intel_display_regs.h)
pub const VIDEO_DIP_SELECT_MASK: u32 = ((3 << 19));

/// `VIDEO_DIP_FREQ_ONCE` (intel_display_regs.h)
pub const VIDEO_DIP_FREQ_ONCE: u32 = ((0 << 16));

/// `VIDEO_DIP_FREQ_VSYNC` (intel_display_regs.h)
pub const VIDEO_DIP_FREQ_VSYNC: u32 = ((1 << 16));

/// `VIDEO_DIP_FREQ_2VSYNC` (intel_display_regs.h)
pub const VIDEO_DIP_FREQ_2VSYNC: u32 = ((2 << 16));

/// `VIDEO_DIP_FREQ_MASK` (intel_display_regs.h)
pub const VIDEO_DIP_FREQ_MASK: u32 = ((3 << 16));

/// `VIDEO_DIP_ENABLE_DRM_GLK` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_DRM_GLK: u32 = ((1 << 28));

/// `PSR_VSC_BIT_7_SET` (intel_display_regs.h)
pub const PSR_VSC_BIT_7_SET: u32 = ((1 << 27));

/// `VSC_SELECT_MASK` (intel_display_regs.h)
pub const VSC_SELECT_MASK: u32 = ((0x3 << 25));

/// `VSC_SELECT_SHIFT` (intel_display_regs.h)
pub const VSC_SELECT_SHIFT: u32 = 25;

/// `VSC_DIP_HW_HEA_DATA` (intel_display_regs.h)
pub const VSC_DIP_HW_HEA_DATA: u32 = ((0 << 25));

/// `VSC_DIP_HW_HEA_SW_DATA` (intel_display_regs.h)
pub const VSC_DIP_HW_HEA_SW_DATA: u32 = ((1 << 25));

/// `VSC_DIP_HW_DATA_SW_HEA` (intel_display_regs.h)
pub const VSC_DIP_HW_DATA_SW_HEA: u32 = ((2 << 25));

/// `VSC_DIP_SW_HEA_DATA` (intel_display_regs.h)
pub const VSC_DIP_SW_HEA_DATA: u32 = ((3 << 25));

/// `VDIP_ENABLE_PPS` (intel_display_regs.h)
pub const VDIP_ENABLE_PPS: u32 = ((1 << 24));

/// `VIDEO_DIP_ENABLE_VSC_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_VSC_HSW: u32 = ((1 << 20));

/// `VIDEO_DIP_ENABLE_GCP_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_GCP_HSW: u32 = ((1 << 16));

/// `VIDEO_DIP_ENABLE_AVI_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_AVI_HSW: u32 = ((1 << 12));

/// `VIDEO_DIP_ENABLE_VS_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_VS_HSW: u32 = ((1 << 8));

/// `VIDEO_DIP_ENABLE_GMP_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_GMP_HSW: u32 = ((1 << 4));

/// `VIDEO_DIP_ENABLE_SPD_HSW` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_SPD_HSW: u32 = ((1 << 0));

/// `VIDEO_DIP_ENABLE_AS_ADL` (intel_display_regs.h)
pub const VIDEO_DIP_ENABLE_AS_ADL: u32 = reg_bit(23);

/// `PCH_GTC_CTL` (intel_display_regs.h)
pub const PCH_GTC_CTL: I915RegT = mmio(0xe7000);

/// `PCH_GTC_ENABLE` (intel_display_regs.h)
pub const PCH_GTC_ENABLE: u32 = ((1 << 31));

/// `DP_A` (intel_display_regs.h)
pub const DP_A: I915RegT = mmio(0x64000);

/// `DP_B` (intel_display_regs.h)
pub const DP_B: I915RegT = mmio(0x64100);

/// `DP_C` (intel_display_regs.h)
pub const DP_C: I915RegT = mmio(0x64200);

/// `DP_D` (intel_display_regs.h)
pub const DP_D: I915RegT = mmio(0x64300);

/// `VLV_DP_B` (intel_display_regs.h)
pub const VLV_DP_B: I915RegT = mmio((VLV_DISPLAY_BASE + 0x64100));

/// `VLV_DP_C` (intel_display_regs.h)
pub const VLV_DP_C: I915RegT = mmio((VLV_DISPLAY_BASE + 0x64200));

/// `CHV_DP_D` (intel_display_regs.h)
pub const CHV_DP_D: I915RegT = mmio((VLV_DISPLAY_BASE + 0x64300));

/// `DP_PORT_EN` (intel_display_regs.h)
pub const DP_PORT_EN: u32 = reg_bit(31);

/// `DP_PIPE_SEL_MASK` (intel_display_regs.h)
pub const DP_PIPE_SEL_MASK: u32 = reg_genmask(30, 30);

/// `DP_PIPE_SEL(pipe)` (intel_display_regs.h)
pub const fn DP_PIPE_SEL(pipe: u32) -> u32 {
    reg_field_prep(DP_PIPE_SEL_MASK, (pipe))
}

/// `DP_PIPE_SEL_MASK_IVB` (intel_display_regs.h)
pub const DP_PIPE_SEL_MASK_IVB: u32 = reg_genmask(30, 29);

/// `DP_PIPE_SEL_IVB(pipe)` (intel_display_regs.h)
pub const fn DP_PIPE_SEL_IVB(pipe: u32) -> u32 {
    reg_field_prep(DP_PIPE_SEL_MASK_IVB, (pipe))
}

/// `DP_PIPE_SEL_SHIFT_CHV` (intel_display_regs.h)
pub const DP_PIPE_SEL_SHIFT_CHV: u32 = 16;

/// `DP_PIPE_SEL_MASK_CHV` (intel_display_regs.h)
pub const DP_PIPE_SEL_MASK_CHV: u32 = reg_genmask(17, 16);

/// `DP_PIPE_SEL_CHV(pipe)` (intel_display_regs.h)
pub const fn DP_PIPE_SEL_CHV(pipe: u32) -> u32 {
    reg_field_prep(DP_PIPE_SEL_MASK_CHV, (pipe))
}

/// `DP_LINK_TRAIN_MASK` (intel_display_regs.h)
pub const DP_LINK_TRAIN_MASK: u32 = reg_genmask(29, 28);

/// `DP_LINK_TRAIN_PAT_1` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_1: u32 = reg_field_prep(DP_LINK_TRAIN_MASK, 0);

/// `DP_LINK_TRAIN_PAT_2` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_2: u32 = reg_field_prep(DP_LINK_TRAIN_MASK, 1);

/// `DP_LINK_TRAIN_PAT_IDLE` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_IDLE: u32 = reg_field_prep(DP_LINK_TRAIN_MASK, 2);

/// `DP_LINK_TRAIN_OFF` (intel_display_regs.h)
pub const DP_LINK_TRAIN_OFF: u32 = reg_field_prep(DP_LINK_TRAIN_MASK, 3);

/// `DP_LINK_TRAIN_MASK_CPT` (intel_display_regs.h)
pub const DP_LINK_TRAIN_MASK_CPT: u32 = reg_genmask(10, 8);

/// `DP_LINK_TRAIN_PAT_1_CPT` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_1_CPT: u32 = reg_field_prep(DP_LINK_TRAIN_MASK_CPT, 0);

/// `DP_LINK_TRAIN_PAT_2_CPT` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_2_CPT: u32 = reg_field_prep(DP_LINK_TRAIN_MASK_CPT, 1);

/// `DP_LINK_TRAIN_PAT_IDLE_CPT` (intel_display_regs.h)
pub const DP_LINK_TRAIN_PAT_IDLE_CPT: u32 = reg_field_prep(DP_LINK_TRAIN_MASK_CPT, 2);

/// `DP_LINK_TRAIN_OFF_CPT` (intel_display_regs.h)
pub const DP_LINK_TRAIN_OFF_CPT: u32 = reg_field_prep(DP_LINK_TRAIN_MASK_CPT, 3);

/// `DP_VOLTAGE_MASK` (intel_display_regs.h)
pub const DP_VOLTAGE_MASK: u32 = reg_genmask(27, 25);

/// `DP_VOLTAGE_0_4` (intel_display_regs.h)
pub const DP_VOLTAGE_0_4: u32 = reg_field_prep(DP_VOLTAGE_MASK, 0);

/// `DP_VOLTAGE_0_6` (intel_display_regs.h)
pub const DP_VOLTAGE_0_6: u32 = reg_field_prep(DP_VOLTAGE_MASK, 1);

/// `DP_VOLTAGE_0_8` (intel_display_regs.h)
pub const DP_VOLTAGE_0_8: u32 = reg_field_prep(DP_VOLTAGE_MASK, 2);

/// `DP_VOLTAGE_1_2` (intel_display_regs.h)
pub const DP_VOLTAGE_1_2: u32 = reg_field_prep(DP_VOLTAGE_MASK, 3);

/// `DP_PRE_EMPHASIS_MASK` (intel_display_regs.h)
pub const DP_PRE_EMPHASIS_MASK: u32 = reg_genmask(24, 22);

/// `DP_PRE_EMPHASIS_0` (intel_display_regs.h)
pub const DP_PRE_EMPHASIS_0: u32 = reg_field_prep(DP_PRE_EMPHASIS_MASK, 0);

/// `DP_PRE_EMPHASIS_3_5` (intel_display_regs.h)
pub const DP_PRE_EMPHASIS_3_5: u32 = reg_field_prep(DP_PRE_EMPHASIS_MASK, 1);

/// `DP_PRE_EMPHASIS_6` (intel_display_regs.h)
pub const DP_PRE_EMPHASIS_6: u32 = reg_field_prep(DP_PRE_EMPHASIS_MASK, 2);

/// `DP_PRE_EMPHASIS_9_5` (intel_display_regs.h)
pub const DP_PRE_EMPHASIS_9_5: u32 = reg_field_prep(DP_PRE_EMPHASIS_MASK, 3);

/// `DP_PORT_WIDTH_MASK` (intel_display_regs.h)
pub const DP_PORT_WIDTH_MASK: u32 = reg_genmask(21, 19);

/// `DP_PORT_WIDTH(width)` (intel_display_regs.h)
pub const fn DP_PORT_WIDTH(width: u32) -> u32 {
    reg_field_prep(DP_PORT_WIDTH_MASK, ((width) - 1))
}

/// `DP_ENHANCED_FRAMING` (intel_display_regs.h)
pub const DP_ENHANCED_FRAMING: u32 = reg_bit(18);

/// `EDP_PLL_FREQ_MASK` (intel_display_regs.h)
pub const EDP_PLL_FREQ_MASK: u32 = reg_genmask(17, 16);

/// `EDP_PLL_FREQ_270MHZ` (intel_display_regs.h)
pub const EDP_PLL_FREQ_270MHZ: u32 = reg_field_prep(EDP_PLL_FREQ_MASK, 0);

/// `EDP_PLL_FREQ_162MHZ` (intel_display_regs.h)
pub const EDP_PLL_FREQ_162MHZ: u32 = reg_field_prep(EDP_PLL_FREQ_MASK, 1);

/// `DP_PORT_REVERSAL` (intel_display_regs.h)
pub const DP_PORT_REVERSAL: u32 = reg_bit(15);

/// `EDP_PLL_ENABLE` (intel_display_regs.h)
pub const EDP_PLL_ENABLE: u32 = reg_bit(14);

/// `DP_CLOCK_OUTPUT_ENABLE` (intel_display_regs.h)
pub const DP_CLOCK_OUTPUT_ENABLE: u32 = reg_bit(13);

/// `DP_SCRAMBLING_DISABLE` (intel_display_regs.h)
pub const DP_SCRAMBLING_DISABLE: u32 = reg_bit(12);

/// `DP_SCRAMBLING_DISABLE_ILK` (intel_display_regs.h)
pub const DP_SCRAMBLING_DISABLE_ILK: u32 = reg_bit(7);

/// `DP_COLOR_RANGE_16_235` (intel_display_regs.h)
pub const DP_COLOR_RANGE_16_235: u32 = reg_bit(8);

/// `DP_AUDIO_OUTPUT_ENABLE` (intel_display_regs.h)
pub const DP_AUDIO_OUTPUT_ENABLE: u32 = reg_bit(6);

/// `DP_SYNC_VS_HIGH` (intel_display_regs.h)
pub const DP_SYNC_VS_HIGH: u32 = reg_bit(4);

/// `DP_SYNC_HS_HIGH` (intel_display_regs.h)
pub const DP_SYNC_HS_HIGH: u32 = reg_bit(3);

/// `DP_DETECTED` (intel_display_regs.h)
pub const DP_DETECTED: u32 = reg_bit(2);

/// `_PIPEA_DATA_M_G4X` (intel_display_regs.h)
pub const _PIPEA_DATA_M_G4X: u32 = 0x70050;

/// `_PIPEB_DATA_M_G4X` (intel_display_regs.h)
pub const _PIPEB_DATA_M_G4X: u32 = 0x71050;

/// `PIPE_DATA_M_G4X(pipe)` (intel_display_regs.h)
pub const fn PIPE_DATA_M_G4X(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_DATA_M_G4X, _PIPEB_DATA_M_G4X)
}

/// `TU_SIZE_MASK` (intel_display_regs.h)
pub const TU_SIZE_MASK: u32 = reg_genmask(30, 25);

/// `TU_SIZE(x)` (intel_display_regs.h)
pub const fn TU_SIZE(x: u32) -> u32 {
    reg_field_prep(TU_SIZE_MASK, ((x) - 1))
}

/// `DATA_LINK_M_N_MASK` (intel_display_regs.h)
pub const DATA_LINK_M_N_MASK: u32 = reg_genmask(23, 0);

/// `DATA_LINK_N_MAX` (intel_display_regs.h)
pub const DATA_LINK_N_MAX: u32 = (0x800000);

/// `_PIPEA_DATA_N_G4X` (intel_display_regs.h)
pub const _PIPEA_DATA_N_G4X: u32 = 0x70054;

/// `_PIPEB_DATA_N_G4X` (intel_display_regs.h)
pub const _PIPEB_DATA_N_G4X: u32 = 0x71054;

/// `PIPE_DATA_N_G4X(pipe)` (intel_display_regs.h)
pub const fn PIPE_DATA_N_G4X(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_DATA_N_G4X, _PIPEB_DATA_N_G4X)
}

/// `_PIPEA_LINK_M_G4X` (intel_display_regs.h)
pub const _PIPEA_LINK_M_G4X: u32 = 0x70060;

/// `_PIPEB_LINK_M_G4X` (intel_display_regs.h)
pub const _PIPEB_LINK_M_G4X: u32 = 0x71060;

/// `PIPE_LINK_M_G4X(pipe)` (intel_display_regs.h)
pub const fn PIPE_LINK_M_G4X(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_LINK_M_G4X, _PIPEB_LINK_M_G4X)
}

/// `_PIPEA_LINK_N_G4X` (intel_display_regs.h)
pub const _PIPEA_LINK_N_G4X: u32 = 0x70064;

/// `_PIPEB_LINK_N_G4X` (intel_display_regs.h)
pub const _PIPEB_LINK_N_G4X: u32 = 0x71064;

/// `PIPE_LINK_N_G4X(pipe)` (intel_display_regs.h)
pub const fn PIPE_LINK_N_G4X(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_LINK_N_G4X, _PIPEB_LINK_N_G4X)
}

/// `_PIPEADSL` (intel_display_regs.h)
pub const _PIPEADSL: u32 = 0x70000;

/// `PIPEDSL_CURR_FIELD` (intel_display_regs.h)
pub const PIPEDSL_CURR_FIELD: u32 = reg_bit(31);

/// `PIPEDSL_LINE_MASK` (intel_display_regs.h)
pub const PIPEDSL_LINE_MASK: u32 = reg_genmask(19, 0);

/// `_TRANSACONF` (intel_display_regs.h)
pub const _TRANSACONF: u32 = 0x70008;

/// `TRANSCONF_ENABLE` (intel_display_regs.h)
pub const TRANSCONF_ENABLE: u32 = reg_bit(31);

/// `TRANSCONF_DOUBLE_WIDE` (intel_display_regs.h)
pub const TRANSCONF_DOUBLE_WIDE: u32 = reg_bit(30);

/// `TRANSCONF_STATE_ENABLE` (intel_display_regs.h)
pub const TRANSCONF_STATE_ENABLE: u32 = reg_bit(30);

/// `TRANSCONF_DSI_PLL_LOCKED` (intel_display_regs.h)
pub const TRANSCONF_DSI_PLL_LOCKED: u32 = reg_bit(29);

/// `TRANSCONF_FRAME_START_DELAY_MASK` (intel_display_regs.h)
pub const TRANSCONF_FRAME_START_DELAY_MASK: u32 = reg_genmask(28, 27);

/// `TRANSCONF_FRAME_START_DELAY(x)` (intel_display_regs.h)
pub const fn TRANSCONF_FRAME_START_DELAY(x: u32) -> u32 {
    reg_field_prep(TRANSCONF_FRAME_START_DELAY_MASK, (x))
}

/// `TRANSCONF_PIPE_LOCKED` (intel_display_regs.h)
pub const TRANSCONF_PIPE_LOCKED: u32 = reg_bit(25);

/// `TRANSCONF_FORCE_BORDER` (intel_display_regs.h)
pub const TRANSCONF_FORCE_BORDER: u32 = reg_bit(25);

/// `TRANSCONF_GAMMA_MODE_MASK_I9XX` (intel_display_regs.h)
pub const TRANSCONF_GAMMA_MODE_MASK_I9XX: u32 = reg_bit(24);

/// `TRANSCONF_GAMMA_MODE_MASK_ILK` (intel_display_regs.h)
pub const TRANSCONF_GAMMA_MODE_MASK_ILK: u32 = reg_genmask(25, 24);

/// `TRANSCONF_GAMMA_MODE_12BIT` (intel_display_regs.h)
pub const TRANSCONF_GAMMA_MODE_12BIT: u32 = reg_field_prep(TRANSCONF_GAMMA_MODE_MASK_ILK, 2);

/// `TRANSCONF_GAMMA_MODE_SPLIT` (intel_display_regs.h)
pub const TRANSCONF_GAMMA_MODE_SPLIT: u32 = reg_field_prep(TRANSCONF_GAMMA_MODE_MASK_ILK, 3);

/// `TRANSCONF_GAMMA_MODE(x)` (intel_display_regs.h)
pub const fn TRANSCONF_GAMMA_MODE(x: u32) -> u32 {
    reg_field_prep(TRANSCONF_GAMMA_MODE_MASK_ILK, (x))
}

/// `TRANSCONF_INTERLACE_MASK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_MASK: u32 = reg_genmask(23, 21);

/// `TRANSCONF_INTERLACE_PROGRESSIVE` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_PROGRESSIVE: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK, 0);

/// `TRANSCONF_INTERLACE_W_SYNC_SHIFT_PANEL` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_W_SYNC_SHIFT_PANEL: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK, 4);

/// `TRANSCONF_INTERLACE_W_SYNC_SHIFT` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_W_SYNC_SHIFT: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK, 5);

/// `TRANSCONF_INTERLACE_W_FIELD_INDICATION` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_W_FIELD_INDICATION: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK, 6);

/// `TRANSCONF_INTERLACE_FIELD_0_ONLY` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_FIELD_0_ONLY: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK, 7);

/// `TRANSCONF_INTERLACE_MASK_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_MASK_ILK: u32 = reg_genmask(23, 21);

/// `TRANSCONF_INTERLACE_MASK_HSW` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_MASK_HSW: u32 = reg_genmask(22, 21);

/// `TRANSCONF_INTERLACE_PF_PD_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_PF_PD_ILK: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK_ILK, 0);

/// `TRANSCONF_INTERLACE_PF_ID_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_PF_ID_ILK: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK_ILK, 1);

/// `TRANSCONF_INTERLACE_IF_ID_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_IF_ID_ILK: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK_ILK, 3);

/// `TRANSCONF_INTERLACE_IF_ID_DBL_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_IF_ID_DBL_ILK: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK_ILK, 4);

/// `TRANSCONF_INTERLACE_PF_ID_DBL_ILK` (intel_display_regs.h)
pub const TRANSCONF_INTERLACE_PF_ID_DBL_ILK: u32 = reg_field_prep(TRANSCONF_INTERLACE_MASK_ILK, 5);

/// `TRANSCONF_REFRESH_RATE_ALT_ILK` (intel_display_regs.h)
pub const TRANSCONF_REFRESH_RATE_ALT_ILK: u32 = reg_bit(20);

/// `TRANSCONF_MSA_TIMING_DELAY_MASK` (intel_display_regs.h)
pub const TRANSCONF_MSA_TIMING_DELAY_MASK: u32 = reg_genmask(19, 18);

/// `TRANSCONF_MSA_TIMING_DELAY(x)` (intel_display_regs.h)
pub const fn TRANSCONF_MSA_TIMING_DELAY(x: u32) -> u32 {
    reg_field_prep(TRANSCONF_MSA_TIMING_DELAY_MASK, (x))
}

/// `TRANSCONF_CXSR_DOWNCLOCK` (intel_display_regs.h)
pub const TRANSCONF_CXSR_DOWNCLOCK: u32 = reg_bit(16);

/// `TRANSCONF_WGC_ENABLE` (intel_display_regs.h)
pub const TRANSCONF_WGC_ENABLE: u32 = reg_bit(15);

/// `TRANSCONF_REFRESH_RATE_ALT_VLV` (intel_display_regs.h)
pub const TRANSCONF_REFRESH_RATE_ALT_VLV: u32 = reg_bit(14);

/// `TRANSCONF_COLOR_RANGE_SELECT` (intel_display_regs.h)
pub const TRANSCONF_COLOR_RANGE_SELECT: u32 = reg_bit(13);

/// `TRANSCONF_OUTPUT_COLORSPACE_MASK` (intel_display_regs.h)
pub const TRANSCONF_OUTPUT_COLORSPACE_MASK: u32 = reg_genmask(12, 11);

/// `TRANSCONF_OUTPUT_COLORSPACE_RGB` (intel_display_regs.h)
pub const TRANSCONF_OUTPUT_COLORSPACE_RGB: u32 = reg_field_prep(TRANSCONF_OUTPUT_COLORSPACE_MASK, 0);

/// `TRANSCONF_OUTPUT_COLORSPACE_YUV601` (intel_display_regs.h)
pub const TRANSCONF_OUTPUT_COLORSPACE_YUV601: u32 = reg_field_prep(TRANSCONF_OUTPUT_COLORSPACE_MASK, 1);

/// `TRANSCONF_OUTPUT_COLORSPACE_YUV709` (intel_display_regs.h)
pub const TRANSCONF_OUTPUT_COLORSPACE_YUV709: u32 = reg_field_prep(TRANSCONF_OUTPUT_COLORSPACE_MASK, 2);

/// `TRANSCONF_OUTPUT_COLORSPACE_YUV_HSW` (intel_display_regs.h)
pub const TRANSCONF_OUTPUT_COLORSPACE_YUV_HSW: u32 = reg_bit(11);

/// `TRANSCONF_BPC_MASK` (intel_display_regs.h)
pub const TRANSCONF_BPC_MASK: u32 = reg_genmask(7, 5);

/// `TRANSCONF_BPC_8` (intel_display_regs.h)
pub const TRANSCONF_BPC_8: u32 = reg_field_prep(TRANSCONF_BPC_MASK, 0);

/// `TRANSCONF_BPC_10` (intel_display_regs.h)
pub const TRANSCONF_BPC_10: u32 = reg_field_prep(TRANSCONF_BPC_MASK, 1);

/// `TRANSCONF_BPC_6` (intel_display_regs.h)
pub const TRANSCONF_BPC_6: u32 = reg_field_prep(TRANSCONF_BPC_MASK, 2);

/// `TRANSCONF_BPC_12` (intel_display_regs.h)
pub const TRANSCONF_BPC_12: u32 = reg_field_prep(TRANSCONF_BPC_MASK, 3);

/// `TRANSCONF_DITHER_EN` (intel_display_regs.h)
pub const TRANSCONF_DITHER_EN: u32 = reg_bit(4);

/// `TRANSCONF_DITHER_TYPE_MASK` (intel_display_regs.h)
pub const TRANSCONF_DITHER_TYPE_MASK: u32 = reg_genmask(3, 2);

/// `TRANSCONF_DITHER_TYPE_SP` (intel_display_regs.h)
pub const TRANSCONF_DITHER_TYPE_SP: u32 = reg_field_prep(TRANSCONF_DITHER_TYPE_MASK, 0);

/// `TRANSCONF_DITHER_TYPE_ST1` (intel_display_regs.h)
pub const TRANSCONF_DITHER_TYPE_ST1: u32 = reg_field_prep(TRANSCONF_DITHER_TYPE_MASK, 1);

/// `TRANSCONF_DITHER_TYPE_ST2` (intel_display_regs.h)
pub const TRANSCONF_DITHER_TYPE_ST2: u32 = reg_field_prep(TRANSCONF_DITHER_TYPE_MASK, 2);

/// `TRANSCONF_DITHER_TYPE_TEMP` (intel_display_regs.h)
pub const TRANSCONF_DITHER_TYPE_TEMP: u32 = reg_field_prep(TRANSCONF_DITHER_TYPE_MASK, 3);

/// `TRANSCONF_PIXEL_COUNT_SCALING_MASK` (intel_display_regs.h)
pub const TRANSCONF_PIXEL_COUNT_SCALING_MASK: u32 = reg_genmask(1, 0);

/// `TRANSCONF_PIXEL_COUNT_SCALING_X4` (intel_display_regs.h)
pub const TRANSCONF_PIXEL_COUNT_SCALING_X4: u32 = 1;

/// `_PIPEASTAT` (intel_display_regs.h)
pub const _PIPEASTAT: u32 = 0x70024;

/// `PIPE_FIFO_UNDERRUN_STATUS` (intel_display_regs.h)
pub const PIPE_FIFO_UNDERRUN_STATUS: u32 = ((1 << 31));

/// `SPRITE1_FLIP_DONE_INT_EN_VLV` (intel_display_regs.h)
pub const SPRITE1_FLIP_DONE_INT_EN_VLV: u32 = ((1 << 30));

/// `PIPE_CRC_ERROR_ENABLE` (intel_display_regs.h)
pub const PIPE_CRC_ERROR_ENABLE: u32 = ((1 << 29));

/// `PIPE_CRC_DONE_ENABLE` (intel_display_regs.h)
pub const PIPE_CRC_DONE_ENABLE: u32 = ((1 << 28));

/// `PERF_COUNTER2_INTERRUPT_EN` (intel_display_regs.h)
pub const PERF_COUNTER2_INTERRUPT_EN: u32 = ((1 << 27));

/// `PIPE_GMBUS_EVENT_ENABLE` (intel_display_regs.h)
pub const PIPE_GMBUS_EVENT_ENABLE: u32 = ((1 << 27));

/// `PLANE_FLIP_DONE_INT_EN_VLV` (intel_display_regs.h)
pub const PLANE_FLIP_DONE_INT_EN_VLV: u32 = ((1 << 26));

/// `PIPE_HOTPLUG_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_HOTPLUG_INTERRUPT_ENABLE: u32 = ((1 << 26));

/// `PIPE_VSYNC_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_VSYNC_INTERRUPT_ENABLE: u32 = ((1 << 25));

/// `PIPE_DISPLAY_LINE_COMPARE_ENABLE` (intel_display_regs.h)
pub const PIPE_DISPLAY_LINE_COMPARE_ENABLE: u32 = ((1 << 24));

/// `PIPE_DPST_EVENT_ENABLE` (intel_display_regs.h)
pub const PIPE_DPST_EVENT_ENABLE: u32 = ((1 << 23));

/// `SPRITE0_FLIP_DONE_INT_EN_VLV` (intel_display_regs.h)
pub const SPRITE0_FLIP_DONE_INT_EN_VLV: u32 = ((1 << 22));

/// `PIPE_LEGACY_BLC_EVENT_ENABLE` (intel_display_regs.h)
pub const PIPE_LEGACY_BLC_EVENT_ENABLE: u32 = ((1 << 22));

/// `PIPE_ODD_FIELD_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_ODD_FIELD_INTERRUPT_ENABLE: u32 = ((1 << 21));

/// `PIPE_EVEN_FIELD_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_EVEN_FIELD_INTERRUPT_ENABLE: u32 = ((1 << 20));

/// `PIPE_B_PSR_INTERRUPT_ENABLE_VLV` (intel_display_regs.h)
pub const PIPE_B_PSR_INTERRUPT_ENABLE_VLV: u32 = ((1 << 19));

/// `PERF_COUNTER_INTERRUPT_EN` (intel_display_regs.h)
pub const PERF_COUNTER_INTERRUPT_EN: u32 = ((1 << 19));

/// `PIPE_HOTPLUG_TV_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_HOTPLUG_TV_INTERRUPT_ENABLE: u32 = ((1 << 18));

/// `PIPE_START_VBLANK_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_START_VBLANK_INTERRUPT_ENABLE: u32 = ((1 << 18));

/// `PIPE_FRAMESTART_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_FRAMESTART_INTERRUPT_ENABLE: u32 = ((1 << 17));

/// `PIPE_VBLANK_INTERRUPT_ENABLE` (intel_display_regs.h)
pub const PIPE_VBLANK_INTERRUPT_ENABLE: u32 = ((1 << 17));

/// `PIPEA_HBLANK_INT_EN_VLV` (intel_display_regs.h)
pub const PIPEA_HBLANK_INT_EN_VLV: u32 = ((1 << 16));

/// `PIPE_OVERLAY_UPDATED_ENABLE` (intel_display_regs.h)
pub const PIPE_OVERLAY_UPDATED_ENABLE: u32 = ((1 << 16));

/// `SPRITE1_FLIP_DONE_INT_STATUS_VLV` (intel_display_regs.h)
pub const SPRITE1_FLIP_DONE_INT_STATUS_VLV: u32 = ((1 << 15));

/// `SPRITE0_FLIP_DONE_INT_STATUS_VLV` (intel_display_regs.h)
pub const SPRITE0_FLIP_DONE_INT_STATUS_VLV: u32 = ((1 << 14));

/// `PIPE_CRC_ERROR_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_CRC_ERROR_INTERRUPT_STATUS: u32 = ((1 << 13));

/// `PIPE_CRC_DONE_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_CRC_DONE_INTERRUPT_STATUS: u32 = ((1 << 12));

/// `PERF_COUNTER2_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PERF_COUNTER2_INTERRUPT_STATUS: u32 = ((1 << 11));

/// `PIPE_GMBUS_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_GMBUS_INTERRUPT_STATUS: u32 = ((1 << 11));

/// `PLANE_FLIP_DONE_INT_STATUS_VLV` (intel_display_regs.h)
pub const PLANE_FLIP_DONE_INT_STATUS_VLV: u32 = ((1 << 10));

/// `PIPE_HOTPLUG_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_HOTPLUG_INTERRUPT_STATUS: u32 = ((1 << 10));

/// `PIPE_VSYNC_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_VSYNC_INTERRUPT_STATUS: u32 = ((1 << 9));

/// `PIPE_DISPLAY_LINE_COMPARE_STATUS` (intel_display_regs.h)
pub const PIPE_DISPLAY_LINE_COMPARE_STATUS: u32 = ((1 << 8));

/// `PIPE_DPST_EVENT_STATUS` (intel_display_regs.h)
pub const PIPE_DPST_EVENT_STATUS: u32 = ((1 << 7));

/// `PIPE_A_PSR_STATUS_VLV` (intel_display_regs.h)
pub const PIPE_A_PSR_STATUS_VLV: u32 = ((1 << 6));

/// `PIPE_LEGACY_BLC_EVENT_STATUS` (intel_display_regs.h)
pub const PIPE_LEGACY_BLC_EVENT_STATUS: u32 = ((1 << 6));

/// `PIPE_ODD_FIELD_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_ODD_FIELD_INTERRUPT_STATUS: u32 = ((1 << 5));

/// `PIPE_EVEN_FIELD_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_EVEN_FIELD_INTERRUPT_STATUS: u32 = ((1 << 4));

/// `PIPE_B_PSR_STATUS_VLV` (intel_display_regs.h)
pub const PIPE_B_PSR_STATUS_VLV: u32 = ((1 << 3));

/// `PERF_COUNTER_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PERF_COUNTER_INTERRUPT_STATUS: u32 = ((1 << 3));

/// `PIPE_HOTPLUG_TV_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_HOTPLUG_TV_INTERRUPT_STATUS: u32 = ((1 << 2));

/// `PIPE_START_VBLANK_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_START_VBLANK_INTERRUPT_STATUS: u32 = ((1 << 2));

/// `PIPE_FRAMESTART_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_FRAMESTART_INTERRUPT_STATUS: u32 = ((1 << 1));

/// `PIPE_VBLANK_INTERRUPT_STATUS` (intel_display_regs.h)
pub const PIPE_VBLANK_INTERRUPT_STATUS: u32 = ((1 << 1));

/// `PIPE_HBLANK_INT_STATUS` (intel_display_regs.h)
pub const PIPE_HBLANK_INT_STATUS: u32 = ((1 << 0));

/// `PIPE_OVERLAY_UPDATED_STATUS` (intel_display_regs.h)
pub const PIPE_OVERLAY_UPDATED_STATUS: u32 = ((1 << 0));

/// `PIPESTAT_INT_ENABLE_MASK` (intel_display_regs.h)
pub const PIPESTAT_INT_ENABLE_MASK: u32 = 0x7fff0000;

/// `PIPESTAT_INT_STATUS_MASK` (intel_display_regs.h)
pub const PIPESTAT_INT_STATUS_MASK: u32 = 0x0000ffff;

/// `_PIPE_ARB_CTL_A` (intel_display_regs.h)
pub const _PIPE_ARB_CTL_A: u32 = 0x70028;

/// `PIPE_ARB_USE_PROG_SLOTS` (intel_display_regs.h)
pub const PIPE_ARB_USE_PROG_SLOTS: u32 = reg_bit(13);

/// `_PIPE_MISC_A` (intel_display_regs.h)
pub const _PIPE_MISC_A: u32 = 0x70030;

/// `_PIPE_MISC_B` (intel_display_regs.h)
pub const _PIPE_MISC_B: u32 = 0x71030;

/// `PIPE_MISC(pipe)` (intel_display_regs.h)
pub const fn PIPE_MISC(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_MISC_A, _PIPE_MISC_B)
}

/// `PIPE_MISC_YUV420_ENABLE` (intel_display_regs.h)
pub const PIPE_MISC_YUV420_ENABLE: u32 = reg_bit(27);

/// `PIPE_MISC_YUV420_MODE_FULL_BLEND` (intel_display_regs.h)
pub const PIPE_MISC_YUV420_MODE_FULL_BLEND: u32 = reg_bit(26);

/// `PIPE_MISC_HDR_MODE_PRECISION` (intel_display_regs.h)
pub const PIPE_MISC_HDR_MODE_PRECISION: u32 = reg_bit(23);

/// `PIPE_MISC_PSR_MASK_PRIMARY_FLIP` (intel_display_regs.h)
pub const PIPE_MISC_PSR_MASK_PRIMARY_FLIP: u32 = reg_bit(23);

/// `PIPE_MISC_PSR_MASK_SPRITE_ENABLE` (intel_display_regs.h)
pub const PIPE_MISC_PSR_MASK_SPRITE_ENABLE: u32 = reg_bit(22);

/// `PIPE_MISC_PSR_MASK_PIPE_REG_WRITE` (intel_display_regs.h)
pub const PIPE_MISC_PSR_MASK_PIPE_REG_WRITE: u32 = reg_bit(21);

/// `PIPE_MISC_PSR_MASK_CURSOR_MOVE` (intel_display_regs.h)
pub const PIPE_MISC_PSR_MASK_CURSOR_MOVE: u32 = reg_bit(21);

/// `PIPE_MISC_PSR_MASK_VBLANK_VSYNC_INT` (intel_display_regs.h)
pub const PIPE_MISC_PSR_MASK_VBLANK_VSYNC_INT: u32 = reg_bit(20);

/// `PIPE_MISC_OUTPUT_COLORSPACE_YUV` (intel_display_regs.h)
pub const PIPE_MISC_OUTPUT_COLORSPACE_YUV: u32 = reg_bit(11);

/// `PIPE_MISC_PIXEL_ROUNDING_TRUNC` (intel_display_regs.h)
pub const PIPE_MISC_PIXEL_ROUNDING_TRUNC: u32 = reg_bit(8);

/// `PIPE_MISC_BPC_MASK` (intel_display_regs.h)
pub const PIPE_MISC_BPC_MASK: u32 = reg_genmask(7, 5);

/// `PIPE_MISC_BPC_8` (intel_display_regs.h)
pub const PIPE_MISC_BPC_8: u32 = reg_field_prep(PIPE_MISC_BPC_MASK, 0);

/// `PIPE_MISC_BPC_10` (intel_display_regs.h)
pub const PIPE_MISC_BPC_10: u32 = reg_field_prep(PIPE_MISC_BPC_MASK, 1);

/// `PIPE_MISC_BPC_6` (intel_display_regs.h)
pub const PIPE_MISC_BPC_6: u32 = reg_field_prep(PIPE_MISC_BPC_MASK, 2);

/// `PIPE_MISC_BPC_12_ADLP` (intel_display_regs.h)
pub const PIPE_MISC_BPC_12_ADLP: u32 = reg_field_prep(PIPE_MISC_BPC_MASK, 4);

/// `PIPE_MISC_DITHER_ENABLE` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_ENABLE: u32 = reg_bit(4);

/// `PIPE_MISC_DITHER_TYPE_MASK` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_TYPE_MASK: u32 = reg_genmask(3, 2);

/// `PIPE_MISC_DITHER_TYPE_SP` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_TYPE_SP: u32 = reg_field_prep(PIPE_MISC_DITHER_TYPE_MASK, 0);

/// `PIPE_MISC_DITHER_TYPE_ST1` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_TYPE_ST1: u32 = reg_field_prep(PIPE_MISC_DITHER_TYPE_MASK, 1);

/// `PIPE_MISC_DITHER_TYPE_ST2` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_TYPE_ST2: u32 = reg_field_prep(PIPE_MISC_DITHER_TYPE_MASK, 2);

/// `PIPE_MISC_DITHER_TYPE_TEMP` (intel_display_regs.h)
pub const PIPE_MISC_DITHER_TYPE_TEMP: u32 = reg_field_prep(PIPE_MISC_DITHER_TYPE_MASK, 3);

/// `_PIPE_MISC2_A` (intel_display_regs.h)
pub const _PIPE_MISC2_A: u32 = 0x7002C;

/// `_PIPE_MISC2_B` (intel_display_regs.h)
pub const _PIPE_MISC2_B: u32 = 0x7102C;

/// `PIPE_MISC2(pipe)` (intel_display_regs.h)
pub const fn PIPE_MISC2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_MISC2_A, _PIPE_MISC2_B)
}

/// `PIPE_MISC2_BUBBLE_COUNTER_MASK` (intel_display_regs.h)
pub const PIPE_MISC2_BUBBLE_COUNTER_MASK: u32 = reg_genmask(31, 24);

/// `PIPE_MISC2_BUBBLE_COUNTER_SCALER_EN` (intel_display_regs.h)
pub const PIPE_MISC2_BUBBLE_COUNTER_SCALER_EN: u32 = reg_field_prep(PIPE_MISC2_BUBBLE_COUNTER_MASK, 80);

/// `PIPE_MISC2_BUBBLE_COUNTER_SCALER_DIS` (intel_display_regs.h)
pub const PIPE_MISC2_BUBBLE_COUNTER_SCALER_DIS: u32 = reg_field_prep(PIPE_MISC2_BUBBLE_COUNTER_MASK, 20);

/// `PIPE_MISC2_FLIP_INFO_PLANE_SEL_MASK` (intel_display_regs.h)
pub const PIPE_MISC2_FLIP_INFO_PLANE_SEL_MASK: u32 = reg_genmask(2, 0);

/// `PIPE_MISC2_FLIP_INFO_PLANE_SEL(plane_id)` (intel_display_regs.h)
pub const fn PIPE_MISC2_FLIP_INFO_PLANE_SEL(plane_id: u32) -> u32 {
    reg_field_prep(PIPE_MISC2_FLIP_INFO_PLANE_SEL_MASK, (plane_id))
}

/// `_UNDERRUN_DBG1_A` (intel_display_regs.h)
pub const _UNDERRUN_DBG1_A: u32 = 0x70064;

/// `_UNDERRUN_DBG1_B` (intel_display_regs.h)
pub const _UNDERRUN_DBG1_B: u32 = 0x71064;

/// `UNDERRUN_DBG1(pipe)` (intel_display_regs.h)
pub const fn UNDERRUN_DBG1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _UNDERRUN_DBG1_A, _UNDERRUN_DBG1_B)
}

/// `UNDERRUN_DBUF_BLOCK_NOT_VALID_MASK` (intel_display_regs.h)
pub const UNDERRUN_DBUF_BLOCK_NOT_VALID_MASK: u32 = reg_genmask(29, 24);

/// `UNDERRUN_DDB_EMPTY_MASK` (intel_display_regs.h)
pub const UNDERRUN_DDB_EMPTY_MASK: u32 = reg_genmask(21, 16);

/// `UNDERRUN_DBUF_NOT_FILLED_MASK` (intel_display_regs.h)
pub const UNDERRUN_DBUF_NOT_FILLED_MASK: u32 = reg_genmask(13, 8);

/// `UNDERRUN_BELOW_WM0_MASK` (intel_display_regs.h)
pub const UNDERRUN_BELOW_WM0_MASK: u32 = reg_genmask(5, 0);

/// `_UNDERRUN_DBG2_A` (intel_display_regs.h)
pub const _UNDERRUN_DBG2_A: u32 = 0x70068;

/// `_UNDERRUN_DBG2_B` (intel_display_regs.h)
pub const _UNDERRUN_DBG2_B: u32 = 0x71068;

/// `UNDERRUN_DBG2(pipe)` (intel_display_regs.h)
pub const fn UNDERRUN_DBG2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _UNDERRUN_DBG2_A, _UNDERRUN_DBG2_B)
}

/// `UNDERRUN_FRAME_LINE_COUNTERS_FROZEN` (intel_display_regs.h)
pub const UNDERRUN_FRAME_LINE_COUNTERS_FROZEN: u32 = reg_bit(31);

/// `UNDERRUN_PIPE_FRAME_COUNT_MASK` (intel_display_regs.h)
pub const UNDERRUN_PIPE_FRAME_COUNT_MASK: u32 = reg_genmask(30, 20);

/// `UNDERRUN_LINE_COUNT_MASK` (intel_display_regs.h)
pub const UNDERRUN_LINE_COUNT_MASK: u32 = reg_genmask(19, 0);

/// `DPINVGTT` (intel_display_regs.h)
pub const DPINVGTT: I915RegT = mmio((VLV_DISPLAY_BASE + 0x7002c));

/// `DPINVGTT_EN_MASK_CHV` (intel_display_regs.h)
pub const DPINVGTT_EN_MASK_CHV: u32 = reg_genmask(27, 16);

/// `DPINVGTT_EN_MASK_VLV` (intel_display_regs.h)
pub const DPINVGTT_EN_MASK_VLV: u32 = reg_genmask(23, 16);

/// `SPRITEF_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITEF_INVALID_GTT_INT_EN: u32 = reg_bit(27);

/// `SPRITEE_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITEE_INVALID_GTT_INT_EN: u32 = reg_bit(26);

/// `PLANEC_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const PLANEC_INVALID_GTT_INT_EN: u32 = reg_bit(25);

/// `CURSORC_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const CURSORC_INVALID_GTT_INT_EN: u32 = reg_bit(24);

/// `CURSORB_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const CURSORB_INVALID_GTT_INT_EN: u32 = reg_bit(23);

/// `CURSORA_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const CURSORA_INVALID_GTT_INT_EN: u32 = reg_bit(22);

/// `SPRITED_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITED_INVALID_GTT_INT_EN: u32 = reg_bit(21);

/// `SPRITEC_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITEC_INVALID_GTT_INT_EN: u32 = reg_bit(20);

/// `PLANEB_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const PLANEB_INVALID_GTT_INT_EN: u32 = reg_bit(19);

/// `SPRITEB_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITEB_INVALID_GTT_INT_EN: u32 = reg_bit(18);

/// `SPRITEA_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const SPRITEA_INVALID_GTT_INT_EN: u32 = reg_bit(17);

/// `PLANEA_INVALID_GTT_INT_EN` (intel_display_regs.h)
pub const PLANEA_INVALID_GTT_INT_EN: u32 = reg_bit(16);

/// `DPINVGTT_STATUS_MASK_CHV` (intel_display_regs.h)
pub const DPINVGTT_STATUS_MASK_CHV: u32 = reg_genmask(11, 0);

/// `DPINVGTT_STATUS_MASK_VLV` (intel_display_regs.h)
pub const DPINVGTT_STATUS_MASK_VLV: u32 = reg_genmask(7, 0);

/// `SPRITEF_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITEF_INVALID_GTT_STATUS: u32 = reg_bit(11);

/// `SPRITEE_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITEE_INVALID_GTT_STATUS: u32 = reg_bit(10);

/// `PLANEC_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const PLANEC_INVALID_GTT_STATUS: u32 = reg_bit(9);

/// `CURSORC_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const CURSORC_INVALID_GTT_STATUS: u32 = reg_bit(8);

/// `CURSORB_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const CURSORB_INVALID_GTT_STATUS: u32 = reg_bit(7);

/// `CURSORA_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const CURSORA_INVALID_GTT_STATUS: u32 = reg_bit(6);

/// `SPRITED_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITED_INVALID_GTT_STATUS: u32 = reg_bit(5);

/// `SPRITEC_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITEC_INVALID_GTT_STATUS: u32 = reg_bit(4);

/// `PLANEB_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const PLANEB_INVALID_GTT_STATUS: u32 = reg_bit(3);

/// `SPRITEB_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITEB_INVALID_GTT_STATUS: u32 = reg_bit(2);

/// `SPRITEA_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const SPRITEA_INVALID_GTT_STATUS: u32 = reg_bit(1);

/// `PLANEA_INVALID_GTT_STATUS` (intel_display_regs.h)
pub const PLANEA_INVALID_GTT_STATUS: u32 = reg_bit(0);

/// `CBR1_VLV` (intel_display_regs.h)
pub const CBR1_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x70400));

/// `CBR_PND_DEADLINE_DISABLE` (intel_display_regs.h)
pub const CBR_PND_DEADLINE_DISABLE: u32 = ((1 << 31));

/// `CBR_PWM_CLOCK_MUX_SELECT` (intel_display_regs.h)
pub const CBR_PWM_CLOCK_MUX_SELECT: u32 = ((1 << 30));

/// `CBR4_VLV` (intel_display_regs.h)
pub const CBR4_VLV: I915RegT = mmio((VLV_DISPLAY_BASE + 0x70450));

/// `CBR_DPLLBMD_PIPE(pipe)` (intel_display_regs.h)
pub const fn CBR_DPLLBMD_PIPE(pipe: u32) -> u32 {
    ((1 << ((7 + ((pipe) * 11)))))
}

/// `_PIPEAFRAMEHIGH` (intel_display_regs.h)
pub const _PIPEAFRAMEHIGH: u32 = 0x70040;

/// `PIPE_FRAME_HIGH_MASK` (intel_display_regs.h)
pub const PIPE_FRAME_HIGH_MASK: u32 = 0x0000ffff;

/// `PIPE_FRAME_HIGH_SHIFT` (intel_display_regs.h)
pub const PIPE_FRAME_HIGH_SHIFT: u32 = 0;

/// `_PIPEAFRAMEPIXEL` (intel_display_regs.h)
pub const _PIPEAFRAMEPIXEL: u32 = 0x70044;

/// `PIPE_FRAME_LOW_MASK` (intel_display_regs.h)
pub const PIPE_FRAME_LOW_MASK: u32 = 0xff000000;

/// `PIPE_FRAME_LOW_SHIFT` (intel_display_regs.h)
pub const PIPE_FRAME_LOW_SHIFT: u32 = 24;

/// `PIPE_PIXEL_MASK` (intel_display_regs.h)
pub const PIPE_PIXEL_MASK: u32 = 0x00ffffff;

/// `PIPE_PIXEL_SHIFT` (intel_display_regs.h)
pub const PIPE_PIXEL_SHIFT: u32 = 0;

/// `_PIPEA_FRMCOUNT_G4X` (intel_display_regs.h)
pub const _PIPEA_FRMCOUNT_G4X: u32 = 0x70040;

/// `_PIPEA_FLIPCOUNT_G4X` (intel_display_regs.h)
pub const _PIPEA_FLIPCOUNT_G4X: u32 = 0x70044;

/// `_CHV_BLEND_A` (intel_display_regs.h)
pub const _CHV_BLEND_A: u32 = 0x60a00;

/// `CHV_BLEND_MASK` (intel_display_regs.h)
pub const CHV_BLEND_MASK: u32 = reg_genmask(31, 30);

/// `CHV_BLEND_LEGACY` (intel_display_regs.h)
pub const CHV_BLEND_LEGACY: u32 = reg_field_prep(CHV_BLEND_MASK, 0);

/// `CHV_BLEND_ANDROID` (intel_display_regs.h)
pub const CHV_BLEND_ANDROID: u32 = reg_field_prep(CHV_BLEND_MASK, 1);

/// `CHV_BLEND_MPO` (intel_display_regs.h)
pub const CHV_BLEND_MPO: u32 = reg_field_prep(CHV_BLEND_MASK, 2);

/// `_CHV_CANVAS_A` (intel_display_regs.h)
pub const _CHV_CANVAS_A: u32 = 0x60a04;

/// `CHV_CANVAS_RED_MASK` (intel_display_regs.h)
pub const CHV_CANVAS_RED_MASK: u32 = reg_genmask(29, 20);

/// `CHV_CANVAS_GREEN_MASK` (intel_display_regs.h)
pub const CHV_CANVAS_GREEN_MASK: u32 = reg_genmask(19, 10);

/// `CHV_CANVAS_BLUE_MASK` (intel_display_regs.h)
pub const CHV_CANVAS_BLUE_MASK: u32 = reg_genmask(9, 0);

/// `DISP_BASEADDR_MASK` (intel_display_regs.h)
pub const DISP_BASEADDR_MASK: u32 = (0xfffff000);

/// `I915_LO_DISPBASE(val)` (intel_display_regs.h)
pub const fn I915_LO_DISPBASE(val: u32) -> u32 {
    (((val) & (!DISP_BASEADDR_MASK)))
}

/// `I915_HI_DISPBASE(val)` (intel_display_regs.h)
pub const fn I915_HI_DISPBASE(val: u32) -> u32 {
    (((val) & DISP_BASEADDR_MASK))
}

/// `SWF_ILK(i)` (intel_display_regs.h)
pub const fn SWF_ILK(i: u32) -> I915RegT {
    mmio((0x4F000 + ((i) * 4)))
}

/// `DEISR` (intel_display_regs.h)
pub const DEISR: I915RegT = mmio(0x44000);

/// `DEIMR` (intel_display_regs.h)
pub const DEIMR: I915RegT = mmio(0x44004);

/// `DEIIR` (intel_display_regs.h)
pub const DEIIR: I915RegT = mmio(0x44008);

/// `DEIER` (intel_display_regs.h)
pub const DEIER: I915RegT = mmio(0x4400c);

/// `DIGITAL_PORT_HOTPLUG_CNTRL` (intel_display_regs.h)
pub const DIGITAL_PORT_HOTPLUG_CNTRL: I915RegT = mmio(0x44030);

/// `DIGITAL_PORTA_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const DIGITAL_PORTA_HOTPLUG_ENABLE: u32 = ((1 << 4));

/// `DIGITAL_PORTA_PULSE_DURATION_2ms` (intel_display_regs.h)
pub const DIGITAL_PORTA_PULSE_DURATION_2ms: u32 = ((0 << 2));

/// `DIGITAL_PORTA_PULSE_DURATION_4_5ms` (intel_display_regs.h)
pub const DIGITAL_PORTA_PULSE_DURATION_4_5ms: u32 = ((1 << 2));

/// `DIGITAL_PORTA_PULSE_DURATION_6ms` (intel_display_regs.h)
pub const DIGITAL_PORTA_PULSE_DURATION_6ms: u32 = ((2 << 2));

/// `DIGITAL_PORTA_PULSE_DURATION_100ms` (intel_display_regs.h)
pub const DIGITAL_PORTA_PULSE_DURATION_100ms: u32 = ((3 << 2));

/// `DIGITAL_PORTA_PULSE_DURATION_MASK` (intel_display_regs.h)
pub const DIGITAL_PORTA_PULSE_DURATION_MASK: u32 = ((3 << 2));

/// `DIGITAL_PORTA_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const DIGITAL_PORTA_HOTPLUG_STATUS_MASK: u32 = ((3 << 0));

/// `DIGITAL_PORTA_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const DIGITAL_PORTA_HOTPLUG_NO_DETECT: u32 = ((0 << 0));

/// `DIGITAL_PORTA_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const DIGITAL_PORTA_HOTPLUG_SHORT_DETECT: u32 = ((1 << 0));

/// `DIGITAL_PORTA_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const DIGITAL_PORTA_HOTPLUG_LONG_DETECT: u32 = ((2 << 0));

/// `RR_HW_CTL` (intel_display_regs.h)
pub const RR_HW_CTL: I915RegT = mmio(0x45300);

/// `RR_HW_LOW_POWER_FRAMES_MASK` (intel_display_regs.h)
pub const RR_HW_LOW_POWER_FRAMES_MASK: u32 = 0xff;

/// `RR_HW_HIGH_POWER_FRAMES_MASK` (intel_display_regs.h)
pub const RR_HW_HIGH_POWER_FRAMES_MASK: u32 = 0xff00;

/// `_PIPEA_DATA_M1` (intel_display_regs.h)
pub const _PIPEA_DATA_M1: u32 = 0x60030;

/// `_PIPEB_DATA_M1` (intel_display_regs.h)
pub const _PIPEB_DATA_M1: u32 = 0x61030;

/// `_PIPEA_DATA_N1` (intel_display_regs.h)
pub const _PIPEA_DATA_N1: u32 = 0x60034;

/// `_PIPEB_DATA_N1` (intel_display_regs.h)
pub const _PIPEB_DATA_N1: u32 = 0x61034;

/// `_PIPEA_DATA_M2` (intel_display_regs.h)
pub const _PIPEA_DATA_M2: u32 = 0x60038;

/// `_PIPEB_DATA_M2` (intel_display_regs.h)
pub const _PIPEB_DATA_M2: u32 = 0x61038;

/// `_PIPEA_DATA_N2` (intel_display_regs.h)
pub const _PIPEA_DATA_N2: u32 = 0x6003c;

/// `_PIPEB_DATA_N2` (intel_display_regs.h)
pub const _PIPEB_DATA_N2: u32 = 0x6103c;

/// `_PIPEA_LINK_M1` (intel_display_regs.h)
pub const _PIPEA_LINK_M1: u32 = 0x60040;

/// `_PIPEB_LINK_M1` (intel_display_regs.h)
pub const _PIPEB_LINK_M1: u32 = 0x61040;

/// `_PIPEA_LINK_N1` (intel_display_regs.h)
pub const _PIPEA_LINK_N1: u32 = 0x60044;

/// `_PIPEB_LINK_N1` (intel_display_regs.h)
pub const _PIPEB_LINK_N1: u32 = 0x61044;

/// `_PIPEA_LINK_M2` (intel_display_regs.h)
pub const _PIPEA_LINK_M2: u32 = 0x60048;

/// `_PIPEB_LINK_M2` (intel_display_regs.h)
pub const _PIPEB_LINK_M2: u32 = 0x61048;

/// `_PIPEA_LINK_N2` (intel_display_regs.h)
pub const _PIPEA_LINK_N2: u32 = 0x6004c;

/// `_PIPEB_LINK_N2` (intel_display_regs.h)
pub const _PIPEB_LINK_N2: u32 = 0x6104c;

/// `_ID(id, a, b)` (intel_display_regs.h)
pub const fn _ID(id: u32, a: u32, b: u32) -> u32 {
    pick_even(id, a, b)
}

/// `_PS_1A_CTRL` (intel_display_regs.h)
pub const _PS_1A_CTRL: u32 = 0x68180;

/// `_PS_2A_CTRL` (intel_display_regs.h)
pub const _PS_2A_CTRL: u32 = 0x68280;

/// `_PS_1B_CTRL` (intel_display_regs.h)
pub const _PS_1B_CTRL: u32 = 0x68980;

/// `_PS_2B_CTRL` (intel_display_regs.h)
pub const _PS_2B_CTRL: u32 = 0x68A80;

/// `_PS_1C_CTRL` (intel_display_regs.h)
pub const _PS_1C_CTRL: u32 = 0x69180;

/// `SKL_PS_CTRL(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_CTRL(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_1A_CTRL, _PS_2A_CTRL), _ID(id, _PS_1B_CTRL, _PS_2B_CTRL))
}

/// `PS_SCALER_EN` (intel_display_regs.h)
pub const PS_SCALER_EN: u32 = reg_bit(31);

/// `PS_SCALER_TYPE_MASK` (intel_display_regs.h)
pub const PS_SCALER_TYPE_MASK: u32 = reg_bit(30);

/// `PS_SCALER_TYPE_NON_LINEAR` (intel_display_regs.h)
pub const PS_SCALER_TYPE_NON_LINEAR: u32 = reg_field_prep(PS_SCALER_TYPE_MASK, 0);

/// `PS_SCALER_TYPE_LINEAR` (intel_display_regs.h)
pub const PS_SCALER_TYPE_LINEAR: u32 = reg_field_prep(PS_SCALER_TYPE_MASK, 1);

/// `SKL_PS_SCALER_MODE_MASK` (intel_display_regs.h)
pub const SKL_PS_SCALER_MODE_MASK: u32 = reg_genmask(29, 28);

/// `SKL_PS_SCALER_MODE_DYN` (intel_display_regs.h)
pub const SKL_PS_SCALER_MODE_DYN: u32 = reg_field_prep(SKL_PS_SCALER_MODE_MASK, 0);

/// `SKL_PS_SCALER_MODE_HQ` (intel_display_regs.h)
pub const SKL_PS_SCALER_MODE_HQ: u32 = reg_field_prep(SKL_PS_SCALER_MODE_MASK, 1);

/// `SKL_PS_SCALER_MODE_NV12` (intel_display_regs.h)
pub const SKL_PS_SCALER_MODE_NV12: u32 = reg_field_prep(SKL_PS_SCALER_MODE_MASK, 2);

/// `PS_SCALER_MODE_MASK` (intel_display_regs.h)
pub const PS_SCALER_MODE_MASK: u32 = reg_bit(29);

/// `PS_SCALER_MODE_NORMAL` (intel_display_regs.h)
pub const PS_SCALER_MODE_NORMAL: u32 = reg_field_prep(PS_SCALER_MODE_MASK, 0);

/// `PS_SCALER_MODE_PLANAR` (intel_display_regs.h)
pub const PS_SCALER_MODE_PLANAR: u32 = reg_field_prep(PS_SCALER_MODE_MASK, 1);

/// `PS_ADAPTIVE_FILTERING_EN` (intel_display_regs.h)
pub const PS_ADAPTIVE_FILTERING_EN: u32 = reg_bit(28);

/// `PS_BINDING_MASK` (intel_display_regs.h)
pub const PS_BINDING_MASK: u32 = reg_genmask(27, 25);

/// `PS_BINDING_PIPE` (intel_display_regs.h)
pub const PS_BINDING_PIPE: u32 = reg_field_prep(PS_BINDING_MASK, 0);

/// `PS_BINDING_PLANE(plane_id)` (intel_display_regs.h)
pub const fn PS_BINDING_PLANE(plane_id: u32) -> u32 {
    reg_field_prep(PS_BINDING_MASK, ((plane_id) + 1))
}

/// `PS_FILTER_MASK` (intel_display_regs.h)
pub const PS_FILTER_MASK: u32 = reg_genmask(24, 23);

/// `PS_FILTER_MEDIUM` (intel_display_regs.h)
pub const PS_FILTER_MEDIUM: u32 = reg_field_prep(PS_FILTER_MASK, 0);

/// `PS_FILTER_PROGRAMMED` (intel_display_regs.h)
pub const PS_FILTER_PROGRAMMED: u32 = reg_field_prep(PS_FILTER_MASK, 1);

/// `PS_FILTER_EDGE_ENHANCE` (intel_display_regs.h)
pub const PS_FILTER_EDGE_ENHANCE: u32 = reg_field_prep(PS_FILTER_MASK, 2);

/// `PS_FILTER_BILINEAR` (intel_display_regs.h)
pub const PS_FILTER_BILINEAR: u32 = reg_field_prep(PS_FILTER_MASK, 3);

/// `PS_ADAPTIVE_FILTER_MASK` (intel_display_regs.h)
pub const PS_ADAPTIVE_FILTER_MASK: u32 = reg_bit(22);

/// `PS_ADAPTIVE_FILTER_MEDIUM` (intel_display_regs.h)
pub const PS_ADAPTIVE_FILTER_MEDIUM: u32 = reg_field_prep(PS_ADAPTIVE_FILTER_MASK, 0);

/// `PS_ADAPTIVE_FILTER_EDGE_ENHANCE` (intel_display_regs.h)
pub const PS_ADAPTIVE_FILTER_EDGE_ENHANCE: u32 = reg_field_prep(PS_ADAPTIVE_FILTER_MASK, 1);

/// `PS_PIPE_SCALER_LOC_MASK` (intel_display_regs.h)
pub const PS_PIPE_SCALER_LOC_MASK: u32 = reg_bit(21);

/// `PS_VERT3TAP` (intel_display_regs.h)
pub const PS_VERT3TAP: u32 = reg_bit(21);

/// `PS_VERT_INT_INVERT_FIELD` (intel_display_regs.h)
pub const PS_VERT_INT_INVERT_FIELD: u32 = reg_bit(20);

/// `PS_PROG_SCALE_FACTOR` (intel_display_regs.h)
pub const PS_PROG_SCALE_FACTOR: u32 = reg_bit(19);

/// `PS_PWRUP_PROGRESS` (intel_display_regs.h)
pub const PS_PWRUP_PROGRESS: u32 = reg_bit(17);

/// `PS_V_FILTER_BYPASS` (intel_display_regs.h)
pub const PS_V_FILTER_BYPASS: u32 = reg_bit(8);

/// `PS_VADAPT_EN` (intel_display_regs.h)
pub const PS_VADAPT_EN: u32 = reg_bit(7);

/// `PS_VADAPT_MODE_MASK` (intel_display_regs.h)
pub const PS_VADAPT_MODE_MASK: u32 = reg_genmask(6, 5);

/// `PS_VADAPT_MODE_LEAST_ADAPT` (intel_display_regs.h)
pub const PS_VADAPT_MODE_LEAST_ADAPT: u32 = reg_field_prep(PS_VADAPT_MODE_MASK, 0);

/// `PS_VADAPT_MODE_MOD_ADAPT` (intel_display_regs.h)
pub const PS_VADAPT_MODE_MOD_ADAPT: u32 = reg_field_prep(PS_VADAPT_MODE_MASK, 1);

/// `PS_VADAPT_MODE_MOST_ADAPT` (intel_display_regs.h)
pub const PS_VADAPT_MODE_MOST_ADAPT: u32 = reg_field_prep(PS_VADAPT_MODE_MASK, 3);

/// `PS_BINDING_Y_MASK` (intel_display_regs.h)
pub const PS_BINDING_Y_MASK: u32 = reg_genmask(7, 5);

/// `PS_BINDING_Y_PLANE(plane_id)` (intel_display_regs.h)
pub const fn PS_BINDING_Y_PLANE(plane_id: u32) -> u32 {
    reg_field_prep(PS_BINDING_Y_MASK, ((plane_id) + 1))
}

/// `PS_Y_VERT_FILTER_SELECT_MASK` (intel_display_regs.h)
pub const PS_Y_VERT_FILTER_SELECT_MASK: u32 = reg_bit(4);

/// `PS_Y_VERT_FILTER_SELECT(set)` (intel_display_regs.h)
pub const fn PS_Y_VERT_FILTER_SELECT(set: u32) -> u32 {
    reg_field_prep(PS_Y_VERT_FILTER_SELECT_MASK, (set))
}

/// `PS_Y_HORZ_FILTER_SELECT_MASK` (intel_display_regs.h)
pub const PS_Y_HORZ_FILTER_SELECT_MASK: u32 = reg_bit(3);

/// `PS_Y_HORZ_FILTER_SELECT(set)` (intel_display_regs.h)
pub const fn PS_Y_HORZ_FILTER_SELECT(set: u32) -> u32 {
    reg_field_prep(PS_Y_HORZ_FILTER_SELECT_MASK, (set))
}

/// `PS_UV_VERT_FILTER_SELECT_MASK` (intel_display_regs.h)
pub const PS_UV_VERT_FILTER_SELECT_MASK: u32 = reg_bit(2);

/// `PS_UV_VERT_FILTER_SELECT(set)` (intel_display_regs.h)
pub const fn PS_UV_VERT_FILTER_SELECT(set: u32) -> u32 {
    reg_field_prep(PS_UV_VERT_FILTER_SELECT_MASK, (set))
}

/// `PS_UV_HORZ_FILTER_SELECT_MASK` (intel_display_regs.h)
pub const PS_UV_HORZ_FILTER_SELECT_MASK: u32 = reg_bit(1);

/// `PS_UV_HORZ_FILTER_SELECT(set)` (intel_display_regs.h)
pub const fn PS_UV_HORZ_FILTER_SELECT(set: u32) -> u32 {
    reg_field_prep(PS_UV_HORZ_FILTER_SELECT_MASK, (set))
}

/// `_PS_PWR_GATE_1A` (intel_display_regs.h)
pub const _PS_PWR_GATE_1A: u32 = 0x68160;

/// `_PS_PWR_GATE_2A` (intel_display_regs.h)
pub const _PS_PWR_GATE_2A: u32 = 0x68260;

/// `_PS_PWR_GATE_1B` (intel_display_regs.h)
pub const _PS_PWR_GATE_1B: u32 = 0x68960;

/// `_PS_PWR_GATE_2B` (intel_display_regs.h)
pub const _PS_PWR_GATE_2B: u32 = 0x68A60;

/// `_PS_PWR_GATE_1C` (intel_display_regs.h)
pub const _PS_PWR_GATE_1C: u32 = 0x69160;

/// `SKL_PS_PWR_GATE(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_PWR_GATE(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_PWR_GATE_1A, _PS_PWR_GATE_2A), _ID(id, _PS_PWR_GATE_1B, _PS_PWR_GATE_2B))
}

/// `PS_PWR_GATE_DIS_OVERRIDE` (intel_display_regs.h)
pub const PS_PWR_GATE_DIS_OVERRIDE: u32 = reg_bit(31);

/// `PS_PWR_GATE_SETTLING_TIME_MASK` (intel_display_regs.h)
pub const PS_PWR_GATE_SETTLING_TIME_MASK: u32 = reg_genmask(4, 3);

/// `PS_PWR_GATE_SETTLING_TIME_32` (intel_display_regs.h)
pub const PS_PWR_GATE_SETTLING_TIME_32: u32 = reg_field_prep(PS_PWR_GATE_SETTLING_TIME_MASK, 0);

/// `PS_PWR_GATE_SETTLING_TIME_64` (intel_display_regs.h)
pub const PS_PWR_GATE_SETTLING_TIME_64: u32 = reg_field_prep(PS_PWR_GATE_SETTLING_TIME_MASK, 1);

/// `PS_PWR_GATE_SETTLING_TIME_96` (intel_display_regs.h)
pub const PS_PWR_GATE_SETTLING_TIME_96: u32 = reg_field_prep(PS_PWR_GATE_SETTLING_TIME_MASK, 2);

/// `PS_PWR_GATE_SETTLING_TIME_128` (intel_display_regs.h)
pub const PS_PWR_GATE_SETTLING_TIME_128: u32 = reg_field_prep(PS_PWR_GATE_SETTLING_TIME_MASK, 3);

/// `PS_PWR_GATE_SLPEN_MASK` (intel_display_regs.h)
pub const PS_PWR_GATE_SLPEN_MASK: u32 = reg_genmask(1, 0);

/// `PS_PWR_GATE_SLPEN_8` (intel_display_regs.h)
pub const PS_PWR_GATE_SLPEN_8: u32 = reg_field_prep(PS_PWR_GATE_SLPEN_MASK, 0);

/// `PS_PWR_GATE_SLPEN_16` (intel_display_regs.h)
pub const PS_PWR_GATE_SLPEN_16: u32 = reg_field_prep(PS_PWR_GATE_SLPEN_MASK, 1);

/// `PS_PWR_GATE_SLPEN_24` (intel_display_regs.h)
pub const PS_PWR_GATE_SLPEN_24: u32 = reg_field_prep(PS_PWR_GATE_SLPEN_MASK, 2);

/// `PS_PWR_GATE_SLPEN_32` (intel_display_regs.h)
pub const PS_PWR_GATE_SLPEN_32: u32 = reg_field_prep(PS_PWR_GATE_SLPEN_MASK, 3);

/// `_PS_WIN_POS_1A` (intel_display_regs.h)
pub const _PS_WIN_POS_1A: u32 = 0x68170;

/// `_PS_WIN_POS_2A` (intel_display_regs.h)
pub const _PS_WIN_POS_2A: u32 = 0x68270;

/// `_PS_WIN_POS_1B` (intel_display_regs.h)
pub const _PS_WIN_POS_1B: u32 = 0x68970;

/// `_PS_WIN_POS_2B` (intel_display_regs.h)
pub const _PS_WIN_POS_2B: u32 = 0x68A70;

/// `_PS_WIN_POS_1C` (intel_display_regs.h)
pub const _PS_WIN_POS_1C: u32 = 0x69170;

/// `SKL_PS_WIN_POS(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_WIN_POS(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_WIN_POS_1A, _PS_WIN_POS_2A), _ID(id, _PS_WIN_POS_1B, _PS_WIN_POS_2B))
}

/// `PS_WIN_XPOS_MASK` (intel_display_regs.h)
pub const PS_WIN_XPOS_MASK: u32 = reg_genmask(31, 16);

/// `PS_WIN_XPOS(x)` (intel_display_regs.h)
pub const fn PS_WIN_XPOS(x: u32) -> u32 {
    reg_field_prep(PS_WIN_XPOS_MASK, (x))
}

/// `PS_WIN_YPOS_MASK` (intel_display_regs.h)
pub const PS_WIN_YPOS_MASK: u32 = reg_genmask(15, 0);

/// `PS_WIN_YPOS(y)` (intel_display_regs.h)
pub const fn PS_WIN_YPOS(y: u32) -> u32 {
    reg_field_prep(PS_WIN_YPOS_MASK, (y))
}

/// `_PS_WIN_SZ_1A` (intel_display_regs.h)
pub const _PS_WIN_SZ_1A: u32 = 0x68174;

/// `_PS_WIN_SZ_2A` (intel_display_regs.h)
pub const _PS_WIN_SZ_2A: u32 = 0x68274;

/// `_PS_WIN_SZ_1B` (intel_display_regs.h)
pub const _PS_WIN_SZ_1B: u32 = 0x68974;

/// `_PS_WIN_SZ_2B` (intel_display_regs.h)
pub const _PS_WIN_SZ_2B: u32 = 0x68A74;

/// `_PS_WIN_SZ_1C` (intel_display_regs.h)
pub const _PS_WIN_SZ_1C: u32 = 0x69174;

/// `SKL_PS_WIN_SZ(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_WIN_SZ(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_WIN_SZ_1A, _PS_WIN_SZ_2A), _ID(id, _PS_WIN_SZ_1B, _PS_WIN_SZ_2B))
}

/// `PS_WIN_XSIZE_MASK` (intel_display_regs.h)
pub const PS_WIN_XSIZE_MASK: u32 = reg_genmask(31, 16);

/// `PS_WIN_XSIZE(w)` (intel_display_regs.h)
pub const fn PS_WIN_XSIZE(w: u32) -> u32 {
    reg_field_prep(PS_WIN_XSIZE_MASK, (w))
}

/// `PS_WIN_YSIZE_MASK` (intel_display_regs.h)
pub const PS_WIN_YSIZE_MASK: u32 = reg_genmask(15, 0);

/// `PS_WIN_YSIZE(h)` (intel_display_regs.h)
pub const fn PS_WIN_YSIZE(h: u32) -> u32 {
    reg_field_prep(PS_WIN_YSIZE_MASK, (h))
}

/// `_PS_VSCALE_1A` (intel_display_regs.h)
pub const _PS_VSCALE_1A: u32 = 0x68184;

/// `_PS_VSCALE_2A` (intel_display_regs.h)
pub const _PS_VSCALE_2A: u32 = 0x68284;

/// `_PS_VSCALE_1B` (intel_display_regs.h)
pub const _PS_VSCALE_1B: u32 = 0x68984;

/// `_PS_VSCALE_2B` (intel_display_regs.h)
pub const _PS_VSCALE_2B: u32 = 0x68A84;

/// `_PS_VSCALE_1C` (intel_display_regs.h)
pub const _PS_VSCALE_1C: u32 = 0x69184;

/// `SKL_PS_VSCALE(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_VSCALE(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_VSCALE_1A, _PS_VSCALE_2A), _ID(id, _PS_VSCALE_1B, _PS_VSCALE_2B))
}

/// `_PS_HSCALE_1A` (intel_display_regs.h)
pub const _PS_HSCALE_1A: u32 = 0x68190;

/// `_PS_HSCALE_2A` (intel_display_regs.h)
pub const _PS_HSCALE_2A: u32 = 0x68290;

/// `_PS_HSCALE_1B` (intel_display_regs.h)
pub const _PS_HSCALE_1B: u32 = 0x68990;

/// `_PS_HSCALE_2B` (intel_display_regs.h)
pub const _PS_HSCALE_2B: u32 = 0x68A90;

/// `_PS_HSCALE_1C` (intel_display_regs.h)
pub const _PS_HSCALE_1C: u32 = 0x69190;

/// `SKL_PS_HSCALE(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_HSCALE(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_HSCALE_1A, _PS_HSCALE_2A), _ID(id, _PS_HSCALE_1B, _PS_HSCALE_2B))
}

/// `_PS_VPHASE_1A` (intel_display_regs.h)
pub const _PS_VPHASE_1A: u32 = 0x68188;

/// `_PS_VPHASE_2A` (intel_display_regs.h)
pub const _PS_VPHASE_2A: u32 = 0x68288;

/// `_PS_VPHASE_1B` (intel_display_regs.h)
pub const _PS_VPHASE_1B: u32 = 0x68988;

/// `_PS_VPHASE_2B` (intel_display_regs.h)
pub const _PS_VPHASE_2B: u32 = 0x68A88;

/// `_PS_VPHASE_1C` (intel_display_regs.h)
pub const _PS_VPHASE_1C: u32 = 0x69188;

/// `SKL_PS_VPHASE(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_VPHASE(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_VPHASE_1A, _PS_VPHASE_2A), _ID(id, _PS_VPHASE_1B, _PS_VPHASE_2B))
}

/// `PS_Y_PHASE_MASK` (intel_display_regs.h)
pub const PS_Y_PHASE_MASK: u32 = reg_genmask(31, 16);

/// `PS_Y_PHASE(x)` (intel_display_regs.h)
pub const fn PS_Y_PHASE(x: u32) -> u32 {
    reg_field_prep(PS_Y_PHASE_MASK, (x))
}

/// `PS_UV_RGB_PHASE_MASK` (intel_display_regs.h)
pub const PS_UV_RGB_PHASE_MASK: u32 = reg_genmask(15, 0);

/// `PS_UV_RGB_PHASE(x)` (intel_display_regs.h)
pub const fn PS_UV_RGB_PHASE(x: u32) -> u32 {
    reg_field_prep(PS_UV_RGB_PHASE_MASK, (x))
}

/// `PS_PHASE_MASK` (intel_display_regs.h)
pub const PS_PHASE_MASK: u32 = ((0x7fff << 1));

/// `PS_PHASE_TRIP` (intel_display_regs.h)
pub const PS_PHASE_TRIP: u32 = ((1 << 0));

/// `_PS_HPHASE_1A` (intel_display_regs.h)
pub const _PS_HPHASE_1A: u32 = 0x68194;

/// `_PS_HPHASE_2A` (intel_display_regs.h)
pub const _PS_HPHASE_2A: u32 = 0x68294;

/// `_PS_HPHASE_1B` (intel_display_regs.h)
pub const _PS_HPHASE_1B: u32 = 0x68994;

/// `_PS_HPHASE_2B` (intel_display_regs.h)
pub const _PS_HPHASE_2B: u32 = 0x68A94;

/// `_PS_HPHASE_1C` (intel_display_regs.h)
pub const _PS_HPHASE_1C: u32 = 0x69194;

/// `SKL_PS_HPHASE(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_HPHASE(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_HPHASE_1A, _PS_HPHASE_2A), _ID(id, _PS_HPHASE_1B, _PS_HPHASE_2B))
}

/// `_PS_ECC_STAT_1A` (intel_display_regs.h)
pub const _PS_ECC_STAT_1A: u32 = 0x681D0;

/// `_PS_ECC_STAT_2A` (intel_display_regs.h)
pub const _PS_ECC_STAT_2A: u32 = 0x682D0;

/// `_PS_ECC_STAT_1B` (intel_display_regs.h)
pub const _PS_ECC_STAT_1B: u32 = 0x689D0;

/// `_PS_ECC_STAT_2B` (intel_display_regs.h)
pub const _PS_ECC_STAT_2B: u32 = 0x68AD0;

/// `_PS_ECC_STAT_1C` (intel_display_regs.h)
pub const _PS_ECC_STAT_1C: u32 = 0x691D0;

/// `SKL_PS_ECC_STAT(pipe, id)` (intel_display_regs.h)
pub const fn SKL_PS_ECC_STAT(pipe: u32, id: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _ID(id, _PS_ECC_STAT_1A, _PS_ECC_STAT_2A), _ID(id, _PS_ECC_STAT_1B, _PS_ECC_STAT_2B))
}

/// `_PS_COEF_SET0_INDEX_1A` (intel_display_regs.h)
pub const _PS_COEF_SET0_INDEX_1A: u32 = 0x68198;

/// `_PS_COEF_SET0_INDEX_2A` (intel_display_regs.h)
pub const _PS_COEF_SET0_INDEX_2A: u32 = 0x68298;

/// `_PS_COEF_SET0_INDEX_1B` (intel_display_regs.h)
pub const _PS_COEF_SET0_INDEX_1B: u32 = 0x68998;

/// `_PS_COEF_SET0_INDEX_2B` (intel_display_regs.h)
pub const _PS_COEF_SET0_INDEX_2B: u32 = 0x68A98;

/// `GLK_PS_COEF_INDEX_SET(pipe, id, set)` (intel_display_regs.h)
pub const fn GLK_PS_COEF_INDEX_SET(pipe: u32, id: u32, set: u32) -> I915RegT {
    _MMIO_PIPE(pipe, (_ID(id, _PS_COEF_SET0_INDEX_1A, _PS_COEF_SET0_INDEX_2A) + ((set) * 8)), (_ID(id, _PS_COEF_SET0_INDEX_1B, _PS_COEF_SET0_INDEX_2B) + ((set) * 8)))
}

/// `PS_COEF_INDEX_AUTO_INC` (intel_display_regs.h)
pub const PS_COEF_INDEX_AUTO_INC: u32 = reg_bit(10);

/// `_PS_COEF_SET0_DATA_1A` (intel_display_regs.h)
pub const _PS_COEF_SET0_DATA_1A: u32 = 0x6819C;

/// `_PS_COEF_SET0_DATA_2A` (intel_display_regs.h)
pub const _PS_COEF_SET0_DATA_2A: u32 = 0x6829C;

/// `_PS_COEF_SET0_DATA_1B` (intel_display_regs.h)
pub const _PS_COEF_SET0_DATA_1B: u32 = 0x6899C;

/// `_PS_COEF_SET0_DATA_2B` (intel_display_regs.h)
pub const _PS_COEF_SET0_DATA_2B: u32 = 0x68A9C;

/// `GLK_PS_COEF_DATA_SET(pipe, id, set)` (intel_display_regs.h)
pub const fn GLK_PS_COEF_DATA_SET(pipe: u32, id: u32, set: u32) -> I915RegT {
    _MMIO_PIPE(pipe, (_ID(id, _PS_COEF_SET0_DATA_1A, _PS_COEF_SET0_DATA_2A) + ((set) * 8)), (_ID(id, _PS_COEF_SET0_DATA_1B, _PS_COEF_SET0_DATA_2B) + ((set) * 8)))
}

/// `DE_ERR_INT_IVB` (intel_display_regs.h)
pub const DE_ERR_INT_IVB: u32 = ((1 << 30));

/// `DE_GSE_IVB` (intel_display_regs.h)
pub const DE_GSE_IVB: u32 = ((1 << 29));

/// `DE_PCH_EVENT_IVB` (intel_display_regs.h)
pub const DE_PCH_EVENT_IVB: u32 = ((1 << 28));

/// `DE_DP_A_HOTPLUG_IVB` (intel_display_regs.h)
pub const DE_DP_A_HOTPLUG_IVB: u32 = ((1 << 27));

/// `DE_AUX_CHANNEL_A_IVB` (intel_display_regs.h)
pub const DE_AUX_CHANNEL_A_IVB: u32 = ((1 << 26));

/// `DE_EDP_PSR_INT_HSW` (intel_display_regs.h)
pub const DE_EDP_PSR_INT_HSW: u32 = ((1 << 19));

/// `DE_SPRITEC_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_SPRITEC_FLIP_DONE_IVB: u32 = ((1 << 14));

/// `DE_PLANEC_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_PLANEC_FLIP_DONE_IVB: u32 = ((1 << 13));

/// `DE_PIPEC_VBLANK_IVB` (intel_display_regs.h)
pub const DE_PIPEC_VBLANK_IVB: u32 = ((1 << 10));

/// `DE_SPRITEB_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_SPRITEB_FLIP_DONE_IVB: u32 = ((1 << 9));

/// `DE_PLANEB_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_PLANEB_FLIP_DONE_IVB: u32 = ((1 << 8));

/// `DE_PIPEB_VBLANK_IVB` (intel_display_regs.h)
pub const DE_PIPEB_VBLANK_IVB: u32 = ((1 << 5));

/// `DE_SPRITEA_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_SPRITEA_FLIP_DONE_IVB: u32 = ((1 << 4));

/// `DE_PLANEA_FLIP_DONE_IVB` (intel_display_regs.h)
pub const DE_PLANEA_FLIP_DONE_IVB: u32 = ((1 << 3));

/// `DE_PLANE_FLIP_DONE_IVB(plane)` (intel_display_regs.h)
pub const fn DE_PLANE_FLIP_DONE_IVB(plane: u32) -> u32 {
    ((1 << ((3 + (5 * (plane))))))
}

/// `DE_PIPEA_VBLANK_IVB` (intel_display_regs.h)
pub const DE_PIPEA_VBLANK_IVB: u32 = ((1 << 0));

/// `DE_PIPE_VBLANK_IVB(pipe)` (intel_display_regs.h)
pub const fn DE_PIPE_VBLANK_IVB(pipe: u32) -> u32 {
    ((1 << (((pipe) * 5))))
}

/// `XELPD_DISPLAY_ERR_FATAL_MASK` (intel_display_regs.h)
pub const XELPD_DISPLAY_ERR_FATAL_MASK: I915RegT = mmio(0x4421c);

/// `GEN8_DE_PIPE_ISR(pipe)` (intel_display_regs.h)
pub const fn GEN8_DE_PIPE_ISR(pipe: u32) -> I915RegT {
    mmio((0x44400 + ((0x10 * (pipe)))))
}

/// `GEN8_DE_PIPE_IMR(pipe)` (intel_display_regs.h)
pub const fn GEN8_DE_PIPE_IMR(pipe: u32) -> I915RegT {
    mmio((0x44404 + ((0x10 * (pipe)))))
}

/// `GEN8_DE_PIPE_IIR(pipe)` (intel_display_regs.h)
pub const fn GEN8_DE_PIPE_IIR(pipe: u32) -> I915RegT {
    mmio((0x44408 + ((0x10 * (pipe)))))
}

/// `GEN8_DE_PIPE_IER(pipe)` (intel_display_regs.h)
pub const fn GEN8_DE_PIPE_IER(pipe: u32) -> I915RegT {
    mmio((0x4440c + ((0x10 * (pipe)))))
}

/// `GEN8_PIPE_FIFO_UNDERRUN` (intel_display_regs.h)
pub const GEN8_PIPE_FIFO_UNDERRUN: u32 = reg_bit(31);

/// `GEN8_PIPE_CDCLK_CRC_ERROR` (intel_display_regs.h)
pub const GEN8_PIPE_CDCLK_CRC_ERROR: u32 = reg_bit(29);

/// `GEN8_PIPE_CDCLK_CRC_DONE` (intel_display_regs.h)
pub const GEN8_PIPE_CDCLK_CRC_DONE: u32 = reg_bit(28);

/// `GEN12_PIPEDMC_INTERRUPT` (intel_display_regs.h)
pub const GEN12_PIPEDMC_INTERRUPT: u32 = reg_bit(26);

/// `GEN12_PIPEDMC_FAULT` (intel_display_regs.h)
pub const GEN12_PIPEDMC_FAULT: u32 = reg_bit(25);

/// `MTL_PIPEDMC_ATS_FAULT` (intel_display_regs.h)
pub const MTL_PIPEDMC_ATS_FAULT: u32 = reg_bit(24);

/// `GEN12_PIPEDMC_FLIPQ_DONE` (intel_display_regs.h)
pub const GEN12_PIPEDMC_FLIPQ_DONE: u32 = reg_bit(24);

/// `GEN11_PIPE_PLANE7_FAULT` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE7_FAULT: u32 = reg_bit(22);

/// `GEN11_PIPE_PLANE6_FAULT` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE6_FAULT: u32 = reg_bit(21);

/// `GEN11_PIPE_PLANE5_FAULT` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE5_FAULT: u32 = reg_bit(20);

/// `GEN12_PIPE_VBLANK_UNMOD` (intel_display_regs.h)
pub const GEN12_PIPE_VBLANK_UNMOD: u32 = reg_bit(19);

/// `MTL_PLANE_ATS_FAULT` (intel_display_regs.h)
pub const MTL_PLANE_ATS_FAULT: u32 = reg_bit(18);

/// `GEN11_PIPE_PLANE7_FLIP_DONE` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE7_FLIP_DONE: u32 = reg_bit(18);

/// `MTL_PIPEDMC_FLIPQ_DONE` (intel_display_regs.h)
pub const MTL_PIPEDMC_FLIPQ_DONE: u32 = reg_bit(17);

/// `GEN11_PIPE_PLANE6_FLIP_DONE` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE6_FLIP_DONE: u32 = reg_bit(17);

/// `GEN11_PIPE_PLANE5_FLIP_DONE` (intel_display_regs.h)
pub const GEN11_PIPE_PLANE5_FLIP_DONE: u32 = reg_bit(16);

/// `GEN12_DSB_2_INT` (intel_display_regs.h)
pub const GEN12_DSB_2_INT: u32 = reg_bit(15);

/// `GEN12_DSB_1_INT` (intel_display_regs.h)
pub const GEN12_DSB_1_INT: u32 = reg_bit(14);

/// `GEN12_DSB_0_INT` (intel_display_regs.h)
pub const GEN12_DSB_0_INT: u32 = reg_bit(13);

/// `GEN12_DSB_INT(dsb_id)` (intel_display_regs.h)
pub const fn GEN12_DSB_INT(dsb_id: u32) -> u32 {
    reg_bit((13 + (dsb_id)))
}

/// `GEN9_PIPE_CURSOR_FAULT` (intel_display_regs.h)
pub const GEN9_PIPE_CURSOR_FAULT: u32 = reg_bit(11);

/// `GEN9_PIPE_PLANE4_FAULT` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE4_FAULT: u32 = reg_bit(10);

/// `GEN8_PIPE_CURSOR_FAULT` (intel_display_regs.h)
pub const GEN8_PIPE_CURSOR_FAULT: u32 = reg_bit(10);

/// `GEN9_PIPE_PLANE3_FAULT` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE3_FAULT: u32 = reg_bit(9);

/// `GEN8_PIPE_SPRITE_FAULT` (intel_display_regs.h)
pub const GEN8_PIPE_SPRITE_FAULT: u32 = reg_bit(9);

/// `GEN9_PIPE_PLANE2_FAULT` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE2_FAULT: u32 = reg_bit(8);

/// `GEN8_PIPE_PRIMARY_FAULT` (intel_display_regs.h)
pub const GEN8_PIPE_PRIMARY_FAULT: u32 = reg_bit(8);

/// `GEN9_PIPE_PLANE1_FAULT` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE1_FAULT: u32 = reg_bit(7);

/// `GEN9_PIPE_PLANE4_FLIP_DONE` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE4_FLIP_DONE: u32 = reg_bit(6);

/// `GEN9_PIPE_PLANE3_FLIP_DONE` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE3_FLIP_DONE: u32 = reg_bit(5);

/// `GEN8_PIPE_SPRITE_FLIP_DONE` (intel_display_regs.h)
pub const GEN8_PIPE_SPRITE_FLIP_DONE: u32 = reg_bit(5);

/// `GEN9_PIPE_PLANE2_FLIP_DONE` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE2_FLIP_DONE: u32 = reg_bit(4);

/// `GEN8_PIPE_PRIMARY_FLIP_DONE` (intel_display_regs.h)
pub const GEN8_PIPE_PRIMARY_FLIP_DONE: u32 = reg_bit(4);

/// `GEN9_PIPE_PLANE1_FLIP_DONE` (intel_display_regs.h)
pub const GEN9_PIPE_PLANE1_FLIP_DONE: u32 = reg_bit(3);

/// `GEN8_PIPE_SCAN_LINE_EVENT` (intel_display_regs.h)
pub const GEN8_PIPE_SCAN_LINE_EVENT: u32 = reg_bit(2);

/// `GEN8_PIPE_VSYNC` (intel_display_regs.h)
pub const GEN8_PIPE_VSYNC: u32 = reg_bit(1);

/// `GEN8_PIPE_VBLANK` (intel_display_regs.h)
pub const GEN8_PIPE_VBLANK: u32 = reg_bit(0);

/// `_HPD_PIN_DDI(hpd_pin)` (intel_display_regs.h)
pub const fn _HPD_PIN_DDI(hpd_pin: u32) -> u32 {
    (((hpd_pin) - (HPD_PORT_A as u32)))
}

/// `_HPD_PIN_TC(hpd_pin)` (intel_display_regs.h)
pub const fn _HPD_PIN_TC(hpd_pin: u32) -> u32 {
    (((hpd_pin) - (HPD_PORT_TC1 as u32)))
}

/// `GEN8_DE_PORT_ISR` (intel_display_regs.h)
pub const GEN8_DE_PORT_ISR: I915RegT = mmio(0x44440);

/// `GEN8_DE_PORT_IMR` (intel_display_regs.h)
pub const GEN8_DE_PORT_IMR: I915RegT = mmio(0x44444);

/// `GEN8_DE_PORT_IIR` (intel_display_regs.h)
pub const GEN8_DE_PORT_IIR: I915RegT = mmio(0x44448);

/// `GEN8_DE_PORT_IER` (intel_display_regs.h)
pub const GEN8_DE_PORT_IER: I915RegT = mmio(0x4444c);

/// `DSI1_NON_TE` (intel_display_regs.h)
pub const DSI1_NON_TE: u32 = ((1 << 31));

/// `DSI0_NON_TE` (intel_display_regs.h)
pub const DSI0_NON_TE: u32 = ((1 << 30));

/// `ICL_AUX_CHANNEL_E` (intel_display_regs.h)
pub const ICL_AUX_CHANNEL_E: u32 = ((1 << 29));

/// `ICL_AUX_CHANNEL_F` (intel_display_regs.h)
pub const ICL_AUX_CHANNEL_F: u32 = ((1 << 28));

/// `GEN9_AUX_CHANNEL_D` (intel_display_regs.h)
pub const GEN9_AUX_CHANNEL_D: u32 = ((1 << 27));

/// `GEN9_AUX_CHANNEL_C` (intel_display_regs.h)
pub const GEN9_AUX_CHANNEL_C: u32 = ((1 << 26));

/// `GEN9_AUX_CHANNEL_B` (intel_display_regs.h)
pub const GEN9_AUX_CHANNEL_B: u32 = ((1 << 25));

/// `DSI1_TE` (intel_display_regs.h)
pub const DSI1_TE: u32 = ((1 << 24));

/// `DSI0_TE` (intel_display_regs.h)
pub const DSI0_TE: u32 = ((1 << 23));

/// `GEN8_DE_PORT_HOTPLUG(hpd_pin)` (intel_display_regs.h)
pub const fn GEN8_DE_PORT_HOTPLUG(hpd_pin: u32) -> u32 {
    reg_bit((3 + _HPD_PIN_DDI(hpd_pin)))
}

/// `BXT_DE_PORT_HOTPLUG_MASK` (intel_display_regs.h)
pub const BXT_DE_PORT_HOTPLUG_MASK: u32 = (((GEN8_DE_PORT_HOTPLUG((HPD_PORT_A as u32)) | GEN8_DE_PORT_HOTPLUG((HPD_PORT_B as u32))) | GEN8_DE_PORT_HOTPLUG((HPD_PORT_C as u32))));

/// `BDW_DE_PORT_HOTPLUG_MASK` (intel_display_regs.h)
pub const BDW_DE_PORT_HOTPLUG_MASK: u32 = GEN8_DE_PORT_HOTPLUG((HPD_PORT_A as u32));

/// `BXT_DE_PORT_GMBUS` (intel_display_regs.h)
pub const BXT_DE_PORT_GMBUS: u32 = ((1 << 1));

/// `GEN8_AUX_CHANNEL_A` (intel_display_regs.h)
pub const GEN8_AUX_CHANNEL_A: u32 = ((1 << 0));

/// `TGL_DE_PORT_AUX_USBC6` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC6: u32 = reg_bit(13);

/// `XELPD_DE_PORT_AUX_DDIE` (intel_display_regs.h)
pub const XELPD_DE_PORT_AUX_DDIE: u32 = reg_bit(13);

/// `TGL_DE_PORT_AUX_USBC5` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC5: u32 = reg_bit(12);

/// `XELPD_DE_PORT_AUX_DDID` (intel_display_regs.h)
pub const XELPD_DE_PORT_AUX_DDID: u32 = reg_bit(12);

/// `TGL_DE_PORT_AUX_USBC4` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC4: u32 = reg_bit(11);

/// `TGL_DE_PORT_AUX_USBC3` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC3: u32 = reg_bit(10);

/// `TGL_DE_PORT_AUX_USBC2` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC2: u32 = reg_bit(9);

/// `TGL_DE_PORT_AUX_USBC1` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_USBC1: u32 = reg_bit(8);

/// `TGL_DE_PORT_AUX_DDIC` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_DDIC: u32 = reg_bit(2);

/// `TGL_DE_PORT_AUX_DDIB` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_DDIB: u32 = reg_bit(1);

/// `TGL_DE_PORT_AUX_DDIA` (intel_display_regs.h)
pub const TGL_DE_PORT_AUX_DDIA: u32 = reg_bit(0);

/// `DE_MASTER_IRQ_CONTROL` (intel_display_regs.h)
pub const DE_MASTER_IRQ_CONTROL: u32 = ((1 << 31));

/// `DE_SPRITEB_FLIP_DONE` (intel_display_regs.h)
pub const DE_SPRITEB_FLIP_DONE: u32 = ((1 << 29));

/// `DE_SPRITEA_FLIP_DONE` (intel_display_regs.h)
pub const DE_SPRITEA_FLIP_DONE: u32 = ((1 << 28));

/// `DE_PLANEB_FLIP_DONE` (intel_display_regs.h)
pub const DE_PLANEB_FLIP_DONE: u32 = ((1 << 27));

/// `DE_PLANEA_FLIP_DONE` (intel_display_regs.h)
pub const DE_PLANEA_FLIP_DONE: u32 = ((1 << 26));

/// `DE_PLANE_FLIP_DONE(plane)` (intel_display_regs.h)
pub const fn DE_PLANE_FLIP_DONE(plane: u32) -> u32 {
    ((1 << ((26 + (plane)))))
}

/// `DE_PCU_EVENT` (intel_display_regs.h)
pub const DE_PCU_EVENT: u32 = ((1 << 25));

/// `DE_GTT_FAULT` (intel_display_regs.h)
pub const DE_GTT_FAULT: u32 = ((1 << 24));

/// `DE_POISON` (intel_display_regs.h)
pub const DE_POISON: u32 = ((1 << 23));

/// `DE_PERFORM_COUNTER` (intel_display_regs.h)
pub const DE_PERFORM_COUNTER: u32 = ((1 << 22));

/// `DE_PCH_EVENT` (intel_display_regs.h)
pub const DE_PCH_EVENT: u32 = ((1 << 21));

/// `DE_AUX_CHANNEL_A` (intel_display_regs.h)
pub const DE_AUX_CHANNEL_A: u32 = ((1 << 20));

/// `DE_DP_A_HOTPLUG` (intel_display_regs.h)
pub const DE_DP_A_HOTPLUG: u32 = ((1 << 19));

/// `DE_GSE` (intel_display_regs.h)
pub const DE_GSE: u32 = ((1 << 18));

/// `DE_PIPEB_VBLANK` (intel_display_regs.h)
pub const DE_PIPEB_VBLANK: u32 = ((1 << 15));

/// `DE_PIPEB_EVEN_FIELD` (intel_display_regs.h)
pub const DE_PIPEB_EVEN_FIELD: u32 = ((1 << 14));

/// `DE_PIPEB_ODD_FIELD` (intel_display_regs.h)
pub const DE_PIPEB_ODD_FIELD: u32 = ((1 << 13));

/// `DE_PIPEB_LINE_COMPARE` (intel_display_regs.h)
pub const DE_PIPEB_LINE_COMPARE: u32 = ((1 << 12));

/// `DE_PIPEB_VSYNC` (intel_display_regs.h)
pub const DE_PIPEB_VSYNC: u32 = ((1 << 11));

/// `DE_PIPEB_CRC_DONE` (intel_display_regs.h)
pub const DE_PIPEB_CRC_DONE: u32 = ((1 << 10));

/// `DE_PIPEB_FIFO_UNDERRUN` (intel_display_regs.h)
pub const DE_PIPEB_FIFO_UNDERRUN: u32 = ((1 << 8));

/// `DE_PIPEA_VBLANK` (intel_display_regs.h)
pub const DE_PIPEA_VBLANK: u32 = ((1 << 7));

/// `DE_PIPE_VBLANK(pipe)` (intel_display_regs.h)
pub const fn DE_PIPE_VBLANK(pipe: u32) -> u32 {
    ((1 << ((7 + (8 * (pipe))))))
}

/// `DE_PIPEA_EVEN_FIELD` (intel_display_regs.h)
pub const DE_PIPEA_EVEN_FIELD: u32 = ((1 << 6));

/// `DE_PIPEA_ODD_FIELD` (intel_display_regs.h)
pub const DE_PIPEA_ODD_FIELD: u32 = ((1 << 5));

/// `DE_PIPEA_LINE_COMPARE` (intel_display_regs.h)
pub const DE_PIPEA_LINE_COMPARE: u32 = ((1 << 4));

/// `DE_PIPEA_VSYNC` (intel_display_regs.h)
pub const DE_PIPEA_VSYNC: u32 = ((1 << 3));

/// `DE_PIPEA_CRC_DONE` (intel_display_regs.h)
pub const DE_PIPEA_CRC_DONE: u32 = ((1 << 2));

/// `DE_PIPE_CRC_DONE(pipe)` (intel_display_regs.h)
pub const fn DE_PIPE_CRC_DONE(pipe: u32) -> u32 {
    ((1 << ((2 + (8 * (pipe))))))
}

/// `DE_PIPEA_FIFO_UNDERRUN` (intel_display_regs.h)
pub const DE_PIPEA_FIFO_UNDERRUN: u32 = ((1 << 0));

/// `DE_PIPE_FIFO_UNDERRUN(pipe)` (intel_display_regs.h)
pub const fn DE_PIPE_FIFO_UNDERRUN(pipe: u32) -> u32 {
    ((1 << ((8 * (pipe)))))
}

/// `RM_TIMEOUT` (intel_display_regs.h)
pub const RM_TIMEOUT: I915RegT = mmio(0x42060);

/// `RM_TIMEOUT_REG_CAPTURE` (intel_display_regs.h)
pub const RM_TIMEOUT_REG_CAPTURE: I915RegT = mmio(0x420E0);

/// `MMIO_TIMEOUT_US(us)` (intel_display_regs.h)
pub const fn MMIO_TIMEOUT_US(us: u32) -> u32 {
    (((us) << 0))
}

/// `GEN8_DE_MISC_ISR` (intel_display_regs.h)
pub const GEN8_DE_MISC_ISR: I915RegT = mmio(0x44460);

/// `GEN8_DE_MISC_IMR` (intel_display_regs.h)
pub const GEN8_DE_MISC_IMR: I915RegT = mmio(0x44464);

/// `GEN8_DE_MISC_IIR` (intel_display_regs.h)
pub const GEN8_DE_MISC_IIR: I915RegT = mmio(0x44468);

/// `GEN8_DE_MISC_IER` (intel_display_regs.h)
pub const GEN8_DE_MISC_IER: I915RegT = mmio(0x4446c);

/// `XELPDP_RM_TIMEOUT` (intel_display_regs.h)
pub const XELPDP_RM_TIMEOUT: u32 = reg_bit(29);

/// `XELPDP_PMDEMAND_RSPTOUT_ERR` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_RSPTOUT_ERR: u32 = reg_bit(27);

/// `GEN8_DE_MISC_GSE` (intel_display_regs.h)
pub const GEN8_DE_MISC_GSE: u32 = reg_bit(27);

/// `GEN8_DE_EDP_PSR` (intel_display_regs.h)
pub const GEN8_DE_EDP_PSR: u32 = reg_bit(19);

/// `XELPDP_PMDEMAND_RSP` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_RSP: u32 = reg_bit(3);

/// `XE2LPD_DBUF_OVERLAP_DETECTED` (intel_display_regs.h)
pub const XE2LPD_DBUF_OVERLAP_DETECTED: u32 = reg_bit(1);

/// `GEN11_DISPLAY_INT_CTL` (intel_display_regs.h)
pub const GEN11_DISPLAY_INT_CTL: I915RegT = mmio(0x44200);

/// `GEN11_DISPLAY_IRQ_ENABLE` (intel_display_regs.h)
pub const GEN11_DISPLAY_IRQ_ENABLE: u32 = ((1 << 31));

/// `GEN11_AUDIO_CODEC_IRQ` (intel_display_regs.h)
pub const GEN11_AUDIO_CODEC_IRQ: u32 = ((1 << 24));

/// `GEN11_DE_PCH_IRQ` (intel_display_regs.h)
pub const GEN11_DE_PCH_IRQ: u32 = ((1 << 23));

/// `GEN11_DE_MISC_IRQ` (intel_display_regs.h)
pub const GEN11_DE_MISC_IRQ: u32 = ((1 << 22));

/// `GEN11_DE_HPD_IRQ` (intel_display_regs.h)
pub const GEN11_DE_HPD_IRQ: u32 = ((1 << 21));

/// `GEN11_DE_PORT_IRQ` (intel_display_regs.h)
pub const GEN11_DE_PORT_IRQ: u32 = ((1 << 20));

/// `GEN11_DE_PIPE_C` (intel_display_regs.h)
pub const GEN11_DE_PIPE_C: u32 = ((1 << 18));

/// `GEN11_DE_PIPE_B` (intel_display_regs.h)
pub const GEN11_DE_PIPE_B: u32 = ((1 << 17));

/// `GEN11_DE_PIPE_A` (intel_display_regs.h)
pub const GEN11_DE_PIPE_A: u32 = ((1 << 16));

/// `GEN11_DE_HPD_ISR` (intel_display_regs.h)
pub const GEN11_DE_HPD_ISR: I915RegT = mmio(0x44470);

/// `GEN11_DE_HPD_IMR` (intel_display_regs.h)
pub const GEN11_DE_HPD_IMR: I915RegT = mmio(0x44474);

/// `GEN11_DE_HPD_IIR` (intel_display_regs.h)
pub const GEN11_DE_HPD_IIR: I915RegT = mmio(0x44478);

/// `GEN11_DE_HPD_IER` (intel_display_regs.h)
pub const GEN11_DE_HPD_IER: I915RegT = mmio(0x4447c);

/// `GEN11_TC_HOTPLUG(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_TC_HOTPLUG(hpd_pin: u32) -> u32 {
    reg_bit((16 + _HPD_PIN_TC(hpd_pin)))
}

/// `GEN11_DE_TC_HOTPLUG_MASK` (intel_display_regs.h)
pub const GEN11_DE_TC_HOTPLUG_MASK: u32 = ((((((GEN11_TC_HOTPLUG((HPD_PORT_TC6 as u32)) | GEN11_TC_HOTPLUG((HPD_PORT_TC5 as u32))) | GEN11_TC_HOTPLUG((HPD_PORT_TC4 as u32))) | GEN11_TC_HOTPLUG((HPD_PORT_TC3 as u32))) | GEN11_TC_HOTPLUG((HPD_PORT_TC2 as u32))) | GEN11_TC_HOTPLUG((HPD_PORT_TC1 as u32))));

/// `GEN11_TBT_HOTPLUG(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_TBT_HOTPLUG(hpd_pin: u32) -> u32 {
    reg_bit(_HPD_PIN_TC(hpd_pin))
}

/// `GEN11_DE_TBT_HOTPLUG_MASK` (intel_display_regs.h)
pub const GEN11_DE_TBT_HOTPLUG_MASK: u32 = ((((((GEN11_TBT_HOTPLUG((HPD_PORT_TC6 as u32)) | GEN11_TBT_HOTPLUG((HPD_PORT_TC5 as u32))) | GEN11_TBT_HOTPLUG((HPD_PORT_TC4 as u32))) | GEN11_TBT_HOTPLUG((HPD_PORT_TC3 as u32))) | GEN11_TBT_HOTPLUG((HPD_PORT_TC2 as u32))) | GEN11_TBT_HOTPLUG((HPD_PORT_TC1 as u32))));

/// `GEN11_TBT_HOTPLUG_CTL` (intel_display_regs.h)
pub const GEN11_TBT_HOTPLUG_CTL: I915RegT = mmio(0x44030);

/// `GEN11_TC_HOTPLUG_CTL` (intel_display_regs.h)
pub const GEN11_TC_HOTPLUG_CTL: I915RegT = mmio(0x44038);

/// `GEN11_HOTPLUG_CTL_ENABLE(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_HOTPLUG_CTL_ENABLE(hpd_pin: u32) -> u32 {
    ((8 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `GEN11_HOTPLUG_CTL_LONG_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_HOTPLUG_CTL_LONG_DETECT(hpd_pin: u32) -> u32 {
    ((2 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `GEN11_HOTPLUG_CTL_SHORT_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_HOTPLUG_CTL_SHORT_DETECT(hpd_pin: u32) -> u32 {
    ((1 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `GEN11_HOTPLUG_CTL_NO_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn GEN11_HOTPLUG_CTL_NO_DETECT(hpd_pin: u32) -> u32 {
    ((0 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `PICAINTERRUPT_ISR` (intel_display_regs.h)
pub const PICAINTERRUPT_ISR: I915RegT = mmio(0x16FE50);

/// `PICAINTERRUPT_IMR` (intel_display_regs.h)
pub const PICAINTERRUPT_IMR: I915RegT = mmio(0x16FE54);

/// `PICAINTERRUPT_IIR` (intel_display_regs.h)
pub const PICAINTERRUPT_IIR: I915RegT = mmio(0x16FE58);

/// `PICAINTERRUPT_IER` (intel_display_regs.h)
pub const PICAINTERRUPT_IER: I915RegT = mmio(0x16FE5C);

/// `XELPDP_DP_ALT_HOTPLUG(hpd_pin)` (intel_display_regs.h)
pub const fn XELPDP_DP_ALT_HOTPLUG(hpd_pin: u32) -> u32 {
    reg_bit((16 + _HPD_PIN_TC(hpd_pin)))
}

/// `XELPDP_DP_ALT_HOTPLUG_MASK` (intel_display_regs.h)
pub const XELPDP_DP_ALT_HOTPLUG_MASK: u32 = reg_genmask(19, 16);

/// `XELPDP_AUX_TC(hpd_pin)` (intel_display_regs.h)
pub const fn XELPDP_AUX_TC(hpd_pin: u32) -> u32 {
    reg_bit((8 + _HPD_PIN_TC(hpd_pin)))
}

/// `XELPDP_AUX_TC_MASK` (intel_display_regs.h)
pub const XELPDP_AUX_TC_MASK: u32 = reg_genmask(11, 8);

/// `XE2LPD_AUX_DDI(hpd_pin)` (intel_display_regs.h)
pub const fn XE2LPD_AUX_DDI(hpd_pin: u32) -> u32 {
    reg_bit((6 + _HPD_PIN_DDI(hpd_pin)))
}

/// `XE2LPD_AUX_DDI_MASK` (intel_display_regs.h)
pub const XE2LPD_AUX_DDI_MASK: u32 = reg_genmask(7, 6);

/// `XELPDP_TBT_HOTPLUG(hpd_pin)` (intel_display_regs.h)
pub const fn XELPDP_TBT_HOTPLUG(hpd_pin: u32) -> u32 {
    reg_bit(_HPD_PIN_TC(hpd_pin))
}

/// `XELPDP_TBT_HOTPLUG_MASK` (intel_display_regs.h)
pub const XELPDP_TBT_HOTPLUG_MASK: u32 = reg_genmask(3, 0);

/// `XELPDP_PORT_HOTPLUG_CTL(hpd_pin)` (intel_display_regs.h)
pub const fn XELPDP_PORT_HOTPLUG_CTL(hpd_pin: u32) -> I915RegT {
    mmio((0x16F270 + ((_HPD_PIN_TC(hpd_pin) * 0x200))))
}

/// `XELPDP_TBT_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const XELPDP_TBT_HOTPLUG_ENABLE: u32 = reg_bit(6);

/// `XELPDP_TBT_HPD_LONG_DETECT` (intel_display_regs.h)
pub const XELPDP_TBT_HPD_LONG_DETECT: u32 = reg_bit(5);

/// `XELPDP_TBT_HPD_SHORT_DETECT` (intel_display_regs.h)
pub const XELPDP_TBT_HPD_SHORT_DETECT: u32 = reg_bit(4);

/// `XELPDP_DP_ALT_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const XELPDP_DP_ALT_HOTPLUG_ENABLE: u32 = reg_bit(2);

/// `XELPDP_DP_ALT_HPD_LONG_DETECT` (intel_display_regs.h)
pub const XELPDP_DP_ALT_HPD_LONG_DETECT: u32 = reg_bit(1);

/// `XELPDP_DP_ALT_HPD_SHORT_DETECT` (intel_display_regs.h)
pub const XELPDP_DP_ALT_HPD_SHORT_DETECT: u32 = reg_bit(0);

/// `XELPDP_INITIATE_PMDEMAND_REQUEST(dword)` (intel_display_regs.h)
pub const fn XELPDP_INITIATE_PMDEMAND_REQUEST(dword: u32) -> I915RegT {
    mmio((0x45230 + (4 * (dword))))
}

/// `XELPDP_PMDEMAND_QCLK_GV_BW_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_QCLK_GV_BW_MASK: u32 = reg_genmask(31, 16);

/// `XELPDP_PMDEMAND_VOLTAGE_INDEX_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_VOLTAGE_INDEX_MASK: u32 = reg_genmask(14, 12);

/// `XELPDP_PMDEMAND_QCLK_GV_INDEX_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_QCLK_GV_INDEX_MASK: u32 = reg_genmask(11, 8);

/// `XE3_PMDEMAND_PIPES_MASK` (intel_display_regs.h)
pub const XE3_PMDEMAND_PIPES_MASK: u32 = reg_genmask(7, 4);

/// `XELPDP_PMDEMAND_PIPES_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_PIPES_MASK: u32 = reg_genmask(7, 6);

/// `XELPDP_PMDEMAND_DBUFS_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_DBUFS_MASK: u32 = reg_genmask(5, 4);

/// `XELPDP_PMDEMAND_PHYS_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_PHYS_MASK: u32 = reg_genmask(2, 0);

/// `XELPDP_PMDEMAND_REQ_ENABLE` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_REQ_ENABLE: u32 = reg_bit(31);

/// `XELPDP_PMDEMAND_CDCLK_FREQ_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_CDCLK_FREQ_MASK: u32 = reg_genmask(30, 20);

/// `XELPDP_PMDEMAND_DDICLK_FREQ_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_DDICLK_FREQ_MASK: u32 = reg_genmask(18, 8);

/// `XELPDP_PMDEMAND_SCALERS_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_SCALERS_MASK: u32 = reg_genmask(6, 4);

/// `XELPDP_PMDEMAND_PLLS_MASK` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_PLLS_MASK: u32 = reg_genmask(2, 0);

/// `GEN12_DCPR_STATUS_1` (intel_display_regs.h)
pub const GEN12_DCPR_STATUS_1: I915RegT = mmio(0x46440);

/// `XELPDP_PMDEMAND_INFLIGHT_STATUS` (intel_display_regs.h)
pub const XELPDP_PMDEMAND_INFLIGHT_STATUS: u32 = reg_bit(26);

/// `XE3P_UNDERRUN_PKGC` (intel_display_regs.h)
pub const XE3P_UNDERRUN_PKGC: u32 = reg_bit(21);

/// `FUSE_STRAP` (intel_display_regs.h)
pub const FUSE_STRAP: I915RegT = mmio(0x42014);

/// `ILK_INTERNAL_GRAPHICS_DISABLE` (intel_display_regs.h)
pub const ILK_INTERNAL_GRAPHICS_DISABLE: u32 = reg_bit(31);

/// `ILK_INTERNAL_DISPLAY_DISABLE` (intel_display_regs.h)
pub const ILK_INTERNAL_DISPLAY_DISABLE: u32 = reg_bit(30);

/// `ILK_DISPLAY_DEBUG_DISABLE` (intel_display_regs.h)
pub const ILK_DISPLAY_DEBUG_DISABLE: u32 = reg_bit(29);

/// `IVB_PIPE_C_DISABLE` (intel_display_regs.h)
pub const IVB_PIPE_C_DISABLE: u32 = reg_bit(28);

/// `ILK_HDCP_DISABLE` (intel_display_regs.h)
pub const ILK_HDCP_DISABLE: u32 = reg_bit(25);

/// `ILK_eDP_A_DISABLE` (intel_display_regs.h)
pub const ILK_eDP_A_DISABLE: u32 = reg_bit(24);

/// `HSW_CDCLK_LIMIT` (intel_display_regs.h)
pub const HSW_CDCLK_LIMIT: u32 = reg_bit(24);

/// `ILK_DESKTOP` (intel_display_regs.h)
pub const ILK_DESKTOP: u32 = reg_bit(23);

/// `HSW_CPU_SSC_ENABLE` (intel_display_regs.h)
pub const HSW_CPU_SSC_ENABLE: u32 = reg_bit(21);

/// `FUSE_STRAP3` (intel_display_regs.h)
pub const FUSE_STRAP3: I915RegT = mmio(0x42020);

/// `HSW_REF_CLK_SELECT` (intel_display_regs.h)
pub const HSW_REF_CLK_SELECT: u32 = reg_bit(1);

/// `CHICKEN_MISC_2` (intel_display_regs.h)
pub const CHICKEN_MISC_2: I915RegT = mmio(0x42084);

/// `CHICKEN_MISC_DISABLE_DPT` (intel_display_regs.h)
pub const CHICKEN_MISC_DISABLE_DPT: u32 = reg_bit(30);

/// `BMG_DARB_HALF_BLK_END_BURST` (intel_display_regs.h)
pub const BMG_DARB_HALF_BLK_END_BURST: u32 = reg_bit(27);

/// `KBL_ARB_FILL_SPARE_14` (intel_display_regs.h)
pub const KBL_ARB_FILL_SPARE_14: u32 = reg_bit(14);

/// `KBL_ARB_FILL_SPARE_13` (intel_display_regs.h)
pub const KBL_ARB_FILL_SPARE_13: u32 = reg_bit(13);

/// `GLK_CL2_PWR_DOWN` (intel_display_regs.h)
pub const GLK_CL2_PWR_DOWN: u32 = reg_bit(12);

/// `GLK_CL1_PWR_DOWN` (intel_display_regs.h)
pub const GLK_CL1_PWR_DOWN: u32 = reg_bit(11);

/// `GLK_CL0_PWR_DOWN` (intel_display_regs.h)
pub const GLK_CL0_PWR_DOWN: u32 = reg_bit(10);

/// `CHICKEN_MISC_3` (intel_display_regs.h)
pub const CHICKEN_MISC_3: I915RegT = mmio(0x42088);

/// `DP_MST_DPT_DPTP_ALIGN_WA(trans)` (intel_display_regs.h)
pub const fn DP_MST_DPT_DPTP_ALIGN_WA(trans: u32) -> u32 {
    reg_bit(((9 + (trans)) - (TRANSCODER_A as u32)))
}

/// `DP_MST_SHORT_HBLANK_WA(trans)` (intel_display_regs.h)
pub const fn DP_MST_SHORT_HBLANK_WA(trans: u32) -> u32 {
    reg_bit(((5 + (trans)) - (TRANSCODER_A as u32)))
}

/// `DP_MST_FEC_BS_JITTER_WA(trans)` (intel_display_regs.h)
pub const fn DP_MST_FEC_BS_JITTER_WA(trans: u32) -> u32 {
    reg_bit(((0 + (trans)) - (TRANSCODER_A as u32)))
}

/// `CHICKEN_MISC_4` (intel_display_regs.h)
pub const CHICKEN_MISC_4: I915RegT = mmio(0x4208c);

/// `CHICKEN_FBC_STRIDE_OVERRIDE` (intel_display_regs.h)
pub const CHICKEN_FBC_STRIDE_OVERRIDE: u32 = reg_bit(13);

/// `CHICKEN_FBC_STRIDE_MASK` (intel_display_regs.h)
pub const CHICKEN_FBC_STRIDE_MASK: u32 = reg_genmask(12, 0);

/// `CHICKEN_FBC_STRIDE(x)` (intel_display_regs.h)
pub const fn CHICKEN_FBC_STRIDE(x: u32) -> u32 {
    reg_field_prep(CHICKEN_FBC_STRIDE_MASK, (x))
}

/// `_CHICKEN_PIPESL_1_A` (intel_display_regs.h)
pub const _CHICKEN_PIPESL_1_A: u32 = 0x420b0;

/// `_CHICKEN_PIPESL_1_B` (intel_display_regs.h)
pub const _CHICKEN_PIPESL_1_B: u32 = 0x420b4;

/// `CHICKEN_PIPESL_1(pipe)` (intel_display_regs.h)
pub const fn CHICKEN_PIPESL_1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _CHICKEN_PIPESL_1_A, _CHICKEN_PIPESL_1_B)
}

/// `HSW_PRI_STRETCH_MAX_MASK` (intel_display_regs.h)
pub const HSW_PRI_STRETCH_MAX_MASK: u32 = reg_genmask(28, 27);

/// `HSW_PRI_STRETCH_MAX_X8` (intel_display_regs.h)
pub const HSW_PRI_STRETCH_MAX_X8: u32 = reg_field_prep(HSW_PRI_STRETCH_MAX_MASK, 0);

/// `HSW_PRI_STRETCH_MAX_X4` (intel_display_regs.h)
pub const HSW_PRI_STRETCH_MAX_X4: u32 = reg_field_prep(HSW_PRI_STRETCH_MAX_MASK, 1);

/// `HSW_PRI_STRETCH_MAX_X2` (intel_display_regs.h)
pub const HSW_PRI_STRETCH_MAX_X2: u32 = reg_field_prep(HSW_PRI_STRETCH_MAX_MASK, 2);

/// `HSW_PRI_STRETCH_MAX_X1` (intel_display_regs.h)
pub const HSW_PRI_STRETCH_MAX_X1: u32 = reg_field_prep(HSW_PRI_STRETCH_MAX_MASK, 3);

/// `HSW_SPR_STRETCH_MAX_MASK` (intel_display_regs.h)
pub const HSW_SPR_STRETCH_MAX_MASK: u32 = reg_genmask(26, 25);

/// `HSW_SPR_STRETCH_MAX_X8` (intel_display_regs.h)
pub const HSW_SPR_STRETCH_MAX_X8: u32 = reg_field_prep(HSW_SPR_STRETCH_MAX_MASK, 0);

/// `HSW_SPR_STRETCH_MAX_X4` (intel_display_regs.h)
pub const HSW_SPR_STRETCH_MAX_X4: u32 = reg_field_prep(HSW_SPR_STRETCH_MAX_MASK, 1);

/// `HSW_SPR_STRETCH_MAX_X2` (intel_display_regs.h)
pub const HSW_SPR_STRETCH_MAX_X2: u32 = reg_field_prep(HSW_SPR_STRETCH_MAX_MASK, 2);

/// `HSW_SPR_STRETCH_MAX_X1` (intel_display_regs.h)
pub const HSW_SPR_STRETCH_MAX_X1: u32 = reg_field_prep(HSW_SPR_STRETCH_MAX_MASK, 3);

/// `HSW_FBCQ_DIS` (intel_display_regs.h)
pub const HSW_FBCQ_DIS: u32 = reg_bit(22);

/// `HSW_UNMASK_VBL_TO_REGS_IN_SRD` (intel_display_regs.h)
pub const HSW_UNMASK_VBL_TO_REGS_IN_SRD: u32 = reg_bit(15);

/// `SKL_PSR_MASK_PLANE_FLIP` (intel_display_regs.h)
pub const SKL_PSR_MASK_PLANE_FLIP: u32 = reg_bit(11);

/// `SKL_PLANE1_STRETCH_MAX_MASK` (intel_display_regs.h)
pub const SKL_PLANE1_STRETCH_MAX_MASK: u32 = reg_genmask(1, 0);

/// `SKL_PLANE1_STRETCH_MAX_X8` (intel_display_regs.h)
pub const SKL_PLANE1_STRETCH_MAX_X8: u32 = reg_field_prep(SKL_PLANE1_STRETCH_MAX_MASK, 0);

/// `SKL_PLANE1_STRETCH_MAX_X4` (intel_display_regs.h)
pub const SKL_PLANE1_STRETCH_MAX_X4: u32 = reg_field_prep(SKL_PLANE1_STRETCH_MAX_MASK, 1);

/// `SKL_PLANE1_STRETCH_MAX_X2` (intel_display_regs.h)
pub const SKL_PLANE1_STRETCH_MAX_X2: u32 = reg_field_prep(SKL_PLANE1_STRETCH_MAX_MASK, 2);

/// `SKL_PLANE1_STRETCH_MAX_X1` (intel_display_regs.h)
pub const SKL_PLANE1_STRETCH_MAX_X1: u32 = reg_field_prep(SKL_PLANE1_STRETCH_MAX_MASK, 3);

/// `BDW_UNMASK_VBL_TO_REGS_IN_SRD` (intel_display_regs.h)
pub const BDW_UNMASK_VBL_TO_REGS_IN_SRD: u32 = reg_bit(0);

/// `_CHICKEN_TRANS_A` (intel_display_regs.h)
pub const _CHICKEN_TRANS_A: u32 = 0x420c0;

/// `_CHICKEN_TRANS_B` (intel_display_regs.h)
pub const _CHICKEN_TRANS_B: u32 = 0x420c4;

/// `_CHICKEN_TRANS_C` (intel_display_regs.h)
pub const _CHICKEN_TRANS_C: u32 = 0x420c8;

/// `_CHICKEN_TRANS_EDP` (intel_display_regs.h)
pub const _CHICKEN_TRANS_EDP: u32 = 0x420cc;

/// `_CHICKEN_TRANS_D` (intel_display_regs.h)
pub const _CHICKEN_TRANS_D: u32 = 0x420d8;

/// `_CHICKEN_TRANS(trans)` (intel_display_regs.h)
pub const fn _CHICKEN_TRANS(trans: u32) -> I915RegT {
    mmio(pick(trans, &[_CHICKEN_TRANS_A, _CHICKEN_TRANS_B, _CHICKEN_TRANS_C, _CHICKEN_TRANS_D, _CHICKEN_TRANS_EDP]))
}

/// `_MTL_CHICKEN_TRANS_A` (intel_display_regs.h)
pub const _MTL_CHICKEN_TRANS_A: u32 = 0x604e0;

/// `_MTL_CHICKEN_TRANS_B` (intel_display_regs.h)
pub const _MTL_CHICKEN_TRANS_B: u32 = 0x614e0;

/// `_MTL_CHICKEN_TRANS(trans)` (intel_display_regs.h)
pub const fn _MTL_CHICKEN_TRANS(trans: u32) -> I915RegT {
    _MMIO_TRANS((trans), _MTL_CHICKEN_TRANS_A, _MTL_CHICKEN_TRANS_B)
}

/// `PIPE_VBLANK_WITH_DELAY` (intel_display_regs.h)
pub const PIPE_VBLANK_WITH_DELAY: u32 = reg_bit(31);

/// `SKL_UNMASK_VBL_TO_PIPE_IN_SRD` (intel_display_regs.h)
pub const SKL_UNMASK_VBL_TO_PIPE_IN_SRD: u32 = reg_bit(30);

/// `HSW_FRAME_START_DELAY_MASK` (intel_display_regs.h)
pub const HSW_FRAME_START_DELAY_MASK: u32 = reg_genmask(28, 27);

/// `HSW_FRAME_START_DELAY(x)` (intel_display_regs.h)
pub const fn HSW_FRAME_START_DELAY(x: u32) -> u32 {
    reg_field_prep(HSW_FRAME_START_DELAY_MASK, x)
}

/// `VSC_DATA_SEL_SOFTWARE_CONTROL` (intel_display_regs.h)
pub const VSC_DATA_SEL_SOFTWARE_CONTROL: u32 = reg_bit(25);

/// `FECSTALL_DIS_DPTSTREAM_DPTTG` (intel_display_regs.h)
pub const FECSTALL_DIS_DPTSTREAM_DPTTG: u32 = reg_bit(23);

/// `DDI_TRAINING_OVERRIDE_ENABLE` (intel_display_regs.h)
pub const DDI_TRAINING_OVERRIDE_ENABLE: u32 = reg_bit(19);

/// `ADLP_1_BASED_X_GRANULARITY` (intel_display_regs.h)
pub const ADLP_1_BASED_X_GRANULARITY: u32 = reg_bit(18);

/// `DDI_TRAINING_OVERRIDE_VALUE` (intel_display_regs.h)
pub const DDI_TRAINING_OVERRIDE_VALUE: u32 = reg_bit(18);

/// `DDIE_TRAINING_OVERRIDE_ENABLE` (intel_display_regs.h)
pub const DDIE_TRAINING_OVERRIDE_ENABLE: u32 = reg_bit(17);

/// `DDIE_TRAINING_OVERRIDE_VALUE` (intel_display_regs.h)
pub const DDIE_TRAINING_OVERRIDE_VALUE: u32 = reg_bit(16);

/// `PSR2_ADD_VERTICAL_LINE_COUNT` (intel_display_regs.h)
pub const PSR2_ADD_VERTICAL_LINE_COUNT: u32 = reg_bit(15);

/// `DP_FEC_BS_JITTER_WA` (intel_display_regs.h)
pub const DP_FEC_BS_JITTER_WA: u32 = reg_bit(15);

/// `PSR2_VSC_ENABLE_PROG_HEADER` (intel_display_regs.h)
pub const PSR2_VSC_ENABLE_PROG_HEADER: u32 = reg_bit(12);

/// `DP_DSC_INSERT_SF_AT_EOL_WA` (intel_display_regs.h)
pub const DP_DSC_INSERT_SF_AT_EOL_WA: u32 = reg_bit(4);

/// `HDCP_LINE_REKEY_DISABLE` (intel_display_regs.h)
pub const HDCP_LINE_REKEY_DISABLE: u32 = reg_bit(0);

/// `DISP_ARB_CTL2` (intel_display_regs.h)
pub const DISP_ARB_CTL2: I915RegT = mmio(0x45004);

/// `DISP_DATA_PARTITION_5_6` (intel_display_regs.h)
pub const DISP_DATA_PARTITION_5_6: u32 = reg_bit(6);

/// `DISP_IPC_ENABLE` (intel_display_regs.h)
pub const DISP_IPC_ENABLE: u32 = reg_bit(3);

/// `GEN7_MSG_CTL` (intel_display_regs.h)
pub const GEN7_MSG_CTL: I915RegT = mmio(0x45010);

/// `WAIT_FOR_PCH_RESET_ACK` (intel_display_regs.h)
pub const WAIT_FOR_PCH_RESET_ACK: u32 = ((1 << 1));

/// `WAIT_FOR_PCH_FLR_ACK` (intel_display_regs.h)
pub const WAIT_FOR_PCH_FLR_ACK: u32 = ((1 << 0));

/// `_BW_BUDDY0_CTL` (intel_display_regs.h)
pub const _BW_BUDDY0_CTL: u32 = 0x45130;

/// `_BW_BUDDY1_CTL` (intel_display_regs.h)
pub const _BW_BUDDY1_CTL: u32 = 0x45140;

/// `BW_BUDDY_CTL(x)` (intel_display_regs.h)
pub const fn BW_BUDDY_CTL(x: u32) -> I915RegT {
    mmio(pick_even(x, _BW_BUDDY0_CTL, _BW_BUDDY1_CTL))
}

/// `BW_BUDDY_DISABLE` (intel_display_regs.h)
pub const BW_BUDDY_DISABLE: u32 = reg_bit(31);

/// `BW_BUDDY_TLB_REQ_TIMER_MASK` (intel_display_regs.h)
pub const BW_BUDDY_TLB_REQ_TIMER_MASK: u32 = reg_genmask(21, 16);

/// `BW_BUDDY_TLB_REQ_TIMER(x)` (intel_display_regs.h)
pub const fn BW_BUDDY_TLB_REQ_TIMER(x: u32) -> u32 {
    reg_field_prep(BW_BUDDY_TLB_REQ_TIMER_MASK, x)
}

/// `_BW_BUDDY0_PAGE_MASK` (intel_display_regs.h)
pub const _BW_BUDDY0_PAGE_MASK: u32 = 0x45134;

/// `_BW_BUDDY1_PAGE_MASK` (intel_display_regs.h)
pub const _BW_BUDDY1_PAGE_MASK: u32 = 0x45144;

/// `BW_BUDDY_PAGE_MASK(x)` (intel_display_regs.h)
pub const fn BW_BUDDY_PAGE_MASK(x: u32) -> I915RegT {
    mmio(pick_even(x, _BW_BUDDY0_PAGE_MASK, _BW_BUDDY1_PAGE_MASK))
}

/// `HSW_NDE_RSTWRN_OPT` (intel_display_regs.h)
pub const HSW_NDE_RSTWRN_OPT: I915RegT = mmio(0x46408);

/// `MTL_RESET_PICA_HANDSHAKE_EN` (intel_display_regs.h)
pub const MTL_RESET_PICA_HANDSHAKE_EN: u32 = reg_bit(6);

/// `RESET_PCH_HANDSHAKE_ENABLE` (intel_display_regs.h)
pub const RESET_PCH_HANDSHAKE_ENABLE: u32 = reg_bit(4);

/// `GEN11_CHICKEN_DCPR_2` (intel_display_regs.h)
pub const GEN11_CHICKEN_DCPR_2: I915RegT = mmio(0x46434);

/// `DCPR_MASK_MAXLATENCY_MEMUP_CLR` (intel_display_regs.h)
pub const DCPR_MASK_MAXLATENCY_MEMUP_CLR: u32 = reg_bit(27);

/// `DCPR_MASK_LPMODE` (intel_display_regs.h)
pub const DCPR_MASK_LPMODE: u32 = reg_bit(26);

/// `DCPR_SEND_RESP_IMM` (intel_display_regs.h)
pub const DCPR_SEND_RESP_IMM: u32 = reg_bit(25);

/// `DCPR_CLEAR_MEMSTAT_DIS` (intel_display_regs.h)
pub const DCPR_CLEAR_MEMSTAT_DIS: u32 = reg_bit(24);

/// `XELPD_CHICKEN_DCPR_3` (intel_display_regs.h)
pub const XELPD_CHICKEN_DCPR_3: I915RegT = mmio(0x46438);

/// `DMD_RSP_TIMEOUT_DISABLE` (intel_display_regs.h)
pub const DMD_RSP_TIMEOUT_DISABLE: u32 = reg_bit(19);

/// `SKL_DFSM` (intel_display_regs.h)
pub const SKL_DFSM: I915RegT = mmio(0x51000);

/// `SKL_DFSM_DISPLAY_PM_DISABLE` (intel_display_regs.h)
pub const SKL_DFSM_DISPLAY_PM_DISABLE: u32 = ((1 << 27));

/// `SKL_DFSM_DISPLAY_HDCP_DISABLE` (intel_display_regs.h)
pub const SKL_DFSM_DISPLAY_HDCP_DISABLE: u32 = ((1 << 25));

/// `SKL_DFSM_CDCLK_LIMIT_MASK` (intel_display_regs.h)
pub const SKL_DFSM_CDCLK_LIMIT_MASK: u32 = ((3 << 23));

/// `SKL_DFSM_CDCLK_LIMIT_675` (intel_display_regs.h)
pub const SKL_DFSM_CDCLK_LIMIT_675: u32 = ((0 << 23));

/// `SKL_DFSM_CDCLK_LIMIT_540` (intel_display_regs.h)
pub const SKL_DFSM_CDCLK_LIMIT_540: u32 = ((1 << 23));

/// `SKL_DFSM_CDCLK_LIMIT_450` (intel_display_regs.h)
pub const SKL_DFSM_CDCLK_LIMIT_450: u32 = ((2 << 23));

/// `SKL_DFSM_CDCLK_LIMIT_337_5` (intel_display_regs.h)
pub const SKL_DFSM_CDCLK_LIMIT_337_5: u32 = ((3 << 23));

/// `ICL_DFSM_DMC_DISABLE` (intel_display_regs.h)
pub const ICL_DFSM_DMC_DISABLE: u32 = ((1 << 23));

/// `SKL_DFSM_PIPE_A_DISABLE` (intel_display_regs.h)
pub const SKL_DFSM_PIPE_A_DISABLE: u32 = ((1 << 30));

/// `SKL_DFSM_PIPE_B_DISABLE` (intel_display_regs.h)
pub const SKL_DFSM_PIPE_B_DISABLE: u32 = ((1 << 21));

/// `SKL_DFSM_PIPE_C_DISABLE` (intel_display_regs.h)
pub const SKL_DFSM_PIPE_C_DISABLE: u32 = ((1 << 28));

/// `TGL_DFSM_PIPE_D_DISABLE` (intel_display_regs.h)
pub const TGL_DFSM_PIPE_D_DISABLE: u32 = ((1 << 22));

/// `GLK_DFSM_DISPLAY_DSC_DISABLE` (intel_display_regs.h)
pub const GLK_DFSM_DISPLAY_DSC_DISABLE: u32 = ((1 << 7));

/// `XE2LPD_DFSM_DBUF_OVERLAP_DISABLE` (intel_display_regs.h)
pub const XE2LPD_DFSM_DBUF_OVERLAP_DISABLE: u32 = ((1 << 3));

/// `GMD_ID_DISPLAY` (intel_display_regs.h)
pub const GMD_ID_DISPLAY: I915RegT = mmio(0x510a0);

/// `GMD_ID_DISPLAY_ARCH_MASK` (intel_display_regs.h)
pub const GMD_ID_DISPLAY_ARCH_MASK: u32 = reg_genmask(31, 22);

/// `GMD_ID_DISPLAY_RELEASE_MASK` (intel_display_regs.h)
pub const GMD_ID_DISPLAY_RELEASE_MASK: u32 = reg_genmask(21, 14);

/// `GMD_ID_DISPLAY_STEP` (intel_display_regs.h)
pub const GMD_ID_DISPLAY_STEP: u32 = reg_genmask(5, 0);

/// `XE2LPD_DE_CAP` (intel_display_regs.h)
pub const XE2LPD_DE_CAP: I915RegT = mmio(0x41100);

/// `XE2LPD_DE_CAP_3DLUT_MASK` (intel_display_regs.h)
pub const XE2LPD_DE_CAP_3DLUT_MASK: u32 = reg_genmask(31, 30);

/// `XE2LPD_DE_CAP_DSC_MASK` (intel_display_regs.h)
pub const XE2LPD_DE_CAP_DSC_MASK: u32 = reg_genmask(29, 28);

/// `XE2LPD_DE_CAP_DSC_REMOVED` (intel_display_regs.h)
pub const XE2LPD_DE_CAP_DSC_REMOVED: u32 = 1;

/// `XE2LPD_DE_CAP_SCALER_MASK` (intel_display_regs.h)
pub const XE2LPD_DE_CAP_SCALER_MASK: u32 = reg_genmask(27, 26);

/// `XE2LPD_DE_CAP_SCALER_SINGLE` (intel_display_regs.h)
pub const XE2LPD_DE_CAP_SCALER_SINGLE: u32 = 1;

/// `SKL_DSSM` (intel_display_regs.h)
pub const SKL_DSSM: I915RegT = mmio(0x51004);

/// `ICL_DSSM_CDCLK_PLL_REFCLK_MASK` (intel_display_regs.h)
pub const ICL_DSSM_CDCLK_PLL_REFCLK_MASK: u32 = ((7 << 29));

/// `ICL_DSSM_CDCLK_PLL_REFCLK_24MHz` (intel_display_regs.h)
pub const ICL_DSSM_CDCLK_PLL_REFCLK_24MHz: u32 = ((0 << 29));

/// `ICL_DSSM_CDCLK_PLL_REFCLK_19_2MHz` (intel_display_regs.h)
pub const ICL_DSSM_CDCLK_PLL_REFCLK_19_2MHz: u32 = ((1 << 29));

/// `ICL_DSSM_CDCLK_PLL_REFCLK_38_4MHz` (intel_display_regs.h)
pub const ICL_DSSM_CDCLK_PLL_REFCLK_38_4MHz: u32 = ((2 << 29));

/// `_PIPEA_CHICKEN` (intel_display_regs.h)
pub const _PIPEA_CHICKEN: u32 = 0x70038;

/// `_PIPEB_CHICKEN` (intel_display_regs.h)
pub const _PIPEB_CHICKEN: u32 = 0x71038;

/// `_PIPEC_CHICKEN` (intel_display_regs.h)
pub const _PIPEC_CHICKEN: u32 = 0x72038;

/// `PIPE_CHICKEN(pipe)` (intel_display_regs.h)
pub const fn PIPE_CHICKEN(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_CHICKEN, _PIPEB_CHICKEN)
}

/// `UNDERRUN_RECOVERY_DISABLE_ADLP` (intel_display_regs.h)
pub const UNDERRUN_RECOVERY_DISABLE_ADLP: u32 = reg_bit(30);

/// `UNDERRUN_RECOVERY_ENABLE_DG2` (intel_display_regs.h)
pub const UNDERRUN_RECOVERY_ENABLE_DG2: u32 = reg_bit(30);

/// `PIXEL_ROUNDING_TRUNC_FB_PASSTHRU` (intel_display_regs.h)
pub const PIXEL_ROUNDING_TRUNC_FB_PASSTHRU: u32 = reg_bit(15);

/// `DG2_RENDER_CCSTAG_4_3_EN` (intel_display_regs.h)
pub const DG2_RENDER_CCSTAG_4_3_EN: u32 = reg_bit(12);

/// `PER_PIXEL_ALPHA_BYPASS_EN` (intel_display_regs.h)
pub const PER_PIXEL_ALPHA_BYPASS_EN: u32 = reg_bit(7);

/// `PCH_DISPLAY_BASE` (intel_display_regs.h)
pub const PCH_DISPLAY_BASE: u32 = 0xc0000;

/// `SDE_AUDIO_POWER_D` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_D: u32 = ((1 << 27));

/// `SDE_AUDIO_POWER_C` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_C: u32 = ((1 << 26));

/// `SDE_AUDIO_POWER_B` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_B: u32 = ((1 << 25));

/// `SDE_AUDIO_POWER_SHIFT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_SHIFT: u32 = (25);

/// `SDE_AUDIO_POWER_MASK` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_MASK: u32 = ((7 << SDE_AUDIO_POWER_SHIFT));

/// `SDE_GMBUS` (intel_display_regs.h)
pub const SDE_GMBUS: u32 = ((1 << 24));

/// `SDE_AUDIO_HDCP_TRANSB` (intel_display_regs.h)
pub const SDE_AUDIO_HDCP_TRANSB: u32 = ((1 << 23));

/// `SDE_AUDIO_HDCP_TRANSA` (intel_display_regs.h)
pub const SDE_AUDIO_HDCP_TRANSA: u32 = ((1 << 22));

/// `SDE_AUDIO_HDCP_MASK` (intel_display_regs.h)
pub const SDE_AUDIO_HDCP_MASK: u32 = ((3 << 22));

/// `SDE_AUDIO_TRANSB` (intel_display_regs.h)
pub const SDE_AUDIO_TRANSB: u32 = ((1 << 21));

/// `SDE_AUDIO_TRANSA` (intel_display_regs.h)
pub const SDE_AUDIO_TRANSA: u32 = ((1 << 20));

/// `SDE_AUDIO_TRANS_MASK` (intel_display_regs.h)
pub const SDE_AUDIO_TRANS_MASK: u32 = ((3 << 20));

/// `SDE_POISON` (intel_display_regs.h)
pub const SDE_POISON: u32 = ((1 << 19));

/// `SDE_FDI_RXB` (intel_display_regs.h)
pub const SDE_FDI_RXB: u32 = ((1 << 17));

/// `SDE_FDI_RXA` (intel_display_regs.h)
pub const SDE_FDI_RXA: u32 = ((1 << 16));

/// `SDE_FDI_MASK` (intel_display_regs.h)
pub const SDE_FDI_MASK: u32 = ((3 << 16));

/// `SDE_AUXD` (intel_display_regs.h)
pub const SDE_AUXD: u32 = ((1 << 15));

/// `SDE_AUXC` (intel_display_regs.h)
pub const SDE_AUXC: u32 = ((1 << 14));

/// `SDE_AUXB` (intel_display_regs.h)
pub const SDE_AUXB: u32 = ((1 << 13));

/// `SDE_AUX_MASK` (intel_display_regs.h)
pub const SDE_AUX_MASK: u32 = ((7 << 13));

/// `SDE_CRT_HOTPLUG` (intel_display_regs.h)
pub const SDE_CRT_HOTPLUG: u32 = ((1 << 11));

/// `SDE_PORTD_HOTPLUG` (intel_display_regs.h)
pub const SDE_PORTD_HOTPLUG: u32 = ((1 << 10));

/// `SDE_PORTC_HOTPLUG` (intel_display_regs.h)
pub const SDE_PORTC_HOTPLUG: u32 = ((1 << 9));

/// `SDE_PORTB_HOTPLUG` (intel_display_regs.h)
pub const SDE_PORTB_HOTPLUG: u32 = ((1 << 8));

/// `SDE_SDVOB_HOTPLUG` (intel_display_regs.h)
pub const SDE_SDVOB_HOTPLUG: u32 = ((1 << 6));

/// `SDE_HOTPLUG_MASK` (intel_display_regs.h)
pub const SDE_HOTPLUG_MASK: u32 = (((((SDE_CRT_HOTPLUG | SDE_SDVOB_HOTPLUG) | SDE_PORTB_HOTPLUG) | SDE_PORTC_HOTPLUG) | SDE_PORTD_HOTPLUG));

/// `SDE_TRANSB_CRC_DONE` (intel_display_regs.h)
pub const SDE_TRANSB_CRC_DONE: u32 = ((1 << 5));

/// `SDE_TRANSB_CRC_ERR` (intel_display_regs.h)
pub const SDE_TRANSB_CRC_ERR: u32 = ((1 << 4));

/// `SDE_TRANSB_FIFO_UNDER` (intel_display_regs.h)
pub const SDE_TRANSB_FIFO_UNDER: u32 = ((1 << 3));

/// `SDE_TRANSA_CRC_DONE` (intel_display_regs.h)
pub const SDE_TRANSA_CRC_DONE: u32 = ((1 << 2));

/// `SDE_TRANSA_CRC_ERR` (intel_display_regs.h)
pub const SDE_TRANSA_CRC_ERR: u32 = ((1 << 1));

/// `SDE_TRANSA_FIFO_UNDER` (intel_display_regs.h)
pub const SDE_TRANSA_FIFO_UNDER: u32 = ((1 << 0));

/// `SDE_TRANS_MASK` (intel_display_regs.h)
pub const SDE_TRANS_MASK: u32 = (0x3f);

/// `SDE_AUDIO_POWER_D_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_D_CPT: u32 = ((1 << 31));

/// `SDE_AUDIO_POWER_C_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_C_CPT: u32 = ((1 << 30));

/// `SDE_AUDIO_POWER_B_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_B_CPT: u32 = ((1 << 29));

/// `SDE_AUDIO_POWER_SHIFT_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_SHIFT_CPT: u32 = 29;

/// `SDE_AUDIO_POWER_MASK_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_POWER_MASK_CPT: u32 = ((7 << 29));

/// `SDE_AUXD_CPT` (intel_display_regs.h)
pub const SDE_AUXD_CPT: u32 = ((1 << 27));

/// `SDE_AUXC_CPT` (intel_display_regs.h)
pub const SDE_AUXC_CPT: u32 = ((1 << 26));

/// `SDE_AUXB_CPT` (intel_display_regs.h)
pub const SDE_AUXB_CPT: u32 = ((1 << 25));

/// `SDE_AUX_MASK_CPT` (intel_display_regs.h)
pub const SDE_AUX_MASK_CPT: u32 = ((7 << 25));

/// `SDE_PORTE_HOTPLUG_SPT` (intel_display_regs.h)
pub const SDE_PORTE_HOTPLUG_SPT: u32 = ((1 << 25));

/// `SDE_PORTA_HOTPLUG_SPT` (intel_display_regs.h)
pub const SDE_PORTA_HOTPLUG_SPT: u32 = ((1 << 24));

/// `SDE_PORTD_HOTPLUG_CPT` (intel_display_regs.h)
pub const SDE_PORTD_HOTPLUG_CPT: u32 = ((1 << 23));

/// `SDE_PORTC_HOTPLUG_CPT` (intel_display_regs.h)
pub const SDE_PORTC_HOTPLUG_CPT: u32 = ((1 << 22));

/// `SDE_PORTB_HOTPLUG_CPT` (intel_display_regs.h)
pub const SDE_PORTB_HOTPLUG_CPT: u32 = ((1 << 21));

/// `SDE_CRT_HOTPLUG_CPT` (intel_display_regs.h)
pub const SDE_CRT_HOTPLUG_CPT: u32 = ((1 << 19));

/// `SDE_SDVOB_HOTPLUG_CPT` (intel_display_regs.h)
pub const SDE_SDVOB_HOTPLUG_CPT: u32 = ((1 << 18));

/// `SDE_HOTPLUG_MASK_CPT` (intel_display_regs.h)
pub const SDE_HOTPLUG_MASK_CPT: u32 = (((((SDE_CRT_HOTPLUG_CPT | SDE_SDVOB_HOTPLUG_CPT) | SDE_PORTD_HOTPLUG_CPT) | SDE_PORTC_HOTPLUG_CPT) | SDE_PORTB_HOTPLUG_CPT));

/// `SDE_HOTPLUG_MASK_SPT` (intel_display_regs.h)
pub const SDE_HOTPLUG_MASK_SPT: u32 = (((((SDE_PORTE_HOTPLUG_SPT | SDE_PORTD_HOTPLUG_CPT) | SDE_PORTC_HOTPLUG_CPT) | SDE_PORTB_HOTPLUG_CPT) | SDE_PORTA_HOTPLUG_SPT));

/// `SDE_GMBUS_CPT` (intel_display_regs.h)
pub const SDE_GMBUS_CPT: u32 = ((1 << 17));

/// `SDE_ERROR_CPT` (intel_display_regs.h)
pub const SDE_ERROR_CPT: u32 = ((1 << 16));

/// `SDE_AUDIO_CP_REQ_C_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_REQ_C_CPT: u32 = ((1 << 10));

/// `SDE_AUDIO_CP_CHG_C_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_CHG_C_CPT: u32 = ((1 << 9));

/// `SDE_FDI_RXC_CPT` (intel_display_regs.h)
pub const SDE_FDI_RXC_CPT: u32 = ((1 << 8));

/// `SDE_AUDIO_CP_REQ_B_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_REQ_B_CPT: u32 = ((1 << 6));

/// `SDE_AUDIO_CP_CHG_B_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_CHG_B_CPT: u32 = ((1 << 5));

/// `SDE_FDI_RXB_CPT` (intel_display_regs.h)
pub const SDE_FDI_RXB_CPT: u32 = ((1 << 4));

/// `SDE_AUDIO_CP_REQ_A_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_REQ_A_CPT: u32 = ((1 << 2));

/// `SDE_AUDIO_CP_CHG_A_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_CHG_A_CPT: u32 = ((1 << 1));

/// `SDE_FDI_RXA_CPT` (intel_display_regs.h)
pub const SDE_FDI_RXA_CPT: u32 = ((1 << 0));

/// `SDE_AUDIO_CP_REQ_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_REQ_CPT: u32 = (((SDE_AUDIO_CP_REQ_C_CPT | SDE_AUDIO_CP_REQ_B_CPT) | SDE_AUDIO_CP_REQ_A_CPT));

/// `SDE_AUDIO_CP_CHG_CPT` (intel_display_regs.h)
pub const SDE_AUDIO_CP_CHG_CPT: u32 = (((SDE_AUDIO_CP_CHG_C_CPT | SDE_AUDIO_CP_CHG_B_CPT) | SDE_AUDIO_CP_CHG_A_CPT));

/// `SDE_FDI_MASK_CPT` (intel_display_regs.h)
pub const SDE_FDI_MASK_CPT: u32 = (((SDE_FDI_RXC_CPT | SDE_FDI_RXB_CPT) | SDE_FDI_RXA_CPT));

/// `SDE_PICAINTERRUPT` (intel_display_regs.h)
pub const SDE_PICAINTERRUPT: u32 = reg_bit(31);

/// `SDE_GMBUS_ICP` (intel_display_regs.h)
pub const SDE_GMBUS_ICP: u32 = ((1 << 23));

/// `SDE_TC_HOTPLUG_ICP(hpd_pin)` (intel_display_regs.h)
pub const fn SDE_TC_HOTPLUG_ICP(hpd_pin: u32) -> u32 {
    reg_bit((24 + _HPD_PIN_TC(hpd_pin)))
}

/// `SDE_TC_HOTPLUG_DG2(hpd_pin)` (intel_display_regs.h)
pub const fn SDE_TC_HOTPLUG_DG2(hpd_pin: u32) -> u32 {
    reg_bit((25 + _HPD_PIN_TC(hpd_pin)))
}

/// `SDE_DDI_HOTPLUG_ICP(hpd_pin)` (intel_display_regs.h)
pub const fn SDE_DDI_HOTPLUG_ICP(hpd_pin: u32) -> u32 {
    reg_bit((16 + _HPD_PIN_DDI(hpd_pin)))
}

/// `SDE_DDI_HOTPLUG_MASK_ICP` (intel_display_regs.h)
pub const SDE_DDI_HOTPLUG_MASK_ICP: u32 = ((((SDE_DDI_HOTPLUG_ICP((HPD_PORT_D as u32)) | SDE_DDI_HOTPLUG_ICP((HPD_PORT_C as u32))) | SDE_DDI_HOTPLUG_ICP((HPD_PORT_B as u32))) | SDE_DDI_HOTPLUG_ICP((HPD_PORT_A as u32))));

/// `SDE_TC_HOTPLUG_MASK_ICP` (intel_display_regs.h)
pub const SDE_TC_HOTPLUG_MASK_ICP: u32 = ((((((SDE_TC_HOTPLUG_ICP((HPD_PORT_TC6 as u32)) | SDE_TC_HOTPLUG_ICP((HPD_PORT_TC5 as u32))) | SDE_TC_HOTPLUG_ICP((HPD_PORT_TC4 as u32))) | SDE_TC_HOTPLUG_ICP((HPD_PORT_TC3 as u32))) | SDE_TC_HOTPLUG_ICP((HPD_PORT_TC2 as u32))) | SDE_TC_HOTPLUG_ICP((HPD_PORT_TC1 as u32))));

/// `SDEISR` (intel_display_regs.h)
pub const SDEISR: I915RegT = mmio(0xc4000);

/// `SDEIMR` (intel_display_regs.h)
pub const SDEIMR: I915RegT = mmio(0xc4004);

/// `SDEIIR` (intel_display_regs.h)
pub const SDEIIR: I915RegT = mmio(0xc4008);

/// `SDEIER` (intel_display_regs.h)
pub const SDEIER: I915RegT = mmio(0xc400c);

/// `SERR_INT` (intel_display_regs.h)
pub const SERR_INT: I915RegT = mmio(0xc4040);

/// `SERR_INT_POISON` (intel_display_regs.h)
pub const SERR_INT_POISON: u32 = ((1 << 31));

/// `SERR_INT_TRANS_FIFO_UNDERRUN(pipe)` (intel_display_regs.h)
pub const fn SERR_INT_TRANS_FIFO_UNDERRUN(pipe: u32) -> u32 {
    ((1 << (((pipe) * 3))))
}

/// `PCH_PORT_HOTPLUG` (intel_display_regs.h)
pub const PCH_PORT_HOTPLUG: I915RegT = mmio(0xc4030);

/// `PORTA_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const PORTA_HOTPLUG_ENABLE: u32 = ((1 << 28));

/// `BXT_DDIA_HPD_INVERT` (intel_display_regs.h)
pub const BXT_DDIA_HPD_INVERT: u32 = ((1 << 27));

/// `PORTA_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const PORTA_HOTPLUG_STATUS_MASK: u32 = ((3 << 24));

/// `PORTA_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const PORTA_HOTPLUG_NO_DETECT: u32 = ((0 << 24));

/// `PORTA_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const PORTA_HOTPLUG_SHORT_DETECT: u32 = ((1 << 24));

/// `PORTA_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const PORTA_HOTPLUG_LONG_DETECT: u32 = ((2 << 24));

/// `PORTD_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const PORTD_HOTPLUG_ENABLE: u32 = ((1 << 20));

/// `PORTD_PULSE_DURATION_2ms` (intel_display_regs.h)
pub const PORTD_PULSE_DURATION_2ms: u32 = ((0 << 18));

/// `PORTD_PULSE_DURATION_4_5ms` (intel_display_regs.h)
pub const PORTD_PULSE_DURATION_4_5ms: u32 = ((1 << 18));

/// `PORTD_PULSE_DURATION_6ms` (intel_display_regs.h)
pub const PORTD_PULSE_DURATION_6ms: u32 = ((2 << 18));

/// `PORTD_PULSE_DURATION_100ms` (intel_display_regs.h)
pub const PORTD_PULSE_DURATION_100ms: u32 = ((3 << 18));

/// `PORTD_PULSE_DURATION_MASK` (intel_display_regs.h)
pub const PORTD_PULSE_DURATION_MASK: u32 = ((3 << 18));

/// `PORTD_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const PORTD_HOTPLUG_STATUS_MASK: u32 = ((3 << 16));

/// `PORTD_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const PORTD_HOTPLUG_NO_DETECT: u32 = ((0 << 16));

/// `PORTD_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const PORTD_HOTPLUG_SHORT_DETECT: u32 = ((1 << 16));

/// `PORTD_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const PORTD_HOTPLUG_LONG_DETECT: u32 = ((2 << 16));

/// `PORTC_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const PORTC_HOTPLUG_ENABLE: u32 = ((1 << 12));

/// `BXT_DDIC_HPD_INVERT` (intel_display_regs.h)
pub const BXT_DDIC_HPD_INVERT: u32 = ((1 << 11));

/// `PORTC_PULSE_DURATION_2ms` (intel_display_regs.h)
pub const PORTC_PULSE_DURATION_2ms: u32 = ((0 << 10));

/// `PORTC_PULSE_DURATION_4_5ms` (intel_display_regs.h)
pub const PORTC_PULSE_DURATION_4_5ms: u32 = ((1 << 10));

/// `PORTC_PULSE_DURATION_6ms` (intel_display_regs.h)
pub const PORTC_PULSE_DURATION_6ms: u32 = ((2 << 10));

/// `PORTC_PULSE_DURATION_100ms` (intel_display_regs.h)
pub const PORTC_PULSE_DURATION_100ms: u32 = ((3 << 10));

/// `PORTC_PULSE_DURATION_MASK` (intel_display_regs.h)
pub const PORTC_PULSE_DURATION_MASK: u32 = ((3 << 10));

/// `PORTC_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const PORTC_HOTPLUG_STATUS_MASK: u32 = ((3 << 8));

/// `PORTC_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const PORTC_HOTPLUG_NO_DETECT: u32 = ((0 << 8));

/// `PORTC_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const PORTC_HOTPLUG_SHORT_DETECT: u32 = ((1 << 8));

/// `PORTC_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const PORTC_HOTPLUG_LONG_DETECT: u32 = ((2 << 8));

/// `PORTB_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const PORTB_HOTPLUG_ENABLE: u32 = ((1 << 4));

/// `BXT_DDIB_HPD_INVERT` (intel_display_regs.h)
pub const BXT_DDIB_HPD_INVERT: u32 = ((1 << 3));

/// `PORTB_PULSE_DURATION_2ms` (intel_display_regs.h)
pub const PORTB_PULSE_DURATION_2ms: u32 = ((0 << 2));

/// `PORTB_PULSE_DURATION_4_5ms` (intel_display_regs.h)
pub const PORTB_PULSE_DURATION_4_5ms: u32 = ((1 << 2));

/// `PORTB_PULSE_DURATION_6ms` (intel_display_regs.h)
pub const PORTB_PULSE_DURATION_6ms: u32 = ((2 << 2));

/// `PORTB_PULSE_DURATION_100ms` (intel_display_regs.h)
pub const PORTB_PULSE_DURATION_100ms: u32 = ((3 << 2));

/// `PORTB_PULSE_DURATION_MASK` (intel_display_regs.h)
pub const PORTB_PULSE_DURATION_MASK: u32 = ((3 << 2));

/// `PORTB_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const PORTB_HOTPLUG_STATUS_MASK: u32 = ((3 << 0));

/// `PORTB_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const PORTB_HOTPLUG_NO_DETECT: u32 = ((0 << 0));

/// `PORTB_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const PORTB_HOTPLUG_SHORT_DETECT: u32 = ((1 << 0));

/// `PORTB_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const PORTB_HOTPLUG_LONG_DETECT: u32 = ((2 << 0));

/// `BXT_DDI_HPD_INVERT_MASK` (intel_display_regs.h)
pub const BXT_DDI_HPD_INVERT_MASK: u32 = (((BXT_DDIA_HPD_INVERT | BXT_DDIB_HPD_INVERT) | BXT_DDIC_HPD_INVERT));

/// `PCH_PORT_HOTPLUG2` (intel_display_regs.h)
pub const PCH_PORT_HOTPLUG2: I915RegT = mmio(0xc403C);

/// `PORTE_HOTPLUG_ENABLE` (intel_display_regs.h)
pub const PORTE_HOTPLUG_ENABLE: u32 = ((1 << 4));

/// `PORTE_HOTPLUG_STATUS_MASK` (intel_display_regs.h)
pub const PORTE_HOTPLUG_STATUS_MASK: u32 = ((3 << 0));

/// `PORTE_HOTPLUG_NO_DETECT` (intel_display_regs.h)
pub const PORTE_HOTPLUG_NO_DETECT: u32 = ((0 << 0));

/// `PORTE_HOTPLUG_SHORT_DETECT` (intel_display_regs.h)
pub const PORTE_HOTPLUG_SHORT_DETECT: u32 = ((1 << 0));

/// `PORTE_HOTPLUG_LONG_DETECT` (intel_display_regs.h)
pub const PORTE_HOTPLUG_LONG_DETECT: u32 = ((2 << 0));

/// `SHOTPLUG_CTL_DDI` (intel_display_regs.h)
pub const SHOTPLUG_CTL_DDI: I915RegT = mmio(0xc4030);

/// `SHOTPLUG_CTL_DDI_HPD_ENABLE(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_ENABLE(hpd_pin: u32) -> u32 {
    ((0x8 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_OUTPUT_DATA(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_OUTPUT_DATA(hpd_pin: u32) -> u32 {
    ((0x4 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_STATUS_MASK(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_STATUS_MASK(hpd_pin: u32) -> u32 {
    ((0x3 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_NO_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_NO_DETECT(hpd_pin: u32) -> u32 {
    ((0x0 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_SHORT_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_SHORT_DETECT(hpd_pin: u32) -> u32 {
    ((0x1 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_LONG_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_LONG_DETECT(hpd_pin: u32) -> u32 {
    ((0x2 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_DDI_HPD_SHORT_LONG_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn SHOTPLUG_CTL_DDI_HPD_SHORT_LONG_DETECT(hpd_pin: u32) -> u32 {
    ((0x3 << ((_HPD_PIN_DDI(hpd_pin) * 4))))
}

/// `SHOTPLUG_CTL_TC` (intel_display_regs.h)
pub const SHOTPLUG_CTL_TC: I915RegT = mmio(0xc4034);

/// `ICP_TC_HPD_ENABLE(hpd_pin)` (intel_display_regs.h)
pub const fn ICP_TC_HPD_ENABLE(hpd_pin: u32) -> u32 {
    ((8 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `ICP_TC_HPD_LONG_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn ICP_TC_HPD_LONG_DETECT(hpd_pin: u32) -> u32 {
    ((2 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `ICP_TC_HPD_SHORT_DETECT(hpd_pin)` (intel_display_regs.h)
pub const fn ICP_TC_HPD_SHORT_DETECT(hpd_pin: u32) -> u32 {
    ((1 << ((_HPD_PIN_TC(hpd_pin) * 4))))
}

/// `SHPD_FILTER_CNT` (intel_display_regs.h)
pub const SHPD_FILTER_CNT: I915RegT = mmio(0xc4038);

/// `SHPD_FILTER_CNT_500_ADJ` (intel_display_regs.h)
pub const SHPD_FILTER_CNT_500_ADJ: u32 = 0x001D9;

/// `SHPD_FILTER_CNT_250` (intel_display_regs.h)
pub const SHPD_FILTER_CNT_250: u32 = 0x000F8;

/// `_PCH_DPLL_A` (intel_display_regs.h)
pub const _PCH_DPLL_A: u32 = 0xc6014;

/// `_PCH_DPLL_B` (intel_display_regs.h)
pub const _PCH_DPLL_B: u32 = 0xc6018;

/// `_PCH_FPA0` (intel_display_regs.h)
pub const _PCH_FPA0: u32 = 0xc6040;

/// `_PCH_FPB0` (intel_display_regs.h)
pub const _PCH_FPB0: u32 = 0xc6048;

/// `FP_CB_TUNE` (intel_display_regs.h)
pub const FP_CB_TUNE: u32 = ((0x3 << 22));

/// `_PCH_FPA1` (intel_display_regs.h)
pub const _PCH_FPA1: u32 = 0xc6044;

/// `_PCH_FPB1` (intel_display_regs.h)
pub const _PCH_FPB1: u32 = 0xc604c;

/// `PCH_DPLL_TEST` (intel_display_regs.h)
pub const PCH_DPLL_TEST: I915RegT = mmio(0xc606c);

/// `PCH_DREF_CONTROL` (intel_display_regs.h)
pub const PCH_DREF_CONTROL: I915RegT = mmio(0xC6200);

/// `DREF_CONTROL_MASK` (intel_display_regs.h)
pub const DREF_CONTROL_MASK: u32 = 0x7fc3;

/// `DREF_CPU_SOURCE_OUTPUT_DISABLE` (intel_display_regs.h)
pub const DREF_CPU_SOURCE_OUTPUT_DISABLE: u32 = ((0 << 13));

/// `DREF_CPU_SOURCE_OUTPUT_DOWNSPREAD` (intel_display_regs.h)
pub const DREF_CPU_SOURCE_OUTPUT_DOWNSPREAD: u32 = ((2 << 13));

/// `DREF_CPU_SOURCE_OUTPUT_NONSPREAD` (intel_display_regs.h)
pub const DREF_CPU_SOURCE_OUTPUT_NONSPREAD: u32 = ((3 << 13));

/// `DREF_CPU_SOURCE_OUTPUT_MASK` (intel_display_regs.h)
pub const DREF_CPU_SOURCE_OUTPUT_MASK: u32 = ((3 << 13));

/// `DREF_SSC_SOURCE_DISABLE` (intel_display_regs.h)
pub const DREF_SSC_SOURCE_DISABLE: u32 = ((0 << 11));

/// `DREF_SSC_SOURCE_ENABLE` (intel_display_regs.h)
pub const DREF_SSC_SOURCE_ENABLE: u32 = ((2 << 11));

/// `DREF_SSC_SOURCE_MASK` (intel_display_regs.h)
pub const DREF_SSC_SOURCE_MASK: u32 = ((3 << 11));

/// `DREF_NONSPREAD_SOURCE_DISABLE` (intel_display_regs.h)
pub const DREF_NONSPREAD_SOURCE_DISABLE: u32 = ((0 << 9));

/// `DREF_NONSPREAD_CK505_ENABLE` (intel_display_regs.h)
pub const DREF_NONSPREAD_CK505_ENABLE: u32 = ((1 << 9));

/// `DREF_NONSPREAD_SOURCE_ENABLE` (intel_display_regs.h)
pub const DREF_NONSPREAD_SOURCE_ENABLE: u32 = ((2 << 9));

/// `DREF_NONSPREAD_SOURCE_MASK` (intel_display_regs.h)
pub const DREF_NONSPREAD_SOURCE_MASK: u32 = ((3 << 9));

/// `DREF_SUPERSPREAD_SOURCE_DISABLE` (intel_display_regs.h)
pub const DREF_SUPERSPREAD_SOURCE_DISABLE: u32 = ((0 << 7));

/// `DREF_SUPERSPREAD_SOURCE_ENABLE` (intel_display_regs.h)
pub const DREF_SUPERSPREAD_SOURCE_ENABLE: u32 = ((2 << 7));

/// `DREF_SUPERSPREAD_SOURCE_MASK` (intel_display_regs.h)
pub const DREF_SUPERSPREAD_SOURCE_MASK: u32 = ((3 << 7));

/// `DREF_SSC4_DOWNSPREAD` (intel_display_regs.h)
pub const DREF_SSC4_DOWNSPREAD: u32 = ((0 << 6));

/// `DREF_SSC4_CENTERSPREAD` (intel_display_regs.h)
pub const DREF_SSC4_CENTERSPREAD: u32 = ((1 << 6));

/// `DREF_SSC1_DISABLE` (intel_display_regs.h)
pub const DREF_SSC1_DISABLE: u32 = ((0 << 1));

/// `DREF_SSC1_ENABLE` (intel_display_regs.h)
pub const DREF_SSC1_ENABLE: u32 = ((1 << 1));

/// `DREF_SSC4_DISABLE` (intel_display_regs.h)
pub const DREF_SSC4_DISABLE: u32 = (0);

/// `DREF_SSC4_ENABLE` (intel_display_regs.h)
pub const DREF_SSC4_ENABLE: u32 = (1);

/// `PCH_RAWCLK_FREQ` (intel_display_regs.h)
pub const PCH_RAWCLK_FREQ: I915RegT = mmio(0xc6204);

/// `FDL_TP1_TIMER_SHIFT` (intel_display_regs.h)
pub const FDL_TP1_TIMER_SHIFT: u32 = 12;

/// `FDL_TP1_TIMER_MASK` (intel_display_regs.h)
pub const FDL_TP1_TIMER_MASK: u32 = ((3 << 12));

/// `FDL_TP2_TIMER_SHIFT` (intel_display_regs.h)
pub const FDL_TP2_TIMER_SHIFT: u32 = 10;

/// `FDL_TP2_TIMER_MASK` (intel_display_regs.h)
pub const FDL_TP2_TIMER_MASK: u32 = ((3 << 10));

/// `RAWCLK_FREQ_MASK` (intel_display_regs.h)
pub const RAWCLK_FREQ_MASK: u32 = 0x3ff;

/// `CNP_RAWCLK_DIV_MASK` (intel_display_regs.h)
pub const CNP_RAWCLK_DIV_MASK: u32 = ((0x3ff << 16));

/// `CNP_RAWCLK_DIV(div)` (intel_display_regs.h)
pub const fn CNP_RAWCLK_DIV(div: u32) -> u32 {
    (((div) << 16))
}

/// `CNP_RAWCLK_FRAC_MASK` (intel_display_regs.h)
pub const CNP_RAWCLK_FRAC_MASK: u32 = ((0xf << 26));

/// `CNP_RAWCLK_DEN(den)` (intel_display_regs.h)
pub const fn CNP_RAWCLK_DEN(den: u32) -> u32 {
    (((den) << 26))
}

/// `ICP_RAWCLK_NUM(num)` (intel_display_regs.h)
pub const fn ICP_RAWCLK_NUM(num: u32) -> u32 {
    (((num) << 11))
}

/// `PCH_DPLL_TMR_CFG` (intel_display_regs.h)
pub const PCH_DPLL_TMR_CFG: I915RegT = mmio(0xc6208);

/// `PCH_SSC4_PARMS` (intel_display_regs.h)
pub const PCH_SSC4_PARMS: I915RegT = mmio(0xc6210);

/// `PCH_SSC4_AUX_PARMS` (intel_display_regs.h)
pub const PCH_SSC4_AUX_PARMS: I915RegT = mmio(0xc6214);

/// `PCH_DPLL_SEL` (intel_display_regs.h)
pub const PCH_DPLL_SEL: I915RegT = mmio(0xc7000);

/// `TRANS_DPLLB_SEL(pipe)` (intel_display_regs.h)
pub const fn TRANS_DPLLB_SEL(pipe: u32) -> u32 {
    ((1 << (((pipe) * 4))))
}

/// `TRANS_DPLLA_SEL(pipe)` (intel_display_regs.h)
pub const fn TRANS_DPLLA_SEL(_pipe: u32) -> u32 {
    0
}

/// `TRANS_DPLL_ENABLE(pipe)` (intel_display_regs.h)
pub const fn TRANS_DPLL_ENABLE(pipe: u32) -> u32 {
    ((1 << ((((pipe) * 4) + 3))))
}

/// `_PCH_TRANS_HTOTAL_A` (intel_display_regs.h)
pub const _PCH_TRANS_HTOTAL_A: u32 = 0xe0000;

/// `_PCH_TRANS_HTOTAL_B` (intel_display_regs.h)
pub const _PCH_TRANS_HTOTAL_B: u32 = 0xe1000;

/// `PCH_TRANS_HTOTAL(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_HTOTAL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_HTOTAL_A, _PCH_TRANS_HTOTAL_B)
}

/// `TRANS_HTOTAL_SHIFT` (intel_display_regs.h)
pub const TRANS_HTOTAL_SHIFT: u32 = 16;

/// `TRANS_HACTIVE_SHIFT` (intel_display_regs.h)
pub const TRANS_HACTIVE_SHIFT: u32 = 0;

/// `_PCH_TRANS_HBLANK_A` (intel_display_regs.h)
pub const _PCH_TRANS_HBLANK_A: u32 = 0xe0004;

/// `_PCH_TRANS_HBLANK_B` (intel_display_regs.h)
pub const _PCH_TRANS_HBLANK_B: u32 = 0xe1004;

/// `PCH_TRANS_HBLANK(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_HBLANK(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_HBLANK_A, _PCH_TRANS_HBLANK_B)
}

/// `TRANS_HBLANK_END_SHIFT` (intel_display_regs.h)
pub const TRANS_HBLANK_END_SHIFT: u32 = 16;

/// `TRANS_HBLANK_START_SHIFT` (intel_display_regs.h)
pub const TRANS_HBLANK_START_SHIFT: u32 = 0;

/// `_PCH_TRANS_HSYNC_A` (intel_display_regs.h)
pub const _PCH_TRANS_HSYNC_A: u32 = 0xe0008;

/// `_PCH_TRANS_HSYNC_B` (intel_display_regs.h)
pub const _PCH_TRANS_HSYNC_B: u32 = 0xe1008;

/// `PCH_TRANS_HSYNC(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_HSYNC(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_HSYNC_A, _PCH_TRANS_HSYNC_B)
}

/// `TRANS_HSYNC_END_SHIFT` (intel_display_regs.h)
pub const TRANS_HSYNC_END_SHIFT: u32 = 16;

/// `TRANS_HSYNC_START_SHIFT` (intel_display_regs.h)
pub const TRANS_HSYNC_START_SHIFT: u32 = 0;

/// `_PCH_TRANS_VTOTAL_A` (intel_display_regs.h)
pub const _PCH_TRANS_VTOTAL_A: u32 = 0xe000c;

/// `_PCH_TRANS_VTOTAL_B` (intel_display_regs.h)
pub const _PCH_TRANS_VTOTAL_B: u32 = 0xe100c;

/// `PCH_TRANS_VTOTAL(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_VTOTAL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_VTOTAL_A, _PCH_TRANS_VTOTAL_B)
}

/// `TRANS_VTOTAL_SHIFT` (intel_display_regs.h)
pub const TRANS_VTOTAL_SHIFT: u32 = 16;

/// `TRANS_VACTIVE_SHIFT` (intel_display_regs.h)
pub const TRANS_VACTIVE_SHIFT: u32 = 0;

/// `_PCH_TRANS_VBLANK_A` (intel_display_regs.h)
pub const _PCH_TRANS_VBLANK_A: u32 = 0xe0010;

/// `_PCH_TRANS_VBLANK_B` (intel_display_regs.h)
pub const _PCH_TRANS_VBLANK_B: u32 = 0xe1010;

/// `PCH_TRANS_VBLANK(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_VBLANK(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_VBLANK_A, _PCH_TRANS_VBLANK_B)
}

/// `TRANS_VBLANK_END_SHIFT` (intel_display_regs.h)
pub const TRANS_VBLANK_END_SHIFT: u32 = 16;

/// `TRANS_VBLANK_START_SHIFT` (intel_display_regs.h)
pub const TRANS_VBLANK_START_SHIFT: u32 = 0;

/// `_PCH_TRANS_VSYNC_A` (intel_display_regs.h)
pub const _PCH_TRANS_VSYNC_A: u32 = 0xe0014;

/// `_PCH_TRANS_VSYNC_B` (intel_display_regs.h)
pub const _PCH_TRANS_VSYNC_B: u32 = 0xe1014;

/// `PCH_TRANS_VSYNC(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_VSYNC(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_VSYNC_A, _PCH_TRANS_VSYNC_B)
}

/// `TRANS_VSYNC_END_SHIFT` (intel_display_regs.h)
pub const TRANS_VSYNC_END_SHIFT: u32 = 16;

/// `TRANS_VSYNC_START_SHIFT` (intel_display_regs.h)
pub const TRANS_VSYNC_START_SHIFT: u32 = 0;

/// `_PCH_TRANS_VSYNCSHIFT_A` (intel_display_regs.h)
pub const _PCH_TRANS_VSYNCSHIFT_A: u32 = 0xe0028;

/// `_PCH_TRANS_VSYNCSHIFT_B` (intel_display_regs.h)
pub const _PCH_TRANS_VSYNCSHIFT_B: u32 = 0xe1028;

/// `PCH_TRANS_VSYNCSHIFT(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_VSYNCSHIFT(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANS_VSYNCSHIFT_A, _PCH_TRANS_VSYNCSHIFT_B)
}

/// `_PCH_TRANSA_DATA_M1` (intel_display_regs.h)
pub const _PCH_TRANSA_DATA_M1: u32 = 0xe0030;

/// `_PCH_TRANSB_DATA_M1` (intel_display_regs.h)
pub const _PCH_TRANSB_DATA_M1: u32 = 0xe1030;

/// `PCH_TRANS_DATA_M1(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_DATA_M1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_DATA_M1, _PCH_TRANSB_DATA_M1)
}

/// `_PCH_TRANSA_DATA_N1` (intel_display_regs.h)
pub const _PCH_TRANSA_DATA_N1: u32 = 0xe0034;

/// `_PCH_TRANSB_DATA_N1` (intel_display_regs.h)
pub const _PCH_TRANSB_DATA_N1: u32 = 0xe1034;

/// `PCH_TRANS_DATA_N1(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_DATA_N1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_DATA_N1, _PCH_TRANSB_DATA_N1)
}

/// `_PCH_TRANSA_DATA_M2` (intel_display_regs.h)
pub const _PCH_TRANSA_DATA_M2: u32 = 0xe0038;

/// `_PCH_TRANSB_DATA_M2` (intel_display_regs.h)
pub const _PCH_TRANSB_DATA_M2: u32 = 0xe1038;

/// `PCH_TRANS_DATA_M2(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_DATA_M2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_DATA_M2, _PCH_TRANSB_DATA_M2)
}

/// `_PCH_TRANSA_DATA_N2` (intel_display_regs.h)
pub const _PCH_TRANSA_DATA_N2: u32 = 0xe003c;

/// `_PCH_TRANSB_DATA_N2` (intel_display_regs.h)
pub const _PCH_TRANSB_DATA_N2: u32 = 0xe103c;

/// `PCH_TRANS_DATA_N2(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_DATA_N2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_DATA_N2, _PCH_TRANSB_DATA_N2)
}

/// `_PCH_TRANSA_LINK_M1` (intel_display_regs.h)
pub const _PCH_TRANSA_LINK_M1: u32 = 0xe0040;

/// `_PCH_TRANSB_LINK_M1` (intel_display_regs.h)
pub const _PCH_TRANSB_LINK_M1: u32 = 0xe1040;

/// `PCH_TRANS_LINK_M1(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_LINK_M1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_LINK_M1, _PCH_TRANSB_LINK_M1)
}

/// `_PCH_TRANSA_LINK_N1` (intel_display_regs.h)
pub const _PCH_TRANSA_LINK_N1: u32 = 0xe0044;

/// `_PCH_TRANSB_LINK_N1` (intel_display_regs.h)
pub const _PCH_TRANSB_LINK_N1: u32 = 0xe1044;

/// `PCH_TRANS_LINK_N1(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_LINK_N1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_LINK_N1, _PCH_TRANSB_LINK_N1)
}

/// `_PCH_TRANSA_LINK_M2` (intel_display_regs.h)
pub const _PCH_TRANSA_LINK_M2: u32 = 0xe0048;

/// `_PCH_TRANSB_LINK_M2` (intel_display_regs.h)
pub const _PCH_TRANSB_LINK_M2: u32 = 0xe1048;

/// `PCH_TRANS_LINK_M2(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_LINK_M2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_LINK_M2, _PCH_TRANSB_LINK_M2)
}

/// `_PCH_TRANSA_LINK_N2` (intel_display_regs.h)
pub const _PCH_TRANSA_LINK_N2: u32 = 0xe004c;

/// `_PCH_TRANSB_LINK_N2` (intel_display_regs.h)
pub const _PCH_TRANSB_LINK_N2: u32 = 0xe104c;

/// `PCH_TRANS_LINK_N2(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANS_LINK_N2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSA_LINK_N2, _PCH_TRANSB_LINK_N2)
}

/// `_VIDEO_DIP_CTL_A` (intel_display_regs.h)
pub const _VIDEO_DIP_CTL_A: u32 = 0xe0200;

/// `_VIDEO_DIP_CTL_B` (intel_display_regs.h)
pub const _VIDEO_DIP_CTL_B: u32 = 0xe1200;

/// `TVIDEO_DIP_CTL(pipe)` (intel_display_regs.h)
pub const fn TVIDEO_DIP_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VIDEO_DIP_CTL_A, _VIDEO_DIP_CTL_B)
}

/// `_VIDEO_DIP_DATA_A` (intel_display_regs.h)
pub const _VIDEO_DIP_DATA_A: u32 = 0xe0208;

/// `_VIDEO_DIP_DATA_B` (intel_display_regs.h)
pub const _VIDEO_DIP_DATA_B: u32 = 0xe1208;

/// `TVIDEO_DIP_DATA(pipe)` (intel_display_regs.h)
pub const fn TVIDEO_DIP_DATA(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VIDEO_DIP_DATA_A, _VIDEO_DIP_DATA_B)
}

/// `_VIDEO_DIP_GCP_A` (intel_display_regs.h)
pub const _VIDEO_DIP_GCP_A: u32 = 0xe0210;

/// `_VIDEO_DIP_GCP_B` (intel_display_regs.h)
pub const _VIDEO_DIP_GCP_B: u32 = 0xe1210;

/// `TVIDEO_DIP_GCP(pipe)` (intel_display_regs.h)
pub const fn TVIDEO_DIP_GCP(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _VIDEO_DIP_GCP_A, _VIDEO_DIP_GCP_B)
}

/// `GCP_COLOR_INDICATION` (intel_display_regs.h)
pub const GCP_COLOR_INDICATION: u32 = ((1 << 2));

/// `GCP_DEFAULT_PHASE_ENABLE` (intel_display_regs.h)
pub const GCP_DEFAULT_PHASE_ENABLE: u32 = ((1 << 1));

/// `GCP_AV_MUTE` (intel_display_regs.h)
pub const GCP_AV_MUTE: u32 = ((1 << 0));

/// `_VLV_VIDEO_DIP_CTL_A` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_CTL_A: u32 = 0x60200;

/// `_VLV_VIDEO_DIP_CTL_B` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_CTL_B: u32 = 0x61170;

/// `_CHV_VIDEO_DIP_CTL_C` (intel_display_regs.h)
pub const _CHV_VIDEO_DIP_CTL_C: u32 = 0x611f0;

/// `VLV_TVIDEO_DIP_CTL(pipe)` (intel_display_regs.h)
pub const fn VLV_TVIDEO_DIP_CTL(pipe: u32) -> I915RegT {
    _MMIO_BASE_PIPE3(VLV_DISPLAY_BASE, (pipe), _VLV_VIDEO_DIP_CTL_A, _VLV_VIDEO_DIP_CTL_B, _CHV_VIDEO_DIP_CTL_C)
}

/// `_VLV_VIDEO_DIP_DATA_A` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_DATA_A: u32 = 0x60208;

/// `_VLV_VIDEO_DIP_DATA_B` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_DATA_B: u32 = 0x61174;

/// `_CHV_VIDEO_DIP_DATA_C` (intel_display_regs.h)
pub const _CHV_VIDEO_DIP_DATA_C: u32 = 0x611f4;

/// `VLV_TVIDEO_DIP_DATA(pipe)` (intel_display_regs.h)
pub const fn VLV_TVIDEO_DIP_DATA(pipe: u32) -> I915RegT {
    _MMIO_BASE_PIPE3(VLV_DISPLAY_BASE, (pipe), _VLV_VIDEO_DIP_DATA_A, _VLV_VIDEO_DIP_DATA_B, _CHV_VIDEO_DIP_DATA_C)
}

/// `_VLV_VIDEO_DIP_GDCP_PAYLOAD_A` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_GDCP_PAYLOAD_A: u32 = 0x60210;

/// `_VLV_VIDEO_DIP_GDCP_PAYLOAD_B` (intel_display_regs.h)
pub const _VLV_VIDEO_DIP_GDCP_PAYLOAD_B: u32 = 0x61178;

/// `_CHV_VIDEO_DIP_GDCP_PAYLOAD_C` (intel_display_regs.h)
pub const _CHV_VIDEO_DIP_GDCP_PAYLOAD_C: u32 = 0x611f8;

/// `VLV_TVIDEO_DIP_GCP(pipe)` (intel_display_regs.h)
pub const fn VLV_TVIDEO_DIP_GCP(pipe: u32) -> I915RegT {
    _MMIO_BASE_PIPE3(VLV_DISPLAY_BASE, (pipe), _VLV_VIDEO_DIP_GDCP_PAYLOAD_A, _VLV_VIDEO_DIP_GDCP_PAYLOAD_B, _CHV_VIDEO_DIP_GDCP_PAYLOAD_C)
}

/// `_HSW_VIDEO_DIP_CTL_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_CTL_A: u32 = 0x60200;

/// `_HSW_VIDEO_DIP_CTL_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_CTL_B: u32 = 0x61200;

/// `_HSW_VIDEO_DIP_AVI_DATA_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_AVI_DATA_A: u32 = 0x60220;

/// `_HSW_VIDEO_DIP_AVI_DATA_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_AVI_DATA_B: u32 = 0x61220;

/// `_HSW_VIDEO_DIP_VS_DATA_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VS_DATA_A: u32 = 0x60260;

/// `_HSW_VIDEO_DIP_VS_DATA_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VS_DATA_B: u32 = 0x61260;

/// `_HSW_VIDEO_DIP_SPD_DATA_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_SPD_DATA_A: u32 = 0x602A0;

/// `_HSW_VIDEO_DIP_SPD_DATA_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_SPD_DATA_B: u32 = 0x612A0;

/// `_HSW_VIDEO_DIP_GMP_DATA_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GMP_DATA_A: u32 = 0x602E0;

/// `_HSW_VIDEO_DIP_GMP_DATA_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GMP_DATA_B: u32 = 0x612E0;

/// `_HSW_VIDEO_DIP_VSC_DATA_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VSC_DATA_A: u32 = 0x60320;

/// `_HSW_VIDEO_DIP_VSC_DATA_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VSC_DATA_B: u32 = 0x61320;

/// `_ADL_VIDEO_DIP_AS_DATA_A` (intel_display_regs.h)
pub const _ADL_VIDEO_DIP_AS_DATA_A: u32 = 0x60484;

/// `_ADL_VIDEO_DIP_AS_DATA_B` (intel_display_regs.h)
pub const _ADL_VIDEO_DIP_AS_DATA_B: u32 = 0x61484;

/// `_GLK_VIDEO_DIP_DRM_DATA_A` (intel_display_regs.h)
pub const _GLK_VIDEO_DIP_DRM_DATA_A: u32 = 0x60440;

/// `_GLK_VIDEO_DIP_DRM_DATA_B` (intel_display_regs.h)
pub const _GLK_VIDEO_DIP_DRM_DATA_B: u32 = 0x61440;

/// `_HSW_VIDEO_DIP_AVI_ECC_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_AVI_ECC_A: u32 = 0x60240;

/// `_HSW_VIDEO_DIP_BVI_ECC_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_BVI_ECC_B: u32 = 0x61240;

/// `_HSW_VIDEO_DIP_VS_ECC_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VS_ECC_A: u32 = 0x60280;

/// `_HSW_VIDEO_DIP_VS_ECC_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VS_ECC_B: u32 = 0x61280;

/// `_HSW_VIDEO_DIP_SPD_ECC_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_SPD_ECC_A: u32 = 0x602C0;

/// `_HSW_VIDEO_DIP_SPD_ECC_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_SPD_ECC_B: u32 = 0x612C0;

/// `_HSW_VIDEO_DIP_GMP_ECC_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GMP_ECC_A: u32 = 0x60300;

/// `_HSW_VIDEO_DIP_GMP_ECC_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GMP_ECC_B: u32 = 0x61300;

/// `_HSW_VIDEO_DIP_VSC_ECC_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VSC_ECC_A: u32 = 0x60344;

/// `_HSW_VIDEO_DIP_VSC_ECC_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_VSC_ECC_B: u32 = 0x61344;

/// `_HSW_VIDEO_DIP_GCP_A` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GCP_A: u32 = 0x60210;

/// `_HSW_VIDEO_DIP_GCP_B` (intel_display_regs.h)
pub const _HSW_VIDEO_DIP_GCP_B: u32 = 0x61210;

/// `_ICL_VIDEO_DIP_PPS_DATA_A` (intel_display_regs.h)
pub const _ICL_VIDEO_DIP_PPS_DATA_A: u32 = 0x60350;

/// `_ICL_VIDEO_DIP_PPS_DATA_B` (intel_display_regs.h)
pub const _ICL_VIDEO_DIP_PPS_DATA_B: u32 = 0x61350;

/// `_ICL_VIDEO_DIP_PPS_ECC_A` (intel_display_regs.h)
pub const _ICL_VIDEO_DIP_PPS_ECC_A: u32 = 0x603D4;

/// `_ICL_VIDEO_DIP_PPS_ECC_B` (intel_display_regs.h)
pub const _ICL_VIDEO_DIP_PPS_ECC_B: u32 = 0x613D4;

/// `_HSW_STEREO_3D_CTL_A` (intel_display_regs.h)
pub const _HSW_STEREO_3D_CTL_A: u32 = 0x70020;

/// `_HSW_STEREO_3D_CTL_B` (intel_display_regs.h)
pub const _HSW_STEREO_3D_CTL_B: u32 = 0x71020;

/// `S3D_ENABLE` (intel_display_regs.h)
pub const S3D_ENABLE: u32 = ((1 << 31));

/// `_PCH_TRANSACONF` (intel_display_regs.h)
pub const _PCH_TRANSACONF: u32 = 0xf0008;

/// `_PCH_TRANSBCONF` (intel_display_regs.h)
pub const _PCH_TRANSBCONF: u32 = 0xf1008;

/// `PCH_TRANSCONF(pipe)` (intel_display_regs.h)
pub const fn PCH_TRANSCONF(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PCH_TRANSACONF, _PCH_TRANSBCONF)
}

/// `LPT_TRANSCONF` (intel_display_regs.h)
pub const LPT_TRANSCONF: I915RegT = PCH_TRANSCONF((PIPE_A as u32));

/// `TRANS_ENABLE` (intel_display_regs.h)
pub const TRANS_ENABLE: u32 = reg_bit(31);

/// `TRANS_STATE_ENABLE` (intel_display_regs.h)
pub const TRANS_STATE_ENABLE: u32 = reg_bit(30);

/// `TRANS_FRAME_START_DELAY_MASK` (intel_display_regs.h)
pub const TRANS_FRAME_START_DELAY_MASK: u32 = reg_genmask(28, 27);

/// `TRANS_FRAME_START_DELAY(x)` (intel_display_regs.h)
pub const fn TRANS_FRAME_START_DELAY(x: u32) -> u32 {
    reg_field_prep(TRANS_FRAME_START_DELAY_MASK, (x))
}

/// `TRANS_INTERLACE_MASK` (intel_display_regs.h)
pub const TRANS_INTERLACE_MASK: u32 = reg_genmask(23, 21);

/// `TRANS_INTERLACE_PROGRESSIVE` (intel_display_regs.h)
pub const TRANS_INTERLACE_PROGRESSIVE: u32 = reg_field_prep(TRANS_INTERLACE_MASK, 0);

/// `TRANS_INTERLACE_LEGACY_VSYNC_IBX` (intel_display_regs.h)
pub const TRANS_INTERLACE_LEGACY_VSYNC_IBX: u32 = reg_field_prep(TRANS_INTERLACE_MASK, 2);

/// `TRANS_INTERLACE_INTERLACED` (intel_display_regs.h)
pub const TRANS_INTERLACE_INTERLACED: u32 = reg_field_prep(TRANS_INTERLACE_MASK, 3);

/// `TRANS_BPC_MASK` (intel_display_regs.h)
pub const TRANS_BPC_MASK: u32 = reg_genmask(7, 5);

/// `TRANS_BPC_8` (intel_display_regs.h)
pub const TRANS_BPC_8: u32 = reg_field_prep(TRANS_BPC_MASK, 0);

/// `TRANS_BPC_10` (intel_display_regs.h)
pub const TRANS_BPC_10: u32 = reg_field_prep(TRANS_BPC_MASK, 1);

/// `TRANS_BPC_6` (intel_display_regs.h)
pub const TRANS_BPC_6: u32 = reg_field_prep(TRANS_BPC_MASK, 2);

/// `TRANS_BPC_12` (intel_display_regs.h)
pub const TRANS_BPC_12: u32 = reg_field_prep(TRANS_BPC_MASK, 3);

/// `_TRANSA_CHICKEN1` (intel_display_regs.h)
pub const _TRANSA_CHICKEN1: u32 = 0xf0060;

/// `_TRANSB_CHICKEN1` (intel_display_regs.h)
pub const _TRANSB_CHICKEN1: u32 = 0xf1060;

/// `TRANS_CHICKEN1(pipe)` (intel_display_regs.h)
pub const fn TRANS_CHICKEN1(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _TRANSA_CHICKEN1, _TRANSB_CHICKEN1)
}

/// `TRANS_CHICKEN1_HDMIUNIT_GC_DISABLE` (intel_display_regs.h)
pub const TRANS_CHICKEN1_HDMIUNIT_GC_DISABLE: u32 = reg_bit(10);

/// `TRANS_CHICKEN1_DP0UNIT_GC_DISABLE` (intel_display_regs.h)
pub const TRANS_CHICKEN1_DP0UNIT_GC_DISABLE: u32 = reg_bit(4);

/// `_TRANSA_CHICKEN2` (intel_display_regs.h)
pub const _TRANSA_CHICKEN2: u32 = 0xf0064;

/// `_TRANSB_CHICKEN2` (intel_display_regs.h)
pub const _TRANSB_CHICKEN2: u32 = 0xf1064;

/// `TRANS_CHICKEN2(pipe)` (intel_display_regs.h)
pub const fn TRANS_CHICKEN2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _TRANSA_CHICKEN2, _TRANSB_CHICKEN2)
}

/// `TRANS_CHICKEN2_TIMING_OVERRIDE` (intel_display_regs.h)
pub const TRANS_CHICKEN2_TIMING_OVERRIDE: u32 = reg_bit(31);

/// `TRANS_CHICKEN2_FDI_POLARITY_REVERSED` (intel_display_regs.h)
pub const TRANS_CHICKEN2_FDI_POLARITY_REVERSED: u32 = reg_bit(29);

/// `TRANS_CHICKEN2_FRAME_START_DELAY_MASK` (intel_display_regs.h)
pub const TRANS_CHICKEN2_FRAME_START_DELAY_MASK: u32 = reg_genmask(28, 27);

/// `TRANS_CHICKEN2_FRAME_START_DELAY(x)` (intel_display_regs.h)
pub const fn TRANS_CHICKEN2_FRAME_START_DELAY(x: u32) -> u32 {
    reg_field_prep(TRANS_CHICKEN2_FRAME_START_DELAY_MASK, (x))
}

/// `TRANS_CHICKEN2_DISABLE_DEEP_COLOR_COUNTER` (intel_display_regs.h)
pub const TRANS_CHICKEN2_DISABLE_DEEP_COLOR_COUNTER: u32 = reg_bit(26);

/// `TRANS_CHICKEN2_DISABLE_DEEP_COLOR_MODESWITCH` (intel_display_regs.h)
pub const TRANS_CHICKEN2_DISABLE_DEEP_COLOR_MODESWITCH: u32 = reg_bit(25);

/// `PCH_DP_B` (intel_display_regs.h)
pub const PCH_DP_B: I915RegT = mmio(0xe4100);

/// `PCH_DP_C` (intel_display_regs.h)
pub const PCH_DP_C: I915RegT = mmio(0xe4200);

/// `PCH_DP_D` (intel_display_regs.h)
pub const PCH_DP_D: I915RegT = mmio(0xe4300);

/// `_TRANS_DP_CTL_A` (intel_display_regs.h)
pub const _TRANS_DP_CTL_A: u32 = 0xe0300;

/// `_TRANS_DP_CTL_B` (intel_display_regs.h)
pub const _TRANS_DP_CTL_B: u32 = 0xe1300;

/// `_TRANS_DP_CTL_C` (intel_display_regs.h)
pub const _TRANS_DP_CTL_C: u32 = 0xe2300;

/// `TRANS_DP_CTL(pipe)` (intel_display_regs.h)
pub const fn TRANS_DP_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _TRANS_DP_CTL_A, _TRANS_DP_CTL_B)
}

/// `TRANS_DP_OUTPUT_ENABLE` (intel_display_regs.h)
pub const TRANS_DP_OUTPUT_ENABLE: u32 = reg_bit(31);

/// `TRANS_DP_PORT_SEL_MASK` (intel_display_regs.h)
pub const TRANS_DP_PORT_SEL_MASK: u32 = reg_genmask(30, 29);

/// `TRANS_DP_PORT_SEL_NONE` (intel_display_regs.h)
pub const TRANS_DP_PORT_SEL_NONE: u32 = reg_field_prep(TRANS_DP_PORT_SEL_MASK, 3);

/// `TRANS_DP_PORT_SEL(port)` (intel_display_regs.h)
pub const fn TRANS_DP_PORT_SEL(port: u32) -> u32 {
    reg_field_prep(TRANS_DP_PORT_SEL_MASK, ((port) - (PORT_B as u32)))
}

/// `TRANS_DP_AUDIO_ONLY` (intel_display_regs.h)
pub const TRANS_DP_AUDIO_ONLY: u32 = reg_bit(26);

/// `TRANS_DP_ENH_FRAMING` (intel_display_regs.h)
pub const TRANS_DP_ENH_FRAMING: u32 = reg_bit(18);

/// `TRANS_DP_BPC_MASK` (intel_display_regs.h)
pub const TRANS_DP_BPC_MASK: u32 = reg_genmask(10, 9);

/// `TRANS_DP_BPC_8` (intel_display_regs.h)
pub const TRANS_DP_BPC_8: u32 = reg_field_prep(TRANS_DP_BPC_MASK, 0);

/// `TRANS_DP_BPC_10` (intel_display_regs.h)
pub const TRANS_DP_BPC_10: u32 = reg_field_prep(TRANS_DP_BPC_MASK, 1);

/// `TRANS_DP_BPC_6` (intel_display_regs.h)
pub const TRANS_DP_BPC_6: u32 = reg_field_prep(TRANS_DP_BPC_MASK, 2);

/// `TRANS_DP_BPC_12` (intel_display_regs.h)
pub const TRANS_DP_BPC_12: u32 = reg_field_prep(TRANS_DP_BPC_MASK, 3);

/// `TRANS_DP_VSYNC_ACTIVE_HIGH` (intel_display_regs.h)
pub const TRANS_DP_VSYNC_ACTIVE_HIGH: u32 = reg_bit(4);

/// `TRANS_DP_HSYNC_ACTIVE_HIGH` (intel_display_regs.h)
pub const TRANS_DP_HSYNC_ACTIVE_HIGH: u32 = reg_bit(3);

/// `_TRANS_DP2_CTL_A` (intel_display_regs.h)
pub const _TRANS_DP2_CTL_A: u32 = 0x600a0;

/// `_TRANS_DP2_CTL_B` (intel_display_regs.h)
pub const _TRANS_DP2_CTL_B: u32 = 0x610a0;

/// `_TRANS_DP2_CTL_C` (intel_display_regs.h)
pub const _TRANS_DP2_CTL_C: u32 = 0x620a0;

/// `_TRANS_DP2_CTL_D` (intel_display_regs.h)
pub const _TRANS_DP2_CTL_D: u32 = 0x630a0;

/// `TRANS_DP2_CTL(trans)` (intel_display_regs.h)
pub const fn TRANS_DP2_CTL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANS_DP2_CTL_A, _TRANS_DP2_CTL_B)
}

/// `TRANS_DP2_128B132B_CHANNEL_CODING` (intel_display_regs.h)
pub const TRANS_DP2_128B132B_CHANNEL_CODING: u32 = reg_bit(31);

/// `TRANS_DP2_PANEL_REPLAY_ENABLE` (intel_display_regs.h)
pub const TRANS_DP2_PANEL_REPLAY_ENABLE: u32 = reg_bit(30);

/// `TRANS_DP2_PR_TUNNELING_ENABLE` (intel_display_regs.h)
pub const TRANS_DP2_PR_TUNNELING_ENABLE: u32 = reg_bit(26);

/// `TRANS_DP2_DEBUG_ENABLE` (intel_display_regs.h)
pub const TRANS_DP2_DEBUG_ENABLE: u32 = reg_bit(23);

/// `_TRANS_DP2_VFREQHIGH_A` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQHIGH_A: u32 = 0x600a4;

/// `_TRANS_DP2_VFREQHIGH_B` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQHIGH_B: u32 = 0x610a4;

/// `_TRANS_DP2_VFREQHIGH_C` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQHIGH_C: u32 = 0x620a4;

/// `_TRANS_DP2_VFREQHIGH_D` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQHIGH_D: u32 = 0x630a4;

/// `TRANS_DP2_VFREQHIGH(trans)` (intel_display_regs.h)
pub const fn TRANS_DP2_VFREQHIGH(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANS_DP2_VFREQHIGH_A, _TRANS_DP2_VFREQHIGH_B)
}

/// `TRANS_DP2_VFREQ_PIXEL_CLOCK_MASK` (intel_display_regs.h)
pub const TRANS_DP2_VFREQ_PIXEL_CLOCK_MASK: u32 = reg_genmask(31, 8);

/// `TRANS_DP2_VFREQ_PIXEL_CLOCK(clk_hz)` (intel_display_regs.h)
pub const fn TRANS_DP2_VFREQ_PIXEL_CLOCK(clk_hz: u32) -> u32 {
    reg_field_prep(TRANS_DP2_VFREQ_PIXEL_CLOCK_MASK, (clk_hz))
}

/// `_TRANS_DP2_VFREQLOW_A` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQLOW_A: u32 = 0x600a8;

/// `_TRANS_DP2_VFREQLOW_B` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQLOW_B: u32 = 0x610a8;

/// `_TRANS_DP2_VFREQLOW_C` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQLOW_C: u32 = 0x620a8;

/// `_TRANS_DP2_VFREQLOW_D` (intel_display_regs.h)
pub const _TRANS_DP2_VFREQLOW_D: u32 = 0x630a8;

/// `TRANS_DP2_VFREQLOW(trans)` (intel_display_regs.h)
pub const fn TRANS_DP2_VFREQLOW(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _TRANS_DP2_VFREQLOW_A, _TRANS_DP2_VFREQLOW_B)
}

/// `_DP_MIN_HBLANK_CTL_A` (intel_display_regs.h)
pub const _DP_MIN_HBLANK_CTL_A: u32 = 0x600ac;

/// `_DP_MIN_HBLANK_CTL_B` (intel_display_regs.h)
pub const _DP_MIN_HBLANK_CTL_B: u32 = 0x610ac;

/// `DP_MIN_HBLANK_CTL(trans)` (intel_display_regs.h)
pub const fn DP_MIN_HBLANK_CTL(trans: u32) -> I915RegT {
    _MMIO_TRANS(trans, _DP_MIN_HBLANK_CTL_A, _DP_MIN_HBLANK_CTL_B)
}

/// `EDP_LINK_TRAIN_400MV_0DB_SNB_A` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_0DB_SNB_A: u32 = ((0x38 << 22));

/// `EDP_LINK_TRAIN_400MV_6DB_SNB_A` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_6DB_SNB_A: u32 = ((0x02 << 22));

/// `EDP_LINK_TRAIN_600MV_3_5DB_SNB_A` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_600MV_3_5DB_SNB_A: u32 = ((0x01 << 22));

/// `EDP_LINK_TRAIN_800MV_0DB_SNB_A` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_800MV_0DB_SNB_A: u32 = ((0x0 << 22));

/// `EDP_LINK_TRAIN_400_600MV_0DB_SNB_B` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400_600MV_0DB_SNB_B: u32 = ((0x0 << 22));

/// `EDP_LINK_TRAIN_400MV_3_5DB_SNB_B` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_3_5DB_SNB_B: u32 = ((0x1 << 22));

/// `EDP_LINK_TRAIN_400_600MV_6DB_SNB_B` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400_600MV_6DB_SNB_B: u32 = ((0x3a << 22));

/// `EDP_LINK_TRAIN_600_800MV_3_5DB_SNB_B` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_600_800MV_3_5DB_SNB_B: u32 = ((0x39 << 22));

/// `EDP_LINK_TRAIN_800_1200MV_0DB_SNB_B` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_800_1200MV_0DB_SNB_B: u32 = ((0x38 << 22));

/// `EDP_LINK_TRAIN_VOL_EMP_MASK_SNB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_VOL_EMP_MASK_SNB: u32 = ((0x3f << 22));

/// `EDP_LINK_TRAIN_400MV_0DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_0DB_IVB: u32 = ((0x24 << 22));

/// `EDP_LINK_TRAIN_400MV_3_5DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_3_5DB_IVB: u32 = ((0x2a << 22));

/// `EDP_LINK_TRAIN_400MV_6DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_400MV_6DB_IVB: u32 = ((0x2f << 22));

/// `EDP_LINK_TRAIN_600MV_0DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_600MV_0DB_IVB: u32 = ((0x30 << 22));

/// `EDP_LINK_TRAIN_600MV_3_5DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_600MV_3_5DB_IVB: u32 = ((0x36 << 22));

/// `EDP_LINK_TRAIN_800MV_0DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_800MV_0DB_IVB: u32 = ((0x38 << 22));

/// `EDP_LINK_TRAIN_800MV_3_5DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_800MV_3_5DB_IVB: u32 = ((0x3e << 22));

/// `EDP_LINK_TRAIN_500MV_0DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_500MV_0DB_IVB: u32 = ((0x00 << 22));

/// `EDP_LINK_TRAIN_1000MV_0DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_1000MV_0DB_IVB: u32 = ((0x20 << 22));

/// `EDP_LINK_TRAIN_500MV_3_5DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_500MV_3_5DB_IVB: u32 = ((0x02 << 22));

/// `EDP_LINK_TRAIN_1000MV_3_5DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_1000MV_3_5DB_IVB: u32 = ((0x22 << 22));

/// `EDP_LINK_TRAIN_1000MV_6DB_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_1000MV_6DB_IVB: u32 = ((0x23 << 22));

/// `EDP_LINK_TRAIN_VOL_EMP_MASK_IVB` (intel_display_regs.h)
pub const EDP_LINK_TRAIN_VOL_EMP_MASK_IVB: u32 = ((0x3f << 22));

/// `VLV_CHICKEN_3` (intel_display_regs.h)
pub const VLV_CHICKEN_3: I915RegT = mmio((VLV_DISPLAY_BASE + 0x7040C));

/// `PIXEL_OVERLAP_CNT_MASK` (intel_display_regs.h)
pub const PIXEL_OVERLAP_CNT_MASK: u32 = ((3 << 30));

/// `PIXEL_OVERLAP_CNT_SHIFT` (intel_display_regs.h)
pub const PIXEL_OVERLAP_CNT_SHIFT: u32 = 30;

/// `HSW_PWR_WELL_CTL1` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL1: I915RegT = mmio(0x45400);

/// `HSW_PWR_WELL_CTL2` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL2: I915RegT = mmio(0x45404);

/// `HSW_PWR_WELL_CTL3` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL3: I915RegT = mmio(0x45408);

/// `HSW_PWR_WELL_CTL4` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL4: I915RegT = mmio(0x4540C);

/// `HSW_PWR_WELL_CTL_REQ(pw_idx)` (intel_display_regs.h)
pub const fn HSW_PWR_WELL_CTL_REQ(pw_idx: u32) -> u32 {
    ((0x2 << (((pw_idx) * 2))))
}

/// `HSW_PWR_WELL_CTL_STATE(pw_idx)` (intel_display_regs.h)
pub const fn HSW_PWR_WELL_CTL_STATE(pw_idx: u32) -> u32 {
    ((0x1 << (((pw_idx) * 2))))
}

/// `HSW_PW_CTL_IDX_GLOBAL` (intel_display_regs.h)
pub const HSW_PW_CTL_IDX_GLOBAL: u32 = 15;

/// `SKL_PW_CTL_IDX_PW_2` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_PW_2: u32 = 15;

/// `SKL_PW_CTL_IDX_PW_1` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_PW_1: u32 = 14;

/// `GLK_PW_CTL_IDX_AUX_C` (intel_display_regs.h)
pub const GLK_PW_CTL_IDX_AUX_C: u32 = 10;

/// `GLK_PW_CTL_IDX_AUX_B` (intel_display_regs.h)
pub const GLK_PW_CTL_IDX_AUX_B: u32 = 9;

/// `GLK_PW_CTL_IDX_AUX_A` (intel_display_regs.h)
pub const GLK_PW_CTL_IDX_AUX_A: u32 = 8;

/// `SKL_PW_CTL_IDX_DDI_D` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_DDI_D: u32 = 4;

/// `SKL_PW_CTL_IDX_DDI_C` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_DDI_C: u32 = 3;

/// `SKL_PW_CTL_IDX_DDI_B` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_DDI_B: u32 = 2;

/// `SKL_PW_CTL_IDX_DDI_A_E` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_DDI_A_E: u32 = 1;

/// `GLK_PW_CTL_IDX_DDI_A` (intel_display_regs.h)
pub const GLK_PW_CTL_IDX_DDI_A: u32 = 1;

/// `SKL_PW_CTL_IDX_MISC_IO` (intel_display_regs.h)
pub const SKL_PW_CTL_IDX_MISC_IO: u32 = 0;

/// `TGL_PW_CTL_IDX_PW_5` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_PW_5: u32 = 4;

/// `ICL_PW_CTL_IDX_PW_4` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_PW_4: u32 = 3;

/// `ICL_PW_CTL_IDX_PW_3` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_PW_3: u32 = 2;

/// `ICL_PW_CTL_IDX_PW_2` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_PW_2: u32 = 1;

/// `ICL_PW_CTL_IDX_PW_1` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_PW_1: u32 = 0;

/// `XELPD_PW_CTL_IDX_PW_D` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_PW_D: u32 = 8;

/// `XELPD_PW_CTL_IDX_PW_C` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_PW_C: u32 = 7;

/// `XELPD_PW_CTL_IDX_PW_B` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_PW_B: u32 = 6;

/// `XELPD_PW_CTL_IDX_PW_A` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_PW_A: u32 = 5;

/// `ICL_PWR_WELL_CTL_AUX1` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_AUX1: I915RegT = mmio(0x45440);

/// `ICL_PWR_WELL_CTL_AUX2` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_AUX2: I915RegT = mmio(0x45444);

/// `ICL_PWR_WELL_CTL_AUX4` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_AUX4: I915RegT = mmio(0x4544C);

/// `TGL_PW_CTL_IDX_AUX_TBT6` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT6: u32 = 14;

/// `TGL_PW_CTL_IDX_AUX_TBT5` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT5: u32 = 13;

/// `TGL_PW_CTL_IDX_AUX_TBT4` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT4: u32 = 12;

/// `ICL_PW_CTL_IDX_AUX_TBT4` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_TBT4: u32 = 11;

/// `TGL_PW_CTL_IDX_AUX_TBT3` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT3: u32 = 11;

/// `ICL_PW_CTL_IDX_AUX_TBT3` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_TBT3: u32 = 10;

/// `TGL_PW_CTL_IDX_AUX_TBT2` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT2: u32 = 10;

/// `ICL_PW_CTL_IDX_AUX_TBT2` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_TBT2: u32 = 9;

/// `TGL_PW_CTL_IDX_AUX_TBT1` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TBT1: u32 = 9;

/// `ICL_PW_CTL_IDX_AUX_TBT1` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_TBT1: u32 = 8;

/// `TGL_PW_CTL_IDX_AUX_TC6` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC6: u32 = 8;

/// `XELPD_PW_CTL_IDX_AUX_E` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_AUX_E: u32 = 8;

/// `TGL_PW_CTL_IDX_AUX_TC5` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC5: u32 = 7;

/// `XELPD_PW_CTL_IDX_AUX_D` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_AUX_D: u32 = 7;

/// `TGL_PW_CTL_IDX_AUX_TC4` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC4: u32 = 6;

/// `ICL_PW_CTL_IDX_AUX_F` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_F: u32 = 5;

/// `TGL_PW_CTL_IDX_AUX_TC3` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC3: u32 = 5;

/// `ICL_PW_CTL_IDX_AUX_E` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_E: u32 = 4;

/// `TGL_PW_CTL_IDX_AUX_TC2` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC2: u32 = 4;

/// `ICL_PW_CTL_IDX_AUX_D` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_D: u32 = 3;

/// `TGL_PW_CTL_IDX_AUX_TC1` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_AUX_TC1: u32 = 3;

/// `ICL_PW_CTL_IDX_AUX_C` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_C: u32 = 2;

/// `ICL_PW_CTL_IDX_AUX_B` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_B: u32 = 1;

/// `ICL_PW_CTL_IDX_AUX_A` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_AUX_A: u32 = 0;

/// `ICL_PWR_WELL_CTL_DDI1` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_DDI1: I915RegT = mmio(0x45450);

/// `ICL_PWR_WELL_CTL_DDI2` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_DDI2: I915RegT = mmio(0x45454);

/// `ICL_PWR_WELL_CTL_DDI4` (intel_display_regs.h)
pub const ICL_PWR_WELL_CTL_DDI4: I915RegT = mmio(0x4545C);

/// `XELPD_PW_CTL_IDX_DDI_E` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_DDI_E: u32 = 8;

/// `TGL_PW_CTL_IDX_DDI_TC6` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC6: u32 = 8;

/// `XELPD_PW_CTL_IDX_DDI_D` (intel_display_regs.h)
pub const XELPD_PW_CTL_IDX_DDI_D: u32 = 7;

/// `TGL_PW_CTL_IDX_DDI_TC5` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC5: u32 = 7;

/// `TGL_PW_CTL_IDX_DDI_TC4` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC4: u32 = 6;

/// `ICL_PW_CTL_IDX_DDI_F` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_F: u32 = 5;

/// `TGL_PW_CTL_IDX_DDI_TC3` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC3: u32 = 5;

/// `ICL_PW_CTL_IDX_DDI_E` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_E: u32 = 4;

/// `TGL_PW_CTL_IDX_DDI_TC2` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC2: u32 = 4;

/// `ICL_PW_CTL_IDX_DDI_D` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_D: u32 = 3;

/// `TGL_PW_CTL_IDX_DDI_TC1` (intel_display_regs.h)
pub const TGL_PW_CTL_IDX_DDI_TC1: u32 = 3;

/// `ICL_PW_CTL_IDX_DDI_C` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_C: u32 = 2;

/// `ICL_PW_CTL_IDX_DDI_B` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_B: u32 = 1;

/// `ICL_PW_CTL_IDX_DDI_A` (intel_display_regs.h)
pub const ICL_PW_CTL_IDX_DDI_A: u32 = 0;

/// `HSW_PWR_WELL_CTL5` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL5: I915RegT = mmio(0x45410);

/// `HSW_PWR_WELL_ENABLE_SINGLE_STEP` (intel_display_regs.h)
pub const HSW_PWR_WELL_ENABLE_SINGLE_STEP: u32 = ((1 << 31));

/// `HSW_PWR_WELL_PWR_GATE_OVERRIDE` (intel_display_regs.h)
pub const HSW_PWR_WELL_PWR_GATE_OVERRIDE: u32 = ((1 << 20));

/// `HSW_PWR_WELL_FORCE_ON` (intel_display_regs.h)
pub const HSW_PWR_WELL_FORCE_ON: u32 = ((1 << 19));

/// `HSW_PWR_WELL_CTL6` (intel_display_regs.h)
pub const HSW_PWR_WELL_CTL6: I915RegT = mmio(0x45414);

/// `CLKGATE_DIS_DSSDSC` (intel_display_regs.h)
pub const CLKGATE_DIS_DSSDSC: I915RegT = mmio(0x46548);

/// `DSS_PIPE_D_GATING_DISABLED` (intel_display_regs.h)
pub const DSS_PIPE_D_GATING_DISABLED: u32 = reg_bit(31);

/// `DSS_PIPE_C_GATING_DISABLED` (intel_display_regs.h)
pub const DSS_PIPE_C_GATING_DISABLED: u32 = reg_bit(29);

/// `DSS_PIPE_B_GATING_DISABLED` (intel_display_regs.h)
pub const DSS_PIPE_B_GATING_DISABLED: u32 = reg_bit(27);

/// `DSS_PIPE_A_GATING_DISABLED` (intel_display_regs.h)
pub const DSS_PIPE_A_GATING_DISABLED: u32 = reg_bit(25);

/// `SKL_FUSE_STATUS` (intel_display_regs.h)
pub const SKL_FUSE_STATUS: I915RegT = mmio(0x42000);

/// `SKL_FUSE_DOWNLOAD_STATUS` (intel_display_regs.h)
pub const SKL_FUSE_DOWNLOAD_STATUS: u32 = ((1 << 31));

/// `SKL_FUSE_PG_DIST_STATUS(pg)` (intel_display_regs.h)
pub const fn SKL_FUSE_PG_DIST_STATUS(pg: u32) -> u32 {
    ((1 << ((27 - (pg)))))
}

/// `_TRANS_DDI_FUNC_CTL_A` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_A: u32 = 0x60400;

/// `_TRANS_DDI_FUNC_CTL_B` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_B: u32 = 0x61400;

/// `_TRANS_DDI_FUNC_CTL_C` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_C: u32 = 0x62400;

/// `_TRANS_DDI_FUNC_CTL_D` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_D: u32 = 0x63400;

/// `_TRANS_DDI_FUNC_CTL_EDP` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_EDP: u32 = 0x6F400;

/// `_TRANS_DDI_FUNC_CTL_DSI0` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_DSI0: u32 = 0x6b400;

/// `_TRANS_DDI_FUNC_CTL_DSI1` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL_DSI1: u32 = 0x6bc00;

/// `TRANS_DDI_FUNC_ENABLE` (intel_display_regs.h)
pub const TRANS_DDI_FUNC_ENABLE: u32 = ((1 << 31));

/// `TRANS_DDI_PORT_SHIFT` (intel_display_regs.h)
pub const TRANS_DDI_PORT_SHIFT: u32 = 28;

/// `TGL_TRANS_DDI_PORT_SHIFT` (intel_display_regs.h)
pub const TGL_TRANS_DDI_PORT_SHIFT: u32 = 27;

/// `TRANS_DDI_PORT_MASK` (intel_display_regs.h)
pub const TRANS_DDI_PORT_MASK: u32 = ((7 << TRANS_DDI_PORT_SHIFT));

/// `TGL_TRANS_DDI_PORT_MASK` (intel_display_regs.h)
pub const TGL_TRANS_DDI_PORT_MASK: u32 = ((0xf << TGL_TRANS_DDI_PORT_SHIFT));

/// `TRANS_DDI_SELECT_PORT(x)` (intel_display_regs.h)
pub const fn TRANS_DDI_SELECT_PORT(x: u32) -> u32 {
    (((x) << TRANS_DDI_PORT_SHIFT))
}

/// `TGL_TRANS_DDI_SELECT_PORT(x)` (intel_display_regs.h)
pub const fn TGL_TRANS_DDI_SELECT_PORT(x: u32) -> u32 {
    (((((x) + 1)) << TGL_TRANS_DDI_PORT_SHIFT))
}

/// `TRANS_DDI_MODE_SELECT_MASK` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_MASK: u32 = ((7 << 24));

/// `TRANS_DDI_MODE_SELECT_HDMI` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_HDMI: u32 = ((0 << 24));

/// `TRANS_DDI_MODE_SELECT_DVI` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_DVI: u32 = ((1 << 24));

/// `TRANS_DDI_MODE_SELECT_DP_SST` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_DP_SST: u32 = ((2 << 24));

/// `TRANS_DDI_MODE_SELECT_DP_MST` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_DP_MST: u32 = ((3 << 24));

/// `TRANS_DDI_MODE_SELECT_FDI_OR_128B132B` (intel_display_regs.h)
pub const TRANS_DDI_MODE_SELECT_FDI_OR_128B132B: u32 = ((4 << 24));

/// `TRANS_DDI_BPC_MASK` (intel_display_regs.h)
pub const TRANS_DDI_BPC_MASK: u32 = ((7 << 20));

/// `TRANS_DDI_BPC_8` (intel_display_regs.h)
pub const TRANS_DDI_BPC_8: u32 = ((0 << 20));

/// `TRANS_DDI_BPC_10` (intel_display_regs.h)
pub const TRANS_DDI_BPC_10: u32 = ((1 << 20));

/// `TRANS_DDI_BPC_6` (intel_display_regs.h)
pub const TRANS_DDI_BPC_6: u32 = ((2 << 20));

/// `TRANS_DDI_BPC_12` (intel_display_regs.h)
pub const TRANS_DDI_BPC_12: u32 = ((3 << 20));

/// `TRANS_DDI_PORT_SYNC_MASTER_SELECT_MASK` (intel_display_regs.h)
pub const TRANS_DDI_PORT_SYNC_MASTER_SELECT_MASK: u32 = reg_genmask(19, 18);

/// `TRANS_DDI_PORT_SYNC_MASTER_SELECT(x)` (intel_display_regs.h)
pub const fn TRANS_DDI_PORT_SYNC_MASTER_SELECT(x: u32) -> u32 {
    reg_field_prep(TRANS_DDI_PORT_SYNC_MASTER_SELECT_MASK, (x))
}

/// `TRANS_DDI_PVSYNC` (intel_display_regs.h)
pub const TRANS_DDI_PVSYNC: u32 = ((1 << 17));

/// `TRANS_DDI_PHSYNC` (intel_display_regs.h)
pub const TRANS_DDI_PHSYNC: u32 = ((1 << 16));

/// `TRANS_DDI_PORT_SYNC_ENABLE` (intel_display_regs.h)
pub const TRANS_DDI_PORT_SYNC_ENABLE: u32 = reg_bit(15);

/// `XE3_TRANS_DDI_HDCP_LINE_REKEY_DISABLE` (intel_display_regs.h)
pub const XE3_TRANS_DDI_HDCP_LINE_REKEY_DISABLE: u32 = reg_bit(15);

/// `TRANS_DDI_EDP_INPUT_MASK` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_MASK: u32 = ((7 << 12));

/// `TRANS_DDI_EDP_INPUT_A_ON` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_A_ON: u32 = ((0 << 12));

/// `TRANS_DDI_EDP_INPUT_A_ONOFF` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_A_ONOFF: u32 = ((4 << 12));

/// `TRANS_DDI_EDP_INPUT_B_ONOFF` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_B_ONOFF: u32 = ((5 << 12));

/// `TRANS_DDI_EDP_INPUT_C_ONOFF` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_C_ONOFF: u32 = ((6 << 12));

/// `TRANS_DDI_EDP_INPUT_D_ONOFF` (intel_display_regs.h)
pub const TRANS_DDI_EDP_INPUT_D_ONOFF: u32 = ((7 << 12));

/// `TRANS_DDI_HDCP_LINE_REKEY_DISABLE` (intel_display_regs.h)
pub const TRANS_DDI_HDCP_LINE_REKEY_DISABLE: u32 = reg_bit(12);

/// `TRANS_DDI_MST_TRANSPORT_SELECT_MASK` (intel_display_regs.h)
pub const TRANS_DDI_MST_TRANSPORT_SELECT_MASK: u32 = reg_genmask(11, 10);

/// `TRANS_DDI_MST_TRANSPORT_SELECT(trans)` (intel_display_regs.h)
pub const fn TRANS_DDI_MST_TRANSPORT_SELECT(trans: u32) -> u32 {
    reg_field_prep(TRANS_DDI_MST_TRANSPORT_SELECT_MASK, trans)
}

/// `TRANS_DDI_HDCP_SIGNALLING` (intel_display_regs.h)
pub const TRANS_DDI_HDCP_SIGNALLING: u32 = ((1 << 9));

/// `TRANS_DDI_DP_VC_PAYLOAD_ALLOC` (intel_display_regs.h)
pub const TRANS_DDI_DP_VC_PAYLOAD_ALLOC: u32 = ((1 << 8));

/// `TRANS_DDI_HDMI_SCRAMBLER_CTS_ENABLE` (intel_display_regs.h)
pub const TRANS_DDI_HDMI_SCRAMBLER_CTS_ENABLE: u32 = ((1 << 7));

/// `TRANS_DDI_HDMI_SCRAMBLER_RESET_FREQ` (intel_display_regs.h)
pub const TRANS_DDI_HDMI_SCRAMBLER_RESET_FREQ: u32 = ((1 << 6));

/// `TRANS_DDI_HDCP_SELECT` (intel_display_regs.h)
pub const TRANS_DDI_HDCP_SELECT: u32 = reg_bit(5);

/// `TRANS_DDI_BFI_ENABLE` (intel_display_regs.h)
pub const TRANS_DDI_BFI_ENABLE: u32 = ((1 << 4));

/// `TRANS_DDI_HIGH_TMDS_CHAR_RATE` (intel_display_regs.h)
pub const TRANS_DDI_HIGH_TMDS_CHAR_RATE: u32 = ((1 << 4));

/// `TRANS_DDI_PORT_WIDTH_MASK` (intel_display_regs.h)
pub const TRANS_DDI_PORT_WIDTH_MASK: u32 = reg_genmask(3, 1);

/// `TRANS_DDI_PORT_WIDTH(width)` (intel_display_regs.h)
pub const fn TRANS_DDI_PORT_WIDTH(width: u32) -> u32 {
    reg_field_prep(TRANS_DDI_PORT_WIDTH_MASK, ((width) - 1))
}

/// `TRANS_DDI_HDMI_SCRAMBLING` (intel_display_regs.h)
pub const TRANS_DDI_HDMI_SCRAMBLING: u32 = ((1 << 0));

/// `TRANS_DDI_HDMI_SCRAMBLING_MASK` (intel_display_regs.h)
pub const TRANS_DDI_HDMI_SCRAMBLING_MASK: u32 = (((TRANS_DDI_HDMI_SCRAMBLER_CTS_ENABLE | TRANS_DDI_HDMI_SCRAMBLER_RESET_FREQ) | TRANS_DDI_HDMI_SCRAMBLING));

/// `_TRANS_DDI_FUNC_CTL2_A` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_A: u32 = 0x60404;

/// `_TRANS_DDI_FUNC_CTL2_B` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_B: u32 = 0x61404;

/// `_TRANS_DDI_FUNC_CTL2_C` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_C: u32 = 0x62404;

/// `_TRANS_DDI_FUNC_CTL2_EDP` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_EDP: u32 = 0x6f404;

/// `_TRANS_DDI_FUNC_CTL2_DSI0` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_DSI0: u32 = 0x6b404;

/// `_TRANS_DDI_FUNC_CTL2_DSI1` (intel_display_regs.h)
pub const _TRANS_DDI_FUNC_CTL2_DSI1: u32 = 0x6bc04;

/// `PORT_SYNC_MODE_ENABLE` (intel_display_regs.h)
pub const PORT_SYNC_MODE_ENABLE: u32 = reg_bit(4);

/// `CMTG_SECONDARY_MODE` (intel_display_regs.h)
pub const CMTG_SECONDARY_MODE: u32 = reg_bit(3);

/// `PORT_SYNC_MODE_MASTER_SELECT_MASK` (intel_display_regs.h)
pub const PORT_SYNC_MODE_MASTER_SELECT_MASK: u32 = reg_genmask(2, 0);

/// `PORT_SYNC_MODE_MASTER_SELECT(x)` (intel_display_regs.h)
pub const fn PORT_SYNC_MODE_MASTER_SELECT(x: u32) -> u32 {
    reg_field_prep(PORT_SYNC_MODE_MASTER_SELECT_MASK, (x))
}

/// `TRANS_CMTG_CHICKEN` (intel_display_regs.h)
pub const TRANS_CMTG_CHICKEN: I915RegT = mmio(0x6fa90);

/// `DISABLE_DPT_CLK_GATING` (intel_display_regs.h)
pub const DISABLE_DPT_CLK_GATING: u32 = reg_bit(1);

/// `_DP_TP_CTL_A` (intel_display_regs.h)
pub const _DP_TP_CTL_A: u32 = 0x64040;

/// `_DP_TP_CTL_B` (intel_display_regs.h)
pub const _DP_TP_CTL_B: u32 = 0x64140;

/// `_TGL_DP_TP_CTL_A` (intel_display_regs.h)
pub const _TGL_DP_TP_CTL_A: u32 = 0x60540;

/// `DP_TP_CTL(port)` (intel_display_regs.h)
pub const fn DP_TP_CTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DP_TP_CTL_A, _DP_TP_CTL_B)
}

/// `DP_TP_CTL_ENABLE` (intel_display_regs.h)
pub const DP_TP_CTL_ENABLE: u32 = reg_bit(31);

/// `DP_TP_CTL_FEC_ENABLE` (intel_display_regs.h)
pub const DP_TP_CTL_FEC_ENABLE: u32 = reg_bit(30);

/// `DP_TP_CTL_MODE_MASK` (intel_display_regs.h)
pub const DP_TP_CTL_MODE_MASK: u32 = reg_bit(27);

/// `DP_TP_CTL_MODE_SST` (intel_display_regs.h)
pub const DP_TP_CTL_MODE_SST: u32 = reg_field_prep(DP_TP_CTL_MODE_MASK, 0);

/// `DP_TP_CTL_MODE_MST` (intel_display_regs.h)
pub const DP_TP_CTL_MODE_MST: u32 = reg_field_prep(DP_TP_CTL_MODE_MASK, 1);

/// `DP_TP_CTL_FORCE_ACT` (intel_display_regs.h)
pub const DP_TP_CTL_FORCE_ACT: u32 = reg_bit(25);

/// `DP_TP_CTL_TRAIN_PAT4_SEL_MASK` (intel_display_regs.h)
pub const DP_TP_CTL_TRAIN_PAT4_SEL_MASK: u32 = reg_genmask(20, 19);

/// `DP_TP_CTL_TRAIN_PAT4_SEL_TP4A` (intel_display_regs.h)
pub const DP_TP_CTL_TRAIN_PAT4_SEL_TP4A: u32 = reg_field_prep(DP_TP_CTL_TRAIN_PAT4_SEL_MASK, 0);

/// `DP_TP_CTL_TRAIN_PAT4_SEL_TP4B` (intel_display_regs.h)
pub const DP_TP_CTL_TRAIN_PAT4_SEL_TP4B: u32 = reg_field_prep(DP_TP_CTL_TRAIN_PAT4_SEL_MASK, 1);

/// `DP_TP_CTL_TRAIN_PAT4_SEL_TP4C` (intel_display_regs.h)
pub const DP_TP_CTL_TRAIN_PAT4_SEL_TP4C: u32 = reg_field_prep(DP_TP_CTL_TRAIN_PAT4_SEL_MASK, 2);

/// `DP_TP_CTL_ENHANCED_FRAME_ENABLE` (intel_display_regs.h)
pub const DP_TP_CTL_ENHANCED_FRAME_ENABLE: u32 = reg_bit(18);

/// `DP_TP_CTL_FDI_AUTOTRAIN` (intel_display_regs.h)
pub const DP_TP_CTL_FDI_AUTOTRAIN: u32 = reg_bit(15);

/// `DP_TP_CTL_LINK_TRAIN_MASK` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_MASK: u32 = reg_genmask(10, 8);

/// `DP_TP_CTL_LINK_TRAIN_PAT1` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_PAT1: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 0);

/// `DP_TP_CTL_LINK_TRAIN_PAT2` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_PAT2: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 1);

/// `DP_TP_CTL_LINK_TRAIN_PAT3` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_PAT3: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 4);

/// `DP_TP_CTL_LINK_TRAIN_PAT4` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_PAT4: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 5);

/// `DP_TP_CTL_LINK_TRAIN_IDLE` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_IDLE: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 2);

/// `DP_TP_CTL_LINK_TRAIN_NORMAL` (intel_display_regs.h)
pub const DP_TP_CTL_LINK_TRAIN_NORMAL: u32 = reg_field_prep(DP_TP_CTL_LINK_TRAIN_MASK, 3);

/// `DP_TP_CTL_SCRAMBLE_DISABLE` (intel_display_regs.h)
pub const DP_TP_CTL_SCRAMBLE_DISABLE: u32 = reg_bit(7);

/// `_DP_TP_STATUS_A` (intel_display_regs.h)
pub const _DP_TP_STATUS_A: u32 = 0x64044;

/// `_DP_TP_STATUS_B` (intel_display_regs.h)
pub const _DP_TP_STATUS_B: u32 = 0x64144;

/// `_TGL_DP_TP_STATUS_A` (intel_display_regs.h)
pub const _TGL_DP_TP_STATUS_A: u32 = 0x60544;

/// `DP_TP_STATUS(port)` (intel_display_regs.h)
pub const fn DP_TP_STATUS(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DP_TP_STATUS_A, _DP_TP_STATUS_B)
}

/// `DP_TP_STATUS_FEC_ENABLE_LIVE` (intel_display_regs.h)
pub const DP_TP_STATUS_FEC_ENABLE_LIVE: u32 = reg_bit(28);

/// `DP_TP_STATUS_IDLE_DONE` (intel_display_regs.h)
pub const DP_TP_STATUS_IDLE_DONE: u32 = reg_bit(25);

/// `DP_TP_STATUS_ACT_SENT` (intel_display_regs.h)
pub const DP_TP_STATUS_ACT_SENT: u32 = reg_bit(24);

/// `DP_TP_STATUS_MODE_STATUS_MST` (intel_display_regs.h)
pub const DP_TP_STATUS_MODE_STATUS_MST: u32 = reg_bit(23);

/// `DP_TP_STATUS_STREAMS_ENABLED_MASK` (intel_display_regs.h)
pub const DP_TP_STATUS_STREAMS_ENABLED_MASK: u32 = reg_genmask(18, 16);

/// `DP_TP_STATUS_AUTOTRAIN_DONE` (intel_display_regs.h)
pub const DP_TP_STATUS_AUTOTRAIN_DONE: u32 = reg_bit(12);

/// `DP_TP_STATUS_PAYLOAD_MAPPING_VC2_MASK` (intel_display_regs.h)
pub const DP_TP_STATUS_PAYLOAD_MAPPING_VC2_MASK: u32 = reg_genmask(9, 8);

/// `DP_TP_STATUS_PAYLOAD_MAPPING_VC1_MASK` (intel_display_regs.h)
pub const DP_TP_STATUS_PAYLOAD_MAPPING_VC1_MASK: u32 = reg_genmask(5, 4);

/// `DP_TP_STATUS_PAYLOAD_MAPPING_VC0_MASK` (intel_display_regs.h)
pub const DP_TP_STATUS_PAYLOAD_MAPPING_VC0_MASK: u32 = reg_genmask(1, 0);

/// `_DDI_BUF_CTL_A` (intel_display_regs.h)
pub const _DDI_BUF_CTL_A: u32 = 0x64000;

/// `_DDI_BUF_CTL_B` (intel_display_regs.h)
pub const _DDI_BUF_CTL_B: u32 = 0x64100;

/// `DDI_BUF_CTL(port)` (intel_display_regs.h)
pub const fn DDI_BUF_CTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DDI_BUF_CTL_A, _DDI_BUF_CTL_B)
}

/// `DDI_BUF_CTL_ENABLE` (intel_display_regs.h)
pub const DDI_BUF_CTL_ENABLE: u32 = reg_bit(31);

/// `XE2LPD_DDI_BUF_D2D_LINK_ENABLE` (intel_display_regs.h)
pub const XE2LPD_DDI_BUF_D2D_LINK_ENABLE: u32 = reg_bit(29);

/// `XE2LPD_DDI_BUF_D2D_LINK_STATE` (intel_display_regs.h)
pub const XE2LPD_DDI_BUF_D2D_LINK_STATE: u32 = reg_bit(28);

/// `DDI_BUF_EMP_MASK` (intel_display_regs.h)
pub const DDI_BUF_EMP_MASK: u32 = reg_genmask(27, 24);

/// `DDI_BUF_TRANS_SELECT(n)` (intel_display_regs.h)
pub const fn DDI_BUF_TRANS_SELECT(n: u32) -> u32 {
    reg_field_prep(DDI_BUF_EMP_MASK, (n))
}

/// `DDI_BUF_PHY_LINK_RATE_MASK` (intel_display_regs.h)
pub const DDI_BUF_PHY_LINK_RATE_MASK: u32 = reg_genmask(23, 20);

/// `DDI_BUF_PHY_LINK_RATE(r)` (intel_display_regs.h)
pub const fn DDI_BUF_PHY_LINK_RATE(r: u32) -> u32 {
    reg_field_prep(DDI_BUF_PHY_LINK_RATE_MASK, (r))
}

/// `DDI_BUF_PORT_DATA_MASK` (intel_display_regs.h)
pub const DDI_BUF_PORT_DATA_MASK: u32 = reg_genmask(19, 18);

/// `DDI_BUF_PORT_DATA_10BIT` (intel_display_regs.h)
pub const DDI_BUF_PORT_DATA_10BIT: u32 = reg_field_prep(DDI_BUF_PORT_DATA_MASK, 0);

/// `DDI_BUF_PORT_DATA_20BIT` (intel_display_regs.h)
pub const DDI_BUF_PORT_DATA_20BIT: u32 = reg_field_prep(DDI_BUF_PORT_DATA_MASK, 1);

/// `DDI_BUF_PORT_DATA_40BIT` (intel_display_regs.h)
pub const DDI_BUF_PORT_DATA_40BIT: u32 = reg_field_prep(DDI_BUF_PORT_DATA_MASK, 2);

/// `DDI_BUF_PORT_REVERSAL` (intel_display_regs.h)
pub const DDI_BUF_PORT_REVERSAL: u32 = reg_bit(16);

/// `DDI_BUF_LANE_STAGGER_DELAY_MASK` (intel_display_regs.h)
pub const DDI_BUF_LANE_STAGGER_DELAY_MASK: u32 = reg_genmask(15, 8);

/// `DDI_BUF_LANE_STAGGER_DELAY(symbols)` (intel_display_regs.h)
pub const fn DDI_BUF_LANE_STAGGER_DELAY(symbols: u32) -> u32 {
    reg_field_prep(DDI_BUF_LANE_STAGGER_DELAY_MASK, (symbols))
}

/// `DDI_BUF_IS_IDLE` (intel_display_regs.h)
pub const DDI_BUF_IS_IDLE: u32 = reg_bit(7);

/// `DDI_BUF_CTL_TC_PHY_OWNERSHIP` (intel_display_regs.h)
pub const DDI_BUF_CTL_TC_PHY_OWNERSHIP: u32 = reg_bit(6);

/// `DDI_A_4_LANES` (intel_display_regs.h)
pub const DDI_A_4_LANES: u32 = reg_bit(4);

/// `DDI_PORT_WIDTH_MASK` (intel_display_regs.h)
pub const DDI_PORT_WIDTH_MASK: u32 = reg_genmask(3, 1);

/// `DDI_PORT_WIDTH_SHIFT` (intel_display_regs.h)
pub const DDI_PORT_WIDTH_SHIFT: u32 = 1;

/// `DDI_INIT_DISPLAY_DETECTED` (intel_display_regs.h)
pub const DDI_INIT_DISPLAY_DETECTED: u32 = reg_bit(0);

/// `_DDI_BUF_TRANS_A` (intel_display_regs.h)
pub const _DDI_BUF_TRANS_A: u32 = 0x64E00;

/// `_DDI_BUF_TRANS_B` (intel_display_regs.h)
pub const _DDI_BUF_TRANS_B: u32 = 0x64E60;

/// `DDI_BUF_TRANS_LO(port, i)` (intel_display_regs.h)
pub const fn DDI_BUF_TRANS_LO(port: u32, i: u32) -> I915RegT {
    mmio((_PORT(port, _DDI_BUF_TRANS_A, _DDI_BUF_TRANS_B) + ((i) * 8)))
}

/// `DDI_BUF_BALANCE_LEG_ENABLE` (intel_display_regs.h)
pub const DDI_BUF_BALANCE_LEG_ENABLE: u32 = ((1 << 31));

/// `DDI_BUF_TRANS_HI(port, i)` (intel_display_regs.h)
pub const fn DDI_BUF_TRANS_HI(port: u32, i: u32) -> I915RegT {
    mmio(((_PORT(port, _DDI_BUF_TRANS_A, _DDI_BUF_TRANS_B) + ((i) * 8)) + 4))
}

/// `_DDI_DP_COMP_CTL_A` (intel_display_regs.h)
pub const _DDI_DP_COMP_CTL_A: u32 = 0x605F0;

/// `_DDI_DP_COMP_CTL_B` (intel_display_regs.h)
pub const _DDI_DP_COMP_CTL_B: u32 = 0x615F0;

/// `DDI_DP_COMP_CTL(pipe)` (intel_display_regs.h)
pub const fn DDI_DP_COMP_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _DDI_DP_COMP_CTL_A, _DDI_DP_COMP_CTL_B)
}

/// `DDI_DP_COMP_CTL_ENABLE` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_ENABLE: u32 = ((1 << 31));

/// `DDI_DP_COMP_CTL_D10_2` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_D10_2: u32 = ((0 << 28));

/// `DDI_DP_COMP_CTL_SCRAMBLED_0` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_SCRAMBLED_0: u32 = ((1 << 28));

/// `DDI_DP_COMP_CTL_PRBS7` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_PRBS7: u32 = ((2 << 28));

/// `DDI_DP_COMP_CTL_CUSTOM80` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_CUSTOM80: u32 = ((3 << 28));

/// `DDI_DP_COMP_CTL_HBR2` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_HBR2: u32 = ((4 << 28));

/// `DDI_DP_COMP_CTL_SCRAMBLED_1` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_SCRAMBLED_1: u32 = ((5 << 28));

/// `DDI_DP_COMP_CTL_HBR2_RESET` (intel_display_regs.h)
pub const DDI_DP_COMP_CTL_HBR2_RESET: u32 = ((0xFC << 0));

/// `_DDI_DP_COMP_PAT_A` (intel_display_regs.h)
pub const _DDI_DP_COMP_PAT_A: u32 = 0x605F4;

/// `_DDI_DP_COMP_PAT_B` (intel_display_regs.h)
pub const _DDI_DP_COMP_PAT_B: u32 = 0x615F4;

/// `DDI_DP_COMP_PAT(pipe, i)` (intel_display_regs.h)
pub const fn DDI_DP_COMP_PAT(pipe: u32, i: u32) -> I915RegT {
    mmio((_PIPE(pipe, _DDI_DP_COMP_PAT_A, _DDI_DP_COMP_PAT_B) + ((i) * 4)))
}

/// `PIXCLK_GATE` (intel_display_regs.h)
pub const PIXCLK_GATE: I915RegT = mmio(0xC6020);

/// `PIXCLK_GATE_UNGATE` (intel_display_regs.h)
pub const PIXCLK_GATE_UNGATE: u32 = ((1 << 0));

/// `PIXCLK_GATE_GATE` (intel_display_regs.h)
pub const PIXCLK_GATE_GATE: u32 = ((0 << 0));

/// `SPLL_CTL` (intel_display_regs.h)
pub const SPLL_CTL: I915RegT = mmio(0x46020);

/// `SPLL_PLL_ENABLE` (intel_display_regs.h)
pub const SPLL_PLL_ENABLE: u32 = ((1 << 31));

/// `SPLL_REF_BCLK` (intel_display_regs.h)
pub const SPLL_REF_BCLK: u32 = ((0 << 28));

/// `SPLL_REF_MUXED_SSC` (intel_display_regs.h)
pub const SPLL_REF_MUXED_SSC: u32 = ((1 << 28));

/// `SPLL_REF_NON_SSC_HSW` (intel_display_regs.h)
pub const SPLL_REF_NON_SSC_HSW: u32 = ((2 << 28));

/// `SPLL_REF_PCH_SSC_BDW` (intel_display_regs.h)
pub const SPLL_REF_PCH_SSC_BDW: u32 = ((2 << 28));

/// `SPLL_REF_LCPLL` (intel_display_regs.h)
pub const SPLL_REF_LCPLL: u32 = ((3 << 28));

/// `SPLL_REF_MASK` (intel_display_regs.h)
pub const SPLL_REF_MASK: u32 = ((3 << 28));

/// `SPLL_FREQ_810MHz` (intel_display_regs.h)
pub const SPLL_FREQ_810MHz: u32 = ((0 << 26));

/// `SPLL_FREQ_1350MHz` (intel_display_regs.h)
pub const SPLL_FREQ_1350MHz: u32 = ((1 << 26));

/// `SPLL_FREQ_2700MHz` (intel_display_regs.h)
pub const SPLL_FREQ_2700MHz: u32 = ((2 << 26));

/// `SPLL_FREQ_MASK` (intel_display_regs.h)
pub const SPLL_FREQ_MASK: u32 = ((3 << 26));

/// `_WRPLL_CTL1` (intel_display_regs.h)
pub const _WRPLL_CTL1: u32 = 0x46040;

/// `_WRPLL_CTL2` (intel_display_regs.h)
pub const _WRPLL_CTL2: u32 = 0x46060;

/// `WRPLL_CTL(pll)` (intel_display_regs.h)
pub const fn WRPLL_CTL(pll: u32) -> I915RegT {
    _MMIO_PIPE(pll, _WRPLL_CTL1, _WRPLL_CTL2)
}

/// `WRPLL_PLL_ENABLE` (intel_display_regs.h)
pub const WRPLL_PLL_ENABLE: u32 = ((1 << 31));

/// `WRPLL_REF_BCLK` (intel_display_regs.h)
pub const WRPLL_REF_BCLK: u32 = ((0 << 28));

/// `WRPLL_REF_PCH_SSC` (intel_display_regs.h)
pub const WRPLL_REF_PCH_SSC: u32 = ((1 << 28));

/// `WRPLL_REF_MUXED_SSC_BDW` (intel_display_regs.h)
pub const WRPLL_REF_MUXED_SSC_BDW: u32 = ((2 << 28));

/// `WRPLL_REF_SPECIAL_HSW` (intel_display_regs.h)
pub const WRPLL_REF_SPECIAL_HSW: u32 = ((2 << 28));

/// `WRPLL_REF_LCPLL` (intel_display_regs.h)
pub const WRPLL_REF_LCPLL: u32 = ((3 << 28));

/// `WRPLL_REF_MASK` (intel_display_regs.h)
pub const WRPLL_REF_MASK: u32 = ((3 << 28));

/// `WRPLL_DIVIDER_REFERENCE(x)` (intel_display_regs.h)
pub const fn WRPLL_DIVIDER_REFERENCE(x: u32) -> u32 {
    (((x) << 0))
}

/// `WRPLL_DIVIDER_REF_MASK` (intel_display_regs.h)
pub const WRPLL_DIVIDER_REF_MASK: u32 = (0xff);

/// `WRPLL_DIVIDER_POST(x)` (intel_display_regs.h)
pub const fn WRPLL_DIVIDER_POST(x: u32) -> u32 {
    (((x) << 8))
}

/// `WRPLL_DIVIDER_POST_MASK` (intel_display_regs.h)
pub const WRPLL_DIVIDER_POST_MASK: u32 = ((0x3f << 8));

/// `WRPLL_DIVIDER_POST_SHIFT` (intel_display_regs.h)
pub const WRPLL_DIVIDER_POST_SHIFT: u32 = 8;

/// `WRPLL_DIVIDER_FEEDBACK(x)` (intel_display_regs.h)
pub const fn WRPLL_DIVIDER_FEEDBACK(x: u32) -> u32 {
    (((x) << 16))
}

/// `WRPLL_DIVIDER_FB_SHIFT` (intel_display_regs.h)
pub const WRPLL_DIVIDER_FB_SHIFT: u32 = 16;

/// `WRPLL_DIVIDER_FB_MASK` (intel_display_regs.h)
pub const WRPLL_DIVIDER_FB_MASK: u32 = ((0xff << 16));

/// `_PORT_CLK_SEL_A` (intel_display_regs.h)
pub const _PORT_CLK_SEL_A: u32 = 0x46100;

/// `_PORT_CLK_SEL_B` (intel_display_regs.h)
pub const _PORT_CLK_SEL_B: u32 = 0x46104;

/// `PORT_CLK_SEL(port)` (intel_display_regs.h)
pub const fn PORT_CLK_SEL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _PORT_CLK_SEL_A, _PORT_CLK_SEL_B)
}

/// `PORT_CLK_SEL_MASK` (intel_display_regs.h)
pub const PORT_CLK_SEL_MASK: u32 = reg_genmask(31, 29);

/// `PORT_CLK_SEL_LCPLL_2700` (intel_display_regs.h)
pub const PORT_CLK_SEL_LCPLL_2700: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 0);

/// `PORT_CLK_SEL_LCPLL_1350` (intel_display_regs.h)
pub const PORT_CLK_SEL_LCPLL_1350: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 1);

/// `PORT_CLK_SEL_LCPLL_810` (intel_display_regs.h)
pub const PORT_CLK_SEL_LCPLL_810: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 2);

/// `PORT_CLK_SEL_SPLL` (intel_display_regs.h)
pub const PORT_CLK_SEL_SPLL: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 3);

/// `PORT_CLK_SEL_WRPLL(pll)` (intel_display_regs.h)
pub const fn PORT_CLK_SEL_WRPLL(pll: u32) -> u32 {
    reg_field_prep(PORT_CLK_SEL_MASK, (4 + (pll)))
}

/// `PORT_CLK_SEL_WRPLL1` (intel_display_regs.h)
pub const PORT_CLK_SEL_WRPLL1: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 4);

/// `PORT_CLK_SEL_WRPLL2` (intel_display_regs.h)
pub const PORT_CLK_SEL_WRPLL2: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 5);

/// `PORT_CLK_SEL_NONE` (intel_display_regs.h)
pub const PORT_CLK_SEL_NONE: u32 = reg_field_prep(PORT_CLK_SEL_MASK, 7);

/// `DDI_CLK_SEL(port)` (intel_display_regs.h)
pub const fn DDI_CLK_SEL(port: u32) -> I915RegT {
    PORT_CLK_SEL(port)
}

/// `DDI_CLK_SEL_MASK` (intel_display_regs.h)
pub const DDI_CLK_SEL_MASK: u32 = reg_genmask(31, 28);

/// `DDI_CLK_SEL_NONE` (intel_display_regs.h)
pub const DDI_CLK_SEL_NONE: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0x0);

/// `DDI_CLK_SEL_MG` (intel_display_regs.h)
pub const DDI_CLK_SEL_MG: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0x8);

/// `DDI_CLK_SEL_TBT_162` (intel_display_regs.h)
pub const DDI_CLK_SEL_TBT_162: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0xC);

/// `DDI_CLK_SEL_TBT_270` (intel_display_regs.h)
pub const DDI_CLK_SEL_TBT_270: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0xD);

/// `DDI_CLK_SEL_TBT_540` (intel_display_regs.h)
pub const DDI_CLK_SEL_TBT_540: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0xE);

/// `DDI_CLK_SEL_TBT_810` (intel_display_regs.h)
pub const DDI_CLK_SEL_TBT_810: u32 = reg_field_prep(DDI_CLK_SEL_MASK, 0xF);

/// `_TRANS_CLK_SEL_A` (intel_display_regs.h)
pub const _TRANS_CLK_SEL_A: u32 = 0x46140;

/// `_TRANS_CLK_SEL_B` (intel_display_regs.h)
pub const _TRANS_CLK_SEL_B: u32 = 0x46144;

/// `TRANS_CLK_SEL(tran)` (intel_display_regs.h)
pub const fn TRANS_CLK_SEL(tran: u32) -> I915RegT {
    _MMIO_TRANS(tran, _TRANS_CLK_SEL_A, _TRANS_CLK_SEL_B)
}

/// `TRANS_CLK_SEL_DISABLED` (intel_display_regs.h)
pub const TRANS_CLK_SEL_DISABLED: u32 = ((0x0 << 29));

/// `TRANS_CLK_SEL_PORT(x)` (intel_display_regs.h)
pub const fn TRANS_CLK_SEL_PORT(x: u32) -> u32 {
    (((((x) + 1)) << 29))
}

/// `TGL_TRANS_CLK_SEL_DISABLED` (intel_display_regs.h)
pub const TGL_TRANS_CLK_SEL_DISABLED: u32 = ((0x0 << 28));

/// `TGL_TRANS_CLK_SEL_PORT(x)` (intel_display_regs.h)
pub const fn TGL_TRANS_CLK_SEL_PORT(x: u32) -> u32 {
    (((((x) + 1)) << 28))
}

/// `CDCLK_FREQ` (intel_display_regs.h)
pub const CDCLK_FREQ: I915RegT = mmio(0x46200);

/// `_TRANSA_MSA_MISC` (intel_display_regs.h)
pub const _TRANSA_MSA_MISC: u32 = 0x60410;

/// `_TRANSB_MSA_MISC` (intel_display_regs.h)
pub const _TRANSB_MSA_MISC: u32 = 0x61410;

/// `_TRANSC_MSA_MISC` (intel_display_regs.h)
pub const _TRANSC_MSA_MISC: u32 = 0x62410;

/// `_TRANS_EDP_MSA_MISC` (intel_display_regs.h)
pub const _TRANS_EDP_MSA_MISC: u32 = 0x6f410;

/// `_TRANS_A_SET_CONTEXT_LATENCY` (intel_display_regs.h)
pub const _TRANS_A_SET_CONTEXT_LATENCY: u32 = 0x6007C;

/// `_TRANS_B_SET_CONTEXT_LATENCY` (intel_display_regs.h)
pub const _TRANS_B_SET_CONTEXT_LATENCY: u32 = 0x6107C;

/// `_TRANS_C_SET_CONTEXT_LATENCY` (intel_display_regs.h)
pub const _TRANS_C_SET_CONTEXT_LATENCY: u32 = 0x6207C;

/// `_TRANS_D_SET_CONTEXT_LATENCY` (intel_display_regs.h)
pub const _TRANS_D_SET_CONTEXT_LATENCY: u32 = 0x6307C;

/// `TRANS_SET_CONTEXT_LATENCY_MASK` (intel_display_regs.h)
pub const TRANS_SET_CONTEXT_LATENCY_MASK: u32 = reg_genmask(15, 0);

/// `TRANS_SET_CONTEXT_LATENCY_VALUE(x)` (intel_display_regs.h)
pub const fn TRANS_SET_CONTEXT_LATENCY_VALUE(x: u32) -> u32 {
    reg_field_prep(TRANS_SET_CONTEXT_LATENCY_MASK, (x))
}

/// `LCPLL_CTL` (intel_display_regs.h)
pub const LCPLL_CTL: I915RegT = mmio(0x130040);

/// `LCPLL_PLL_DISABLE` (intel_display_regs.h)
pub const LCPLL_PLL_DISABLE: u32 = ((1 << 31));

/// `LCPLL_PLL_LOCK` (intel_display_regs.h)
pub const LCPLL_PLL_LOCK: u32 = ((1 << 30));

/// `LCPLL_REF_NON_SSC` (intel_display_regs.h)
pub const LCPLL_REF_NON_SSC: u32 = ((0 << 28));

/// `LCPLL_REF_BCLK` (intel_display_regs.h)
pub const LCPLL_REF_BCLK: u32 = ((2 << 28));

/// `LCPLL_REF_PCH_SSC` (intel_display_regs.h)
pub const LCPLL_REF_PCH_SSC: u32 = ((3 << 28));

/// `LCPLL_REF_MASK` (intel_display_regs.h)
pub const LCPLL_REF_MASK: u32 = ((3 << 28));

/// `LCPLL_CLK_FREQ_MASK` (intel_display_regs.h)
pub const LCPLL_CLK_FREQ_MASK: u32 = ((3 << 26));

/// `LCPLL_CLK_FREQ_450` (intel_display_regs.h)
pub const LCPLL_CLK_FREQ_450: u32 = ((0 << 26));

/// `LCPLL_CLK_FREQ_54O_BDW` (intel_display_regs.h)
pub const LCPLL_CLK_FREQ_54O_BDW: u32 = ((1 << 26));

/// `LCPLL_CLK_FREQ_337_5_BDW` (intel_display_regs.h)
pub const LCPLL_CLK_FREQ_337_5_BDW: u32 = ((2 << 26));

/// `LCPLL_CLK_FREQ_675_BDW` (intel_display_regs.h)
pub const LCPLL_CLK_FREQ_675_BDW: u32 = ((3 << 26));

/// `LCPLL_CD_CLOCK_DISABLE` (intel_display_regs.h)
pub const LCPLL_CD_CLOCK_DISABLE: u32 = ((1 << 25));

/// `LCPLL_ROOT_CD_CLOCK_DISABLE` (intel_display_regs.h)
pub const LCPLL_ROOT_CD_CLOCK_DISABLE: u32 = ((1 << 24));

/// `LCPLL_CD2X_CLOCK_DISABLE` (intel_display_regs.h)
pub const LCPLL_CD2X_CLOCK_DISABLE: u32 = ((1 << 23));

/// `LCPLL_POWER_DOWN_ALLOW` (intel_display_regs.h)
pub const LCPLL_POWER_DOWN_ALLOW: u32 = ((1 << 22));

/// `LCPLL_CD_SOURCE_FCLK` (intel_display_regs.h)
pub const LCPLL_CD_SOURCE_FCLK: u32 = ((1 << 21));

/// `LCPLL_CD_SOURCE_FCLK_DONE` (intel_display_regs.h)
pub const LCPLL_CD_SOURCE_FCLK_DONE: u32 = ((1 << 19));

/// `CDCLK_CTL` (intel_display_regs.h)
pub const CDCLK_CTL: I915RegT = mmio(0x46000);

/// `CDCLK_FREQ_SEL_MASK` (intel_display_regs.h)
pub const CDCLK_FREQ_SEL_MASK: u32 = reg_genmask(27, 26);

/// `CDCLK_FREQ_450_432` (intel_display_regs.h)
pub const CDCLK_FREQ_450_432: u32 = reg_field_prep(CDCLK_FREQ_SEL_MASK, 0);

/// `CDCLK_FREQ_540` (intel_display_regs.h)
pub const CDCLK_FREQ_540: u32 = reg_field_prep(CDCLK_FREQ_SEL_MASK, 1);

/// `CDCLK_FREQ_337_308` (intel_display_regs.h)
pub const CDCLK_FREQ_337_308: u32 = reg_field_prep(CDCLK_FREQ_SEL_MASK, 2);

/// `CDCLK_FREQ_675_617` (intel_display_regs.h)
pub const CDCLK_FREQ_675_617: u32 = reg_field_prep(CDCLK_FREQ_SEL_MASK, 3);

/// `MDCLK_SOURCE_SEL_MASK` (intel_display_regs.h)
pub const MDCLK_SOURCE_SEL_MASK: u32 = reg_genmask(25, 25);

/// `MDCLK_SOURCE_SEL_CD2XCLK` (intel_display_regs.h)
pub const MDCLK_SOURCE_SEL_CD2XCLK: u32 = reg_field_prep(MDCLK_SOURCE_SEL_MASK, 0);

/// `MDCLK_SOURCE_SEL_CDCLK_PLL` (intel_display_regs.h)
pub const MDCLK_SOURCE_SEL_CDCLK_PLL: u32 = reg_field_prep(MDCLK_SOURCE_SEL_MASK, 1);

/// `BXT_CDCLK_CD2X_DIV_SEL_MASK` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_DIV_SEL_MASK: u32 = reg_genmask(23, 22);

/// `BXT_CDCLK_CD2X_DIV_SEL_1` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_DIV_SEL_1: u32 = reg_field_prep(BXT_CDCLK_CD2X_DIV_SEL_MASK, 0);

/// `BXT_CDCLK_CD2X_DIV_SEL_1_5` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_DIV_SEL_1_5: u32 = reg_field_prep(BXT_CDCLK_CD2X_DIV_SEL_MASK, 1);

/// `BXT_CDCLK_CD2X_DIV_SEL_2` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_DIV_SEL_2: u32 = reg_field_prep(BXT_CDCLK_CD2X_DIV_SEL_MASK, 2);

/// `BXT_CDCLK_CD2X_DIV_SEL_4` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_DIV_SEL_4: u32 = reg_field_prep(BXT_CDCLK_CD2X_DIV_SEL_MASK, 3);

/// `BXT_CDCLK_CD2X_PIPE(pipe)` (intel_display_regs.h)
pub const fn BXT_CDCLK_CD2X_PIPE(pipe: u32) -> u32 {
    (((pipe) << 20))
}

/// `CDCLK_DIVMUX_CD_OVERRIDE` (intel_display_regs.h)
pub const CDCLK_DIVMUX_CD_OVERRIDE: u32 = ((1 << 19));

/// `BXT_CDCLK_CD2X_PIPE_NONE` (intel_display_regs.h)
pub const BXT_CDCLK_CD2X_PIPE_NONE: u32 = BXT_CDCLK_CD2X_PIPE(3);

/// `ICL_CDCLK_CD2X_PIPE(pipe)` (intel_display_regs.h)
pub const fn ICL_CDCLK_CD2X_PIPE(pipe: u32) -> u32 {
    ((pick(pipe, &[0, 2, 6]) << 19))
}

/// `ICL_CDCLK_CD2X_PIPE_NONE` (intel_display_regs.h)
pub const ICL_CDCLK_CD2X_PIPE_NONE: u32 = ((7 << 19));

/// `TGL_CDCLK_CD2X_PIPE(pipe)` (intel_display_regs.h)
pub const fn TGL_CDCLK_CD2X_PIPE(pipe: u32) -> u32 {
    BXT_CDCLK_CD2X_PIPE(pipe)
}

/// `TGL_CDCLK_CD2X_PIPE_NONE` (intel_display_regs.h)
pub const TGL_CDCLK_CD2X_PIPE_NONE: u32 = ICL_CDCLK_CD2X_PIPE_NONE;

/// `BXT_CDCLK_SSA_PRECHARGE_ENABLE` (intel_display_regs.h)
pub const BXT_CDCLK_SSA_PRECHARGE_ENABLE: u32 = ((1 << 16));

/// `CDCLK_FREQ_DECIMAL_MASK` (intel_display_regs.h)
pub const CDCLK_FREQ_DECIMAL_MASK: u32 = (0x7ff);

/// `CDCLK_SQUASH_CTL` (intel_display_regs.h)
pub const CDCLK_SQUASH_CTL: I915RegT = mmio(0x46008);

/// `CDCLK_SQUASH_ENABLE` (intel_display_regs.h)
pub const CDCLK_SQUASH_ENABLE: u32 = reg_bit(31);

/// `CDCLK_SQUASH_WINDOW_SIZE_MASK` (intel_display_regs.h)
pub const CDCLK_SQUASH_WINDOW_SIZE_MASK: u32 = reg_genmask(27, 24);

/// `CDCLK_SQUASH_WINDOW_SIZE(x)` (intel_display_regs.h)
pub const fn CDCLK_SQUASH_WINDOW_SIZE(x: u32) -> u32 {
    reg_field_prep(CDCLK_SQUASH_WINDOW_SIZE_MASK, (x))
}

/// `CDCLK_SQUASH_WAVEFORM_MASK` (intel_display_regs.h)
pub const CDCLK_SQUASH_WAVEFORM_MASK: u32 = reg_genmask(15, 0);

/// `CDCLK_SQUASH_WAVEFORM(x)` (intel_display_regs.h)
pub const fn CDCLK_SQUASH_WAVEFORM(x: u32) -> u32 {
    reg_field_prep(CDCLK_SQUASH_WAVEFORM_MASK, (x))
}

/// `LCPLL1_CTL` (intel_display_regs.h)
pub const LCPLL1_CTL: I915RegT = mmio(0x46010);

/// `LCPLL2_CTL` (intel_display_regs.h)
pub const LCPLL2_CTL: I915RegT = mmio(0x46014);

/// `LCPLL_PLL_ENABLE` (intel_display_regs.h)
pub const LCPLL_PLL_ENABLE: u32 = ((1 << 31));

/// `DPLL_CTRL1` (intel_display_regs.h)
pub const DPLL_CTRL1: I915RegT = mmio(0x6C058);

/// `DPLL_CTRL1_HDMI_MODE(id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_HDMI_MODE(id: u32) -> u32 {
    ((1 << ((((id) * 6) + 5))))
}

/// `DPLL_CTRL1_SSC(id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_SSC(id: u32) -> u32 {
    ((1 << ((((id) * 6) + 4))))
}

/// `DPLL_CTRL1_LINK_RATE_MASK(id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_LINK_RATE_MASK(id: u32) -> u32 {
    ((7 << ((((id) * 6) + 1))))
}

/// `DPLL_CTRL1_LINK_RATE_SHIFT(id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_LINK_RATE_SHIFT(id: u32) -> u32 {
    ((((id) * 6) + 1))
}

/// `DPLL_CTRL1_LINK_RATE(linkrate, id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_LINK_RATE(linkrate: u32, id: u32) -> u32 {
    (((linkrate) << ((((id) * 6) + 1))))
}

/// `DPLL_CTRL1_OVERRIDE(id)` (intel_display_regs.h)
pub const fn DPLL_CTRL1_OVERRIDE(id: u32) -> u32 {
    ((1 << (((id) * 6))))
}

/// `DPLL_CTRL1_LINK_RATE_2700` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_2700: u32 = 0;

/// `DPLL_CTRL1_LINK_RATE_1350` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_1350: u32 = 1;

/// `DPLL_CTRL1_LINK_RATE_810` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_810: u32 = 2;

/// `DPLL_CTRL1_LINK_RATE_1620` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_1620: u32 = 3;

/// `DPLL_CTRL1_LINK_RATE_1080` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_1080: u32 = 4;

/// `DPLL_CTRL1_LINK_RATE_2160` (intel_display_regs.h)
pub const DPLL_CTRL1_LINK_RATE_2160: u32 = 5;

/// `DPLL_CTRL2` (intel_display_regs.h)
pub const DPLL_CTRL2: I915RegT = mmio(0x6C05C);

/// `DPLL_CTRL2_DDI_CLK_OFF(port)` (intel_display_regs.h)
pub const fn DPLL_CTRL2_DDI_CLK_OFF(port: u32) -> u32 {
    ((1 << (((port) + 15))))
}

/// `DPLL_CTRL2_DDI_CLK_SEL_MASK(port)` (intel_display_regs.h)
pub const fn DPLL_CTRL2_DDI_CLK_SEL_MASK(port: u32) -> u32 {
    ((3 << ((((port) * 3) + 1))))
}

/// `DPLL_CTRL2_DDI_CLK_SEL_SHIFT(port)` (intel_display_regs.h)
pub const fn DPLL_CTRL2_DDI_CLK_SEL_SHIFT(port: u32) -> u32 {
    ((((port) * 3) + 1))
}

/// `DPLL_CTRL2_DDI_CLK_SEL(clk, port)` (intel_display_regs.h)
pub const fn DPLL_CTRL2_DDI_CLK_SEL(clk: u32, port: u32) -> u32 {
    (((clk) << ((((port) * 3) + 1))))
}

/// `DPLL_CTRL2_DDI_SEL_OVERRIDE(port)` (intel_display_regs.h)
pub const fn DPLL_CTRL2_DDI_SEL_OVERRIDE(port: u32) -> u32 {
    ((1 << (((port) * 3))))
}

/// `DPLL_STATUS` (intel_display_regs.h)
pub const DPLL_STATUS: I915RegT = mmio(0x6C060);

/// `DPLL_LOCK(id)` (intel_display_regs.h)
pub const fn DPLL_LOCK(id: u32) -> u32 {
    ((1 << (((id) * 8))))
}

/// `_DPLL1_CFGCR1` (intel_display_regs.h)
pub const _DPLL1_CFGCR1: u32 = 0x6C040;

/// `_DPLL2_CFGCR1` (intel_display_regs.h)
pub const _DPLL2_CFGCR1: u32 = 0x6C048;

/// `_DPLL3_CFGCR1` (intel_display_regs.h)
pub const _DPLL3_CFGCR1: u32 = 0x6C050;

/// `DPLL_CFGCR1_FREQ_ENABLE` (intel_display_regs.h)
pub const DPLL_CFGCR1_FREQ_ENABLE: u32 = ((1 << 31));

/// `DPLL_CFGCR1_DCO_FRACTION_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR1_DCO_FRACTION_MASK: u32 = ((0x7fff << 9));

/// `DPLL_CFGCR1_DCO_FRACTION(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1_DCO_FRACTION(x: u32) -> u32 {
    (((x) << 9))
}

/// `DPLL_CFGCR1_DCO_INTEGER_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR1_DCO_INTEGER_MASK: u32 = (0x1ff);

/// `_DPLL1_CFGCR2` (intel_display_regs.h)
pub const _DPLL1_CFGCR2: u32 = 0x6C044;

/// `_DPLL2_CFGCR2` (intel_display_regs.h)
pub const _DPLL2_CFGCR2: u32 = 0x6C04C;

/// `_DPLL3_CFGCR2` (intel_display_regs.h)
pub const _DPLL3_CFGCR2: u32 = 0x6C054;

/// `DPLL_CFGCR2_QDIV_RATIO_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR2_QDIV_RATIO_MASK: u32 = ((0xff << 8));

/// `DPLL_CFGCR2_QDIV_RATIO(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR2_QDIV_RATIO(x: u32) -> u32 {
    (((x) << 8))
}

/// `DPLL_CFGCR2_QDIV_MODE(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR2_QDIV_MODE(x: u32) -> u32 {
    (((x) << 7))
}

/// `DPLL_CFGCR2_KDIV_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR2_KDIV_MASK: u32 = ((3 << 5));

/// `DPLL_CFGCR2_KDIV(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR2_KDIV(x: u32) -> u32 {
    (((x) << 5))
}

/// `DPLL_CFGCR2_KDIV_5` (intel_display_regs.h)
pub const DPLL_CFGCR2_KDIV_5: u32 = ((0 << 5));

/// `DPLL_CFGCR2_KDIV_2` (intel_display_regs.h)
pub const DPLL_CFGCR2_KDIV_2: u32 = ((1 << 5));

/// `DPLL_CFGCR2_KDIV_3` (intel_display_regs.h)
pub const DPLL_CFGCR2_KDIV_3: u32 = ((2 << 5));

/// `DPLL_CFGCR2_KDIV_1` (intel_display_regs.h)
pub const DPLL_CFGCR2_KDIV_1: u32 = ((3 << 5));

/// `DPLL_CFGCR2_PDIV_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_MASK: u32 = ((7 << 2));

/// `DPLL_CFGCR2_PDIV(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR2_PDIV(x: u32) -> u32 {
    (((x) << 2))
}

/// `DPLL_CFGCR2_PDIV_1` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_1: u32 = ((0 << 2));

/// `DPLL_CFGCR2_PDIV_2` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_2: u32 = ((1 << 2));

/// `DPLL_CFGCR2_PDIV_3` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_3: u32 = ((2 << 2));

/// `DPLL_CFGCR2_PDIV_7` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_7: u32 = ((4 << 2));

/// `DPLL_CFGCR2_PDIV_7_INVALID` (intel_display_regs.h)
pub const DPLL_CFGCR2_PDIV_7_INVALID: u32 = ((5 << 2));

/// `DPLL_CFGCR2_CENTRAL_FREQ_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR2_CENTRAL_FREQ_MASK: u32 = (3);

/// `ICL_DPCLKA_CFGCR0` (intel_display_regs.h)
pub const ICL_DPCLKA_CFGCR0: I915RegT = mmio(0x164280);

/// `ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy)` (intel_display_regs.h)
pub const fn ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy: u32) -> u32 {
    ((1 << pick(phy, &[10, 11, 24, 4, 5])))
}

/// `RKL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy)` (intel_display_regs.h)
pub const fn RKL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy: u32) -> u32 {
    reg_bit(((phy) + 10))
}

/// `ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)` (intel_display_regs.h)
pub const fn ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy: u32) -> u32 {
    (((phy) * 2))
}

/// `ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy)` (intel_display_regs.h)
pub const fn ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy: u32) -> u32 {
    ((3 << ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `ICL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll, phy)` (intel_display_regs.h)
pub const fn ICL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll: u32, phy: u32) -> u32 {
    (((pll) << ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)` (intel_display_regs.h)
pub const fn RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy: u32) -> u32 {
    pick(phy, &[0, 2, 4, 27])
}

/// `RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy)` (intel_display_regs.h)
pub const fn RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy: u32) -> u32 {
    ((3 << RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `RKL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll, phy)` (intel_display_regs.h)
pub const fn RKL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll: u32, phy: u32) -> u32 {
    (((pll) << RKL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `_DG1_DPCLKA_CFGCR0` (intel_display_regs.h)
pub const _DG1_DPCLKA_CFGCR0: u32 = 0x164280;

/// `_DG1_DPCLKA1_CFGCR0` (intel_display_regs.h)
pub const _DG1_DPCLKA1_CFGCR0: u32 = 0x16C280;

/// `_DG1_DPCLKA_PHY_IDX(phy)` (intel_display_regs.h)
pub const fn _DG1_DPCLKA_PHY_IDX(phy: u32) -> u32 {
    (((phy) % 2))
}

/// `_DG1_DPCLKA_PLL_IDX(pll)` (intel_display_regs.h)
pub const fn _DG1_DPCLKA_PLL_IDX(pll: u32) -> u32 {
    (((pll) % 2))
}

/// `DG1_DPCLKA_CFGCR0(phy)` (intel_display_regs.h)
pub const fn DG1_DPCLKA_CFGCR0(phy: u32) -> I915RegT {
    _MMIO_PHY(((phy) / 2), _DG1_DPCLKA_CFGCR0, _DG1_DPCLKA1_CFGCR0)
}

/// `DG1_DPCLKA_CFGCR0_DDI_CLK_OFF(phy)` (intel_display_regs.h)
pub const fn DG1_DPCLKA_CFGCR0_DDI_CLK_OFF(phy: u32) -> u32 {
    reg_bit((_DG1_DPCLKA_PHY_IDX(phy) + 10))
}

/// `DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)` (intel_display_regs.h)
pub const fn DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy: u32) -> u32 {
    ((_DG1_DPCLKA_PHY_IDX(phy) * 2))
}

/// `DG1_DPCLKA_CFGCR0_DDI_CLK_SEL(pll, phy)` (intel_display_regs.h)
pub const fn DG1_DPCLKA_CFGCR0_DDI_CLK_SEL(pll: u32, phy: u32) -> u32 {
    ((_DG1_DPCLKA_PLL_IDX(pll) << DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy)` (intel_display_regs.h)
pub const fn DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_MASK(phy: u32) -> u32 {
    ((0x3 << DG1_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy)))
}

/// `_ADLS_DPCLKA_CFGCR0` (intel_display_regs.h)
pub const _ADLS_DPCLKA_CFGCR0: u32 = 0x164280;

/// `_ADLS_DPCLKA_CFGCR1` (intel_display_regs.h)
pub const _ADLS_DPCLKA_CFGCR1: u32 = 0x1642BC;

/// `ADLS_DPCLKA_CFGCR(phy)` (intel_display_regs.h)
pub const fn ADLS_DPCLKA_CFGCR(phy: u32) -> I915RegT {
    _MMIO_PHY(((phy) / 3), _ADLS_DPCLKA_CFGCR0, _ADLS_DPCLKA_CFGCR1)
}

/// `ADLS_DPCLKA_CFGCR_DDI_SHIFT(phy)` (intel_display_regs.h)
pub const fn ADLS_DPCLKA_CFGCR_DDI_SHIFT(phy: u32) -> u32 {
    (((((phy) % 3)) * 2))
}

/// `ADLS_DPCLKA_DDII_SEL_MASK` (intel_display_regs.h)
pub const ADLS_DPCLKA_DDII_SEL_MASK: u32 = reg_genmask(5, 4);

/// `ADLS_DPCLKA_DDIB_SEL_MASK` (intel_display_regs.h)
pub const ADLS_DPCLKA_DDIB_SEL_MASK: u32 = reg_genmask(3, 2);

/// `ADLS_DPCLKA_DDIA_SEL_MASK` (intel_display_regs.h)
pub const ADLS_DPCLKA_DDIA_SEL_MASK: u32 = reg_genmask(1, 0);

/// `ADLS_DPCLKA_DDIK_SEL_MASK` (intel_display_regs.h)
pub const ADLS_DPCLKA_DDIK_SEL_MASK: u32 = reg_genmask(3, 2);

/// `ADLS_DPCLKA_DDIJ_SEL_MASK` (intel_display_regs.h)
pub const ADLS_DPCLKA_DDIJ_SEL_MASK: u32 = reg_genmask(1, 0);

/// `ADLS_DPCLKA_CFGCR_DDI_CLK_SEL_MASK(phy)` (intel_display_regs.h)
pub const fn ADLS_DPCLKA_CFGCR_DDI_CLK_SEL_MASK(phy: u32) -> u32 {
    pick((phy), &[ADLS_DPCLKA_DDIA_SEL_MASK, ADLS_DPCLKA_DDIB_SEL_MASK, ADLS_DPCLKA_DDII_SEL_MASK, ADLS_DPCLKA_DDIJ_SEL_MASK, ADLS_DPCLKA_DDIK_SEL_MASK])
}

/// `_DPLL0_ENABLE` (intel_display_regs.h)
pub const _DPLL0_ENABLE: u32 = 0x46010;

/// `_DPLL1_ENABLE` (intel_display_regs.h)
pub const _DPLL1_ENABLE: u32 = 0x46014;

/// `_ADLS_DPLL2_ENABLE` (intel_display_regs.h)
pub const _ADLS_DPLL2_ENABLE: u32 = 0x46018;

/// `_ADLS_DPLL3_ENABLE` (intel_display_regs.h)
pub const _ADLS_DPLL3_ENABLE: u32 = 0x46030;

/// `PLL_ENABLE` (intel_display_regs.h)
pub const PLL_ENABLE: u32 = reg_bit(31);

/// `PLL_LOCK` (intel_display_regs.h)
pub const PLL_LOCK: u32 = reg_bit(30);

/// `PLL_POWER_ENABLE` (intel_display_regs.h)
pub const PLL_POWER_ENABLE: u32 = reg_bit(27);

/// `PLL_POWER_STATE` (intel_display_regs.h)
pub const PLL_POWER_STATE: u32 = reg_bit(26);

/// `ICL_DPLL_ENABLE(pll)` (intel_display_regs.h)
pub const fn ICL_DPLL_ENABLE(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 3, _DPLL0_ENABLE, _DPLL1_ENABLE, _ADLS_DPLL3_ENABLE, _ADLS_DPLL3_ENABLE))
}

/// `_DG2_PLL3_ENABLE` (intel_display_regs.h)
pub const _DG2_PLL3_ENABLE: u32 = 0x4601C;

/// `DG2_PLL_ENABLE(pll)` (intel_display_regs.h)
pub const fn DG2_PLL_ENABLE(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 3, _DPLL0_ENABLE, _DPLL1_ENABLE, _DG2_PLL3_ENABLE, _DG2_PLL3_ENABLE))
}

/// `TBT_PLL_ENABLE` (intel_display_regs.h)
pub const TBT_PLL_ENABLE: I915RegT = mmio(0x46020);

/// `_MG_PLL1_ENABLE` (intel_display_regs.h)
pub const _MG_PLL1_ENABLE: u32 = 0x46030;

/// `_MG_PLL2_ENABLE` (intel_display_regs.h)
pub const _MG_PLL2_ENABLE: u32 = 0x46034;

/// `_MG_PLL3_ENABLE` (intel_display_regs.h)
pub const _MG_PLL3_ENABLE: u32 = 0x46038;

/// `_MG_PLL4_ENABLE` (intel_display_regs.h)
pub const _MG_PLL4_ENABLE: u32 = 0x4603C;

/// `MG_PLL_ENABLE(tc_port)` (intel_display_regs.h)
pub const fn MG_PLL_ENABLE(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), _MG_PLL1_ENABLE, _MG_PLL2_ENABLE)
}

/// `DG1_DPLL_ENABLE(pll)` (intel_display_regs.h)
pub const fn DG1_DPLL_ENABLE(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _DPLL0_ENABLE, _DPLL1_ENABLE, _MG_PLL1_ENABLE, _MG_PLL2_ENABLE))
}

/// `PORTTC1_PLL_ENABLE` (intel_display_regs.h)
pub const PORTTC1_PLL_ENABLE: u32 = 0x46038;

/// `PORTTC2_PLL_ENABLE` (intel_display_regs.h)
pub const PORTTC2_PLL_ENABLE: u32 = 0x46040;

/// `ADLP_PORTTC_PLL_ENABLE(tc_port)` (intel_display_regs.h)
pub const fn ADLP_PORTTC_PLL_ENABLE(tc_port: u32) -> I915RegT {
    _MMIO_PORT((tc_port), PORTTC1_PLL_ENABLE, PORTTC2_PLL_ENABLE)
}

/// `_ICL_DPLL0_CFGCR0` (intel_display_regs.h)
pub const _ICL_DPLL0_CFGCR0: u32 = 0x164000;

/// `_ICL_DPLL1_CFGCR0` (intel_display_regs.h)
pub const _ICL_DPLL1_CFGCR0: u32 = 0x164080;

/// `ICL_DPLL_CFGCR0(pll)` (intel_display_regs.h)
pub const fn ICL_DPLL_CFGCR0(pll: u32) -> I915RegT {
    _MMIO_PLL(pll, _ICL_DPLL0_CFGCR0, _ICL_DPLL1_CFGCR0)
}

/// `DPLL_CFGCR0_HDMI_MODE` (intel_display_regs.h)
pub const DPLL_CFGCR0_HDMI_MODE: u32 = ((1 << 30));

/// `DPLL_CFGCR0_SSC_ENABLE` (intel_display_regs.h)
pub const DPLL_CFGCR0_SSC_ENABLE: u32 = ((1 << 29));

/// `DPLL_CFGCR0_SSC_ENABLE_ICL` (intel_display_regs.h)
pub const DPLL_CFGCR0_SSC_ENABLE_ICL: u32 = ((1 << 25));

/// `DPLL_CFGCR0_LINK_RATE_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_MASK: u32 = ((0xf << 25));

/// `DPLL_CFGCR0_LINK_RATE_2700` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_2700: u32 = ((0 << 25));

/// `DPLL_CFGCR0_LINK_RATE_1350` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_1350: u32 = ((1 << 25));

/// `DPLL_CFGCR0_LINK_RATE_810` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_810: u32 = ((2 << 25));

/// `DPLL_CFGCR0_LINK_RATE_1620` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_1620: u32 = ((3 << 25));

/// `DPLL_CFGCR0_LINK_RATE_1080` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_1080: u32 = ((4 << 25));

/// `DPLL_CFGCR0_LINK_RATE_2160` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_2160: u32 = ((5 << 25));

/// `DPLL_CFGCR0_LINK_RATE_3240` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_3240: u32 = ((6 << 25));

/// `DPLL_CFGCR0_LINK_RATE_4050` (intel_display_regs.h)
pub const DPLL_CFGCR0_LINK_RATE_4050: u32 = ((7 << 25));

/// `DPLL_CFGCR0_DCO_FRACTION_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR0_DCO_FRACTION_MASK: u32 = ((0x7fff << 10));

/// `DPLL_CFGCR0_DCO_FRACTION_SHIFT` (intel_display_regs.h)
pub const DPLL_CFGCR0_DCO_FRACTION_SHIFT: u32 = (10);

/// `DPLL_CFGCR0_DCO_FRACTION(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR0_DCO_FRACTION(x: u32) -> u32 {
    (((x) << 10))
}

/// `DPLL_CFGCR0_DCO_INTEGER_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR0_DCO_INTEGER_MASK: u32 = (0x3ff);

/// `_ICL_DPLL0_CFGCR1` (intel_display_regs.h)
pub const _ICL_DPLL0_CFGCR1: u32 = 0x164004;

/// `_ICL_DPLL1_CFGCR1` (intel_display_regs.h)
pub const _ICL_DPLL1_CFGCR1: u32 = 0x164084;

/// `ICL_DPLL_CFGCR1(pll)` (intel_display_regs.h)
pub const fn ICL_DPLL_CFGCR1(pll: u32) -> I915RegT {
    _MMIO_PLL(pll, _ICL_DPLL0_CFGCR1, _ICL_DPLL1_CFGCR1)
}

/// `DPLL_CFGCR1_QDIV_RATIO_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR1_QDIV_RATIO_MASK: u32 = ((0xff << 10));

/// `DPLL_CFGCR1_QDIV_RATIO_SHIFT` (intel_display_regs.h)
pub const DPLL_CFGCR1_QDIV_RATIO_SHIFT: u32 = (10);

/// `DPLL_CFGCR1_QDIV_RATIO(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1_QDIV_RATIO(x: u32) -> u32 {
    (((x) << 10))
}

/// `DPLL_CFGCR1_QDIV_MODE_SHIFT` (intel_display_regs.h)
pub const DPLL_CFGCR1_QDIV_MODE_SHIFT: u32 = (9);

/// `DPLL_CFGCR1_QDIV_MODE(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1_QDIV_MODE(x: u32) -> u32 {
    (((x) << 9))
}

/// `DPLL_CFGCR1_KDIV_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR1_KDIV_MASK: u32 = ((7 << 6));

/// `DPLL_CFGCR1_KDIV_SHIFT` (intel_display_regs.h)
pub const DPLL_CFGCR1_KDIV_SHIFT: u32 = (6);

/// `DPLL_CFGCR1_KDIV(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1_KDIV(x: u32) -> u32 {
    (((x) << 6))
}

/// `DPLL_CFGCR1_KDIV_1` (intel_display_regs.h)
pub const DPLL_CFGCR1_KDIV_1: u32 = ((1 << 6));

/// `DPLL_CFGCR1_KDIV_2` (intel_display_regs.h)
pub const DPLL_CFGCR1_KDIV_2: u32 = ((2 << 6));

/// `DPLL_CFGCR1_KDIV_3` (intel_display_regs.h)
pub const DPLL_CFGCR1_KDIV_3: u32 = ((4 << 6));

/// `DPLL_CFGCR1_PDIV_MASK` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_MASK: u32 = ((0xf << 2));

/// `DPLL_CFGCR1_PDIV_SHIFT` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_SHIFT: u32 = (2);

/// `DPLL_CFGCR1_PDIV(x)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1_PDIV(x: u32) -> u32 {
    (((x) << 2))
}

/// `DPLL_CFGCR1_PDIV_2` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_2: u32 = ((1 << 2));

/// `DPLL_CFGCR1_PDIV_3` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_3: u32 = ((2 << 2));

/// `DPLL_CFGCR1_PDIV_5` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_5: u32 = ((4 << 2));

/// `DPLL_CFGCR1_PDIV_7` (intel_display_regs.h)
pub const DPLL_CFGCR1_PDIV_7: u32 = ((8 << 2));

/// `DPLL_CFGCR1_CENTRAL_FREQ` (intel_display_regs.h)
pub const DPLL_CFGCR1_CENTRAL_FREQ: u32 = ((3 << 0));

/// `DPLL_CFGCR1_CENTRAL_FREQ_8400` (intel_display_regs.h)
pub const DPLL_CFGCR1_CENTRAL_FREQ_8400: u32 = ((3 << 0));

/// `TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL` (intel_display_regs.h)
pub const TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL: u32 = ((0 << 0));

/// `_TGL_DPLL0_CFGCR0` (intel_display_regs.h)
pub const _TGL_DPLL0_CFGCR0: u32 = 0x164284;

/// `_TGL_DPLL1_CFGCR0` (intel_display_regs.h)
pub const _TGL_DPLL1_CFGCR0: u32 = 0x16428C;

/// `_TGL_TBTPLL_CFGCR0` (intel_display_regs.h)
pub const _TGL_TBTPLL_CFGCR0: u32 = 0x16429C;

/// `TGL_DPLL_CFGCR0(pll)` (intel_display_regs.h)
pub const fn TGL_DPLL_CFGCR0(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR0, _TGL_DPLL1_CFGCR0, _TGL_TBTPLL_CFGCR0, _TGL_TBTPLL_CFGCR0))
}

/// `RKL_DPLL_CFGCR0(pll)` (intel_display_regs.h)
pub const fn RKL_DPLL_CFGCR0(pll: u32) -> I915RegT {
    _MMIO_PLL(pll, _TGL_DPLL0_CFGCR0, _TGL_DPLL1_CFGCR0)
}

/// `_TGL_DPLL0_DIV0` (intel_display_regs.h)
pub const _TGL_DPLL0_DIV0: u32 = 0x164B00;

/// `_TGL_DPLL1_DIV0` (intel_display_regs.h)
pub const _TGL_DPLL1_DIV0: u32 = 0x164C00;

/// `TGL_DPLL0_DIV0(pll)` (intel_display_regs.h)
pub const fn TGL_DPLL0_DIV0(pll: u32) -> I915RegT {
    _MMIO_PLL(pll, _TGL_DPLL0_DIV0, _TGL_DPLL1_DIV0)
}

/// `TGL_DPLL0_DIV0_AFC_STARTUP_MASK` (intel_display_regs.h)
pub const TGL_DPLL0_DIV0_AFC_STARTUP_MASK: u32 = reg_genmask(27, 25);

/// `TGL_DPLL0_DIV0_AFC_STARTUP(val)` (intel_display_regs.h)
pub const fn TGL_DPLL0_DIV0_AFC_STARTUP(val: u32) -> u32 {
    reg_field_prep(TGL_DPLL0_DIV0_AFC_STARTUP_MASK, (val))
}

/// `_TGL_DPLL0_CFGCR1` (intel_display_regs.h)
pub const _TGL_DPLL0_CFGCR1: u32 = 0x164288;

/// `_TGL_DPLL1_CFGCR1` (intel_display_regs.h)
pub const _TGL_DPLL1_CFGCR1: u32 = 0x164290;

/// `_TGL_TBTPLL_CFGCR1` (intel_display_regs.h)
pub const _TGL_TBTPLL_CFGCR1: u32 = 0x1642A0;

/// `TGL_DPLL_CFGCR1(pll)` (intel_display_regs.h)
pub const fn TGL_DPLL_CFGCR1(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR1, _TGL_DPLL1_CFGCR1, _TGL_TBTPLL_CFGCR1, _TGL_TBTPLL_CFGCR1))
}

/// `RKL_DPLL_CFGCR1(pll)` (intel_display_regs.h)
pub const fn RKL_DPLL_CFGCR1(pll: u32) -> I915RegT {
    _MMIO_PLL(pll, _TGL_DPLL0_CFGCR1, _TGL_DPLL1_CFGCR1)
}

/// `_DG1_DPLL2_CFGCR0` (intel_display_regs.h)
pub const _DG1_DPLL2_CFGCR0: u32 = 0x16C284;

/// `_DG1_DPLL3_CFGCR0` (intel_display_regs.h)
pub const _DG1_DPLL3_CFGCR0: u32 = 0x16C28C;

/// `DG1_DPLL_CFGCR0(pll)` (intel_display_regs.h)
pub const fn DG1_DPLL_CFGCR0(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR0, _TGL_DPLL1_CFGCR0, _DG1_DPLL2_CFGCR0, _DG1_DPLL3_CFGCR0))
}

/// `_DG1_DPLL2_CFGCR1` (intel_display_regs.h)
pub const _DG1_DPLL2_CFGCR1: u32 = 0x16C288;

/// `_DG1_DPLL3_CFGCR1` (intel_display_regs.h)
pub const _DG1_DPLL3_CFGCR1: u32 = 0x16C290;

/// `DG1_DPLL_CFGCR1(pll)` (intel_display_regs.h)
pub const fn DG1_DPLL_CFGCR1(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR1, _TGL_DPLL1_CFGCR1, _DG1_DPLL2_CFGCR1, _DG1_DPLL3_CFGCR1))
}

/// `_ADLS_DPLL4_CFGCR0` (intel_display_regs.h)
pub const _ADLS_DPLL4_CFGCR0: u32 = 0x164294;

/// `_ADLS_DPLL3_CFGCR0` (intel_display_regs.h)
pub const _ADLS_DPLL3_CFGCR0: u32 = 0x1642C0;

/// `ADLS_DPLL_CFGCR0(pll)` (intel_display_regs.h)
pub const fn ADLS_DPLL_CFGCR0(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR0, _TGL_DPLL1_CFGCR0, _ADLS_DPLL4_CFGCR0, _ADLS_DPLL3_CFGCR0))
}

/// `_ADLS_DPLL4_CFGCR1` (intel_display_regs.h)
pub const _ADLS_DPLL4_CFGCR1: u32 = 0x164298;

/// `_ADLS_DPLL3_CFGCR1` (intel_display_regs.h)
pub const _ADLS_DPLL3_CFGCR1: u32 = 0x1642C4;

/// `ADLS_DPLL_CFGCR1(pll)` (intel_display_regs.h)
pub const fn ADLS_DPLL_CFGCR1(pll: u32) -> I915RegT {
    mmio(pick_even_2ranges(pll, 2, _TGL_DPLL0_CFGCR1, _TGL_DPLL1_CFGCR1, _ADLS_DPLL4_CFGCR1, _ADLS_DPLL3_CFGCR1))
}

/// `BXT_DE_PLL_CTL` (intel_display_regs.h)
pub const BXT_DE_PLL_CTL: I915RegT = mmio(0x6d000);

/// `BXT_DE_PLL_RATIO(x)` (intel_display_regs.h)
pub const fn BXT_DE_PLL_RATIO(x: u32) -> u32 {
    (x)
}

/// `BXT_DE_PLL_RATIO_MASK` (intel_display_regs.h)
pub const BXT_DE_PLL_RATIO_MASK: u32 = 0xff;

/// `BXT_DE_PLL_ENABLE` (intel_display_regs.h)
pub const BXT_DE_PLL_ENABLE: I915RegT = mmio(0x46070);

/// `BXT_DE_PLL_PLL_ENABLE` (intel_display_regs.h)
pub const BXT_DE_PLL_PLL_ENABLE: u32 = ((1 << 31));

/// `BXT_DE_PLL_LOCK` (intel_display_regs.h)
pub const BXT_DE_PLL_LOCK: u32 = ((1 << 30));

/// `BXT_DE_PLL_FREQ_REQ` (intel_display_regs.h)
pub const BXT_DE_PLL_FREQ_REQ: u32 = ((1 << 23));

/// `BXT_DE_PLL_FREQ_REQ_ACK` (intel_display_regs.h)
pub const BXT_DE_PLL_FREQ_REQ_ACK: u32 = ((1 << 22));

/// `ICL_CDCLK_PLL_RATIO(x)` (intel_display_regs.h)
pub const fn ICL_CDCLK_PLL_RATIO(x: u32) -> u32 {
    (x)
}

/// `ICL_CDCLK_PLL_RATIO_MASK` (intel_display_regs.h)
pub const ICL_CDCLK_PLL_RATIO_MASK: u32 = 0xff;

/// `DC_STATE_EN` (intel_display_regs.h)
pub const DC_STATE_EN: I915RegT = mmio(0x45504);

/// `DC_STATE_DISABLE` (intel_display_regs.h)
pub const DC_STATE_DISABLE: u32 = 0;

/// `DC_STATE_EN_DC3CO` (intel_display_regs.h)
pub const DC_STATE_EN_DC3CO: u32 = reg_bit(30);

/// `DC_STATE_DC3CO_STATUS` (intel_display_regs.h)
pub const DC_STATE_DC3CO_STATUS: u32 = reg_bit(29);

/// `HOLD_PHY_CLKREQ_PG1_LATCH` (intel_display_regs.h)
pub const HOLD_PHY_CLKREQ_PG1_LATCH: u32 = reg_bit(21);

/// `HOLD_PHY_PG1_LATCH` (intel_display_regs.h)
pub const HOLD_PHY_PG1_LATCH: u32 = reg_bit(20);

/// `DC_STATE_EN_UPTO_DC5` (intel_display_regs.h)
pub const DC_STATE_EN_UPTO_DC5: u32 = ((1 << 0));

/// `DC_STATE_EN_DC9` (intel_display_regs.h)
pub const DC_STATE_EN_DC9: u32 = ((1 << 3));

/// `DC_STATE_EN_UPTO_DC6` (intel_display_regs.h)
pub const DC_STATE_EN_UPTO_DC6: u32 = ((2 << 0));

/// `DC_STATE_EN_UPTO_DC5_DC6_MASK` (intel_display_regs.h)
pub const DC_STATE_EN_UPTO_DC5_DC6_MASK: u32 = 0x3;

/// `DC_STATE_DEBUG` (intel_display_regs.h)
pub const DC_STATE_DEBUG: I915RegT = mmio(0x45520);

/// `DC_STATE_DEBUG_MASK_CORES` (intel_display_regs.h)
pub const DC_STATE_DEBUG_MASK_CORES: u32 = ((1 << 0));

/// `DC_STATE_DEBUG_MASK_MEMORY_UP` (intel_display_regs.h)
pub const DC_STATE_DEBUG_MASK_MEMORY_UP: u32 = ((1 << 1));

/// `D_COMP_BDW` (intel_display_regs.h)
pub const D_COMP_BDW: I915RegT = mmio(0x138144);

/// `_WM_LINETIME_A` (intel_display_regs.h)
pub const _WM_LINETIME_A: u32 = 0x45270;

/// `_WM_LINETIME_B` (intel_display_regs.h)
pub const _WM_LINETIME_B: u32 = 0x45274;

/// `WM_LINETIME(pipe)` (intel_display_regs.h)
pub const fn WM_LINETIME(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _WM_LINETIME_A, _WM_LINETIME_B)
}

/// `HSW_LINETIME_MASK` (intel_display_regs.h)
pub const HSW_LINETIME_MASK: u32 = reg_genmask(8, 0);

/// `HSW_LINETIME(x)` (intel_display_regs.h)
pub const fn HSW_LINETIME(x: u32) -> u32 {
    reg_field_prep(HSW_LINETIME_MASK, (x))
}

/// `HSW_IPS_LINETIME_MASK` (intel_display_regs.h)
pub const HSW_IPS_LINETIME_MASK: u32 = reg_genmask(24, 16);

/// `HSW_IPS_LINETIME(x)` (intel_display_regs.h)
pub const fn HSW_IPS_LINETIME(x: u32) -> u32 {
    reg_field_prep(HSW_IPS_LINETIME_MASK, (x))
}

/// `SFUSE_STRAP` (intel_display_regs.h)
pub const SFUSE_STRAP: I915RegT = mmio(0xc2014);

/// `SFUSE_STRAP_FUSE_LOCK` (intel_display_regs.h)
pub const SFUSE_STRAP_FUSE_LOCK: u32 = ((1 << 13));

/// `SFUSE_STRAP_RAW_FREQUENCY` (intel_display_regs.h)
pub const SFUSE_STRAP_RAW_FREQUENCY: u32 = ((1 << 8));

/// `SFUSE_STRAP_DISPLAY_DISABLED` (intel_display_regs.h)
pub const SFUSE_STRAP_DISPLAY_DISABLED: u32 = ((1 << 7));

/// `SFUSE_STRAP_CRT_DISABLED` (intel_display_regs.h)
pub const SFUSE_STRAP_CRT_DISABLED: u32 = ((1 << 6));

/// `SFUSE_STRAP_DDIF_DETECTED` (intel_display_regs.h)
pub const SFUSE_STRAP_DDIF_DETECTED: u32 = ((1 << 3));

/// `SFUSE_STRAP_DDIB_DETECTED` (intel_display_regs.h)
pub const SFUSE_STRAP_DDIB_DETECTED: u32 = ((1 << 2));

/// `SFUSE_STRAP_DDIC_DETECTED` (intel_display_regs.h)
pub const SFUSE_STRAP_DDIC_DETECTED: u32 = ((1 << 1));

/// `SFUSE_STRAP_DDID_DETECTED` (intel_display_regs.h)
pub const SFUSE_STRAP_DDID_DETECTED: u32 = ((1 << 0));

/// `SOUTH_CHICKEN1` (intel_display_regs.h)
pub const SOUTH_CHICKEN1: I915RegT = mmio(0xc2000);

/// `FDIA_PHASE_SYNC_SHIFT_OVR` (intel_display_regs.h)
pub const FDIA_PHASE_SYNC_SHIFT_OVR: u32 = 19;

/// `FDIA_PHASE_SYNC_SHIFT_EN` (intel_display_regs.h)
pub const FDIA_PHASE_SYNC_SHIFT_EN: u32 = 18;

/// `INVERT_DDIE_HPD` (intel_display_regs.h)
pub const INVERT_DDIE_HPD: u32 = reg_bit(28);

/// `INVERT_DDID_HPD_MTP` (intel_display_regs.h)
pub const INVERT_DDID_HPD_MTP: u32 = reg_bit(27);

/// `INVERT_TC4_HPD` (intel_display_regs.h)
pub const INVERT_TC4_HPD: u32 = reg_bit(26);

/// `INVERT_TC3_HPD` (intel_display_regs.h)
pub const INVERT_TC3_HPD: u32 = reg_bit(25);

/// `INVERT_TC2_HPD` (intel_display_regs.h)
pub const INVERT_TC2_HPD: u32 = reg_bit(24);

/// `INVERT_TC1_HPD` (intel_display_regs.h)
pub const INVERT_TC1_HPD: u32 = reg_bit(23);

/// `INVERT_DDID_HPD` (intel_display_regs.h)
pub const INVERT_DDID_HPD: u32 = ((1 << 18));

/// `INVERT_DDIC_HPD` (intel_display_regs.h)
pub const INVERT_DDIC_HPD: u32 = ((1 << 17));

/// `INVERT_DDIB_HPD` (intel_display_regs.h)
pub const INVERT_DDIB_HPD: u32 = ((1 << 16));

/// `INVERT_DDIA_HPD` (intel_display_regs.h)
pub const INVERT_DDIA_HPD: u32 = ((1 << 15));

/// `FDI_PHASE_SYNC_OVR(pipe)` (intel_display_regs.h)
pub const fn FDI_PHASE_SYNC_OVR(pipe: u32) -> u32 {
    ((1 << ((FDIA_PHASE_SYNC_SHIFT_OVR - (((pipe) * 2))))))
}

/// `FDI_PHASE_SYNC_EN(pipe)` (intel_display_regs.h)
pub const fn FDI_PHASE_SYNC_EN(pipe: u32) -> u32 {
    ((1 << ((FDIA_PHASE_SYNC_SHIFT_EN - (((pipe) * 2))))))
}

/// `FDI_BC_BIFURCATION_SELECT` (intel_display_regs.h)
pub const FDI_BC_BIFURCATION_SELECT: u32 = ((1 << 12));

/// `CHASSIS_CLK_REQ_DURATION_MASK` (intel_display_regs.h)
pub const CHASSIS_CLK_REQ_DURATION_MASK: u32 = ((0xf << 8));

/// `CHASSIS_CLK_REQ_DURATION(x)` (intel_display_regs.h)
pub const fn CHASSIS_CLK_REQ_DURATION(x: u32) -> u32 {
    (((x) << 8))
}

/// `SBCLK_RUN_REFCLK_DIS` (intel_display_regs.h)
pub const SBCLK_RUN_REFCLK_DIS: u32 = ((1 << 7));

/// `ICP_SECOND_PPS_IO_SELECT` (intel_display_regs.h)
pub const ICP_SECOND_PPS_IO_SELECT: u32 = reg_bit(2);

/// `SPT_PWM_GRANULARITY` (intel_display_regs.h)
pub const SPT_PWM_GRANULARITY: u32 = ((1 << 0));

/// `SOUTH_CHICKEN2` (intel_display_regs.h)
pub const SOUTH_CHICKEN2: I915RegT = mmio(0xc2004);

/// `FDI_MPHY_IOSFSB_RESET_STATUS` (intel_display_regs.h)
pub const FDI_MPHY_IOSFSB_RESET_STATUS: u32 = ((1 << 13));

/// `FDI_MPHY_IOSFSB_RESET_CTL` (intel_display_regs.h)
pub const FDI_MPHY_IOSFSB_RESET_CTL: u32 = ((1 << 12));

/// `LPT_PWM_GRANULARITY` (intel_display_regs.h)
pub const LPT_PWM_GRANULARITY: u32 = ((1 << 5));

/// `DPLS_EDP_PPS_FIX_DIS` (intel_display_regs.h)
pub const DPLS_EDP_PPS_FIX_DIS: u32 = ((1 << 0));

/// `SOUTH_DSPCLK_GATE_D` (intel_display_regs.h)
pub const SOUTH_DSPCLK_GATE_D: I915RegT = mmio(0xc2020);

/// `PCH_GMBUSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PCH_GMBUSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 31));

/// `PCH_DPLUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PCH_DPLUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 30));

/// `PCH_DPLSUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PCH_DPLSUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 29));

/// `PCH_DPMGUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PCH_DPMGUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 15));

/// `PCH_CPUNIT_CLOCK_GATE_DISABLE` (intel_display_regs.h)
pub const PCH_CPUNIT_CLOCK_GATE_DISABLE: u32 = ((1 << 14));

/// `CNP_PWM_CGE_GATING_DISABLE` (intel_display_regs.h)
pub const CNP_PWM_CGE_GATING_DISABLE: u32 = ((1 << 13));

/// `PCH_LP_PARTITION_LEVEL_DISABLE` (intel_display_regs.h)
pub const PCH_LP_PARTITION_LEVEL_DISABLE: u32 = ((1 << 12));

/// `GEN4_TIMESTAMP` (intel_display_regs.h)
pub const GEN4_TIMESTAMP: I915RegT = mmio(0x2358);

/// `ILK_TIMESTAMP_HI` (intel_display_regs.h)
pub const ILK_TIMESTAMP_HI: I915RegT = mmio(0x70070);

/// `IVB_TIMESTAMP_CTR` (intel_display_regs.h)
pub const IVB_TIMESTAMP_CTR: I915RegT = mmio(0x44070);

/// `_PIPE_FRMTMSTMP_A` (intel_display_regs.h)
pub const _PIPE_FRMTMSTMP_A: u32 = 0x70048;

/// `_PIPE_FRMTMSTMP_B` (intel_display_regs.h)
pub const _PIPE_FRMTMSTMP_B: u32 = 0x71048;

/// `PIPE_FRMTMSTMP(pipe)` (intel_display_regs.h)
pub const fn PIPE_FRMTMSTMP(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_FRMTMSTMP_A, _PIPE_FRMTMSTMP_B)
}

/// `_PIPE_FLIPTMSTMP_A` (intel_display_regs.h)
pub const _PIPE_FLIPTMSTMP_A: u32 = 0x7004C;

/// `_PIPE_FLIPTMSTMP_B` (intel_display_regs.h)
pub const _PIPE_FLIPTMSTMP_B: u32 = 0x7104C;

/// `PIPE_FLIPTMSTMP(pipe)` (intel_display_regs.h)
pub const fn PIPE_FLIPTMSTMP(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_FLIPTMSTMP_A, _PIPE_FLIPTMSTMP_B)
}

/// `_PIPE_FLIPDONETMSTMP_A` (intel_display_regs.h)
pub const _PIPE_FLIPDONETMSTMP_A: u32 = 0x70054;

/// `_PIPE_FLIPDONETMSTMP_B` (intel_display_regs.h)
pub const _PIPE_FLIPDONETMSTMP_B: u32 = 0x71054;

/// `PIPE_FLIPDONETIMSTMP(pipe)` (intel_display_regs.h)
pub const fn PIPE_FLIPDONETIMSTMP(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPE_FLIPDONETMSTMP_A, _PIPE_FLIPDONETMSTMP_B)
}

/// `_VLV_PIPE_MSA_MISC_A` (intel_display_regs.h)
pub const _VLV_PIPE_MSA_MISC_A: u32 = 0x70048;

/// `VLV_MSA_MISC1_HW_ENABLE` (intel_display_regs.h)
pub const VLV_MSA_MISC1_HW_ENABLE: u32 = reg_bit(31);

/// `VLV_MSA_MISC1_SW_S3D_MASK` (intel_display_regs.h)
pub const VLV_MSA_MISC1_SW_S3D_MASK: u32 = reg_genmask(2, 0);

/// `_ICL_PHY_MISC_A` (intel_display_regs.h)
pub const _ICL_PHY_MISC_A: u32 = 0x64C00;

/// `_ICL_PHY_MISC_B` (intel_display_regs.h)
pub const _ICL_PHY_MISC_B: u32 = 0x64C04;

/// `_DG2_PHY_MISC_TC1` (intel_display_regs.h)
pub const _DG2_PHY_MISC_TC1: u32 = 0x64C14;

/// `ICL_PHY_MISC(port)` (intel_display_regs.h)
pub const fn ICL_PHY_MISC(port: u32) -> I915RegT {
    _MMIO_PORT(port, _ICL_PHY_MISC_A, _ICL_PHY_MISC_B)
}

/// `ICL_PHY_MISC_MUX_DDID` (intel_display_regs.h)
pub const ICL_PHY_MISC_MUX_DDID: u32 = ((1 << 28));

/// `ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN` (intel_display_regs.h)
pub const ICL_PHY_MISC_DE_IO_COMP_PWR_DOWN: u32 = ((1 << 23));

/// `DG2_PHY_DP_TX_ACK_MASK` (intel_display_regs.h)
pub const DG2_PHY_DP_TX_ACK_MASK: u32 = reg_genmask(23, 20);

/// `PORT_TX_DFLEXDPSP(fia)` (intel_display_regs.h)
pub const fn PORT_TX_DFLEXDPSP(fia: u32) -> I915RegT {
    _MMIO_FIA((fia), 0x008A0)
}

/// `MODULAR_FIA_MASK` (intel_display_regs.h)
pub const MODULAR_FIA_MASK: u32 = ((1 << 4));

/// `TC_LIVE_STATE_TBT(idx)` (intel_display_regs.h)
pub const fn TC_LIVE_STATE_TBT(idx: u32) -> u32 {
    ((1 << ((((idx) * 8) + 6))))
}

/// `TC_LIVE_STATE_TC(idx)` (intel_display_regs.h)
pub const fn TC_LIVE_STATE_TC(idx: u32) -> u32 {
    ((1 << ((((idx) * 8) + 5))))
}

/// `DP_LANE_ASSIGNMENT_SHIFT(idx)` (intel_display_regs.h)
pub const fn DP_LANE_ASSIGNMENT_SHIFT(idx: u32) -> u32 {
    (((idx) * 8))
}

/// `DP_LANE_ASSIGNMENT_MASK(idx)` (intel_display_regs.h)
pub const fn DP_LANE_ASSIGNMENT_MASK(idx: u32) -> u32 {
    ((0xf << (((idx) * 8))))
}

/// `DP_LANE_ASSIGNMENT(idx, x)` (intel_display_regs.h)
pub const fn DP_LANE_ASSIGNMENT(idx: u32, x: u32) -> u32 {
    (((x) << (((idx) * 8))))
}

/// `PORT_TX_DFLEXDPPMS(fia)` (intel_display_regs.h)
pub const fn PORT_TX_DFLEXDPPMS(fia: u32) -> I915RegT {
    _MMIO_FIA((fia), 0x00890)
}

/// `DP_PHY_MODE_STATUS_COMPLETED(idx)` (intel_display_regs.h)
pub const fn DP_PHY_MODE_STATUS_COMPLETED(idx: u32) -> u32 {
    ((1 << (idx)))
}

/// `PORT_TX_DFLEXDPCSSS(fia)` (intel_display_regs.h)
pub const fn PORT_TX_DFLEXDPCSSS(fia: u32) -> I915RegT {
    _MMIO_FIA((fia), 0x00894)
}

/// `DP_PHY_MODE_STATUS_NOT_SAFE(idx)` (intel_display_regs.h)
pub const fn DP_PHY_MODE_STATUS_NOT_SAFE(idx: u32) -> u32 {
    ((1 << (idx)))
}

/// `PORT_TX_DFLEXPA1(fia)` (intel_display_regs.h)
pub const fn PORT_TX_DFLEXPA1(fia: u32) -> I915RegT {
    _MMIO_FIA((fia), 0x00880)
}

/// `DP_PIN_ASSIGNMENT_SHIFT(idx)` (intel_display_regs.h)
pub const fn DP_PIN_ASSIGNMENT_SHIFT(idx: u32) -> u32 {
    (((idx) * 4))
}

/// `DP_PIN_ASSIGNMENT_MASK(idx)` (intel_display_regs.h)
pub const fn DP_PIN_ASSIGNMENT_MASK(idx: u32) -> u32 {
    ((0xf << (((idx) * 4))))
}

/// `DP_PIN_ASSIGNMENT(idx, x)` (intel_display_regs.h)
pub const fn DP_PIN_ASSIGNMENT(idx: u32, x: u32) -> u32 {
    (((x) << (((idx) * 4))))
}

/// `_TCSS_DDI_STATUS_1` (intel_display_regs.h)
pub const _TCSS_DDI_STATUS_1: u32 = 0x161500;

/// `_TCSS_DDI_STATUS_2` (intel_display_regs.h)
pub const _TCSS_DDI_STATUS_2: u32 = 0x161504;

/// `TCSS_DDI_STATUS(tc)` (intel_display_regs.h)
pub const fn TCSS_DDI_STATUS(tc: u32) -> I915RegT {
    mmio(pick_even(tc, _TCSS_DDI_STATUS_1, _TCSS_DDI_STATUS_2))
}

/// `TCSS_DDI_STATUS_PIN_ASSIGNMENT_MASK` (intel_display_regs.h)
pub const TCSS_DDI_STATUS_PIN_ASSIGNMENT_MASK: u32 = reg_genmask(28, 25);

/// `TCSS_DDI_STATUS_READY` (intel_display_regs.h)
pub const TCSS_DDI_STATUS_READY: u32 = reg_bit(2);

/// `TCSS_DDI_STATUS_HPD_LIVE_STATUS_TBT` (intel_display_regs.h)
pub const TCSS_DDI_STATUS_HPD_LIVE_STATUS_TBT: u32 = reg_bit(1);

/// `TCSS_DDI_STATUS_HPD_LIVE_STATUS_ALT` (intel_display_regs.h)
pub const TCSS_DDI_STATUS_HPD_LIVE_STATUS_ALT: u32 = reg_bit(0);

/// `CLKREQ_POLICY` (intel_display_regs.h)
pub const CLKREQ_POLICY: I915RegT = mmio(0x101038);

/// `CLKREQ_POLICY_MEM_UP_OVRD` (intel_display_regs.h)
pub const CLKREQ_POLICY_MEM_UP_OVRD: u32 = reg_bit(1);

/// `CLKGATE_DIS_MISC` (intel_display_regs.h)
pub const CLKGATE_DIS_MISC: I915RegT = mmio(0x46534);

/// `CLKGATE_DIS_MISC_DMASC_GATING_DIS` (intel_display_regs.h)
pub const CLKGATE_DIS_MISC_DMASC_GATING_DIS: u32 = reg_bit(21);

/// `_MTL_CLKGATE_DIS_TRANS_A` (intel_display_regs.h)
pub const _MTL_CLKGATE_DIS_TRANS_A: u32 = 0x604E8;

/// `_MTL_CLKGATE_DIS_TRANS_B` (intel_display_regs.h)
pub const _MTL_CLKGATE_DIS_TRANS_B: u32 = 0x614E8;

/// `MTL_CLKGATE_DIS_TRANS_DMASC_GATING_DIS` (intel_display_regs.h)
pub const MTL_CLKGATE_DIS_TRANS_DMASC_GATING_DIS: u32 = reg_bit(7);

/// `_MTL_PIPE_CLKGATE_DIS2_A` (intel_display_regs.h)
pub const _MTL_PIPE_CLKGATE_DIS2_A: u32 = 0x60114;

/// `_MTL_PIPE_CLKGATE_DIS2_B` (intel_display_regs.h)
pub const _MTL_PIPE_CLKGATE_DIS2_B: u32 = 0x61114;

/// `MTL_PIPE_CLKGATE_DIS2(pipe)` (intel_display_regs.h)
pub const fn MTL_PIPE_CLKGATE_DIS2(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _MTL_PIPE_CLKGATE_DIS2_A, _MTL_PIPE_CLKGATE_DIS2_B)
}

/// `MTL_DPFC_GATING_DIS` (intel_display_regs.h)
pub const MTL_DPFC_GATING_DIS: u32 = reg_bit(6);

/// `MTL_MEM_SS_INFO_GLOBAL` (intel_display_regs.h)
pub const MTL_MEM_SS_INFO_GLOBAL: I915RegT = mmio(0x45700);

/// `XE3P_ECC_IMPACTING_DE` (intel_display_regs.h)
pub const XE3P_ECC_IMPACTING_DE: u32 = reg_bit(12);

/// `MTL_N_OF_ENABLED_QGV_POINTS_MASK` (intel_display_regs.h)
pub const MTL_N_OF_ENABLED_QGV_POINTS_MASK: u32 = reg_genmask(11, 8);

/// `MTL_N_OF_POPULATED_CH_MASK` (intel_display_regs.h)
pub const MTL_N_OF_POPULATED_CH_MASK: u32 = reg_genmask(7, 4);

/// `MTL_DDR_TYPE_MASK` (intel_display_regs.h)
pub const MTL_DDR_TYPE_MASK: u32 = reg_genmask(3, 0);

/// `MTL_MEM_SS_INFO_QGV_POINT_OFFSET` (intel_display_regs.h)
pub const MTL_MEM_SS_INFO_QGV_POINT_OFFSET: u32 = 0x45710;

/// `MTL_MEM_SS_INFO_QGV_POINT_LOW(point)` (intel_display_regs.h)
pub const fn MTL_MEM_SS_INFO_QGV_POINT_LOW(point: u32) -> I915RegT {
    mmio((MTL_MEM_SS_INFO_QGV_POINT_OFFSET + ((point) * 8)))
}

/// `MTL_TRCD_MASK` (intel_display_regs.h)
pub const MTL_TRCD_MASK: u32 = reg_genmask(31, 24);

/// `MTL_TRP_MASK` (intel_display_regs.h)
pub const MTL_TRP_MASK: u32 = reg_genmask(23, 16);

/// `MTL_DCLK_MASK` (intel_display_regs.h)
pub const MTL_DCLK_MASK: u32 = reg_genmask(15, 0);

/// `MTL_MEM_SS_INFO_QGV_POINT_HIGH(point)` (intel_display_regs.h)
pub const fn MTL_MEM_SS_INFO_QGV_POINT_HIGH(point: u32) -> I915RegT {
    mmio(((MTL_MEM_SS_INFO_QGV_POINT_OFFSET + ((point) * 8)) + 4))
}

/// `MTL_TRAS_MASK` (intel_display_regs.h)
pub const MTL_TRAS_MASK: u32 = reg_genmask(16, 8);

/// `MTL_TRDPRE_MASK` (intel_display_regs.h)
pub const MTL_TRDPRE_MASK: u32 = reg_genmask(7, 0);

/// `FW_BLC` (intel_display_regs.h)
pub const FW_BLC: I915RegT = mmio(0x20d8);

/// `FW_BLC2` (intel_display_regs.h)
pub const FW_BLC2: I915RegT = mmio(0x20dc);

/// `FW_BLC_SELF` (intel_display_regs.h)
pub const FW_BLC_SELF: I915RegT = mmio(0x20e0);

/// `FW_BLC_SELF_EN_MASK` (intel_display_regs.h)
pub const FW_BLC_SELF_EN_MASK: u32 = reg_bit(31);

/// `FW_BLC_SELF_FIFO_MASK` (intel_display_regs.h)
pub const FW_BLC_SELF_FIFO_MASK: u32 = reg_bit(16);

/// `FW_BLC_SELF_EN` (intel_display_regs.h)
pub const FW_BLC_SELF_EN: u32 = reg_bit(15);

/// `DPLL(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn DPLL(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_BASE_PIPE3(DISPLAY_MMIO_BASE(display), pipe, _DPLL_A, _DPLL_B, _CHV_DPLL_C)
}

/// `DPLL_MD(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn DPLL_MD(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_BASE_PIPE3(DISPLAY_MMIO_BASE(display), pipe, _DPLL_A_MD, _DPLL_B_MD, _CHV_DPLL_C_MD)
}

/// `TRANS_HTOTAL(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_HTOTAL(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_HTOTAL_A)
}

/// `TRANS_HBLANK(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_HBLANK(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_HBLANK_A)
}

/// `TRANS_HSYNC(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_HSYNC(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_HSYNC_A)
}

/// `TRANS_VTOTAL(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_VTOTAL(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_VTOTAL_A)
}

/// `TRANS_VBLANK(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_VBLANK(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_VBLANK_A)
}

/// `TRANS_VSYNC(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_VSYNC(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_VSYNC_A)
}

/// `PIPESRC(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPESRC(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _PIPEASRC)
}

/// `TRANS_VSYNCSHIFT(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_VSYNCSHIFT(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_VSYNCSHIFT_A)
}

/// `TRANS_MULT(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANS_MULT(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _TRANS_MULT_A)
}

/// `PORT_HOTPLUG_EN(dev_priv)` (intel_display_regs.h)
pub unsafe fn PORT_HOTPLUG_EN(display: *const IntelDisplay) -> I915RegT {
    mmio(DISPLAY_MMIO_BASE(display) + 0x61110)
}

/// `PORT_HOTPLUG_STAT(dev_priv)` (intel_display_regs.h)
pub unsafe fn PORT_HOTPLUG_STAT(display: *const IntelDisplay) -> I915RegT {
    mmio(DISPLAY_MMIO_BASE(display) + 0x61114)
}

/// `PIPEDSL(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPEDSL(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPEADSL)
}

/// `TRANSCONF(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn TRANSCONF(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_PIPE2(display, trans, _TRANSACONF)
}

/// `PIPESTAT(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPESTAT(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPEASTAT)
}

/// `PIPE_ARB_CTL(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPE_ARB_CTL(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPE_ARB_CTL_A)
}

/// `PIPEFRAME(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPEFRAME(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPEAFRAMEHIGH)
}

/// `PIPEFRAMEPIXEL(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPEFRAMEPIXEL(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPEAFRAMEPIXEL)
}

/// `PIPE_FRMCOUNT_G4X(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn PIPE_FRMCOUNT_G4X(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _PIPEA_FRMCOUNT_G4X)
}

/// `CHV_BLEND(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn CHV_BLEND(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _CHV_BLEND_A)
}

/// `CHV_CANVAS(dev_priv, pipe)` (intel_display_regs.h)
pub unsafe fn CHV_CANVAS(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_TRANS2(display, pipe, _CHV_CANVAS_A)
}

/// `PIPE_DATA_M1(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_DATA_M1(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_DATA_M1)
}

/// `PIPE_DATA_N1(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_DATA_N1(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_DATA_N1)
}

/// `PIPE_DATA_M2(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_DATA_M2(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_DATA_M2)
}

/// `PIPE_DATA_N2(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_DATA_N2(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_DATA_N2)
}

/// `PIPE_LINK_M1(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_LINK_M1(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_LINK_M1)
}

/// `PIPE_LINK_N1(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_LINK_N1(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_LINK_N1)
}

/// `PIPE_LINK_M2(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_LINK_M2(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_LINK_M2)
}

/// `PIPE_LINK_N2(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn PIPE_LINK_N2(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _PIPEA_LINK_N2)
}

/// `GEN8_DE_PIPE_IRQ_REGS(pipe)` (intel_display_regs.h)
pub const fn GEN8_DE_PIPE_IRQ_REGS(pipe: u32) -> IntelIrqRegs {
    INTEL_IRQ_REGS(GEN8_DE_PIPE_IMR(pipe), GEN8_DE_PIPE_IER(pipe), GEN8_DE_PIPE_IIR(pipe))
}

/// `CHICKEN_TRANS(display, trans)` (intel_display_regs.h)
pub unsafe fn CHICKEN_TRANS(display: *const IntelDisplay, trans: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 14 {
        _MTL_CHICKEN_TRANS(trans)
    } else {
        _CHICKEN_TRANS(trans)
    }
}

/// `PCH_DPLL(pll)` (intel_display_regs.h)
pub const fn PCH_DPLL(pll: u32) -> I915RegT {
    mmio(if pll == 0 { _PCH_DPLL_A } else { _PCH_DPLL_B })
}

/// `PCH_FP0(pll)` (intel_display_regs.h)
pub const fn PCH_FP0(pll: u32) -> I915RegT {
    mmio(if pll == 0 { _PCH_FPA0 } else { _PCH_FPB0 })
}

/// `PCH_FP1(pll)` (intel_display_regs.h)
pub const fn PCH_FP1(pll: u32) -> I915RegT {
    mmio(if pll == 0 { _PCH_FPA1 } else { _PCH_FPB1 })
}

/// `HSW_TVIDEO_DIP_CTL(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_CTL(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_CTL_A)
}

/// `HSW_TVIDEO_DIP_AVI_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_AVI_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_AVI_DATA_A + i * 4)
}

/// `HSW_TVIDEO_DIP_VS_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_VS_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_VS_DATA_A + i * 4)
}

/// `HSW_TVIDEO_DIP_SPD_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_SPD_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_SPD_DATA_A + i * 4)
}

/// `HSW_TVIDEO_DIP_GMP_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_GMP_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_GMP_DATA_A + i * 4)
}

/// `HSW_TVIDEO_DIP_VSC_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_VSC_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_VSC_DATA_A + i * 4)
}

/// `ADL_TVIDEO_DIP_AS_SDP_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn ADL_TVIDEO_DIP_AS_SDP_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _ADL_VIDEO_DIP_AS_DATA_A + i * 4)
}

/// `GLK_TVIDEO_DIP_DRM_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn GLK_TVIDEO_DIP_DRM_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _GLK_VIDEO_DIP_DRM_DATA_A + i * 4)
}

/// `HSW_TVIDEO_DIP_GCP(dev_priv, trans)` (intel_display_regs.h)
pub unsafe fn HSW_TVIDEO_DIP_GCP(display: *const IntelDisplay, trans: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _HSW_VIDEO_DIP_GCP_A)
}

/// `ICL_VIDEO_DIP_PPS_DATA(dev_priv, trans, i)` (intel_display_regs.h)
pub unsafe fn ICL_VIDEO_DIP_PPS_DATA(display: *const IntelDisplay, trans: u32, i: u32) -> I915RegT {
    _MMIO_TRANS2(display, trans, _ICL_VIDEO_DIP_PPS_DATA_A + i * 4)
}

/// `TRANS_DDI_FUNC_CTL(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TRANS_DDI_FUNC_CTL(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TRANS_DDI_FUNC_CTL_A)
}

/// `TRANS_DDI_FUNC_CTL2(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TRANS_DDI_FUNC_CTL2(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TRANS_DDI_FUNC_CTL2_A)
}

/// `TGL_DP_TP_CTL(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TGL_DP_TP_CTL(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TGL_DP_TP_CTL_A)
}

/// `TGL_DP_TP_STATUS(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TGL_DP_TP_STATUS(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TGL_DP_TP_STATUS_A)
}

pub const fn DDI_PORT_WIDTH_ENCODE(width: u32) -> u32 {
    if width == 3 { 4 } else { width - 1 }
}

/// `DDI_PORT_WIDTH(width)` (intel_display_regs.h)
pub const fn DDI_PORT_WIDTH(width: u32) -> u32 {
    reg_field_prep(DDI_PORT_WIDTH_MASK, DDI_PORT_WIDTH_ENCODE(width))
}

/// `TRANS_MSA_MISC(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TRANS_MSA_MISC(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TRANSA_MSA_MISC)
}

/// `TRANS_SET_CONTEXT_LATENCY(dev_priv, tran)` (intel_display_regs.h)
pub unsafe fn TRANS_SET_CONTEXT_LATENCY(display: *const IntelDisplay, tran: u32) -> I915RegT {
    _MMIO_TRANS2(display, tran, _TRANS_A_SET_CONTEXT_LATENCY)
}

pub const SKL_DPLL1: u32 = 1;

/// `DPLL_CFGCR1(id)` (intel_display_regs.h)
pub const fn DPLL_CFGCR1(id: u32) -> I915RegT {
    _MMIO_PIPE(id - SKL_DPLL1, _DPLL1_CFGCR1, _DPLL2_CFGCR1)
}

/// `DPLL_CFGCR2(id)` (intel_display_regs.h)
pub const fn DPLL_CFGCR2(id: u32) -> I915RegT {
    _MMIO_PIPE(id - SKL_DPLL1, _DPLL1_CFGCR2, _DPLL2_CFGCR2)
}

pub const TC_PORT_4: u32 = 3;

/// `ICL_DPCLKA_CFGCR0_TC_CLK_OFF(tc_port)` (intel_display_regs.h)
pub const fn ICL_DPCLKA_CFGCR0_TC_CLK_OFF(tc_port: u32) -> u32 {
    1 << (if tc_port < TC_PORT_4 { tc_port + 12 } else { tc_port - TC_PORT_4 + 21 })
}

/// `VLV_PIPE_MSA_MISC(__display, pipe)` (intel_display_regs.h)
pub unsafe fn VLV_PIPE_MSA_MISC(display: *const IntelDisplay, pipe: u32) -> I915RegT {
    _MMIO_PIPE2(display, pipe, _VLV_PIPE_MSA_MISC_A)
}

pub const DPIO_PHY0: u32 = 0;

/// `PHY_POWERGOOD(phy)` (intel_display_regs.h)
pub const fn PHY_POWERGOOD(phy: u32) -> u32 {
    if phy == DPIO_PHY0 { 1 << 31 } else { 1 << 30 }
}

/// `GEN9_PIPE_PLANE_FLIP_DONE(plane_id)` (intel_display_regs.h)
pub const fn GEN9_PIPE_PLANE_FLIP_DONE(plane_id: u32) -> u32 {
    reg_bit((if plane_id >= (PLANE_5 as u32) { 16 - (PLANE_5 as u32) } else { 3 - (PLANE_1 as u32) }) + plane_id)
}
