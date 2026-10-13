// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_cx0_phy_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `_DDI_CLK_VALFREQ_A` (intel_cx0_phy_regs.h)
pub const _DDI_CLK_VALFREQ_A: u32 = 0x64030;

/// `_DDI_CLK_VALFREQ_B` (intel_cx0_phy_regs.h)
pub const _DDI_CLK_VALFREQ_B: u32 = 0x64130;

/// `DDI_CLK_VALFREQ(port)` (intel_cx0_phy_regs.h)
pub const fn DDI_CLK_VALFREQ(port: u32) -> I915RegT {
    _MMIO_PORT(port, _DDI_CLK_VALFREQ_A, _DDI_CLK_VALFREQ_B)
}

/// `_XELPDP_PORT_M2P_MSGBUS_CTL_LN0_A` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_A: u32 = 0x64040;

/// `_XELPDP_PORT_M2P_MSGBUS_CTL_LN0_B` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_B: u32 = 0x64140;

/// `_XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC1` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC1: u32 = 0x16F240;

/// `_XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC2` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC2: u32 = 0x16F440;

/// `_XELPDP_PORT_M2P_MSGBUS_CTL(idx, lane)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_M2P_MSGBUS_CTL(idx: u32, lane: u32) -> I915RegT {
    mmio((pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_A, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_B, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC1, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC2) + ((lane) * 4)))
}

/// `XELPDP_PORT_M2P_TRANSACTION_PENDING` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_TRANSACTION_PENDING: u32 = reg_bit(31);

/// `XELPDP_PORT_M2P_COMMAND_TYPE_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_COMMAND_TYPE_MASK: u32 = reg_genmask(30, 27);

/// `XELPDP_PORT_M2P_COMMAND_WRITE_UNCOMMITTED` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_COMMAND_WRITE_UNCOMMITTED: u32 = reg_field_prep(XELPDP_PORT_M2P_COMMAND_TYPE_MASK, 0x1);

/// `XELPDP_PORT_M2P_COMMAND_WRITE_COMMITTED` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_COMMAND_WRITE_COMMITTED: u32 = reg_field_prep(XELPDP_PORT_M2P_COMMAND_TYPE_MASK, 0x2);

/// `XELPDP_PORT_M2P_COMMAND_READ` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_COMMAND_READ: u32 = reg_field_prep(XELPDP_PORT_M2P_COMMAND_TYPE_MASK, 0x3);

/// `XELPDP_PORT_P2P_TRANSACTION_PENDING` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2P_TRANSACTION_PENDING: u32 = reg_bit(24);

/// `XELPDP_PORT_M2P_DATA_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_DATA_MASK: u32 = reg_genmask(23, 16);

/// `XELPDP_PORT_M2P_DATA(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_PORT_M2P_DATA(val: u32) -> u32 {
    reg_field_prep(XELPDP_PORT_M2P_DATA_MASK, val)
}

/// `XELPDP_PORT_M2P_TRANSACTION_RESET` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_TRANSACTION_RESET: u32 = reg_bit(15);

/// `XELPDP_PORT_M2P_ADDRESS_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_M2P_ADDRESS_MASK: u32 = reg_genmask(11, 0);

/// `XELPDP_PORT_M2P_ADDRESS(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_PORT_M2P_ADDRESS(val: u32) -> u32 {
    reg_field_prep(XELPDP_PORT_M2P_ADDRESS_MASK, val)
}

/// `_XELPDP_PORT_P2M_MSGBUS_STATUS(idx, lane)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_P2M_MSGBUS_STATUS(idx: u32, lane: u32) -> I915RegT {
    mmio(((pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_A, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_B, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC1, _XELPDP_PORT_M2P_MSGBUS_CTL_LN0_USBC2) + ((lane) * 4)) + 8))
}

pub const fn __xe2lpd_port_idx(port: u32) -> u32 {
    if port >= (PORT_TC1 as u32) {
        port
    } else {
        (PORT_TC4 as u32) + 1 + port - (PORT_A as u32)
    }
}

/// `XELPDP_PORT_P2M_MSGBUS_STATUS(i915__, port, lane)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_P2M_MSGBUS_STATUS(display: *const IntelDisplay, port: u32, lane: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_P2M_MSGBUS_STATUS(__xe2lpd_port_idx(port), lane)
    } else {
        _XELPDP_PORT_P2M_MSGBUS_STATUS(port, lane)
    }
}

/// `XELPDP_PORT_P2M_RESPONSE_READY` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_RESPONSE_READY: u32 = reg_bit(31);

/// `XELPDP_PORT_P2M_COMMAND_TYPE_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_COMMAND_TYPE_MASK: u32 = reg_genmask(30, 27);

/// `XELPDP_PORT_P2M_COMMAND_READ_ACK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_COMMAND_READ_ACK: u32 = 0x4;

/// `XELPDP_PORT_P2M_COMMAND_WRITE_ACK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_COMMAND_WRITE_ACK: u32 = 0x5;

/// `XELPDP_PORT_P2M_DATA_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_DATA_MASK: u32 = reg_genmask(23, 16);

/// `XELPDP_PORT_P2M_DATA(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_PORT_P2M_DATA(val: u32) -> u32 {
    reg_field_prep(XELPDP_PORT_P2M_DATA_MASK, val)
}

/// `XELPDP_PORT_P2M_ERROR_SET` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_P2M_ERROR_SET: u32 = reg_bit(15);

/// `XELPDP_MSGBUS_TIMEOUT_MS` (intel_cx0_phy_regs.h)
pub const XELPDP_MSGBUS_TIMEOUT_MS: u32 = 1;

/// `XELPDP_PCLK_PLL_ENABLE_TIMEOUT_US` (intel_cx0_phy_regs.h)
pub const XELPDP_PCLK_PLL_ENABLE_TIMEOUT_US: u32 = 3200;

/// `XELPDP_PCLK_PLL_DISABLE_TIMEOUT_US` (intel_cx0_phy_regs.h)
pub const XELPDP_PCLK_PLL_DISABLE_TIMEOUT_US: u32 = 20;

/// `XELPDP_PORT_BUF_SOC_READY_TIMEOUT_US` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_SOC_READY_TIMEOUT_US: u32 = 100;

/// `XELPDP_PORT_RESET_START_TIMEOUT_US` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_RESET_START_TIMEOUT_US: u32 = 10;

/// `XELPDP_PORT_POWERDOWN_UPDATE_TIMEOUT_MS` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_POWERDOWN_UPDATE_TIMEOUT_MS: u32 = 2;

