// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/icl_dsi_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_combo_phy_regs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `_MMIO_DSI(tc, dsi0, dsi1)` (icl_dsi_regs.h)
pub const fn _MMIO_DSI(tc: u32, dsi0: u32, dsi1: u32) -> I915RegT {
    _MMIO_TRANS(((tc) - (TRANSCODER_DSI_0 as u32)), dsi0, dsi1)
}

/// `_ICL_DSI_ESC_CLK_DIV0` (icl_dsi_regs.h)
pub const _ICL_DSI_ESC_CLK_DIV0: u32 = 0x6b090;

/// `_ICL_DSI_ESC_CLK_DIV1` (icl_dsi_regs.h)
pub const _ICL_DSI_ESC_CLK_DIV1: u32 = 0x6b890;

/// `ICL_DSI_ESC_CLK_DIV(port)` (icl_dsi_regs.h)
pub const fn ICL_DSI_ESC_CLK_DIV(port: u32) -> I915RegT {
    _MMIO_PORT((port), _ICL_DSI_ESC_CLK_DIV0, _ICL_DSI_ESC_CLK_DIV1)
}

/// `_ICL_DPHY_ESC_CLK_DIV0` (icl_dsi_regs.h)
pub const _ICL_DPHY_ESC_CLK_DIV0: u32 = 0x162190;

/// `_ICL_DPHY_ESC_CLK_DIV1` (icl_dsi_regs.h)
pub const _ICL_DPHY_ESC_CLK_DIV1: u32 = 0x6C190;

/// `ICL_DPHY_ESC_CLK_DIV(port)` (icl_dsi_regs.h)
pub const fn ICL_DPHY_ESC_CLK_DIV(port: u32) -> I915RegT {
    _MMIO_PORT((port), _ICL_DPHY_ESC_CLK_DIV0, _ICL_DPHY_ESC_CLK_DIV1)
}

/// `ICL_BYTE_CLK_PER_ESC_CLK_MASK` (icl_dsi_regs.h)
pub const ICL_BYTE_CLK_PER_ESC_CLK_MASK: u32 = ((0x1f << 16));

/// `ICL_BYTE_CLK_PER_ESC_CLK_SHIFT` (icl_dsi_regs.h)
pub const ICL_BYTE_CLK_PER_ESC_CLK_SHIFT: u32 = 16;

/// `ICL_ESC_CLK_DIV_MASK` (icl_dsi_regs.h)
pub const ICL_ESC_CLK_DIV_MASK: u32 = 0x1ff;

/// `ICL_ESC_CLK_DIV_SHIFT` (icl_dsi_regs.h)
pub const ICL_ESC_CLK_DIV_SHIFT: u32 = 0;

/// `DSI_MAX_ESC_CLK` (icl_dsi_regs.h)
pub const DSI_MAX_ESC_CLK: u32 = 20000;

/// `_ADL_MIPIO_REG` (icl_dsi_regs.h)
pub const _ADL_MIPIO_REG: u32 = 0x180;

/// `ADL_MIPIO_DW(port, dw)` (icl_dsi_regs.h)
pub const fn ADL_MIPIO_DW(port: u32, dw: u32) -> I915RegT {
    mmio(((_ICL_COMBOPHY(port) + _ADL_MIPIO_REG) + (4 * (dw))))
}

/// `TX_ESC_CLK_DIV_PHY_MASK` (icl_dsi_regs.h)
pub const TX_ESC_CLK_DIV_PHY_MASK: u32 = reg_genmask(23, 16);

/// `TX_ESC_CLK_DIV_PHY` (icl_dsi_regs.h)
pub const TX_ESC_CLK_DIV_PHY: u32 = reg_field_prep(TX_ESC_CLK_DIV_PHY_MASK, 0x7f);

/// `_DSI_CMD_FRMCTL_0` (icl_dsi_regs.h)
pub const _DSI_CMD_FRMCTL_0: u32 = 0x6b034;

/// `_DSI_CMD_FRMCTL_1` (icl_dsi_regs.h)
pub const _DSI_CMD_FRMCTL_1: u32 = 0x6b834;

/// `DSI_CMD_FRMCTL(port)` (icl_dsi_regs.h)
pub const fn DSI_CMD_FRMCTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_CMD_FRMCTL_0, _DSI_CMD_FRMCTL_1)
}

/// `DSI_FRAME_UPDATE_REQUEST` (icl_dsi_regs.h)
pub const DSI_FRAME_UPDATE_REQUEST: u32 = ((1 << 31));

/// `DSI_PERIODIC_FRAME_UPDATE_ENABLE` (icl_dsi_regs.h)
pub const DSI_PERIODIC_FRAME_UPDATE_ENABLE: u32 = ((1 << 29));

/// `DSI_NULL_PACKET_ENABLE` (icl_dsi_regs.h)
pub const DSI_NULL_PACKET_ENABLE: u32 = ((1 << 28));

/// `DSI_FRAME_IN_PROGRESS` (icl_dsi_regs.h)
pub const DSI_FRAME_IN_PROGRESS: u32 = ((1 << 0));

/// `_DSI_INTR_MASK_REG_0` (icl_dsi_regs.h)
pub const _DSI_INTR_MASK_REG_0: u32 = 0x6b070;

/// `_DSI_INTR_MASK_REG_1` (icl_dsi_regs.h)
pub const _DSI_INTR_MASK_REG_1: u32 = 0x6b870;

/// `DSI_INTR_MASK_REG(port)` (icl_dsi_regs.h)
pub const fn DSI_INTR_MASK_REG(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_INTR_MASK_REG_0, _DSI_INTR_MASK_REG_1)
}

