// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/vlv_dsi_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `VLV_MIPI_BASE` (vlv_dsi_regs.h)
pub const VLV_MIPI_BASE: u32 = VLV_DISPLAY_BASE;

/// `BXT_MIPI_BASE` (vlv_dsi_regs.h)
pub const BXT_MIPI_BASE: u32 = 0x60000;

/// `_BXT_MIPIA_TRANS_HACTIVE` (vlv_dsi_regs.h)
pub const _BXT_MIPIA_TRANS_HACTIVE: u32 = 0xb0f8;

/// `_BXT_MIPIC_TRANS_HACTIVE` (vlv_dsi_regs.h)
pub const _BXT_MIPIC_TRANS_HACTIVE: u32 = 0xb8f8;

/// `_BXT_MIPIA_TRANS_VACTIVE` (vlv_dsi_regs.h)
pub const _BXT_MIPIA_TRANS_VACTIVE: u32 = 0xb0fc;

/// `_BXT_MIPIC_TRANS_VACTIVE` (vlv_dsi_regs.h)
pub const _BXT_MIPIC_TRANS_VACTIVE: u32 = 0xb8fc;

/// `_BXT_MIPIA_TRANS_VTOTAL` (vlv_dsi_regs.h)
pub const _BXT_MIPIA_TRANS_VTOTAL: u32 = 0xb100;

/// `_BXT_MIPIC_TRANS_VTOTAL` (vlv_dsi_regs.h)
pub const _BXT_MIPIC_TRANS_VTOTAL: u32 = 0xb900;

/// `BXT_P_DSI_REGULATOR_CFG` (vlv_dsi_regs.h)
pub const BXT_P_DSI_REGULATOR_CFG: I915RegT = mmio(0x160020);

/// `STAP_SELECT` (vlv_dsi_regs.h)
pub const STAP_SELECT: u32 = ((1 << 0));

/// `BXT_P_DSI_REGULATOR_TX_CTRL` (vlv_dsi_regs.h)
pub const BXT_P_DSI_REGULATOR_TX_CTRL: I915RegT = mmio(0x160054);

/// `HS_IO_CTRL_SELECT` (vlv_dsi_regs.h)
pub const HS_IO_CTRL_SELECT: u32 = ((1 << 0));

/// `_MIPIA_PORT_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_PORT_CTRL: u32 = 0x61190;

/// `_MIPIC_PORT_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_PORT_CTRL: u32 = 0x61700;

/// `_BXT_MIPIA_PORT_CTRL` (vlv_dsi_regs.h)
pub const _BXT_MIPIA_PORT_CTRL: u32 = 0xb0c0;

/// `_BXT_MIPIC_PORT_CTRL` (vlv_dsi_regs.h)
pub const _BXT_MIPIC_PORT_CTRL: u32 = 0xb8c0;

/// `DPI_ENABLE` (vlv_dsi_regs.h)
pub const DPI_ENABLE: u32 = ((1 << 31));

/// `MIPIA_MIPI4DPHY_DELAY_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const MIPIA_MIPI4DPHY_DELAY_COUNT_SHIFT: u32 = 27;

/// `MIPIA_MIPI4DPHY_DELAY_COUNT_MASK` (vlv_dsi_regs.h)
pub const MIPIA_MIPI4DPHY_DELAY_COUNT_MASK: u32 = ((0xf << 27));

/// `DUAL_LINK_MODE_SHIFT` (vlv_dsi_regs.h)
pub const DUAL_LINK_MODE_SHIFT: u32 = 26;

/// `DUAL_LINK_MODE_MASK` (vlv_dsi_regs.h)
pub const DUAL_LINK_MODE_MASK: u32 = ((1 << 26));

/// `DUAL_LINK_MODE_FRONT_BACK` (vlv_dsi_regs.h)
pub const DUAL_LINK_MODE_FRONT_BACK: u32 = ((0 << 26));

/// `DUAL_LINK_MODE_PIXEL_ALTERNATIVE` (vlv_dsi_regs.h)
pub const DUAL_LINK_MODE_PIXEL_ALTERNATIVE: u32 = ((1 << 26));

/// `DITHERING_ENABLE` (vlv_dsi_regs.h)
pub const DITHERING_ENABLE: u32 = ((1 << 25));

/// `FLOPPED_HSTX` (vlv_dsi_regs.h)
pub const FLOPPED_HSTX: u32 = ((1 << 23));

/// `DE_INVERT` (vlv_dsi_regs.h)
pub const DE_INVERT: u32 = ((1 << 19));

/// `MIPIA_FLISDSI_DELAY_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const MIPIA_FLISDSI_DELAY_COUNT_SHIFT: u32 = 18;

/// `MIPIA_FLISDSI_DELAY_COUNT_MASK` (vlv_dsi_regs.h)
pub const MIPIA_FLISDSI_DELAY_COUNT_MASK: u32 = ((0xf << 18));

/// `AFE_LATCHOUT` (vlv_dsi_regs.h)
pub const AFE_LATCHOUT: u32 = ((1 << 17));

/// `LP_OUTPUT_HOLD` (vlv_dsi_regs.h)
pub const LP_OUTPUT_HOLD: u32 = ((1 << 16));

/// `MIPIC_FLISDSI_DELAY_COUNT_HIGH_SHIFT` (vlv_dsi_regs.h)
pub const MIPIC_FLISDSI_DELAY_COUNT_HIGH_SHIFT: u32 = 15;

/// `MIPIC_FLISDSI_DELAY_COUNT_HIGH_MASK` (vlv_dsi_regs.h)
pub const MIPIC_FLISDSI_DELAY_COUNT_HIGH_MASK: u32 = ((1 << 15));

/// `MIPIC_MIPI4DPHY_DELAY_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const MIPIC_MIPI4DPHY_DELAY_COUNT_SHIFT: u32 = 11;

/// `MIPIC_MIPI4DPHY_DELAY_COUNT_MASK` (vlv_dsi_regs.h)
pub const MIPIC_MIPI4DPHY_DELAY_COUNT_MASK: u32 = ((0xf << 11));

/// `CSB_SHIFT` (vlv_dsi_regs.h)
pub const CSB_SHIFT: u32 = 9;

/// `CSB_MASK` (vlv_dsi_regs.h)
pub const CSB_MASK: u32 = ((3 << 9));

/// `CSB_20MHZ` (vlv_dsi_regs.h)
pub const CSB_20MHZ: u32 = ((0 << 9));

/// `CSB_10MHZ` (vlv_dsi_regs.h)
pub const CSB_10MHZ: u32 = ((1 << 9));

/// `CSB_40MHZ` (vlv_dsi_regs.h)
pub const CSB_40MHZ: u32 = ((2 << 9));

/// `BANDGAP_MASK` (vlv_dsi_regs.h)
pub const BANDGAP_MASK: u32 = ((1 << 8));

/// `BANDGAP_PNW_CIRCUIT` (vlv_dsi_regs.h)
pub const BANDGAP_PNW_CIRCUIT: u32 = ((0 << 8));