/// `XELPDP_PORT_RESET_END_TIMEOUT_MS` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_RESET_END_TIMEOUT_MS: u32 = 15;

/// `XELPDP_REFCLK_ENABLE_TIMEOUT_US` (intel_cx0_phy_regs.h)
pub const XELPDP_REFCLK_ENABLE_TIMEOUT_US: u32 = 10;

/// `_XELPDP_PORT_BUF_CTL1_LN0_A` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_BUF_CTL1_LN0_A: u32 = 0x64004;

/// `_XELPDP_PORT_BUF_CTL1_LN0_B` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_BUF_CTL1_LN0_B: u32 = 0x64104;

/// `_XELPDP_PORT_BUF_CTL1_LN0_USBC1` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_BUF_CTL1_LN0_USBC1: u32 = 0x16F200;

/// `_XELPDP_PORT_BUF_CTL1_LN0_USBC2` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_BUF_CTL1_LN0_USBC2: u32 = 0x16F400;

/// `_XELPDP_PORT_BUF_CTL1(idx)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_BUF_CTL1(idx: u32) -> I915RegT {
    mmio(pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_BUF_CTL1_LN0_A, _XELPDP_PORT_BUF_CTL1_LN0_B, _XELPDP_PORT_BUF_CTL1_LN0_USBC1, _XELPDP_PORT_BUF_CTL1_LN0_USBC2))
}


/// `XELPDP_PORT_BUF_CTL1(i915__, port)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_BUF_CTL1(display: *const IntelDisplay, port: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_BUF_CTL1(__xe2lpd_port_idx(port))
    } else {
        _XELPDP_PORT_BUF_CTL1(port)
    }
}

/// `XELPDP_PORT_BUF_D2D_LINK_ENABLE` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_D2D_LINK_ENABLE: u32 = reg_bit(29);

/// `XELPDP_PORT_BUF_D2D_LINK_STATE` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_D2D_LINK_STATE: u32 = reg_bit(28);

/// `XELPDP_PORT_BUF_SOC_PHY_READY` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_SOC_PHY_READY: u32 = reg_bit(24);

/// `XELPDP_PORT_BUF_PORT_DATA_WIDTH_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_PORT_DATA_WIDTH_MASK: u32 = reg_genmask(19, 18);

/// `XELPDP_PORT_BUF_PORT_DATA_10BIT` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_PORT_DATA_10BIT: u32 = reg_field_prep(XELPDP_PORT_BUF_PORT_DATA_WIDTH_MASK, 0);

/// `XELPDP_PORT_BUF_PORT_DATA_20BIT` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_PORT_DATA_20BIT: u32 = reg_field_prep(XELPDP_PORT_BUF_PORT_DATA_WIDTH_MASK, 1);

/// `XELPDP_PORT_BUF_PORT_DATA_40BIT` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_PORT_DATA_40BIT: u32 = reg_field_prep(XELPDP_PORT_BUF_PORT_DATA_WIDTH_MASK, 2);

/// `XELPDP_PORT_REVERSAL` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_REVERSAL: u32 = reg_bit(16);

/// `XE3PLPDP_PHY_MODE_MASK` (intel_cx0_phy_regs.h)
pub const XE3PLPDP_PHY_MODE_MASK: u32 = reg_genmask(15, 12);

/// `XE3PLPDP_PHY_MODE_DP` (intel_cx0_phy_regs.h)
pub const XE3PLPDP_PHY_MODE_DP: u32 = reg_field_prep(XE3PLPDP_PHY_MODE_MASK, 0x3);

/// `XELPDP_PORT_BUF_IO_SELECT_TBT` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_IO_SELECT_TBT: u32 = reg_bit(11);

/// `XELPDP_PORT_BUF_PHY_IDLE` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_BUF_PHY_IDLE: u32 = reg_bit(7);

/// `XELPDP_TC_PHY_OWNERSHIP` (intel_cx0_phy_regs.h)
pub const XELPDP_TC_PHY_OWNERSHIP: u32 = reg_bit(6);

/// `XELPDP_TCSS_POWER_REQUEST` (intel_cx0_phy_regs.h)
pub const XELPDP_TCSS_POWER_REQUEST: u32 = reg_bit(5);

/// `XELPDP_TCSS_POWER_STATE` (intel_cx0_phy_regs.h)
pub const XELPDP_TCSS_POWER_STATE: u32 = reg_bit(4);

/// `XELPDP_PORT_WIDTH_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_WIDTH_MASK: u32 = reg_genmask(3, 1);

/// `XELPDP_PORT_WIDTH(width)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_PORT_WIDTH(width: u32) -> u32 {
    reg_field_prep(XELPDP_PORT_WIDTH_MASK, if width == 3 { 4 } else { width - 1 })
}

/// `_XELPDP_PORT_BUF_CTL2(idx)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_BUF_CTL2(idx: u32) -> I915RegT {
    mmio((pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_BUF_CTL1_LN0_A, _XELPDP_PORT_BUF_CTL1_LN0_B, _XELPDP_PORT_BUF_CTL1_LN0_USBC1, _XELPDP_PORT_BUF_CTL1_LN0_USBC2) + 4))
}

/// `XELPDP_PORT_BUF_CTL2(i915__, port)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_BUF_CTL2(display: *const IntelDisplay, port: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_BUF_CTL2(__xe2lpd_port_idx(port))
    } else {
        _XELPDP_PORT_BUF_CTL2(port)
    }
}

/// `XELPDP_LANE_PIPE_RESET(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PIPE_RESET(lane: u32) -> u32 {
    pick(lane, &[reg_bit(31), reg_bit(30)])
}

/// `XELPDP_LANE_PHY_CURRENT_STATUS(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PHY_CURRENT_STATUS(lane: u32) -> u32 {
    pick(lane, &[reg_bit(29), reg_bit(28)])
}

/// `XE3PLPDP_LANE_PHY_PULSE_STATUS(lane)` (intel_cx0_phy_regs.h)
pub const fn XE3PLPDP_LANE_PHY_PULSE_STATUS(lane: u32) -> u32 {
    pick(lane, &[reg_bit(27), reg_bit(26)])
}

/// `XELPDP_LANE_POWERDOWN_UPDATE(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_POWERDOWN_UPDATE(lane: u32) -> u32 {
    pick(lane, &[reg_bit(25), reg_bit(24)])
}

/// `_XELPDP_LANE0_POWERDOWN_NEW_STATE_MASK` (intel_cx0_phy_regs.h)
pub const _XELPDP_LANE0_POWERDOWN_NEW_STATE_MASK: u32 = reg_genmask(23, 20);