/// `_DSI_INTR_IDENT_REG_0` (icl_dsi_regs.h)
pub const _DSI_INTR_IDENT_REG_0: u32 = 0x6b074;

/// `_DSI_INTR_IDENT_REG_1` (icl_dsi_regs.h)
pub const _DSI_INTR_IDENT_REG_1: u32 = 0x6b874;

/// `DSI_INTR_IDENT_REG(port)` (icl_dsi_regs.h)
pub const fn DSI_INTR_IDENT_REG(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_INTR_IDENT_REG_0, _DSI_INTR_IDENT_REG_1)
}

/// `DSI_TE_EVENT` (icl_dsi_regs.h)
pub const DSI_TE_EVENT: u32 = ((1 << 31));

/// `DSI_RX_DATA_OR_BTA_TERMINATED` (icl_dsi_regs.h)
pub const DSI_RX_DATA_OR_BTA_TERMINATED: u32 = ((1 << 30));

/// `DSI_TX_DATA` (icl_dsi_regs.h)
pub const DSI_TX_DATA: u32 = ((1 << 29));

/// `DSI_ULPS_ENTRY_DONE` (icl_dsi_regs.h)
pub const DSI_ULPS_ENTRY_DONE: u32 = ((1 << 28));

/// `DSI_NON_TE_TRIGGER_RECEIVED` (icl_dsi_regs.h)
pub const DSI_NON_TE_TRIGGER_RECEIVED: u32 = ((1 << 27));

/// `DSI_HOST_CHKSUM_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_CHKSUM_ERROR: u32 = ((1 << 26));

/// `DSI_HOST_MULTI_ECC_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_MULTI_ECC_ERROR: u32 = ((1 << 25));

/// `DSI_HOST_SINGL_ECC_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_SINGL_ECC_ERROR: u32 = ((1 << 24));

/// `DSI_HOST_CONTENTION_DETECTED` (icl_dsi_regs.h)
pub const DSI_HOST_CONTENTION_DETECTED: u32 = ((1 << 23));

/// `DSI_HOST_FALSE_CONTROL_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_FALSE_CONTROL_ERROR: u32 = ((1 << 22));

/// `DSI_HOST_TIMEOUT_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_TIMEOUT_ERROR: u32 = ((1 << 21));

/// `DSI_HOST_LOW_POWER_TX_SYNC_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_LOW_POWER_TX_SYNC_ERROR: u32 = ((1 << 20));

/// `DSI_HOST_ESCAPE_MODE_ENTRY_ERROR` (icl_dsi_regs.h)
pub const DSI_HOST_ESCAPE_MODE_ENTRY_ERROR: u32 = ((1 << 19));

/// `DSI_FRAME_UPDATE_DONE` (icl_dsi_regs.h)
pub const DSI_FRAME_UPDATE_DONE: u32 = ((1 << 16));

/// `DSI_PROTOCOL_VIOLATION_REPORTED` (icl_dsi_regs.h)
pub const DSI_PROTOCOL_VIOLATION_REPORTED: u32 = ((1 << 15));

/// `DSI_INVALID_TX_LENGTH` (icl_dsi_regs.h)
pub const DSI_INVALID_TX_LENGTH: u32 = ((1 << 13));

/// `DSI_INVALID_VC` (icl_dsi_regs.h)
pub const DSI_INVALID_VC: u32 = ((1 << 12));

/// `DSI_INVALID_DATA_TYPE` (icl_dsi_regs.h)
pub const DSI_INVALID_DATA_TYPE: u32 = ((1 << 11));

/// `DSI_PERIPHERAL_CHKSUM_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_CHKSUM_ERROR: u32 = ((1 << 10));

/// `DSI_PERIPHERAL_MULTI_ECC_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_MULTI_ECC_ERROR: u32 = ((1 << 9));

/// `DSI_PERIPHERAL_SINGLE_ECC_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_SINGLE_ECC_ERROR: u32 = ((1 << 8));

/// `DSI_PERIPHERAL_CONTENTION_DETECTED` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_CONTENTION_DETECTED: u32 = ((1 << 7));

/// `DSI_PERIPHERAL_FALSE_CTRL_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_FALSE_CTRL_ERROR: u32 = ((1 << 6));

/// `DSI_PERIPHERAL_TIMEOUT_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_TIMEOUT_ERROR: u32 = ((1 << 5));

/// `DSI_PERIPHERAL_LP_TX_SYNC_ERROR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_LP_TX_SYNC_ERROR: u32 = ((1 << 4));

/// `DSI_PERIPHERAL_ESC_MODE_ENTRY_CMD_ERR` (icl_dsi_regs.h)
pub const DSI_PERIPHERAL_ESC_MODE_ENTRY_CMD_ERR: u32 = ((1 << 3));

/// `DSI_EOT_SYNC_ERROR` (icl_dsi_regs.h)
pub const DSI_EOT_SYNC_ERROR: u32 = ((1 << 2));

/// `DSI_SOT_SYNC_ERROR` (icl_dsi_regs.h)
pub const DSI_SOT_SYNC_ERROR: u32 = ((1 << 1));

/// `DSI_SOT_ERROR` (icl_dsi_regs.h)
pub const DSI_SOT_ERROR: u32 = ((1 << 0));

/// `_ICL_DSI_IO_MODECTL_0` (icl_dsi_regs.h)
pub const _ICL_DSI_IO_MODECTL_0: u32 = 0x6B094;