/// `BANDGAP_LNC_CIRCUIT` (vlv_dsi_regs.h)
pub const BANDGAP_LNC_CIRCUIT: u32 = ((1 << 8));

/// `MIPIC_FLISDSI_DELAY_COUNT_LOW_SHIFT` (vlv_dsi_regs.h)
pub const MIPIC_FLISDSI_DELAY_COUNT_LOW_SHIFT: u32 = 5;

/// `MIPIC_FLISDSI_DELAY_COUNT_LOW_MASK` (vlv_dsi_regs.h)
pub const MIPIC_FLISDSI_DELAY_COUNT_LOW_MASK: u32 = ((7 << 5));

/// `TEARING_EFFECT_DELAY` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_DELAY: u32 = ((1 << 4));

/// `TEARING_EFFECT_SHIFT` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_SHIFT: u32 = 2;

/// `TEARING_EFFECT_MASK` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_MASK: u32 = ((3 << 2));

/// `TEARING_EFFECT_OFF` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_OFF: u32 = ((0 << 2));

/// `TEARING_EFFECT_DSI` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_DSI: u32 = ((1 << 2));

/// `TEARING_EFFECT_GPIO` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_GPIO: u32 = ((2 << 2));

/// `LANE_CONFIGURATION_SHIFT` (vlv_dsi_regs.h)
pub const LANE_CONFIGURATION_SHIFT: u32 = 0;

/// `LANE_CONFIGURATION_MASK` (vlv_dsi_regs.h)
pub const LANE_CONFIGURATION_MASK: u32 = ((3 << 0));

/// `LANE_CONFIGURATION_4LANE` (vlv_dsi_regs.h)
pub const LANE_CONFIGURATION_4LANE: u32 = ((0 << 0));

/// `LANE_CONFIGURATION_DUAL_LINK_A` (vlv_dsi_regs.h)
pub const LANE_CONFIGURATION_DUAL_LINK_A: u32 = ((1 << 0));

/// `LANE_CONFIGURATION_DUAL_LINK_B` (vlv_dsi_regs.h)
pub const LANE_CONFIGURATION_DUAL_LINK_B: u32 = ((2 << 0));

/// `_MIPIA_TEARING_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_TEARING_CTRL: u32 = 0x61194;

/// `_MIPIC_TEARING_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_TEARING_CTRL: u32 = 0x61704;

/// `TEARING_EFFECT_DELAY_SHIFT` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_DELAY_SHIFT: u32 = 0;

/// `TEARING_EFFECT_DELAY_MASK` (vlv_dsi_regs.h)
pub const TEARING_EFFECT_DELAY_MASK: u32 = ((0xffff << 0));

/// `_MIPIA_DEVICE_READY` (vlv_dsi_regs.h)
pub const _MIPIA_DEVICE_READY: u32 = 0xb000;

/// `_MIPIC_DEVICE_READY` (vlv_dsi_regs.h)
pub const _MIPIC_DEVICE_READY: u32 = 0xb800;

/// `BUS_POSSESSION` (vlv_dsi_regs.h)
pub const BUS_POSSESSION: u32 = ((1 << 3));

/// `ULPS_STATE_MASK` (vlv_dsi_regs.h)
pub const ULPS_STATE_MASK: u32 = ((3 << 1));

/// `ULPS_STATE_ENTER` (vlv_dsi_regs.h)
pub const ULPS_STATE_ENTER: u32 = ((2 << 1));

/// `ULPS_STATE_EXIT` (vlv_dsi_regs.h)
pub const ULPS_STATE_EXIT: u32 = ((1 << 1));

/// `ULPS_STATE_NORMAL_OPERATION` (vlv_dsi_regs.h)
pub const ULPS_STATE_NORMAL_OPERATION: u32 = ((0 << 1));

/// `DEVICE_READY` (vlv_dsi_regs.h)
pub const DEVICE_READY: u32 = ((1 << 0));

/// `_MIPIA_INTR_STAT` (vlv_dsi_regs.h)
pub const _MIPIA_INTR_STAT: u32 = 0xb004;

/// `_MIPIC_INTR_STAT` (vlv_dsi_regs.h)
pub const _MIPIC_INTR_STAT: u32 = 0xb804;

/// `_MIPIA_INTR_EN` (vlv_dsi_regs.h)
pub const _MIPIA_INTR_EN: u32 = 0xb008;

/// `_MIPIC_INTR_EN` (vlv_dsi_regs.h)
pub const _MIPIC_INTR_EN: u32 = 0xb808;

/// `TEARING_EFFECT` (vlv_dsi_regs.h)
pub const TEARING_EFFECT: u32 = ((1 << 31));

/// `SPL_PKT_SENT_INTERRUPT` (vlv_dsi_regs.h)
pub const SPL_PKT_SENT_INTERRUPT: u32 = ((1 << 30));

/// `GEN_READ_DATA_AVAIL` (vlv_dsi_regs.h)
pub const GEN_READ_DATA_AVAIL: u32 = ((1 << 29));

/// `LP_GENERIC_WR_FIFO_FULL` (vlv_dsi_regs.h)
pub const LP_GENERIC_WR_FIFO_FULL: u32 = ((1 << 28));

/// `HS_GENERIC_WR_FIFO_FULL` (vlv_dsi_regs.h)
pub const HS_GENERIC_WR_FIFO_FULL: u32 = ((1 << 27));

/// `RX_PROT_VIOLATION` (vlv_dsi_regs.h)
pub const RX_PROT_VIOLATION: u32 = ((1 << 26));

/// `RX_INVALID_TX_LENGTH` (vlv_dsi_regs.h)
pub const RX_INVALID_TX_LENGTH: u32 = ((1 << 25));

/// `ACK_WITH_NO_ERROR` (vlv_dsi_regs.h)
pub const ACK_WITH_NO_ERROR: u32 = ((1 << 24));

/// `TURN_AROUND_ACK_TIMEOUT` (vlv_dsi_regs.h)
pub const TURN_AROUND_ACK_TIMEOUT: u32 = ((1 << 23));

/// `LP_RX_TIMEOUT` (vlv_dsi_regs.h)
pub const LP_RX_TIMEOUT: u32 = ((1 << 22));

/// `HS_TX_TIMEOUT` (vlv_dsi_regs.h)
pub const HS_TX_TIMEOUT: u32 = ((1 << 21));

/// `DPI_FIFO_UNDERRUN` (vlv_dsi_regs.h)
pub const DPI_FIFO_UNDERRUN: u32 = ((1 << 20));

/// `LOW_CONTENTION` (vlv_dsi_regs.h)
pub const LOW_CONTENTION: u32 = ((1 << 19));

/// `HIGH_CONTENTION` (vlv_dsi_regs.h)
pub const HIGH_CONTENTION: u32 = ((1 << 18));

/// `TXDSI_VC_ID_INVALID` (vlv_dsi_regs.h)
pub const TXDSI_VC_ID_INVALID: u32 = ((1 << 17));

/// `TXDSI_DATA_TYPE_NOT_RECOGNISED` (vlv_dsi_regs.h)
pub const TXDSI_DATA_TYPE_NOT_RECOGNISED: u32 = ((1 << 16));