/// `_XELPDP_LANE0_POWERDOWN_NEW_STATE(val)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_LANE0_POWERDOWN_NEW_STATE(val: u32) -> u32 {
    reg_field_prep(_XELPDP_LANE0_POWERDOWN_NEW_STATE_MASK, val)
}

/// `_XELPDP_LANE1_POWERDOWN_NEW_STATE_MASK` (intel_cx0_phy_regs.h)
pub const _XELPDP_LANE1_POWERDOWN_NEW_STATE_MASK: u32 = reg_genmask(19, 16);

/// `_XELPDP_LANE1_POWERDOWN_NEW_STATE(val)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_LANE1_POWERDOWN_NEW_STATE(val: u32) -> u32 {
    reg_field_prep(_XELPDP_LANE1_POWERDOWN_NEW_STATE_MASK, val)
}

/// `XELPDP_LANE_POWERDOWN_NEW_STATE(lane, val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_POWERDOWN_NEW_STATE(lane: u32, val: u32) -> u32 {
    pick(lane, &[_XELPDP_LANE0_POWERDOWN_NEW_STATE(val), _XELPDP_LANE1_POWERDOWN_NEW_STATE(val)])
}

/// `XELPDP_LANE_POWERDOWN_NEW_STATE_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_LANE_POWERDOWN_NEW_STATE_MASK: u32 = reg_genmask(3, 0);

/// `XELPDP_POWER_STATE_READY_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_POWER_STATE_READY_MASK: u32 = reg_genmask(7, 4);

/// `XELPDP_POWER_STATE_READY(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_POWER_STATE_READY(val: u32) -> u32 {
    reg_field_prep(XELPDP_POWER_STATE_READY_MASK, val)
}

/// `_XELPDP_PORT_BUF_CTL3(idx)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_BUF_CTL3(idx: u32) -> I915RegT {
    mmio((pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_BUF_CTL1_LN0_A, _XELPDP_PORT_BUF_CTL1_LN0_B, _XELPDP_PORT_BUF_CTL1_LN0_USBC1, _XELPDP_PORT_BUF_CTL1_LN0_USBC2) + 8))
}

/// `XELPDP_PORT_BUF_CTL3(i915__, port)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_BUF_CTL3(display: *const IntelDisplay, port: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_BUF_CTL3(__xe2lpd_port_idx(port))
    } else {
        _XELPDP_PORT_BUF_CTL3(port)
    }
}

/// `XELPDP_PLL_LANE_STAGGERING_DELAY_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PLL_LANE_STAGGERING_DELAY_MASK: u32 = reg_genmask(15, 8);

/// `XELPDP_PLL_LANE_STAGGERING_DELAY(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_PLL_LANE_STAGGERING_DELAY(val: u32) -> u32 {
    reg_field_prep(XELPDP_PLL_LANE_STAGGERING_DELAY_MASK, val)
}

/// `XELPDP_POWER_STATE_ACTIVE_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_POWER_STATE_ACTIVE_MASK: u32 = reg_genmask(3, 0);

/// `XELPDP_POWER_STATE_ACTIVE(val)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_POWER_STATE_ACTIVE(val: u32) -> u32 {
    reg_field_prep(XELPDP_POWER_STATE_ACTIVE_MASK, val)
}

/// `XELPDP_P0_STATE_ACTIVE` (intel_cx0_phy_regs.h)
pub const XELPDP_P0_STATE_ACTIVE: u32 = 0x0;

/// `XELPDP_P2_STATE_READY` (intel_cx0_phy_regs.h)
pub const XELPDP_P2_STATE_READY: u32 = 0x2;

/// `XE3PLPD_P4_STATE_DISABLE` (intel_cx0_phy_regs.h)
pub const XE3PLPD_P4_STATE_DISABLE: u32 = 0x4;

/// `XELPDP_P2PG_STATE_DISABLE` (intel_cx0_phy_regs.h)
pub const XELPDP_P2PG_STATE_DISABLE: u32 = 0x9;

/// `XELPDP_P4PG_STATE_DISABLE` (intel_cx0_phy_regs.h)
pub const XELPDP_P4PG_STATE_DISABLE: u32 = 0xC;

/// `XELPDP_P2_STATE_RESET` (intel_cx0_phy_regs.h)
pub const XELPDP_P2_STATE_RESET: u32 = 0x2;

/// `_XELPDP_PORT_MSGBUS_TIMER_LN0_A` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_MSGBUS_TIMER_LN0_A: u32 = 0x640d8;

/// `_XELPDP_PORT_MSGBUS_TIMER_LN0_B` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_MSGBUS_TIMER_LN0_B: u32 = 0x641d8;

/// `_XELPDP_PORT_MSGBUS_TIMER_LN0_USBC1` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_MSGBUS_TIMER_LN0_USBC1: u32 = 0x16f258;

/// `_XELPDP_PORT_MSGBUS_TIMER_LN0_USBC2` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_MSGBUS_TIMER_LN0_USBC2: u32 = 0x16f458;

/// `_XELPDP_PORT_MSGBUS_TIMER(port, lane)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_MSGBUS_TIMER(port: u32, lane: u32) -> I915RegT {
    mmio((pick_even_2ranges(port, (PORT_TC1 as u32), _XELPDP_PORT_MSGBUS_TIMER_LN0_A, _XELPDP_PORT_MSGBUS_TIMER_LN0_B, _XELPDP_PORT_MSGBUS_TIMER_LN0_USBC1, _XELPDP_PORT_MSGBUS_TIMER_LN0_USBC2) + ((lane) * 4)))
}

/// `XELPDP_PORT_MSGBUS_TIMER(i915__, port, lane)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_MSGBUS_TIMER(display: *const IntelDisplay, port: u32, lane: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_MSGBUS_TIMER(__xe2lpd_port_idx(port), lane)
    } else {
        _XELPDP_PORT_MSGBUS_TIMER(port, lane)
    }
}

/// `XELPDP_PORT_MSGBUS_TIMER_TIMED_OUT` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_MSGBUS_TIMER_TIMED_OUT: u32 = reg_bit(31);

/// `XELPDP_PORT_MSGBUS_TIMER_VAL_MASK` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_MSGBUS_TIMER_VAL_MASK: u32 = reg_genmask(23, 0);

/// `XELPDP_PORT_MSGBUS_TIMER_VAL` (intel_cx0_phy_regs.h)
pub const XELPDP_PORT_MSGBUS_TIMER_VAL: u32 = reg_field_prep(XELPDP_PORT_MSGBUS_TIMER_VAL_MASK, 0xa000);

