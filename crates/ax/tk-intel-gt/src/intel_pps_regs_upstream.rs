// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_pps_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `PPS_BASE` (intel_pps_regs.h)
pub const PPS_BASE: u32 = 0x61200;

/// `VLV_PPS_BASE` (intel_pps_regs.h)
pub const VLV_PPS_BASE: u32 = ((VLV_DISPLAY_BASE + PPS_BASE));

/// `PCH_PPS_BASE` (intel_pps_regs.h)
pub const PCH_PPS_BASE: u32 = 0xC7200;

/// `_PP_STATUS` (intel_pps_regs.h)
pub const _PP_STATUS: u32 = 0x61200;

/// `PP_ON` (intel_pps_regs.h)
pub const PP_ON: u32 = reg_bit(31);

/// `PP_READY` (intel_pps_regs.h)
pub const PP_READY: u32 = reg_bit(30);

/// `PP_SEQUENCE_MASK` (intel_pps_regs.h)
pub const PP_SEQUENCE_MASK: u32 = reg_genmask(29, 28);

/// `PP_SEQUENCE_NONE` (intel_pps_regs.h)
pub const PP_SEQUENCE_NONE: u32 = reg_field_prep(PP_SEQUENCE_MASK, 0);

/// `PP_SEQUENCE_POWER_UP` (intel_pps_regs.h)
pub const PP_SEQUENCE_POWER_UP: u32 = reg_field_prep(PP_SEQUENCE_MASK, 1);

/// `PP_SEQUENCE_POWER_DOWN` (intel_pps_regs.h)
pub const PP_SEQUENCE_POWER_DOWN: u32 = reg_field_prep(PP_SEQUENCE_MASK, 2);

/// `PP_CYCLE_DELAY_ACTIVE` (intel_pps_regs.h)
pub const PP_CYCLE_DELAY_ACTIVE: u32 = reg_bit(27);

/// `PP_SEQUENCE_STATE_MASK` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_MASK: u32 = reg_genmask(3, 0);

/// `PP_SEQUENCE_STATE_OFF_IDLE` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_OFF_IDLE: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x0);

/// `PP_SEQUENCE_STATE_OFF_S0_1` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_OFF_S0_1: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x1);

/// `PP_SEQUENCE_STATE_OFF_S0_2` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_OFF_S0_2: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x2);

/// `PP_SEQUENCE_STATE_OFF_S0_3` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_OFF_S0_3: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x3);

/// `PP_SEQUENCE_STATE_ON_IDLE` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_ON_IDLE: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x8);

/// `PP_SEQUENCE_STATE_ON_S1_1` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_ON_S1_1: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0x9);

/// `PP_SEQUENCE_STATE_ON_S1_2` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_ON_S1_2: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0xa);

/// `PP_SEQUENCE_STATE_ON_S1_3` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_ON_S1_3: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0xb);

/// `PP_SEQUENCE_STATE_RESET` (intel_pps_regs.h)
pub const PP_SEQUENCE_STATE_RESET: u32 = reg_field_prep(PP_SEQUENCE_STATE_MASK, 0xf);

/// `_PP_CONTROL` (intel_pps_regs.h)
pub const _PP_CONTROL: u32 = 0x61204;

/// `PANEL_UNLOCK_MASK` (intel_pps_regs.h)
pub const PANEL_UNLOCK_MASK: u32 = reg_genmask(31, 16);

/// `PANEL_UNLOCK_REGS` (intel_pps_regs.h)
pub const PANEL_UNLOCK_REGS: u32 = reg_field_prep(PANEL_UNLOCK_MASK, 0xabcd);

/// `BXT_POWER_CYCLE_DELAY_MASK` (intel_pps_regs.h)
pub const BXT_POWER_CYCLE_DELAY_MASK: u32 = reg_genmask(8, 4);

/// `EDP_FORCE_VDD` (intel_pps_regs.h)
pub const EDP_FORCE_VDD: u32 = reg_bit(3);

/// `EDP_BLC_ENABLE` (intel_pps_regs.h)
pub const EDP_BLC_ENABLE: u32 = reg_bit(2);

/// `PANEL_POWER_RESET` (intel_pps_regs.h)
pub const PANEL_POWER_RESET: u32 = reg_bit(1);

/// `PANEL_POWER_ON` (intel_pps_regs.h)
pub const PANEL_POWER_ON: u32 = reg_bit(0);

/// `_PP_ON_DELAYS` (intel_pps_regs.h)
pub const _PP_ON_DELAYS: u32 = 0x61208;

/// `PANEL_PORT_SELECT_MASK` (intel_pps_regs.h)
pub const PANEL_PORT_SELECT_MASK: u32 = reg_genmask(31, 30);

/// `PANEL_PORT_SELECT_LVDS` (intel_pps_regs.h)
pub const PANEL_PORT_SELECT_LVDS: u32 = reg_field_prep(PANEL_PORT_SELECT_MASK, 0);

/// `PANEL_PORT_SELECT_DPA` (intel_pps_regs.h)
pub const PANEL_PORT_SELECT_DPA: u32 = reg_field_prep(PANEL_PORT_SELECT_MASK, 1);

/// `PANEL_PORT_SELECT_DPC` (intel_pps_regs.h)
pub const PANEL_PORT_SELECT_DPC: u32 = reg_field_prep(PANEL_PORT_SELECT_MASK, 2);

/// `PANEL_PORT_SELECT_DPD` (intel_pps_regs.h)
pub const PANEL_PORT_SELECT_DPD: u32 = reg_field_prep(PANEL_PORT_SELECT_MASK, 3);

/// `PANEL_PORT_SELECT_VLV(port)` (intel_pps_regs.h)
pub const fn PANEL_PORT_SELECT_VLV(port: u32) -> u32 {
    reg_field_prep(PANEL_PORT_SELECT_MASK, port)
}

/// `PANEL_POWER_UP_DELAY_MASK` (intel_pps_regs.h)
pub const PANEL_POWER_UP_DELAY_MASK: u32 = reg_genmask(28, 16);

/// `PANEL_LIGHT_ON_DELAY_MASK` (intel_pps_regs.h)
pub const PANEL_LIGHT_ON_DELAY_MASK: u32 = reg_genmask(12, 0);

/// `_PP_OFF_DELAYS` (intel_pps_regs.h)
pub const _PP_OFF_DELAYS: u32 = 0x6120C;

/// `PANEL_POWER_DOWN_DELAY_MASK` (intel_pps_regs.h)
pub const PANEL_POWER_DOWN_DELAY_MASK: u32 = reg_genmask(28, 16);

/// `PANEL_LIGHT_OFF_DELAY_MASK` (intel_pps_regs.h)
pub const PANEL_LIGHT_OFF_DELAY_MASK: u32 = reg_genmask(12, 0);

/// `_PP_DIVISOR` (intel_pps_regs.h)
pub const _PP_DIVISOR: u32 = 0x61210;

/// `PP_REFERENCE_DIVIDER_MASK` (intel_pps_regs.h)
pub const PP_REFERENCE_DIVIDER_MASK: u32 = reg_genmask(31, 8);

/// `PANEL_POWER_CYCLE_DELAY_MASK` (intel_pps_regs.h)
pub const PANEL_POWER_CYCLE_DELAY_MASK: u32 = reg_genmask(4, 0);