/// `_ICL_DSI_IO_MODECTL_1` (icl_dsi_regs.h)
pub const _ICL_DSI_IO_MODECTL_1: u32 = 0x6B894;

/// `ICL_DSI_IO_MODECTL(port)` (icl_dsi_regs.h)
pub const fn ICL_DSI_IO_MODECTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _ICL_DSI_IO_MODECTL_0, _ICL_DSI_IO_MODECTL_1)
}

/// `COMBO_PHY_MODE_DSI` (icl_dsi_regs.h)
pub const COMBO_PHY_MODE_DSI: u32 = ((1 << 0));

/// `_TGL_DSI_CHKN_REG_0` (icl_dsi_regs.h)
pub const _TGL_DSI_CHKN_REG_0: u32 = 0x6B0C0;

/// `_TGL_DSI_CHKN_REG_1` (icl_dsi_regs.h)
pub const _TGL_DSI_CHKN_REG_1: u32 = 0x6B8C0;

/// `TGL_DSI_CHKN_REG(port)` (icl_dsi_regs.h)
pub const fn TGL_DSI_CHKN_REG(port: u32) -> I915RegT {
    _MMIO_PORT(port, _TGL_DSI_CHKN_REG_0, _TGL_DSI_CHKN_REG_1)
}

/// `TGL_DSI_CHKN_LSHS_GB_MASK` (icl_dsi_regs.h)
pub const TGL_DSI_CHKN_LSHS_GB_MASK: u32 = reg_genmask(15, 12);

/// `TGL_DSI_CHKN_LSHS_GB(byte_clocks)` (icl_dsi_regs.h)
pub const fn TGL_DSI_CHKN_LSHS_GB(byte_clocks: u32) -> u32 {
    reg_field_prep(TGL_DSI_CHKN_LSHS_GB_MASK, (byte_clocks))
}

/// `_ICL_DSI_T_INIT_MASTER_0` (icl_dsi_regs.h)
pub const _ICL_DSI_T_INIT_MASTER_0: u32 = 0x6b088;

/// `_ICL_DSI_T_INIT_MASTER_1` (icl_dsi_regs.h)
pub const _ICL_DSI_T_INIT_MASTER_1: u32 = 0x6b888;

/// `ICL_DSI_T_INIT_MASTER(port)` (icl_dsi_regs.h)
pub const fn ICL_DSI_T_INIT_MASTER(port: u32) -> I915RegT {
    _MMIO_PORT(port, _ICL_DSI_T_INIT_MASTER_0, _ICL_DSI_T_INIT_MASTER_1)
}

/// `DSI_T_INIT_MASTER_MASK` (icl_dsi_regs.h)
pub const DSI_T_INIT_MASTER_MASK: u32 = reg_genmask(15, 0);

/// `_DPHY_CLK_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DPHY_CLK_TIMING_PARAM_0: u32 = 0x162180;

/// `_DPHY_CLK_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DPHY_CLK_TIMING_PARAM_1: u32 = 0x6c180;

/// `DPHY_CLK_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DPHY_CLK_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DPHY_CLK_TIMING_PARAM_0, _DPHY_CLK_TIMING_PARAM_1)
}

/// `_DSI_CLK_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DSI_CLK_TIMING_PARAM_0: u32 = 0x6b080;

/// `_DSI_CLK_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DSI_CLK_TIMING_PARAM_1: u32 = 0x6b880;

/// `DSI_CLK_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DSI_CLK_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_CLK_TIMING_PARAM_0, _DSI_CLK_TIMING_PARAM_1)
}

/// `CLK_PREPARE_OVERRIDE` (icl_dsi_regs.h)
pub const CLK_PREPARE_OVERRIDE: u32 = ((1 << 31));

/// `CLK_PREPARE(x)` (icl_dsi_regs.h)
pub const fn CLK_PREPARE(x: u32) -> u32 {
    (((x) << 28))
}

/// `CLK_PREPARE_MASK` (icl_dsi_regs.h)
pub const CLK_PREPARE_MASK: u32 = ((0x7 << 28));

/// `CLK_PREPARE_SHIFT` (icl_dsi_regs.h)
pub const CLK_PREPARE_SHIFT: u32 = 28;

/// `CLK_ZERO_OVERRIDE` (icl_dsi_regs.h)
pub const CLK_ZERO_OVERRIDE: u32 = ((1 << 27));

/// `CLK_ZERO(x)` (icl_dsi_regs.h)
pub const fn CLK_ZERO(x: u32) -> u32 {
    (((x) << 20))
}

/// `CLK_ZERO_MASK` (icl_dsi_regs.h)
pub const CLK_ZERO_MASK: u32 = ((0xf << 20));

/// `CLK_ZERO_SHIFT` (icl_dsi_regs.h)
pub const CLK_ZERO_SHIFT: u32 = 20;

/// `CLK_PRE_OVERRIDE` (icl_dsi_regs.h)
pub const CLK_PRE_OVERRIDE: u32 = ((1 << 19));

/// `CLK_PRE(x)` (icl_dsi_regs.h)
pub const fn CLK_PRE(x: u32) -> u32 {
    (((x) << 16))
}

/// `CLK_PRE_MASK` (icl_dsi_regs.h)
pub const CLK_PRE_MASK: u32 = ((0x3 << 16));

/// `CLK_PRE_SHIFT` (icl_dsi_regs.h)
pub const CLK_PRE_SHIFT: u32 = 16;

/// `CLK_POST_OVERRIDE` (icl_dsi_regs.h)
pub const CLK_POST_OVERRIDE: u32 = ((1 << 15));