/// `_XELPDP_PORT_CLOCK_CTL_A` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_CLOCK_CTL_A: u32 = 0x640E0;

/// `_XELPDP_PORT_CLOCK_CTL_B` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_CLOCK_CTL_B: u32 = 0x641E0;

/// `_XELPDP_PORT_CLOCK_CTL_USBC1` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_CLOCK_CTL_USBC1: u32 = 0x16F260;

/// `_XELPDP_PORT_CLOCK_CTL_USBC2` (intel_cx0_phy_regs.h)
pub const _XELPDP_PORT_CLOCK_CTL_USBC2: u32 = 0x16F460;

/// `_XELPDP_PORT_CLOCK_CTL(idx)` (intel_cx0_phy_regs.h)
pub const fn _XELPDP_PORT_CLOCK_CTL(idx: u32) -> I915RegT {
    mmio(pick_even_2ranges(idx, (PORT_TC1 as u32), _XELPDP_PORT_CLOCK_CTL_A, _XELPDP_PORT_CLOCK_CTL_B, _XELPDP_PORT_CLOCK_CTL_USBC1, _XELPDP_PORT_CLOCK_CTL_USBC2))
}

/// `XELPDP_PORT_CLOCK_CTL(i915__, port)` (intel_cx0_phy_regs.h)
pub unsafe fn XELPDP_PORT_CLOCK_CTL(display: *const IntelDisplay, port: u32) -> I915RegT {
    if unsafe { DISPLAY_VER(display) } >= 20 {
        _XELPDP_PORT_CLOCK_CTL(__xe2lpd_port_idx(port))
    } else {
        _XELPDP_PORT_CLOCK_CTL(port)
    }
}

/// `XELPDP_LANE_PCLK_PLL_REQUEST(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PCLK_PLL_REQUEST(lane: u32) -> u32 {
    reg_bit((31 - (((lane) * 4))))
}

/// `XELPDP_LANE_PCLK_PLL_ACK(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PCLK_PLL_ACK(lane: u32) -> u32 {
    reg_bit((30 - (((lane) * 4))))
}

/// `XELPDP_LANE_PCLK_REFCLK_REQUEST(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PCLK_REFCLK_REQUEST(lane: u32) -> u32 {
    reg_bit((29 - (((lane) * 4))))
}

/// `XELPDP_LANE_PCLK_REFCLK_ACK(lane)` (intel_cx0_phy_regs.h)
pub const fn XELPDP_LANE_PCLK_REFCLK_ACK(lane: u32) -> u32 {
    reg_bit((28 - (((lane) * 4))))
}

/// `XELPDP_TBT_CLOCK_REQUEST` (intel_cx0_phy_regs.h)
pub const XELPDP_TBT_CLOCK_REQUEST: u32 = reg_bit(19);

/// `XELPDP_TBT_CLOCK_ACK` (intel_cx0_phy_regs.h)
pub const XELPDP_TBT_CLOCK_ACK: u32 = reg_bit(18);

/// `_XELPDP_DDI_CLOCK_SELECT_MASK` (intel_cx0_phy_regs.h)
pub const _XELPDP_DDI_CLOCK_SELECT_MASK: u32 = reg_genmask(15, 12);

/// `_XE3_DDI_CLOCK_SELECT_MASK` (intel_cx0_phy_regs.h)
pub const _XE3_DDI_CLOCK_SELECT_MASK: u32 = reg_genmask(16, 12);

/// `XELPDP_DDI_CLOCK_SELECT_NONE` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_NONE: u32 = 0x0;

/// `XELPDP_DDI_CLOCK_SELECT_MAXPCLK` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_MAXPCLK: u32 = 0x8;

/// `XELPDP_DDI_CLOCK_SELECT_DIV18CLK` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_DIV18CLK: u32 = 0x9;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_162` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_162: u32 = 0xc;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_270` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_270: u32 = 0xd;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_540` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_540: u32 = 0xe;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_810` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_810: u32 = 0xf;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_312_5` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_312_5: u32 = 0x18;

/// `XELPDP_DDI_CLOCK_SELECT_TBT_625` (intel_cx0_phy_regs.h)
pub const XELPDP_DDI_CLOCK_SELECT_TBT_625: u32 = 0x19;

/// `XELPDP_FORWARD_CLOCK_UNGATE` (intel_cx0_phy_regs.h)
pub const XELPDP_FORWARD_CLOCK_UNGATE: u32 = reg_bit(10);

/// `XELPDP_LANE1_PHY_CLOCK_SELECT` (intel_cx0_phy_regs.h)
pub const XELPDP_LANE1_PHY_CLOCK_SELECT: u32 = reg_bit(8);

/// `XELPDP_SSC_ENABLE_PLLA` (intel_cx0_phy_regs.h)
pub const XELPDP_SSC_ENABLE_PLLA: u32 = reg_bit(1);

/// `XELPDP_SSC_ENABLE_PLLB` (intel_cx0_phy_regs.h)
pub const XELPDP_SSC_ENABLE_PLLB: u32 = reg_bit(0);

/// `TCSS_DISP_MAILBOX_IN_CMD` (intel_cx0_phy_regs.h)
pub const TCSS_DISP_MAILBOX_IN_CMD: I915RegT = mmio(0x161300);

/// `TCSS_DISP_MAILBOX_IN_CMD_RUN_BUSY` (intel_cx0_phy_regs.h)
pub const TCSS_DISP_MAILBOX_IN_CMD_RUN_BUSY: u32 = reg_bit(31);

/// `TCSS_DISP_MAILBOX_IN_CMD_CMD_MASK` (intel_cx0_phy_regs.h)
pub const TCSS_DISP_MAILBOX_IN_CMD_CMD_MASK: u32 = reg_genmask(7, 0);

/// `TCSS_DISP_MAILBOX_IN_CMD_DATA(val)` (intel_cx0_phy_regs.h)
pub const fn TCSS_DISP_MAILBOX_IN_CMD_DATA(val: u32) -> u32 {
    reg_field_prep(TCSS_DISP_MAILBOX_IN_CMD_CMD_MASK, val)
}

/// `TCSS_DISP_MAILBOX_IN_DATA` (intel_cx0_phy_regs.h)
pub const TCSS_DISP_MAILBOX_IN_DATA: I915RegT = mmio(0x161304);

/// `PHY_C10_VDR_PLL(idx)` (intel_cx0_phy_regs.h)
pub const fn PHY_C10_VDR_PLL(idx: u32) -> u32 {
    ((0xC00 + (idx)))
}

/// `C10_PLL0_SSC_EN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_SSC_EN: u32 = reg_bit(0);