/// `TXCHECKSUM_ERROR` (vlv_dsi_regs.h)
pub const TXCHECKSUM_ERROR: u32 = ((1 << 15));

/// `TXECC_MULTIBIT_ERROR` (vlv_dsi_regs.h)
pub const TXECC_MULTIBIT_ERROR: u32 = ((1 << 14));

/// `TXECC_SINGLE_BIT_ERROR` (vlv_dsi_regs.h)
pub const TXECC_SINGLE_BIT_ERROR: u32 = ((1 << 13));

/// `TXFALSE_CONTROL_ERROR` (vlv_dsi_regs.h)
pub const TXFALSE_CONTROL_ERROR: u32 = ((1 << 12));

/// `RXDSI_VC_ID_INVALID` (vlv_dsi_regs.h)
pub const RXDSI_VC_ID_INVALID: u32 = ((1 << 11));

/// `RXDSI_DATA_TYPE_NOT_REGOGNISED` (vlv_dsi_regs.h)
pub const RXDSI_DATA_TYPE_NOT_REGOGNISED: u32 = ((1 << 10));

/// `RXCHECKSUM_ERROR` (vlv_dsi_regs.h)
pub const RXCHECKSUM_ERROR: u32 = ((1 << 9));

/// `RXECC_MULTIBIT_ERROR` (vlv_dsi_regs.h)
pub const RXECC_MULTIBIT_ERROR: u32 = ((1 << 8));

/// `RXECC_SINGLE_BIT_ERROR` (vlv_dsi_regs.h)
pub const RXECC_SINGLE_BIT_ERROR: u32 = ((1 << 7));

/// `RXFALSE_CONTROL_ERROR` (vlv_dsi_regs.h)
pub const RXFALSE_CONTROL_ERROR: u32 = ((1 << 6));

/// `RXHS_RECEIVE_TIMEOUT_ERROR` (vlv_dsi_regs.h)
pub const RXHS_RECEIVE_TIMEOUT_ERROR: u32 = ((1 << 5));

/// `RX_LP_TX_SYNC_ERROR` (vlv_dsi_regs.h)
pub const RX_LP_TX_SYNC_ERROR: u32 = ((1 << 4));

/// `RXEXCAPE_MODE_ENTRY_ERROR` (vlv_dsi_regs.h)
pub const RXEXCAPE_MODE_ENTRY_ERROR: u32 = ((1 << 3));

/// `RXEOT_SYNC_ERROR` (vlv_dsi_regs.h)
pub const RXEOT_SYNC_ERROR: u32 = ((1 << 2));

/// `RXSOT_SYNC_ERROR` (vlv_dsi_regs.h)
pub const RXSOT_SYNC_ERROR: u32 = ((1 << 1));

/// `RXSOT_ERROR` (vlv_dsi_regs.h)
pub const RXSOT_ERROR: u32 = ((1 << 0));

/// `_MIPIA_DSI_FUNC_PRG` (vlv_dsi_regs.h)
pub const _MIPIA_DSI_FUNC_PRG: u32 = 0xb00c;

/// `_MIPIC_DSI_FUNC_PRG` (vlv_dsi_regs.h)
pub const _MIPIC_DSI_FUNC_PRG: u32 = 0xb80c;

/// `CMD_MODE_DATA_WIDTH_MASK` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_MASK: u32 = ((7 << 13));

/// `CMD_MODE_NOT_SUPPORTED` (vlv_dsi_regs.h)
pub const CMD_MODE_NOT_SUPPORTED: u32 = ((0 << 13));

/// `CMD_MODE_DATA_WIDTH_16_BIT` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_16_BIT: u32 = ((1 << 13));

/// `CMD_MODE_DATA_WIDTH_9_BIT` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_9_BIT: u32 = ((2 << 13));

/// `CMD_MODE_DATA_WIDTH_8_BIT` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_8_BIT: u32 = ((3 << 13));

/// `CMD_MODE_DATA_WIDTH_OPTION1` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_OPTION1: u32 = ((4 << 13));

/// `CMD_MODE_DATA_WIDTH_OPTION2` (vlv_dsi_regs.h)
pub const CMD_MODE_DATA_WIDTH_OPTION2: u32 = ((5 << 13));

/// `VID_MODE_FORMAT_MASK` (vlv_dsi_regs.h)
pub const VID_MODE_FORMAT_MASK: u32 = ((0xf << 7));

/// `VID_MODE_NOT_SUPPORTED` (vlv_dsi_regs.h)
pub const VID_MODE_NOT_SUPPORTED: u32 = ((0 << 7));

/// `VID_MODE_FORMAT_RGB565` (vlv_dsi_regs.h)
pub const VID_MODE_FORMAT_RGB565: u32 = ((1 << 7));

/// `VID_MODE_FORMAT_RGB666_PACKED` (vlv_dsi_regs.h)
pub const VID_MODE_FORMAT_RGB666_PACKED: u32 = ((2 << 7));

/// `VID_MODE_FORMAT_RGB666` (vlv_dsi_regs.h)
pub const VID_MODE_FORMAT_RGB666: u32 = ((3 << 7));

/// `VID_MODE_FORMAT_RGB888` (vlv_dsi_regs.h)
pub const VID_MODE_FORMAT_RGB888: u32 = ((4 << 7));

/// `CMD_MODE_CHANNEL_NUMBER_SHIFT` (vlv_dsi_regs.h)
pub const CMD_MODE_CHANNEL_NUMBER_SHIFT: u32 = 5;

/// `CMD_MODE_CHANNEL_NUMBER_MASK` (vlv_dsi_regs.h)
pub const CMD_MODE_CHANNEL_NUMBER_MASK: u32 = ((3 << 5));

/// `VID_MODE_CHANNEL_NUMBER_SHIFT` (vlv_dsi_regs.h)
pub const VID_MODE_CHANNEL_NUMBER_SHIFT: u32 = 3;

/// `VID_MODE_CHANNEL_NUMBER_MASK` (vlv_dsi_regs.h)
pub const VID_MODE_CHANNEL_NUMBER_MASK: u32 = ((3 << 3));

/// `DATA_LANES_PRG_REG_SHIFT` (vlv_dsi_regs.h)
pub const DATA_LANES_PRG_REG_SHIFT: u32 = 0;

/// `DATA_LANES_PRG_REG_MASK` (vlv_dsi_regs.h)
pub const DATA_LANES_PRG_REG_MASK: u32 = ((7 << 0));

/// `_MIPIA_HS_TX_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIA_HS_TX_TIMEOUT: u32 = 0xb010;

/// `_MIPIC_HS_TX_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIC_HS_TX_TIMEOUT: u32 = 0xb810;

/// `HIGH_SPEED_TX_TIMEOUT_COUNTER_MASK` (vlv_dsi_regs.h)
pub const HIGH_SPEED_TX_TIMEOUT_COUNTER_MASK: u32 = 0xffffff;

/// `_MIPIA_LP_RX_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIA_LP_RX_TIMEOUT: u32 = 0xb014;