/// `CLK_POST(x)` (icl_dsi_regs.h)
pub const fn CLK_POST(x: u32) -> u32 {
    (((x) << 8))
}

/// `CLK_POST_MASK` (icl_dsi_regs.h)
pub const CLK_POST_MASK: u32 = ((0x7 << 8));

/// `CLK_POST_SHIFT` (icl_dsi_regs.h)
pub const CLK_POST_SHIFT: u32 = 8;

/// `CLK_TRAIL_OVERRIDE` (icl_dsi_regs.h)
pub const CLK_TRAIL_OVERRIDE: u32 = ((1 << 7));

/// `CLK_TRAIL(x)` (icl_dsi_regs.h)
pub const fn CLK_TRAIL(x: u32) -> u32 {
    (((x) << 0))
}

/// `CLK_TRAIL_MASK` (icl_dsi_regs.h)
pub const CLK_TRAIL_MASK: u32 = ((0xf << 0));

/// `CLK_TRAIL_SHIFT` (icl_dsi_regs.h)
pub const CLK_TRAIL_SHIFT: u32 = 0;

/// `_DPHY_DATA_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DPHY_DATA_TIMING_PARAM_0: u32 = 0x162184;

/// `_DPHY_DATA_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DPHY_DATA_TIMING_PARAM_1: u32 = 0x6c184;

/// `DPHY_DATA_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DPHY_DATA_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DPHY_DATA_TIMING_PARAM_0, _DPHY_DATA_TIMING_PARAM_1)
}

/// `_DSI_DATA_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DSI_DATA_TIMING_PARAM_0: u32 = 0x6B084;

/// `_DSI_DATA_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DSI_DATA_TIMING_PARAM_1: u32 = 0x6B884;

/// `DSI_DATA_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DSI_DATA_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_DATA_TIMING_PARAM_0, _DSI_DATA_TIMING_PARAM_1)
}

/// `HS_PREPARE_OVERRIDE` (icl_dsi_regs.h)
pub const HS_PREPARE_OVERRIDE: u32 = ((1 << 31));

/// `HS_PREPARE(x)` (icl_dsi_regs.h)
pub const fn HS_PREPARE(x: u32) -> u32 {
    (((x) << 24))
}

/// `HS_PREPARE_MASK` (icl_dsi_regs.h)
pub const HS_PREPARE_MASK: u32 = ((0x7 << 24));

/// `HS_PREPARE_SHIFT` (icl_dsi_regs.h)
pub const HS_PREPARE_SHIFT: u32 = 24;

/// `HS_ZERO_OVERRIDE` (icl_dsi_regs.h)
pub const HS_ZERO_OVERRIDE: u32 = ((1 << 23));

/// `HS_ZERO(x)` (icl_dsi_regs.h)
pub const fn HS_ZERO(x: u32) -> u32 {
    (((x) << 16))
}

/// `HS_ZERO_MASK` (icl_dsi_regs.h)
pub const HS_ZERO_MASK: u32 = ((0xf << 16));

/// `HS_ZERO_SHIFT` (icl_dsi_regs.h)
pub const HS_ZERO_SHIFT: u32 = 16;

/// `HS_TRAIL_OVERRIDE` (icl_dsi_regs.h)
pub const HS_TRAIL_OVERRIDE: u32 = ((1 << 15));

/// `HS_TRAIL(x)` (icl_dsi_regs.h)
pub const fn HS_TRAIL(x: u32) -> u32 {
    (((x) << 8))
}

/// `HS_TRAIL_MASK` (icl_dsi_regs.h)
pub const HS_TRAIL_MASK: u32 = ((0x7 << 8));

/// `HS_TRAIL_SHIFT` (icl_dsi_regs.h)
pub const HS_TRAIL_SHIFT: u32 = 8;

/// `HS_EXIT_OVERRIDE` (icl_dsi_regs.h)
pub const HS_EXIT_OVERRIDE: u32 = ((1 << 7));

/// `HS_EXIT(x)` (icl_dsi_regs.h)
pub const fn HS_EXIT(x: u32) -> u32 {
    (((x) << 0))
}

/// `HS_EXIT_MASK` (icl_dsi_regs.h)
pub const HS_EXIT_MASK: u32 = ((0x7 << 0));

/// `HS_EXIT_SHIFT` (icl_dsi_regs.h)
pub const HS_EXIT_SHIFT: u32 = 0;

/// `_DPHY_TA_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DPHY_TA_TIMING_PARAM_0: u32 = 0x162188;

/// `_DPHY_TA_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DPHY_TA_TIMING_PARAM_1: u32 = 0x6c188;

/// `DPHY_TA_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DPHY_TA_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DPHY_TA_TIMING_PARAM_0, _DPHY_TA_TIMING_PARAM_1)
}

/// `_DSI_TA_TIMING_PARAM_0` (icl_dsi_regs.h)
pub const _DSI_TA_TIMING_PARAM_0: u32 = 0x6b098;

/// `_DSI_TA_TIMING_PARAM_1` (icl_dsi_regs.h)
pub const _DSI_TA_TIMING_PARAM_1: u32 = 0x6b898;

/// `DSI_TA_TIMING_PARAM(port)` (icl_dsi_regs.h)
pub const fn DSI_TA_TIMING_PARAM(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DSI_TA_TIMING_PARAM_0, _DSI_TA_TIMING_PARAM_1)
}

/// `TA_SURE_OVERRIDE` (icl_dsi_regs.h)
pub const TA_SURE_OVERRIDE: u32 = ((1 << 31));