/// `C10_PLL0_DIVCLK_EN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_DIVCLK_EN: u32 = reg_bit(1);

/// `C10_PLL0_DIV5CLK_EN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_DIV5CLK_EN: u32 = reg_bit(2);

/// `C10_PLL0_WORDDIV2_EN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_WORDDIV2_EN: u32 = reg_bit(3);

/// `C10_PLL0_FRACEN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_FRACEN: u32 = reg_bit(4);

/// `C10_PLL0_PMIX_EN` (intel_cx0_phy_regs.h)
pub const C10_PLL0_PMIX_EN: u32 = reg_bit(5);

/// `C10_PLL0_ANA_FREQ_VCO_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL0_ANA_FREQ_VCO_MASK: u32 = reg_genmask(7, 6);

/// `C10_PLL1_DIV_MULTIPLIER_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL1_DIV_MULTIPLIER_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL2_MULTIPLIERL_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL2_MULTIPLIERL_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL3_MULTIPLIERH_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL3_MULTIPLIERH_MASK: u32 = reg_genmask(3, 0);

/// `C10_PLL8_SSC_UP_SPREAD` (intel_cx0_phy_regs.h)
pub const C10_PLL8_SSC_UP_SPREAD: u32 = reg_bit(5);

/// `C10_PLL9_FRACN_DENL_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL9_FRACN_DENL_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL10_FRACN_DENH_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL10_FRACN_DENH_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL11_FRACN_QUOT_L_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL11_FRACN_QUOT_L_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL12_FRACN_QUOT_H_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL12_FRACN_QUOT_H_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL13_FRACN_REM_L_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL13_FRACN_REM_L_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL14_FRACN_REM_H_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL14_FRACN_REM_H_MASK: u32 = reg_genmask(7, 0);

/// `C10_PLL15_TXCLKDIV_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL15_TXCLKDIV_MASK: u32 = reg_genmask(2, 0);

/// `C10_PLL15_HDMIDIV_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL15_HDMIDIV_MASK: u32 = reg_genmask(5, 3);

/// `C10_PLL15_PIXELCLKDIV_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL15_PIXELCLKDIV_MASK: u32 = reg_genmask(7, 6);

/// `C10_PLL16_ANA_CPINT` (intel_cx0_phy_regs.h)
pub const C10_PLL16_ANA_CPINT: u32 = reg_genmask(6, 0);

/// `C10_PLL16_ANA_CPINTGS_L` (intel_cx0_phy_regs.h)
pub const C10_PLL16_ANA_CPINTGS_L: u32 = reg_bit(7);

/// `C10_PLL17_ANA_CPINTGS_H_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL17_ANA_CPINTGS_H_MASK: u32 = reg_genmask(5, 0);

/// `C10_PLL17_ANA_CPPROP_L_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL17_ANA_CPPROP_L_MASK: u32 = reg_genmask(7, 6);

/// `C10_PLL18_ANA_CPPROP_H_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL18_ANA_CPPROP_H_MASK: u32 = reg_genmask(4, 0);

/// `C10_PLL18_ANA_CPPROPGS_L_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL18_ANA_CPPROPGS_L_MASK: u32 = reg_genmask(7, 5);

/// `C10_PLL19_ANA_CPPROPGS_H_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL19_ANA_CPPROPGS_H_MASK: u32 = reg_genmask(3, 0);

/// `C10_PLL19_ANA_V2I_MASK` (intel_cx0_phy_regs.h)
pub const C10_PLL19_ANA_V2I_MASK: u32 = reg_genmask(5, 4);

/// `PHY_C10_VDR_CMN(idx)` (intel_cx0_phy_regs.h)
pub const fn PHY_C10_VDR_CMN(idx: u32) -> u32 {
    ((0xC20 + (idx)))
}

/// `C10_CMN0_REF_RANGE` (intel_cx0_phy_regs.h)
pub const C10_CMN0_REF_RANGE: u32 = reg_field_prep(reg_genmask(4, 0), 1);

/// `C10_CMN0_REF_CLK_MPLLB_DIV` (intel_cx0_phy_regs.h)
pub const C10_CMN0_REF_CLK_MPLLB_DIV: u32 = reg_field_prep(reg_genmask(7, 5), 1);

/// `C10_CMN3_TXVBOOST_MASK` (intel_cx0_phy_regs.h)
pub const C10_CMN3_TXVBOOST_MASK: u32 = reg_genmask(7, 5);

/// `C10_CMN3_TXVBOOST(val)` (intel_cx0_phy_regs.h)
pub const fn C10_CMN3_TXVBOOST(val: u32) -> u32 {
    reg_field_prep(C10_CMN3_TXVBOOST_MASK, val)
}

/// `PHY_C10_VDR_TX(idx)` (intel_cx0_phy_regs.h)
pub const fn PHY_C10_VDR_TX(idx: u32) -> u32 {
    ((0xC30 + (idx)))
}

/// `C10_TX0_TX_MPLLB_SEL` (intel_cx0_phy_regs.h)
pub const C10_TX0_TX_MPLLB_SEL: u32 = reg_bit(4);

/// `C10_TX1_TERMCTL_MASK` (intel_cx0_phy_regs.h)
pub const C10_TX1_TERMCTL_MASK: u32 = reg_genmask(7, 5);

/// `C10_TX1_TERMCTL(val)` (intel_cx0_phy_regs.h)
pub const fn C10_TX1_TERMCTL(val: u32) -> u32 {
    reg_field_prep(C10_TX1_TERMCTL_MASK, val)
}

/// `PHY_C10_VDR_CONTROL(idx)` (intel_cx0_phy_regs.h)
pub const fn PHY_C10_VDR_CONTROL(idx: u32) -> u32 {
    (((0xC70 + (idx)) - 1))
}

/// `C10_VDR_CTRL_MSGBUS_ACCESS` (intel_cx0_phy_regs.h)
pub const C10_VDR_CTRL_MSGBUS_ACCESS: u32 = reg_bit(2);

/// `C10_VDR_CTRL_MASTER_LANE` (intel_cx0_phy_regs.h)
pub const C10_VDR_CTRL_MASTER_LANE: u32 = reg_bit(1);

/// `C10_VDR_CTRL_UPDATE_CFG` (intel_cx0_phy_regs.h)
pub const C10_VDR_CTRL_UPDATE_CFG: u32 = reg_bit(0);

/// `PHY_C10_VDR_CUSTOM_WIDTH` (intel_cx0_phy_regs.h)
pub const PHY_C10_VDR_CUSTOM_WIDTH: u32 = 0xD02;