/// `_MIPIC_LP_RX_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIC_LP_RX_TIMEOUT: u32 = 0xb814;

/// `LOW_POWER_RX_TIMEOUT_COUNTER_MASK` (vlv_dsi_regs.h)
pub const LOW_POWER_RX_TIMEOUT_COUNTER_MASK: u32 = 0xffffff;

/// `_MIPIA_TURN_AROUND_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIA_TURN_AROUND_TIMEOUT: u32 = 0xb018;

/// `_MIPIC_TURN_AROUND_TIMEOUT` (vlv_dsi_regs.h)
pub const _MIPIC_TURN_AROUND_TIMEOUT: u32 = 0xb818;

/// `TURN_AROUND_TIMEOUT_MASK` (vlv_dsi_regs.h)
pub const TURN_AROUND_TIMEOUT_MASK: u32 = 0x3f;

/// `_MIPIA_DEVICE_RESET_TIMER` (vlv_dsi_regs.h)
pub const _MIPIA_DEVICE_RESET_TIMER: u32 = 0xb01c;

/// `_MIPIC_DEVICE_RESET_TIMER` (vlv_dsi_regs.h)
pub const _MIPIC_DEVICE_RESET_TIMER: u32 = 0xb81c;

/// `DEVICE_RESET_TIMER_MASK` (vlv_dsi_regs.h)
pub const DEVICE_RESET_TIMER_MASK: u32 = 0xffff;

/// `_MIPIA_DPI_RESOLUTION` (vlv_dsi_regs.h)
pub const _MIPIA_DPI_RESOLUTION: u32 = 0xb020;

/// `_MIPIC_DPI_RESOLUTION` (vlv_dsi_regs.h)
pub const _MIPIC_DPI_RESOLUTION: u32 = 0xb820;

/// `VERTICAL_ADDRESS_SHIFT` (vlv_dsi_regs.h)
pub const VERTICAL_ADDRESS_SHIFT: u32 = 16;

/// `VERTICAL_ADDRESS_MASK` (vlv_dsi_regs.h)
pub const VERTICAL_ADDRESS_MASK: u32 = ((0xffff << 16));

/// `HORIZONTAL_ADDRESS_SHIFT` (vlv_dsi_regs.h)
pub const HORIZONTAL_ADDRESS_SHIFT: u32 = 0;

/// `HORIZONTAL_ADDRESS_MASK` (vlv_dsi_regs.h)
pub const HORIZONTAL_ADDRESS_MASK: u32 = 0xffff;

/// `_MIPIA_DBI_FIFO_THROTTLE` (vlv_dsi_regs.h)
pub const _MIPIA_DBI_FIFO_THROTTLE: u32 = 0xb024;

/// `_MIPIC_DBI_FIFO_THROTTLE` (vlv_dsi_regs.h)
pub const _MIPIC_DBI_FIFO_THROTTLE: u32 = 0xb824;

/// `DBI_FIFO_EMPTY_HALF` (vlv_dsi_regs.h)
pub const DBI_FIFO_EMPTY_HALF: u32 = ((0 << 0));

/// `DBI_FIFO_EMPTY_QUARTER` (vlv_dsi_regs.h)
pub const DBI_FIFO_EMPTY_QUARTER: u32 = ((1 << 0));

/// `DBI_FIFO_EMPTY_7_LOCATIONS` (vlv_dsi_regs.h)
pub const DBI_FIFO_EMPTY_7_LOCATIONS: u32 = ((2 << 0));

/// `_MIPIA_HSYNC_PADDING_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_HSYNC_PADDING_COUNT: u32 = 0xb028;

/// `_MIPIC_HSYNC_PADDING_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_HSYNC_PADDING_COUNT: u32 = 0xb828;

/// `_MIPIA_HBP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_HBP_COUNT: u32 = 0xb02c;

/// `_MIPIC_HBP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_HBP_COUNT: u32 = 0xb82c;

/// `_MIPIA_HFP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_HFP_COUNT: u32 = 0xb030;

/// `_MIPIC_HFP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_HFP_COUNT: u32 = 0xb830;

/// `_MIPIA_HACTIVE_AREA_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_HACTIVE_AREA_COUNT: u32 = 0xb034;

/// `_MIPIC_HACTIVE_AREA_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_HACTIVE_AREA_COUNT: u32 = 0xb834;

/// `_MIPIA_VSYNC_PADDING_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_VSYNC_PADDING_COUNT: u32 = 0xb038;

/// `_MIPIC_VSYNC_PADDING_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_VSYNC_PADDING_COUNT: u32 = 0xb838;

/// `_MIPIA_VBP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_VBP_COUNT: u32 = 0xb03c;

/// `_MIPIC_VBP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_VBP_COUNT: u32 = 0xb83c;

/// `_MIPIA_VFP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_VFP_COUNT: u32 = 0xb040;

/// `_MIPIC_VFP_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_VFP_COUNT: u32 = 0xb840;

/// `_MIPIA_HIGH_LOW_SWITCH_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_HIGH_LOW_SWITCH_COUNT: u32 = 0xb044;

/// `_MIPIC_HIGH_LOW_SWITCH_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_HIGH_LOW_SWITCH_COUNT: u32 = 0xb844;

/// `_MIPIA_DPI_CONTROL` (vlv_dsi_regs.h)
pub const _MIPIA_DPI_CONTROL: u32 = 0xb048;

/// `_MIPIC_DPI_CONTROL` (vlv_dsi_regs.h)
pub const _MIPIC_DPI_CONTROL: u32 = 0xb848;

/// `DPI_LP_MODE` (vlv_dsi_regs.h)
pub const DPI_LP_MODE: u32 = ((1 << 6));

/// `BACKLIGHT_OFF` (vlv_dsi_regs.h)
pub const BACKLIGHT_OFF: u32 = ((1 << 5));

/// `BACKLIGHT_ON` (vlv_dsi_regs.h)
pub const BACKLIGHT_ON: u32 = ((1 << 4));

/// `COLOR_MODE_OFF` (vlv_dsi_regs.h)
pub const COLOR_MODE_OFF: u32 = ((1 << 3));

/// `COLOR_MODE_ON` (vlv_dsi_regs.h)
pub const COLOR_MODE_ON: u32 = ((1 << 2));

/// `TURN_ON` (vlv_dsi_regs.h)
pub const TURN_ON: u32 = ((1 << 1));

/// `SHUTDOWN` (vlv_dsi_regs.h)
pub const SHUTDOWN: u32 = ((1 << 0));

/// `_MIPIA_DPI_DATA` (vlv_dsi_regs.h)
pub const _MIPIA_DPI_DATA: u32 = 0xb04c;

/// `_MIPIC_DPI_DATA` (vlv_dsi_regs.h)
pub const _MIPIC_DPI_DATA: u32 = 0xb84c;

/// `COMMAND_BYTE_SHIFT` (vlv_dsi_regs.h)
pub const COMMAND_BYTE_SHIFT: u32 = 0;