/// `TA_SURE(x)` (icl_dsi_regs.h)
pub const fn TA_SURE(x: u32) -> u32 {
    (((x) << 16))
}

/// `TA_SURE_MASK` (icl_dsi_regs.h)
pub const TA_SURE_MASK: u32 = ((0x1f << 16));

/// `TA_SURE_SHIFT` (icl_dsi_regs.h)
pub const TA_SURE_SHIFT: u32 = 16;

/// `TA_GO_OVERRIDE` (icl_dsi_regs.h)
pub const TA_GO_OVERRIDE: u32 = ((1 << 15));

/// `TA_GO(x)` (icl_dsi_regs.h)
pub const fn TA_GO(x: u32) -> u32 {
    (((x) << 8))
}

/// `TA_GO_MASK` (icl_dsi_regs.h)
pub const TA_GO_MASK: u32 = ((0xf << 8));

/// `TA_GO_SHIFT` (icl_dsi_regs.h)
pub const TA_GO_SHIFT: u32 = 8;

/// `TA_GET_OVERRIDE` (icl_dsi_regs.h)
pub const TA_GET_OVERRIDE: u32 = ((1 << 7));

/// `TA_GET(x)` (icl_dsi_regs.h)
pub const fn TA_GET(x: u32) -> u32 {
    (((x) << 0))
}

/// `TA_GET_MASK` (icl_dsi_regs.h)
pub const TA_GET_MASK: u32 = ((0xf << 0));

/// `TA_GET_SHIFT` (icl_dsi_regs.h)
pub const TA_GET_SHIFT: u32 = 0;

/// `_DSI_TRANS_FUNC_CONF_0` (icl_dsi_regs.h)
pub const _DSI_TRANS_FUNC_CONF_0: u32 = 0x6b030;

/// `_DSI_TRANS_FUNC_CONF_1` (icl_dsi_regs.h)
pub const _DSI_TRANS_FUNC_CONF_1: u32 = 0x6b830;

/// `DSI_TRANS_FUNC_CONF(tc)` (icl_dsi_regs.h)
pub const fn DSI_TRANS_FUNC_CONF(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_TRANS_FUNC_CONF_0, _DSI_TRANS_FUNC_CONF_1)
}

/// `OP_MODE_MASK` (icl_dsi_regs.h)
pub const OP_MODE_MASK: u32 = ((0x3 << 28));

/// `OP_MODE_SHIFT` (icl_dsi_regs.h)
pub const OP_MODE_SHIFT: u32 = 28;

/// `CMD_MODE_NO_GATE` (icl_dsi_regs.h)
pub const CMD_MODE_NO_GATE: u32 = ((0x0 << 28));

/// `CMD_MODE_TE_GATE` (icl_dsi_regs.h)
pub const CMD_MODE_TE_GATE: u32 = ((0x1 << 28));

/// `VIDEO_MODE_SYNC_EVENT` (icl_dsi_regs.h)
pub const VIDEO_MODE_SYNC_EVENT: u32 = ((0x2 << 28));

/// `VIDEO_MODE_SYNC_PULSE` (icl_dsi_regs.h)
pub const VIDEO_MODE_SYNC_PULSE: u32 = ((0x3 << 28));

/// `TE_SOURCE_GPIO` (icl_dsi_regs.h)
pub const TE_SOURCE_GPIO: u32 = ((1 << 27));

/// `LINK_READY` (icl_dsi_regs.h)
pub const LINK_READY: u32 = ((1 << 20));

/// `PIX_FMT_MASK` (icl_dsi_regs.h)
pub const PIX_FMT_MASK: u32 = ((0x3 << 16));

/// `PIX_FMT_SHIFT` (icl_dsi_regs.h)
pub const PIX_FMT_SHIFT: u32 = 16;

/// `PIX_FMT_RGB565` (icl_dsi_regs.h)
pub const PIX_FMT_RGB565: u32 = ((0x0 << 16));

/// `PIX_FMT_RGB666_PACKED` (icl_dsi_regs.h)
pub const PIX_FMT_RGB666_PACKED: u32 = ((0x1 << 16));

/// `PIX_FMT_RGB666_LOOSE` (icl_dsi_regs.h)
pub const PIX_FMT_RGB666_LOOSE: u32 = ((0x2 << 16));

/// `PIX_FMT_RGB888` (icl_dsi_regs.h)
pub const PIX_FMT_RGB888: u32 = ((0x3 << 16));

/// `PIX_FMT_RGB101010` (icl_dsi_regs.h)
pub const PIX_FMT_RGB101010: u32 = ((0x4 << 16));

/// `PIX_FMT_RGB121212` (icl_dsi_regs.h)
pub const PIX_FMT_RGB121212: u32 = ((0x5 << 16));

/// `PIX_FMT_COMPRESSED` (icl_dsi_regs.h)
pub const PIX_FMT_COMPRESSED: u32 = ((0x6 << 16));

/// `BGR_TRANSMISSION` (icl_dsi_regs.h)
pub const BGR_TRANSMISSION: u32 = ((1 << 15));

/// `PIX_VIRT_CHAN(x)` (icl_dsi_regs.h)
pub const fn PIX_VIRT_CHAN(x: u32) -> u32 {
    (((x) << 12))
}

/// `PIX_VIRT_CHAN_MASK` (icl_dsi_regs.h)
pub const PIX_VIRT_CHAN_MASK: u32 = ((0x3 << 12));

/// `PIX_VIRT_CHAN_SHIFT` (icl_dsi_regs.h)
pub const PIX_VIRT_CHAN_SHIFT: u32 = 12;