/// `C10_VDR_CUSTOM_WIDTH_MASK` (intel_cx0_phy_regs.h)
pub const C10_VDR_CUSTOM_WIDTH_MASK: u32 = reg_genmask(1, 0);

/// `C10_VDR_CUSTOM_WIDTH_8_10` (intel_cx0_phy_regs.h)
pub const C10_VDR_CUSTOM_WIDTH_8_10: u32 = reg_field_prep(C10_VDR_CUSTOM_WIDTH_MASK, 0);

/// `PHY_C10_VDR_OVRD` (intel_cx0_phy_regs.h)
pub const PHY_C10_VDR_OVRD: u32 = 0xD71;

/// `PHY_C10_VDR_OVRD_TX1` (intel_cx0_phy_regs.h)
pub const PHY_C10_VDR_OVRD_TX1: u32 = reg_bit(0);

/// `PHY_C10_VDR_OVRD_TX2` (intel_cx0_phy_regs.h)
pub const PHY_C10_VDR_OVRD_TX2: u32 = reg_bit(2);

/// `PHY_C10_VDR_PRE_OVRD_TX1` (intel_cx0_phy_regs.h)
pub const PHY_C10_VDR_PRE_OVRD_TX1: u32 = 0xD80;

/// `C10_PHY_OVRD_LEVEL_MASK` (intel_cx0_phy_regs.h)
pub const C10_PHY_OVRD_LEVEL_MASK: u32 = reg_genmask(5, 0);

/// `C10_PHY_OVRD_LEVEL(val)` (intel_cx0_phy_regs.h)
pub const fn C10_PHY_OVRD_LEVEL(val: u32) -> u32 {
    reg_field_prep(C10_PHY_OVRD_LEVEL_MASK, val)
}

/// `PHY_CX0_VDROVRD_CTL(lane, tx, control)` (intel_cx0_phy_regs.h)
pub const fn PHY_CX0_VDROVRD_CTL(lane: u32, tx: u32, control: u32) -> u32 {
    (((PHY_C10_VDR_PRE_OVRD_TX1 + ((((lane) ^ (tx))) * 0x10)) + (control)))
}

/// `PHY_CX0_TX_CONTROL(tx, control)` (intel_cx0_phy_regs.h)
pub const fn PHY_CX0_TX_CONTROL(tx: u32, control: u32) -> u32 {
    (((0x400 + ((((tx) - 1)) * 0x200)) + (control)))
}

/// `CONTROL2_DISABLE_SINGLE_TX` (intel_cx0_phy_regs.h)
pub const CONTROL2_DISABLE_SINGLE_TX: u32 = reg_bit(6);

/// `PHY_CMN1_CONTROL(tx, control)` (intel_cx0_phy_regs.h)
pub const fn PHY_CMN1_CONTROL(tx: u32, control: u32) -> u32 {
    (((0x800 + ((((tx) - 1)) * 0x200)) + (control)))
}

/// `CONTROL0_MAC_TRANSMIT_LFPS` (intel_cx0_phy_regs.h)
pub const CONTROL0_MAC_TRANSMIT_LFPS: u32 = reg_bit(1);

/// `PHY_C20_WR_ADDRESS_L` (intel_cx0_phy_regs.h)
pub const PHY_C20_WR_ADDRESS_L: u32 = 0xC02;

/// `PHY_C20_WR_DATA_L` (intel_cx0_phy_regs.h)
pub const PHY_C20_WR_DATA_L: u32 = 0xC04;

/// `PHY_C20_RD_ADDRESS_L` (intel_cx0_phy_regs.h)
pub const PHY_C20_RD_ADDRESS_L: u32 = 0xC06;

/// `PHY_C20_RD_DATA_L` (intel_cx0_phy_regs.h)
pub const PHY_C20_RD_DATA_L: u32 = 0xC08;

/// `PHY_C20_VDR_CUSTOM_SERDES_RATE` (intel_cx0_phy_regs.h)
pub const PHY_C20_VDR_CUSTOM_SERDES_RATE: u32 = 0xD00;

/// `PHY_C20_IS_HDMI_FRL` (intel_cx0_phy_regs.h)
pub const PHY_C20_IS_HDMI_FRL: u32 = reg_bit(7);

/// `PHY_C20_IS_DP` (intel_cx0_phy_regs.h)
pub const PHY_C20_IS_DP: u32 = reg_bit(6);

/// `PHY_C20_DP_RATE_MASK` (intel_cx0_phy_regs.h)
pub const PHY_C20_DP_RATE_MASK: u32 = reg_genmask(4, 1);

/// `PHY_C20_DP_RATE(val)` (intel_cx0_phy_regs.h)
pub const fn PHY_C20_DP_RATE(val: u32) -> u32 {
    reg_field_prep(PHY_C20_DP_RATE_MASK, val)
}

/// `PHY_C20_CONTEXT_TOGGLE` (intel_cx0_phy_regs.h)
pub const PHY_C20_CONTEXT_TOGGLE: u32 = reg_bit(0);

/// `PHY_C20_VDR_HDMI_RATE` (intel_cx0_phy_regs.h)
pub const PHY_C20_VDR_HDMI_RATE: u32 = 0xD01;

/// `PHY_C20_HDMI_RATE_MASK` (intel_cx0_phy_regs.h)
pub const PHY_C20_HDMI_RATE_MASK: u32 = reg_genmask(1, 0);

/// `PHY_C20_HDMI_RATE(val)` (intel_cx0_phy_regs.h)
pub const fn PHY_C20_HDMI_RATE(val: u32) -> u32 {
    reg_field_prep(PHY_C20_HDMI_RATE_MASK, val)
}

/// `PHY_C20_VDR_CUSTOM_WIDTH` (intel_cx0_phy_regs.h)
pub const PHY_C20_VDR_CUSTOM_WIDTH: u32 = 0xD02;

/// `PHY_C20_CUSTOM_WIDTH_MASK` (intel_cx0_phy_regs.h)
pub const PHY_C20_CUSTOM_WIDTH_MASK: u32 = reg_genmask(1, 0);

/// `PHY_C20_CUSTOM_WIDTH(val)` (intel_cx0_phy_regs.h)
pub const fn PHY_C20_CUSTOM_WIDTH(val: u32) -> u32 {
    reg_field_prep(PHY_C20_CUSTOM_WIDTH_MASK, val)
}

/// `_MTL_C20_A_TX_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_A_TX_CNTX_CFG: u32 = 0xCF2E;

/// `_MTL_C20_B_TX_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_B_TX_CNTX_CFG: u32 = 0xCF2A;