/// `COMMAND_BYTE_MASK` (vlv_dsi_regs.h)
pub const COMMAND_BYTE_MASK: u32 = ((0x3f << 0));

/// `_MIPIA_INIT_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_INIT_COUNT: u32 = 0xb050;

/// `_MIPIC_INIT_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_INIT_COUNT: u32 = 0xb850;

/// `MASTER_INIT_TIMER_SHIFT` (vlv_dsi_regs.h)
pub const MASTER_INIT_TIMER_SHIFT: u32 = 0;

/// `MASTER_INIT_TIMER_MASK` (vlv_dsi_regs.h)
pub const MASTER_INIT_TIMER_MASK: u32 = ((0xffff << 0));

/// `_MIPIA_MAX_RETURN_PKT_SIZE` (vlv_dsi_regs.h)
pub const _MIPIA_MAX_RETURN_PKT_SIZE: u32 = 0xb054;

/// `_MIPIC_MAX_RETURN_PKT_SIZE` (vlv_dsi_regs.h)
pub const _MIPIC_MAX_RETURN_PKT_SIZE: u32 = 0xb854;

/// `MAX_RETURN_PKT_SIZE_SHIFT` (vlv_dsi_regs.h)
pub const MAX_RETURN_PKT_SIZE_SHIFT: u32 = 0;

/// `MAX_RETURN_PKT_SIZE_MASK` (vlv_dsi_regs.h)
pub const MAX_RETURN_PKT_SIZE_MASK: u32 = ((0x3ff << 0));

/// `_MIPIA_VIDEO_MODE_FORMAT` (vlv_dsi_regs.h)
pub const _MIPIA_VIDEO_MODE_FORMAT: u32 = 0xb058;

/// `_MIPIC_VIDEO_MODE_FORMAT` (vlv_dsi_regs.h)
pub const _MIPIC_VIDEO_MODE_FORMAT: u32 = 0xb858;

/// `RANDOM_DPI_DISPLAY_RESOLUTION` (vlv_dsi_regs.h)
pub const RANDOM_DPI_DISPLAY_RESOLUTION: u32 = ((1 << 4));

/// `DISABLE_VIDEO_BTA` (vlv_dsi_regs.h)
pub const DISABLE_VIDEO_BTA: u32 = ((1 << 3));

/// `IP_TG_CONFIG` (vlv_dsi_regs.h)
pub const IP_TG_CONFIG: u32 = ((1 << 2));

/// `VIDEO_MODE_NON_BURST_WITH_SYNC_PULSE` (vlv_dsi_regs.h)
pub const VIDEO_MODE_NON_BURST_WITH_SYNC_PULSE: u32 = ((1 << 0));

/// `VIDEO_MODE_NON_BURST_WITH_SYNC_EVENTS` (vlv_dsi_regs.h)
pub const VIDEO_MODE_NON_BURST_WITH_SYNC_EVENTS: u32 = ((2 << 0));

/// `VIDEO_MODE_BURST` (vlv_dsi_regs.h)
pub const VIDEO_MODE_BURST: u32 = ((3 << 0));

/// `_MIPIA_EOT_DISABLE` (vlv_dsi_regs.h)
pub const _MIPIA_EOT_DISABLE: u32 = 0xb05c;

/// `_MIPIC_EOT_DISABLE` (vlv_dsi_regs.h)
pub const _MIPIC_EOT_DISABLE: u32 = 0xb85c;

/// `BXT_DEFEATURE_DPI_FIFO_CTR` (vlv_dsi_regs.h)
pub const BXT_DEFEATURE_DPI_FIFO_CTR: u32 = ((1 << 9));

/// `BXT_DPHY_DEFEATURE_EN` (vlv_dsi_regs.h)
pub const BXT_DPHY_DEFEATURE_EN: u32 = ((1 << 8));

/// `LP_RX_TIMEOUT_ERROR_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const LP_RX_TIMEOUT_ERROR_RECOVERY_DISABLE: u32 = ((1 << 7));

/// `HS_RX_TIMEOUT_ERROR_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const HS_RX_TIMEOUT_ERROR_RECOVERY_DISABLE: u32 = ((1 << 6));

/// `LOW_CONTENTION_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const LOW_CONTENTION_RECOVERY_DISABLE: u32 = ((1 << 5));

/// `HIGH_CONTENTION_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const HIGH_CONTENTION_RECOVERY_DISABLE: u32 = ((1 << 4));

/// `TXDSI_TYPE_NOT_RECOGNISED_ERROR_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const TXDSI_TYPE_NOT_RECOGNISED_ERROR_RECOVERY_DISABLE: u32 = ((1 << 3));

/// `TXECC_MULTIBIT_ERROR_RECOVERY_DISABLE` (vlv_dsi_regs.h)
pub const TXECC_MULTIBIT_ERROR_RECOVERY_DISABLE: u32 = ((1 << 2));

/// `CLOCKSTOP` (vlv_dsi_regs.h)
pub const CLOCKSTOP: u32 = ((1 << 1));

/// `EOT_DISABLE` (vlv_dsi_regs.h)
pub const EOT_DISABLE: u32 = ((1 << 0));

/// `_MIPIA_LP_BYTECLK` (vlv_dsi_regs.h)
pub const _MIPIA_LP_BYTECLK: u32 = 0xb060;

/// `_MIPIC_LP_BYTECLK` (vlv_dsi_regs.h)
pub const _MIPIC_LP_BYTECLK: u32 = 0xb860;

/// `LP_BYTECLK_SHIFT` (vlv_dsi_regs.h)
pub const LP_BYTECLK_SHIFT: u32 = 0;

/// `LP_BYTECLK_MASK` (vlv_dsi_regs.h)
pub const LP_BYTECLK_MASK: u32 = ((0xffff << 0));

/// `_MIPIA_TLPX_TIME_COUNT` (vlv_dsi_regs.h)
pub const _MIPIA_TLPX_TIME_COUNT: u32 = 0xb0a4;

/// `_MIPIC_TLPX_TIME_COUNT` (vlv_dsi_regs.h)
pub const _MIPIC_TLPX_TIME_COUNT: u32 = 0xb8a4;

/// `_MIPIA_CLK_LANE_TIMING` (vlv_dsi_regs.h)
pub const _MIPIA_CLK_LANE_TIMING: u32 = 0xb098;

/// `_MIPIC_CLK_LANE_TIMING` (vlv_dsi_regs.h)
pub const _MIPIC_CLK_LANE_TIMING: u32 = 0xb898;

/// `_MIPIA_LP_GEN_DATA` (vlv_dsi_regs.h)
pub const _MIPIA_LP_GEN_DATA: u32 = 0xb064;

/// `_MIPIC_LP_GEN_DATA` (vlv_dsi_regs.h)
pub const _MIPIC_LP_GEN_DATA: u32 = 0xb864;

/// `_MIPIA_HS_GEN_DATA` (vlv_dsi_regs.h)
pub const _MIPIA_HS_GEN_DATA: u32 = 0xb068;

/// `_MIPIC_HS_GEN_DATA` (vlv_dsi_regs.h)
pub const _MIPIC_HS_GEN_DATA: u32 = 0xb868;