/// `PIX_BUF_THRESHOLD_MASK` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_MASK: u32 = ((0x3 << 10));

/// `PIX_BUF_THRESHOLD_SHIFT` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_SHIFT: u32 = 10;

/// `PIX_BUF_THRESHOLD_1_4` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_1_4: u32 = ((0x0 << 10));

/// `PIX_BUF_THRESHOLD_1_2` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_1_2: u32 = ((0x1 << 10));

/// `PIX_BUF_THRESHOLD_3_4` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_3_4: u32 = ((0x2 << 10));

/// `PIX_BUF_THRESHOLD_FULL` (icl_dsi_regs.h)
pub const PIX_BUF_THRESHOLD_FULL: u32 = ((0x3 << 10));

/// `CONTINUOUS_CLK_MASK` (icl_dsi_regs.h)
pub const CONTINUOUS_CLK_MASK: u32 = ((0x3 << 8));

/// `CONTINUOUS_CLK_SHIFT` (icl_dsi_regs.h)
pub const CONTINUOUS_CLK_SHIFT: u32 = 8;

/// `CLK_ENTER_LP_AFTER_DATA` (icl_dsi_regs.h)
pub const CLK_ENTER_LP_AFTER_DATA: u32 = ((0x0 << 8));

/// `CLK_HS_OR_LP` (icl_dsi_regs.h)
pub const CLK_HS_OR_LP: u32 = ((0x2 << 8));

/// `CLK_HS_CONTINUOUS` (icl_dsi_regs.h)
pub const CLK_HS_CONTINUOUS: u32 = ((0x3 << 8));

/// `LP_CLK_DURING_LPM` (icl_dsi_regs.h)
pub const LP_CLK_DURING_LPM: u32 = ((1 << 7));

/// `LINK_CALIBRATION_MASK` (icl_dsi_regs.h)
pub const LINK_CALIBRATION_MASK: u32 = ((0x3 << 4));

/// `LINK_CALIBRATION_SHIFT` (icl_dsi_regs.h)
pub const LINK_CALIBRATION_SHIFT: u32 = 4;

/// `CALIBRATION_DISABLED` (icl_dsi_regs.h)
pub const CALIBRATION_DISABLED: u32 = ((0x0 << 4));

/// `CALIBRATION_ENABLED_INITIAL_ONLY` (icl_dsi_regs.h)
pub const CALIBRATION_ENABLED_INITIAL_ONLY: u32 = ((0x2 << 4));

/// `CALIBRATION_ENABLED_INITIAL_PERIODIC` (icl_dsi_regs.h)
pub const CALIBRATION_ENABLED_INITIAL_PERIODIC: u32 = ((0x3 << 4));

/// `BLANKING_PACKET_ENABLE` (icl_dsi_regs.h)
pub const BLANKING_PACKET_ENABLE: u32 = ((1 << 2));

/// `S3D_ORIENTATION_LANDSCAPE` (icl_dsi_regs.h)
pub const S3D_ORIENTATION_LANDSCAPE: u32 = ((1 << 1));

/// `EOTP_DISABLED` (icl_dsi_regs.h)
pub const EOTP_DISABLED: u32 = ((1 << 0));

/// `_DSI_CMD_RXCTL_0` (icl_dsi_regs.h)
pub const _DSI_CMD_RXCTL_0: u32 = 0x6b0d4;

/// `_DSI_CMD_RXCTL_1` (icl_dsi_regs.h)
pub const _DSI_CMD_RXCTL_1: u32 = 0x6b8d4;

/// `DSI_CMD_RXCTL(tc)` (icl_dsi_regs.h)
pub const fn DSI_CMD_RXCTL(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_CMD_RXCTL_0, _DSI_CMD_RXCTL_1)
}

/// `READ_UNLOADS_DW` (icl_dsi_regs.h)
pub const READ_UNLOADS_DW: u32 = ((1 << 16));

/// `RECEIVED_UNASSIGNED_TRIGGER` (icl_dsi_regs.h)
pub const RECEIVED_UNASSIGNED_TRIGGER: u32 = ((1 << 15));

/// `RECEIVED_ACKNOWLEDGE_TRIGGER` (icl_dsi_regs.h)
pub const RECEIVED_ACKNOWLEDGE_TRIGGER: u32 = ((1 << 14));

/// `RECEIVED_TEAR_EFFECT_TRIGGER` (icl_dsi_regs.h)
pub const RECEIVED_TEAR_EFFECT_TRIGGER: u32 = ((1 << 13));

/// `RECEIVED_RESET_TRIGGER` (icl_dsi_regs.h)
pub const RECEIVED_RESET_TRIGGER: u32 = ((1 << 12));

/// `RECEIVED_PAYLOAD_WAS_LOST` (icl_dsi_regs.h)
pub const RECEIVED_PAYLOAD_WAS_LOST: u32 = ((1 << 11));

/// `RECEIVED_CRC_WAS_LOST` (icl_dsi_regs.h)
pub const RECEIVED_CRC_WAS_LOST: u32 = ((1 << 10));

/// `NUMBER_RX_PLOAD_DW_MASK` (icl_dsi_regs.h)
pub const NUMBER_RX_PLOAD_DW_MASK: u32 = ((0xff << 0));

/// `NUMBER_RX_PLOAD_DW_SHIFT` (icl_dsi_regs.h)
pub const NUMBER_RX_PLOAD_DW_SHIFT: u32 = 0;

/// `_DSI_CMD_TXCTL_0` (icl_dsi_regs.h)
pub const _DSI_CMD_TXCTL_0: u32 = 0x6b0d0;