/// `_MTL_C20_A_CMN_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_A_CMN_CNTX_CFG: u32 = 0xCDAA;

/// `_MTL_C20_B_CMN_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_B_CMN_CNTX_CFG: u32 = 0xCDA5;

/// `_MTL_C20_A_MPLLA_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_A_MPLLA_CFG: u32 = 0xCCF0;

/// `_MTL_C20_B_MPLLA_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_B_MPLLA_CFG: u32 = 0xCCE5;

/// `_MTL_C20_A_MPLLB_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_A_MPLLB_CFG: u32 = 0xCB5A;

/// `_MTL_C20_B_MPLLB_CFG` (intel_cx0_phy_regs.h)
pub const _MTL_C20_B_MPLLB_CFG: u32 = 0xCB4E;

/// `_XE2HPD_C20_A_TX_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_A_TX_CNTX_CFG: u32 = 0xCF5E;

/// `_XE2HPD_C20_B_TX_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_B_TX_CNTX_CFG: u32 = 0xCF5A;

/// `_XE2HPD_C20_A_CMN_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_A_CMN_CNTX_CFG: u32 = 0xCE8E;

/// `_XE2HPD_C20_B_CMN_CNTX_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_B_CMN_CNTX_CFG: u32 = 0xCE89;

/// `_XE2HPD_C20_A_MPLLA_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_A_MPLLA_CFG: u32 = 0xCE58;

/// `_XE2HPD_C20_B_MPLLA_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_B_MPLLA_CFG: u32 = 0xCE4D;

/// `_XE2HPD_C20_A_MPLLB_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_A_MPLLB_CFG: u32 = 0xCCC2;

/// `_XE2HPD_C20_B_MPLLB_CFG` (intel_cx0_phy_regs.h)
pub const _XE2HPD_C20_B_MPLLB_CFG: u32 = 0xCCB6;

/// `C20_PHY_TX_RATE` (intel_cx0_phy_regs.h)
pub const C20_PHY_TX_RATE: u32 = reg_genmask(2, 0);

/// `C20_PHY_TX_MISC_MASK` (intel_cx0_phy_regs.h)
pub const C20_PHY_TX_MISC_MASK: u32 = reg_genmask(7, 0);

/// `C20_PHY_TX_MISC(val)` (intel_cx0_phy_regs.h)
pub const fn C20_PHY_TX_MISC(val: u32) -> u32 {
    reg_field_prep(C20_PHY_TX_MISC_MASK, (val))
}

/// `C20_PHY_TX_DCC_CAL_RANGE_MASK` (intel_cx0_phy_regs.h)
pub const C20_PHY_TX_DCC_CAL_RANGE_MASK: u32 = reg_genmask(11, 8);

/// `C20_PHY_TX_DCC_CAL_RANGE(val)` (intel_cx0_phy_regs.h)
pub const fn C20_PHY_TX_DCC_CAL_RANGE(val: u32) -> u32 {
    reg_field_prep(C20_PHY_TX_DCC_CAL_RANGE_MASK, (val))
}

/// `C20_PHY_TX_DCC_BYPASS` (intel_cx0_phy_regs.h)
pub const C20_PHY_TX_DCC_BYPASS: u32 = reg_bit(12);

/// `C20_PHY_TX_TERM_CTL_MASK` (intel_cx0_phy_regs.h)
pub const C20_PHY_TX_TERM_CTL_MASK: u32 = reg_genmask(15, 13);

/// `C20_PHY_TX_TERM_CTL(val)` (intel_cx0_phy_regs.h)
pub const fn C20_PHY_TX_TERM_CTL(val: u32) -> u32 {
    reg_field_prep(C20_PHY_TX_TERM_CTL_MASK, (val))
}

/// `C20_MPLLA_FRACEN` (intel_cx0_phy_regs.h)
pub const C20_MPLLA_FRACEN: u32 = reg_bit(14);

/// `C20_FB_CLK_DIV4_EN` (intel_cx0_phy_regs.h)
pub const C20_FB_CLK_DIV4_EN: u32 = reg_bit(13);

/// `C20_MPLLA_TX_CLK_DIV_MASK` (intel_cx0_phy_regs.h)
pub const C20_MPLLA_TX_CLK_DIV_MASK: u32 = reg_genmask(10, 8);

/// `C20_MPLLB_TX_CLK_DIV_MASK` (intel_cx0_phy_regs.h)
pub const C20_MPLLB_TX_CLK_DIV_MASK: u32 = reg_genmask(15, 13);

/// `C20_MPLLB_FRACEN` (intel_cx0_phy_regs.h)
pub const C20_MPLLB_FRACEN: u32 = reg_bit(13);

/// `C20_REF_CLK_MPLLB_DIV_MASK` (intel_cx0_phy_regs.h)
pub const C20_REF_CLK_MPLLB_DIV_MASK: u32 = reg_genmask(12, 10);

/// `C20_MULTIPLIER_MASK` (intel_cx0_phy_regs.h)
pub const C20_MULTIPLIER_MASK: u32 = reg_genmask(11, 0);

/// `C20_PHY_USE_MPLLB` (intel_cx0_phy_regs.h)
pub const C20_PHY_USE_MPLLB: u32 = reg_bit(7);

/// `C20_PHY_VSWING_PREEMPH_MASK` (intel_cx0_phy_regs.h)
pub const C20_PHY_VSWING_PREEMPH_MASK: u32 = reg_genmask(5, 0);

/// `C20_PHY_VSWING_PREEMPH(val)` (intel_cx0_phy_regs.h)
pub const fn C20_PHY_VSWING_PREEMPH(val: u32) -> u32 {
    reg_field_prep(C20_PHY_VSWING_PREEMPH_MASK, val)
}

/// `RAWLANEAONX_DIG_TX_MPLLB_CAL_DONE_BANK(idx)` (intel_cx0_phy_regs.h)
pub const fn RAWLANEAONX_DIG_TX_MPLLB_CAL_DONE_BANK(idx: u32) -> u32 {
    ((0x303D + (idx)))
}

/// `REFCLK_38_4_MHZ` (intel_cx0_phy_regs.h)
pub const REFCLK_38_4_MHZ: u32 = 38400000;

/// `DATARATE_3000000000` (intel_cx0_phy_regs.h)
pub const DATARATE_3000000000: u32 = 3000000000;

/// `DATARATE_3500000000` (intel_cx0_phy_regs.h)
pub const DATARATE_3500000000: u32 = 3500000000;

/// `DATARATE_4000000000` (intel_cx0_phy_regs.h)
pub const DATARATE_4000000000: u32 = 4000000000;