/// `_MIPIA_LP_GEN_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_LP_GEN_CTRL: u32 = 0xb06c;

/// `_MIPIC_LP_GEN_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_LP_GEN_CTRL: u32 = 0xb86c;

/// `_MIPIA_HS_GEN_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_HS_GEN_CTRL: u32 = 0xb070;

/// `_MIPIC_HS_GEN_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_HS_GEN_CTRL: u32 = 0xb870;

/// `LONG_PACKET_WORD_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const LONG_PACKET_WORD_COUNT_SHIFT: u32 = 8;

/// `LONG_PACKET_WORD_COUNT_MASK` (vlv_dsi_regs.h)
pub const LONG_PACKET_WORD_COUNT_MASK: u32 = ((0xffff << 8));

/// `SHORT_PACKET_PARAM_SHIFT` (vlv_dsi_regs.h)
pub const SHORT_PACKET_PARAM_SHIFT: u32 = 8;

/// `SHORT_PACKET_PARAM_MASK` (vlv_dsi_regs.h)
pub const SHORT_PACKET_PARAM_MASK: u32 = ((0xffff << 8));

/// `VIRTUAL_CHANNEL_SHIFT` (vlv_dsi_regs.h)
pub const VIRTUAL_CHANNEL_SHIFT: u32 = 6;

/// `VIRTUAL_CHANNEL_MASK` (vlv_dsi_regs.h)
pub const VIRTUAL_CHANNEL_MASK: u32 = ((3 << 6));

/// `DATA_TYPE_SHIFT` (vlv_dsi_regs.h)
pub const DATA_TYPE_SHIFT: u32 = 0;

/// `DATA_TYPE_MASK` (vlv_dsi_regs.h)
pub const DATA_TYPE_MASK: u32 = ((0x3f << 0));

/// `_MIPIA_GEN_FIFO_STAT` (vlv_dsi_regs.h)
pub const _MIPIA_GEN_FIFO_STAT: u32 = 0xb074;

/// `_MIPIC_GEN_FIFO_STAT` (vlv_dsi_regs.h)
pub const _MIPIC_GEN_FIFO_STAT: u32 = 0xb874;

/// `DPI_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const DPI_FIFO_EMPTY: u32 = ((1 << 28));

/// `DBI_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const DBI_FIFO_EMPTY: u32 = ((1 << 27));

/// `LP_CTRL_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const LP_CTRL_FIFO_EMPTY: u32 = ((1 << 26));

/// `LP_CTRL_FIFO_HALF_EMPTY` (vlv_dsi_regs.h)
pub const LP_CTRL_FIFO_HALF_EMPTY: u32 = ((1 << 25));

/// `LP_CTRL_FIFO_FULL` (vlv_dsi_regs.h)
pub const LP_CTRL_FIFO_FULL: u32 = ((1 << 24));

/// `HS_CTRL_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const HS_CTRL_FIFO_EMPTY: u32 = ((1 << 18));

/// `HS_CTRL_FIFO_HALF_EMPTY` (vlv_dsi_regs.h)
pub const HS_CTRL_FIFO_HALF_EMPTY: u32 = ((1 << 17));

/// `HS_CTRL_FIFO_FULL` (vlv_dsi_regs.h)
pub const HS_CTRL_FIFO_FULL: u32 = ((1 << 16));

/// `LP_DATA_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const LP_DATA_FIFO_EMPTY: u32 = ((1 << 10));

/// `LP_DATA_FIFO_HALF_EMPTY` (vlv_dsi_regs.h)
pub const LP_DATA_FIFO_HALF_EMPTY: u32 = ((1 << 9));

/// `LP_DATA_FIFO_FULL` (vlv_dsi_regs.h)
pub const LP_DATA_FIFO_FULL: u32 = ((1 << 8));

/// `HS_DATA_FIFO_EMPTY` (vlv_dsi_regs.h)
pub const HS_DATA_FIFO_EMPTY: u32 = ((1 << 2));

/// `HS_DATA_FIFO_HALF_EMPTY` (vlv_dsi_regs.h)
pub const HS_DATA_FIFO_HALF_EMPTY: u32 = ((1 << 1));

/// `HS_DATA_FIFO_FULL` (vlv_dsi_regs.h)
pub const HS_DATA_FIFO_FULL: u32 = ((1 << 0));

/// `_MIPIA_HS_LS_DBI_ENABLE` (vlv_dsi_regs.h)
pub const _MIPIA_HS_LS_DBI_ENABLE: u32 = 0xb078;

/// `_MIPIC_HS_LS_DBI_ENABLE` (vlv_dsi_regs.h)
pub const _MIPIC_HS_LS_DBI_ENABLE: u32 = 0xb878;

/// `DBI_HS_LP_MODE_MASK` (vlv_dsi_regs.h)
pub const DBI_HS_LP_MODE_MASK: u32 = ((1 << 0));

/// `DBI_LP_MODE` (vlv_dsi_regs.h)
pub const DBI_LP_MODE: u32 = ((1 << 0));

/// `DBI_HS_MODE` (vlv_dsi_regs.h)
pub const DBI_HS_MODE: u32 = ((0 << 0));

/// `_MIPIA_DPHY_PARAM` (vlv_dsi_regs.h)
pub const _MIPIA_DPHY_PARAM: u32 = 0xb080;

/// `_MIPIC_DPHY_PARAM` (vlv_dsi_regs.h)
pub const _MIPIC_DPHY_PARAM: u32 = 0xb880;

/// `EXIT_ZERO_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const EXIT_ZERO_COUNT_SHIFT: u32 = 24;

/// `EXIT_ZERO_COUNT_MASK` (vlv_dsi_regs.h)
pub const EXIT_ZERO_COUNT_MASK: u32 = ((0x3f << 24));

/// `TRAIL_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const TRAIL_COUNT_SHIFT: u32 = 16;

/// `TRAIL_COUNT_MASK` (vlv_dsi_regs.h)
pub const TRAIL_COUNT_MASK: u32 = ((0x1f << 16));

/// `CLK_ZERO_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const CLK_ZERO_COUNT_SHIFT: u32 = 8;

/// `CLK_ZERO_COUNT_MASK` (vlv_dsi_regs.h)
pub const CLK_ZERO_COUNT_MASK: u32 = ((0xff << 8));

/// `PREPARE_COUNT_SHIFT` (vlv_dsi_regs.h)
pub const PREPARE_COUNT_SHIFT: u32 = 0;

/// `PREPARE_COUNT_MASK` (vlv_dsi_regs.h)
pub const PREPARE_COUNT_MASK: u32 = ((0x3f << 0));

/// `_MIPIA_DBI_BW_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_DBI_BW_CTRL: u32 = 0xb084;

/// `_MIPIC_DBI_BW_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_DBI_BW_CTRL: u32 = 0xb884;

/// `_MIPIA_CLK_LANE_SWITCH_TIME_CNT` (vlv_dsi_regs.h)
pub const _MIPIA_CLK_LANE_SWITCH_TIME_CNT: u32 = 0xb088;