/// `_DSI_CMD_TXCTL_1` (icl_dsi_regs.h)
pub const _DSI_CMD_TXCTL_1: u32 = 0x6b8d0;

/// `DSI_CMD_TXCTL(tc)` (icl_dsi_regs.h)
pub const fn DSI_CMD_TXCTL(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_CMD_TXCTL_0, _DSI_CMD_TXCTL_1)
}

/// `KEEP_LINK_IN_HS` (icl_dsi_regs.h)
pub const KEEP_LINK_IN_HS: u32 = ((1 << 24));

/// `FREE_HEADER_CREDIT_MASK` (icl_dsi_regs.h)
pub const FREE_HEADER_CREDIT_MASK: u32 = ((0x1f << 8));

/// `FREE_HEADER_CREDIT_SHIFT` (icl_dsi_regs.h)
pub const FREE_HEADER_CREDIT_SHIFT: u32 = 0x8;

/// `FREE_PLOAD_CREDIT_MASK` (icl_dsi_regs.h)
pub const FREE_PLOAD_CREDIT_MASK: u32 = ((0xff << 0));

/// `FREE_PLOAD_CREDIT_SHIFT` (icl_dsi_regs.h)
pub const FREE_PLOAD_CREDIT_SHIFT: u32 = 0;

/// `MAX_HEADER_CREDIT` (icl_dsi_regs.h)
pub const MAX_HEADER_CREDIT: u32 = 0x10;

/// `MAX_PLOAD_CREDIT` (icl_dsi_regs.h)
pub const MAX_PLOAD_CREDIT: u32 = 0x40;

/// `_DSI_CMD_TXHDR_0` (icl_dsi_regs.h)
pub const _DSI_CMD_TXHDR_0: u32 = 0x6b100;

/// `_DSI_CMD_TXHDR_1` (icl_dsi_regs.h)
pub const _DSI_CMD_TXHDR_1: u32 = 0x6b900;

/// `DSI_CMD_TXHDR(tc)` (icl_dsi_regs.h)
pub const fn DSI_CMD_TXHDR(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_CMD_TXHDR_0, _DSI_CMD_TXHDR_1)
}

/// `PAYLOAD_PRESENT` (icl_dsi_regs.h)
pub const PAYLOAD_PRESENT: u32 = ((1 << 31));

/// `LP_DATA_TRANSFER` (icl_dsi_regs.h)
pub const LP_DATA_TRANSFER: u32 = ((1 << 30));

/// `VBLANK_FENCE` (icl_dsi_regs.h)
pub const VBLANK_FENCE: u32 = ((1 << 29));

/// `PIPELINE_FLUSH` (icl_dsi_regs.h)
pub const PIPELINE_FLUSH: u32 = ((1 << 28));

/// `PARAM_WC_MASK` (icl_dsi_regs.h)
pub const PARAM_WC_MASK: u32 = ((0xffff << 8));

/// `PARAM_WC_LOWER_SHIFT` (icl_dsi_regs.h)
pub const PARAM_WC_LOWER_SHIFT: u32 = 8;

/// `PARAM_WC_UPPER_SHIFT` (icl_dsi_regs.h)
pub const PARAM_WC_UPPER_SHIFT: u32 = 16;

/// `VC_MASK` (icl_dsi_regs.h)
pub const VC_MASK: u32 = ((0x3 << 6));

/// `VC_SHIFT` (icl_dsi_regs.h)
pub const VC_SHIFT: u32 = 6;

/// `DT_MASK` (icl_dsi_regs.h)
pub const DT_MASK: u32 = ((0x3f << 0));

/// `DT_SHIFT` (icl_dsi_regs.h)
pub const DT_SHIFT: u32 = 0;

/// `_DSI_CMD_TXPYLD_0` (icl_dsi_regs.h)
pub const _DSI_CMD_TXPYLD_0: u32 = 0x6b104;

/// `_DSI_CMD_TXPYLD_1` (icl_dsi_regs.h)
pub const _DSI_CMD_TXPYLD_1: u32 = 0x6b904;

/// `DSI_CMD_TXPYLD(tc)` (icl_dsi_regs.h)
pub const fn DSI_CMD_TXPYLD(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_CMD_TXPYLD_0, _DSI_CMD_TXPYLD_1)
}

/// `_DSI_LP_MSG_0` (icl_dsi_regs.h)
pub const _DSI_LP_MSG_0: u32 = 0x6b0d8;

/// `_DSI_LP_MSG_1` (icl_dsi_regs.h)
pub const _DSI_LP_MSG_1: u32 = 0x6b8d8;

/// `DSI_LP_MSG(tc)` (icl_dsi_regs.h)
pub const fn DSI_LP_MSG(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_LP_MSG_0, _DSI_LP_MSG_1)
}

/// `LPTX_IN_PROGRESS` (icl_dsi_regs.h)
pub const LPTX_IN_PROGRESS: u32 = ((1 << 17));

/// `LINK_IN_ULPS` (icl_dsi_regs.h)
pub const LINK_IN_ULPS: u32 = ((1 << 16));

/// `LINK_ULPS_TYPE_LP11` (icl_dsi_regs.h)
pub const LINK_ULPS_TYPE_LP11: u32 = ((1 << 8));

/// `LINK_ENTER_ULPS` (icl_dsi_regs.h)
pub const LINK_ENTER_ULPS: u32 = ((1 << 0));

/// `_DSI_HSTX_TO_0` (icl_dsi_regs.h)
pub const _DSI_HSTX_TO_0: u32 = 0x6b044;