/// `MPLL_FRACN_DEN` (intel_cx0_phy_regs.h)
pub const MPLL_FRACN_DEN: u32 = 0xFFFF;

/// `SSC_UP_SPREAD` (intel_cx0_phy_regs.h)
pub const SSC_UP_SPREAD: u32 = reg_bit(9);

/// `WORD_CLK_DIV` (intel_cx0_phy_regs.h)
pub const WORD_CLK_DIV: u32 = reg_bit(8);

/// `MPLL_TX_CLK_DIV(val)` (intel_cx0_phy_regs.h)
pub const fn MPLL_TX_CLK_DIV(val: u32) -> u32 {
    reg_field_prep(C20_MPLLB_TX_CLK_DIV_MASK, val)
}

/// `MPLL_MULTIPLIER(val)` (intel_cx0_phy_regs.h)
pub const fn MPLL_MULTIPLIER(val: u32) -> u32 {
    reg_field_prep(C20_MULTIPLIER_MASK, val)
}

/// `MPLLB_ANA_FREQ_VCO_0` (intel_cx0_phy_regs.h)
pub const MPLLB_ANA_FREQ_VCO_0: u32 = 0;

/// `MPLLB_ANA_FREQ_VCO_1` (intel_cx0_phy_regs.h)
pub const MPLLB_ANA_FREQ_VCO_1: u32 = 1;

/// `MPLLB_ANA_FREQ_VCO_2` (intel_cx0_phy_regs.h)
pub const MPLLB_ANA_FREQ_VCO_2: u32 = 2;

/// `MPLLB_ANA_FREQ_VCO_3` (intel_cx0_phy_regs.h)
pub const MPLLB_ANA_FREQ_VCO_3: u32 = 3;

/// `MPLLB_ANA_FREQ_VCO_MASK` (intel_cx0_phy_regs.h)
pub const MPLLB_ANA_FREQ_VCO_MASK: u32 = reg_genmask(15, 14);

/// `MPLLB_ANA_FREQ_VCO(val)` (intel_cx0_phy_regs.h)
pub const fn MPLLB_ANA_FREQ_VCO(val: u32) -> u32 {
    reg_field_prep(MPLLB_ANA_FREQ_VCO_MASK, val)
}

/// `MPLL_DIV_MULTIPLIER_MASK` (intel_cx0_phy_regs.h)
pub const MPLL_DIV_MULTIPLIER_MASK: u32 = reg_genmask(7, 0);

/// `MPLL_DIV_MULTIPLIER(val)` (intel_cx0_phy_regs.h)
pub const fn MPLL_DIV_MULTIPLIER(val: u32) -> u32 {
    reg_field_prep(MPLL_DIV_MULTIPLIER_MASK, val)
}

/// `CAL_DAC_CODE_31` (intel_cx0_phy_regs.h)
pub const CAL_DAC_CODE_31: u32 = 31;

/// `CAL_DAC_CODE_MASK` (intel_cx0_phy_regs.h)
pub const CAL_DAC_CODE_MASK: u32 = reg_genmask(14, 10);

/// `CAL_DAC_CODE(val)` (intel_cx0_phy_regs.h)
pub const fn CAL_DAC_CODE(val: u32) -> u32 {
    reg_field_prep(CAL_DAC_CODE_MASK, val)
}

/// `CP_INT_GS_28` (intel_cx0_phy_regs.h)
pub const CP_INT_GS_28: u32 = 28;

/// `CP_INT_GS_MASK` (intel_cx0_phy_regs.h)
pub const CP_INT_GS_MASK: u32 = reg_genmask(6, 0);

/// `CP_INT_GS(val)` (intel_cx0_phy_regs.h)
pub const fn CP_INT_GS(val: u32) -> u32 {
    reg_field_prep(CP_INT_GS_MASK, val)
}

/// `CP_PROP_GS_30` (intel_cx0_phy_regs.h)
pub const CP_PROP_GS_30: u32 = 30;

/// `CP_PROP_GS_MASK` (intel_cx0_phy_regs.h)
pub const CP_PROP_GS_MASK: u32 = reg_genmask(13, 7);

/// `CP_PROP_GS(val)` (intel_cx0_phy_regs.h)
pub const fn CP_PROP_GS(val: u32) -> u32 {
    reg_field_prep(CP_PROP_GS_MASK, val)
}

/// `CP_INT_6` (intel_cx0_phy_regs.h)
pub const CP_INT_6: u32 = 6;

/// `CP_INT_MASK` (intel_cx0_phy_regs.h)
pub const CP_INT_MASK: u32 = reg_genmask(6, 0);

/// `CP_INT(val)` (intel_cx0_phy_regs.h)
pub const fn CP_INT(val: u32) -> u32 {
    reg_field_prep(CP_INT_MASK, val)
}

/// `CP_PROP_20` (intel_cx0_phy_regs.h)
pub const CP_PROP_20: u32 = 20;

/// `CP_PROP_MASK` (intel_cx0_phy_regs.h)
pub const CP_PROP_MASK: u32 = reg_genmask(13, 7);

/// `CP_PROP(val)` (intel_cx0_phy_regs.h)
pub const fn CP_PROP(val: u32) -> u32 {
    reg_field_prep(CP_PROP_MASK, val)
}

/// `V2I_2` (intel_cx0_phy_regs.h)
pub const V2I_2: u32 = 2;

/// `V2I_MASK` (intel_cx0_phy_regs.h)
pub const V2I_MASK: u32 = reg_genmask(15, 14);

/// `V2I(val)` (intel_cx0_phy_regs.h)
pub const fn V2I(val: u32) -> u32 {
    reg_field_prep(V2I_MASK, val)
}

/// `HDMI_DIV_1` (intel_cx0_phy_regs.h)
pub const HDMI_DIV_1: u32 = 1;

/// `HDMI_DIV_MASK` (intel_cx0_phy_regs.h)
pub const HDMI_DIV_MASK: u32 = reg_genmask(2, 0);

/// `HDMI_DIV(val)` (intel_cx0_phy_regs.h)
pub const fn HDMI_DIV(val: u32) -> u32 {
    reg_field_prep(HDMI_DIV_MASK, val)
}

/// `PICA_PHY_CONFIG_CONTROL` (intel_cx0_phy_regs.h)
pub const PICA_PHY_CONFIG_CONTROL: I915RegT = mmio(0x16FE68);

/// `EDP_ON_TYPEC` (intel_cx0_phy_regs.h)
pub const EDP_ON_TYPEC: u32 = reg_bit(31);