/// `_MIPIC_CLK_LANE_SWITCH_TIME_CNT` (vlv_dsi_regs.h)
pub const _MIPIC_CLK_LANE_SWITCH_TIME_CNT: u32 = 0xb888;

/// `LP_HS_SSW_CNT_SHIFT` (vlv_dsi_regs.h)
pub const LP_HS_SSW_CNT_SHIFT: u32 = 16;

/// `LP_HS_SSW_CNT_MASK` (vlv_dsi_regs.h)
pub const LP_HS_SSW_CNT_MASK: u32 = ((0xffff << 16));

/// `HS_LP_PWR_SW_CNT_SHIFT` (vlv_dsi_regs.h)
pub const HS_LP_PWR_SW_CNT_SHIFT: u32 = 0;

/// `HS_LP_PWR_SW_CNT_MASK` (vlv_dsi_regs.h)
pub const HS_LP_PWR_SW_CNT_MASK: u32 = ((0xffff << 0));

/// `_MIPIA_STOP_STATE_STALL` (vlv_dsi_regs.h)
pub const _MIPIA_STOP_STATE_STALL: u32 = 0xb08c;

/// `_MIPIC_STOP_STATE_STALL` (vlv_dsi_regs.h)
pub const _MIPIC_STOP_STATE_STALL: u32 = 0xb88c;

/// `STOP_STATE_STALL_COUNTER_SHIFT` (vlv_dsi_regs.h)
pub const STOP_STATE_STALL_COUNTER_SHIFT: u32 = 0;

/// `STOP_STATE_STALL_COUNTER_MASK` (vlv_dsi_regs.h)
pub const STOP_STATE_STALL_COUNTER_MASK: u32 = ((0xff << 0));

/// `_MIPIA_INTR_STAT_REG_1` (vlv_dsi_regs.h)
pub const _MIPIA_INTR_STAT_REG_1: u32 = 0xb090;

/// `_MIPIC_INTR_STAT_REG_1` (vlv_dsi_regs.h)
pub const _MIPIC_INTR_STAT_REG_1: u32 = 0xb890;

/// `_MIPIA_INTR_EN_REG_1` (vlv_dsi_regs.h)
pub const _MIPIA_INTR_EN_REG_1: u32 = 0xb094;

/// `_MIPIC_INTR_EN_REG_1` (vlv_dsi_regs.h)
pub const _MIPIC_INTR_EN_REG_1: u32 = 0xb894;

/// `RX_CONTENTION_DETECTED` (vlv_dsi_regs.h)
pub const RX_CONTENTION_DETECTED: u32 = ((1 << 0));

/// `DBI_TYPEC_ENABLE` (vlv_dsi_regs.h)
pub const DBI_TYPEC_ENABLE: u32 = ((1 << 31));

/// `DBI_TYPEC_WIP` (vlv_dsi_regs.h)
pub const DBI_TYPEC_WIP: u32 = ((1 << 30));

/// `DBI_TYPEC_OPTION_SHIFT` (vlv_dsi_regs.h)
pub const DBI_TYPEC_OPTION_SHIFT: u32 = 28;

/// `DBI_TYPEC_OPTION_MASK` (vlv_dsi_regs.h)
pub const DBI_TYPEC_OPTION_MASK: u32 = ((3 << 28));

/// `DBI_TYPEC_FREQ_SHIFT` (vlv_dsi_regs.h)
pub const DBI_TYPEC_FREQ_SHIFT: u32 = 24;

/// `DBI_TYPEC_FREQ_MASK` (vlv_dsi_regs.h)
pub const DBI_TYPEC_FREQ_MASK: u32 = ((0xf << 24));

/// `DBI_TYPEC_OVERRIDE` (vlv_dsi_regs.h)
pub const DBI_TYPEC_OVERRIDE: u32 = ((1 << 8));

/// `DBI_TYPEC_OVERRIDE_COUNTER_SHIFT` (vlv_dsi_regs.h)
pub const DBI_TYPEC_OVERRIDE_COUNTER_SHIFT: u32 = 0;

/// `DBI_TYPEC_OVERRIDE_COUNTER_MASK` (vlv_dsi_regs.h)
pub const DBI_TYPEC_OVERRIDE_COUNTER_MASK: u32 = ((0xff << 0));

/// `_MIPIA_CTRL` (vlv_dsi_regs.h)
pub const _MIPIA_CTRL: u32 = 0xb104;

/// `_MIPIC_CTRL` (vlv_dsi_regs.h)
pub const _MIPIC_CTRL: u32 = 0xb904;

/// `ESCAPE_CLOCK_DIVIDER_SHIFT` (vlv_dsi_regs.h)
pub const ESCAPE_CLOCK_DIVIDER_SHIFT: u32 = 5;

/// `ESCAPE_CLOCK_DIVIDER_MASK` (vlv_dsi_regs.h)
pub const ESCAPE_CLOCK_DIVIDER_MASK: u32 = ((3 << 5));

/// `ESCAPE_CLOCK_DIVIDER_1` (vlv_dsi_regs.h)
pub const ESCAPE_CLOCK_DIVIDER_1: u32 = ((0 << 5));

/// `ESCAPE_CLOCK_DIVIDER_2` (vlv_dsi_regs.h)
pub const ESCAPE_CLOCK_DIVIDER_2: u32 = ((1 << 5));

/// `ESCAPE_CLOCK_DIVIDER_4` (vlv_dsi_regs.h)
pub const ESCAPE_CLOCK_DIVIDER_4: u32 = ((2 << 5));

/// `READ_REQUEST_PRIORITY_SHIFT` (vlv_dsi_regs.h)
pub const READ_REQUEST_PRIORITY_SHIFT: u32 = 3;

/// `READ_REQUEST_PRIORITY_MASK` (vlv_dsi_regs.h)
pub const READ_REQUEST_PRIORITY_MASK: u32 = ((3 << 3));

/// `READ_REQUEST_PRIORITY_LOW` (vlv_dsi_regs.h)
pub const READ_REQUEST_PRIORITY_LOW: u32 = ((0 << 3));

/// `READ_REQUEST_PRIORITY_HIGH` (vlv_dsi_regs.h)
pub const READ_REQUEST_PRIORITY_HIGH: u32 = ((3 << 3));

/// `RGB_FLIP_TO_BGR` (vlv_dsi_regs.h)
pub const RGB_FLIP_TO_BGR: u32 = ((1 << 2));

/// `BXT_PIPE_SELECT_SHIFT` (vlv_dsi_regs.h)
pub const BXT_PIPE_SELECT_SHIFT: u32 = 7;

/// `BXT_PIPE_SELECT_MASK` (vlv_dsi_regs.h)
pub const BXT_PIPE_SELECT_MASK: u32 = ((7 << 7));

/// `BXT_PIPE_SELECT(pipe)` (vlv_dsi_regs.h)
pub const fn BXT_PIPE_SELECT(pipe: u32) -> u32 {
    (((pipe) << 7))
}