/// `_DSI_HSTX_TO_1` (icl_dsi_regs.h)
pub const _DSI_HSTX_TO_1: u32 = 0x6b844;

/// `DSI_HSTX_TO(tc)` (icl_dsi_regs.h)
pub const fn DSI_HSTX_TO(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_HSTX_TO_0, _DSI_HSTX_TO_1)
}

/// `HSTX_TIMEOUT_VALUE_MASK` (icl_dsi_regs.h)
pub const HSTX_TIMEOUT_VALUE_MASK: u32 = ((0xffff << 16));

/// `HSTX_TIMEOUT_VALUE_SHIFT` (icl_dsi_regs.h)
pub const HSTX_TIMEOUT_VALUE_SHIFT: u32 = 16;

/// `HSTX_TIMEOUT_VALUE(x)` (icl_dsi_regs.h)
pub const fn HSTX_TIMEOUT_VALUE(x: u32) -> u32 {
    (((x) << 16))
}

/// `HSTX_TIMED_OUT` (icl_dsi_regs.h)
pub const HSTX_TIMED_OUT: u32 = ((1 << 0));

/// `_DSI_LPRX_HOST_TO_0` (icl_dsi_regs.h)
pub const _DSI_LPRX_HOST_TO_0: u32 = 0x6b048;

/// `_DSI_LPRX_HOST_TO_1` (icl_dsi_regs.h)
pub const _DSI_LPRX_HOST_TO_1: u32 = 0x6b848;

/// `DSI_LPRX_HOST_TO(tc)` (icl_dsi_regs.h)
pub const fn DSI_LPRX_HOST_TO(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_LPRX_HOST_TO_0, _DSI_LPRX_HOST_TO_1)
}

/// `LPRX_TIMED_OUT` (icl_dsi_regs.h)
pub const LPRX_TIMED_OUT: u32 = ((1 << 16));

/// `LPRX_TIMEOUT_VALUE_MASK` (icl_dsi_regs.h)
pub const LPRX_TIMEOUT_VALUE_MASK: u32 = ((0xffff << 0));

/// `LPRX_TIMEOUT_VALUE_SHIFT` (icl_dsi_regs.h)
pub const LPRX_TIMEOUT_VALUE_SHIFT: u32 = 0;

/// `LPRX_TIMEOUT_VALUE(x)` (icl_dsi_regs.h)
pub const fn LPRX_TIMEOUT_VALUE(x: u32) -> u32 {
    (((x) << 0))
}

/// `_DSI_PWAIT_TO_0` (icl_dsi_regs.h)
pub const _DSI_PWAIT_TO_0: u32 = 0x6b040;

/// `_DSI_PWAIT_TO_1` (icl_dsi_regs.h)
pub const _DSI_PWAIT_TO_1: u32 = 0x6b840;

/// `DSI_PWAIT_TO(tc)` (icl_dsi_regs.h)
pub const fn DSI_PWAIT_TO(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_PWAIT_TO_0, _DSI_PWAIT_TO_1)
}

/// `PRESET_TIMEOUT_VALUE_MASK` (icl_dsi_regs.h)
pub const PRESET_TIMEOUT_VALUE_MASK: u32 = ((0xffff << 16));

/// `PRESET_TIMEOUT_VALUE_SHIFT` (icl_dsi_regs.h)
pub const PRESET_TIMEOUT_VALUE_SHIFT: u32 = 16;

/// `PRESET_TIMEOUT_VALUE(x)` (icl_dsi_regs.h)
pub const fn PRESET_TIMEOUT_VALUE(x: u32) -> u32 {
    (((x) << 16))
}

/// `PRESPONSE_TIMEOUT_VALUE_MASK` (icl_dsi_regs.h)
pub const PRESPONSE_TIMEOUT_VALUE_MASK: u32 = ((0xffff << 0));

/// `PRESPONSE_TIMEOUT_VALUE_SHIFT` (icl_dsi_regs.h)
pub const PRESPONSE_TIMEOUT_VALUE_SHIFT: u32 = 0;

/// `PRESPONSE_TIMEOUT_VALUE(x)` (icl_dsi_regs.h)
pub const fn PRESPONSE_TIMEOUT_VALUE(x: u32) -> u32 {
    (((x) << 0))
}

/// `_DSI_TA_TO_0` (icl_dsi_regs.h)
pub const _DSI_TA_TO_0: u32 = 0x6b04c;

/// `_DSI_TA_TO_1` (icl_dsi_regs.h)
pub const _DSI_TA_TO_1: u32 = 0x6b84c;

/// `DSI_TA_TO(tc)` (icl_dsi_regs.h)
pub const fn DSI_TA_TO(tc: u32) -> I915RegT {
    _MMIO_DSI(tc, _DSI_TA_TO_0, _DSI_TA_TO_1)
}

/// `TA_TIMED_OUT` (icl_dsi_regs.h)
pub const TA_TIMED_OUT: u32 = ((1 << 16));

/// `TA_TIMEOUT_VALUE_MASK` (icl_dsi_regs.h)
pub const TA_TIMEOUT_VALUE_MASK: u32 = ((0xffff << 0));

/// `TA_TIMEOUT_VALUE_SHIFT` (icl_dsi_regs.h)
pub const TA_TIMEOUT_VALUE_SHIFT: u32 = 0;

/// `TA_TIMEOUT_VALUE(x)` (icl_dsi_regs.h)
pub const fn TA_TIMEOUT_VALUE(x: u32) -> u32 {
    (((x) << 0))
}