/// `GLK_PHY_STATUS_PORT_READY` (vlv_dsi_regs.h)
pub const GLK_PHY_STATUS_PORT_READY: u32 = ((1 << 31));

/// `GLK_ULPS_NOT_ACTIVE` (vlv_dsi_regs.h)
pub const GLK_ULPS_NOT_ACTIVE: u32 = ((1 << 30));

/// `GLK_MIPIIO_RESET_RELEASED` (vlv_dsi_regs.h)
pub const GLK_MIPIIO_RESET_RELEASED: u32 = ((1 << 28));

/// `GLK_CLOCK_LANE_STOP_STATE` (vlv_dsi_regs.h)
pub const GLK_CLOCK_LANE_STOP_STATE: u32 = ((1 << 27));

/// `GLK_DATA_LANE_STOP_STATE` (vlv_dsi_regs.h)
pub const GLK_DATA_LANE_STOP_STATE: u32 = ((1 << 26));

/// `GLK_LP_WAKE` (vlv_dsi_regs.h)
pub const GLK_LP_WAKE: u32 = ((1 << 22));

/// `GLK_LP11_LOW_PWR_MODE` (vlv_dsi_regs.h)
pub const GLK_LP11_LOW_PWR_MODE: u32 = ((1 << 21));

/// `GLK_LP00_LOW_PWR_MODE` (vlv_dsi_regs.h)
pub const GLK_LP00_LOW_PWR_MODE: u32 = ((1 << 20));

/// `GLK_FIREWALL_ENABLE` (vlv_dsi_regs.h)
pub const GLK_FIREWALL_ENABLE: u32 = ((1 << 16));

/// `BXT_PIXEL_OVERLAP_CNT_MASK` (vlv_dsi_regs.h)
pub const BXT_PIXEL_OVERLAP_CNT_MASK: u32 = ((0xf << 10));

/// `BXT_PIXEL_OVERLAP_CNT_SHIFT` (vlv_dsi_regs.h)
pub const BXT_PIXEL_OVERLAP_CNT_SHIFT: u32 = 10;

/// `BXT_DSC_ENABLE` (vlv_dsi_regs.h)
pub const BXT_DSC_ENABLE: u32 = ((1 << 3));

/// `BXT_RGB_FLIP` (vlv_dsi_regs.h)
pub const BXT_RGB_FLIP: u32 = ((1 << 2));

/// `GLK_MIPIIO_PORT_POWERED` (vlv_dsi_regs.h)
pub const GLK_MIPIIO_PORT_POWERED: u32 = ((1 << 1));

/// `GLK_MIPIIO_ENABLE` (vlv_dsi_regs.h)
pub const GLK_MIPIIO_ENABLE: u32 = ((1 << 0));

/// `_MIPIA_DATA_ADDRESS` (vlv_dsi_regs.h)
pub const _MIPIA_DATA_ADDRESS: u32 = 0xb108;

/// `_MIPIC_DATA_ADDRESS` (vlv_dsi_regs.h)
pub const _MIPIC_DATA_ADDRESS: u32 = 0xb908;

/// `DATA_MEM_ADDRESS_SHIFT` (vlv_dsi_regs.h)
pub const DATA_MEM_ADDRESS_SHIFT: u32 = 5;

/// `DATA_MEM_ADDRESS_MASK` (vlv_dsi_regs.h)
pub const DATA_MEM_ADDRESS_MASK: u32 = ((0x7ffffff << 5));

/// `DATA_VALID` (vlv_dsi_regs.h)
pub const DATA_VALID: u32 = ((1 << 0));

/// `_MIPIA_DATA_LENGTH` (vlv_dsi_regs.h)
pub const _MIPIA_DATA_LENGTH: u32 = 0xb10c;

/// `_MIPIC_DATA_LENGTH` (vlv_dsi_regs.h)
pub const _MIPIC_DATA_LENGTH: u32 = 0xb90c;

/// `DATA_LENGTH_SHIFT` (vlv_dsi_regs.h)
pub const DATA_LENGTH_SHIFT: u32 = 0;

/// `DATA_LENGTH_MASK` (vlv_dsi_regs.h)
pub const DATA_LENGTH_MASK: u32 = ((0xfffff << 0));

/// `_MIPIA_COMMAND_ADDRESS` (vlv_dsi_regs.h)
pub const _MIPIA_COMMAND_ADDRESS: u32 = 0xb110;

/// `_MIPIC_COMMAND_ADDRESS` (vlv_dsi_regs.h)
pub const _MIPIC_COMMAND_ADDRESS: u32 = 0xb910;

/// `COMMAND_MEM_ADDRESS_SHIFT` (vlv_dsi_regs.h)
pub const COMMAND_MEM_ADDRESS_SHIFT: u32 = 5;

/// `COMMAND_MEM_ADDRESS_MASK` (vlv_dsi_regs.h)
pub const COMMAND_MEM_ADDRESS_MASK: u32 = ((0x7ffffff << 5));

/// `AUTO_PWG_ENABLE` (vlv_dsi_regs.h)
pub const AUTO_PWG_ENABLE: u32 = ((1 << 2));

/// `MEMORY_WRITE_DATA_FROM_PIPE_RENDERING` (vlv_dsi_regs.h)
pub const MEMORY_WRITE_DATA_FROM_PIPE_RENDERING: u32 = ((1 << 1));

/// `COMMAND_VALID` (vlv_dsi_regs.h)
pub const COMMAND_VALID: u32 = ((1 << 0));

/// `_MIPIA_COMMAND_LENGTH` (vlv_dsi_regs.h)
pub const _MIPIA_COMMAND_LENGTH: u32 = 0xb114;

/// `_MIPIC_COMMAND_LENGTH` (vlv_dsi_regs.h)
pub const _MIPIC_COMMAND_LENGTH: u32 = 0xb914;

/// `COMMAND_LENGTH_SHIFT(n)` (vlv_dsi_regs.h)
pub const fn COMMAND_LENGTH_SHIFT(n: u32) -> u32 {
    ((8 * (n)))
}

/// `COMMAND_LENGTH_MASK(n)` (vlv_dsi_regs.h)
pub const fn COMMAND_LENGTH_MASK(n: u32) -> u32 {
    ((0xff << ((8 * (n)))))
}

/// `_MIPIA_READ_DATA_RETURN0` (vlv_dsi_regs.h)
pub const _MIPIA_READ_DATA_RETURN0: u32 = 0xb118;

/// `_MIPIC_READ_DATA_RETURN0` (vlv_dsi_regs.h)
pub const _MIPIC_READ_DATA_RETURN0: u32 = 0xb918;

/// `_MIPIA_READ_DATA_VALID` (vlv_dsi_regs.h)
pub const _MIPIA_READ_DATA_VALID: u32 = 0xb138;

/// `_MIPIC_READ_DATA_VALID` (vlv_dsi_regs.h)
pub const _MIPIC_READ_DATA_VALID: u32 = 0xb938;

/// `READ_DATA_VALID(n)` (vlv_dsi_regs.h)
pub const fn READ_DATA_VALID(n: u32) -> u32 {
    ((1 << (n)))
}
