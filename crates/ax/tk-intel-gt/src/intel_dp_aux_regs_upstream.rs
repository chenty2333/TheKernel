// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_dp_aux_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_display_limits_types_upstream::*;

/// `_DPA_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _DPA_AUX_CH_CTL: u32 = 0x64010;

/// `_DPB_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _DPB_AUX_CH_CTL: u32 = 0x64110;

/// `DP_AUX_CH_CTL(aux_ch)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL(aux_ch: u32) -> I915RegT {
    _MMIO_PORT(aux_ch, _DPA_AUX_CH_CTL, _DPB_AUX_CH_CTL)
}

/// `VLV_DP_AUX_CH_CTL(aux_ch)` (intel_dp_aux_regs.h)
pub const fn VLV_DP_AUX_CH_CTL(aux_ch: u32) -> I915RegT {
    mmio((VLV_DISPLAY_BASE + _PORT(aux_ch, _DPA_AUX_CH_CTL, _DPB_AUX_CH_CTL)))
}

/// `_PCH_DPB_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _PCH_DPB_AUX_CH_CTL: u32 = 0xe4110;

/// `_PCH_DPC_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _PCH_DPC_AUX_CH_CTL: u32 = 0xe4210;

/// `PCH_DP_AUX_CH_CTL(aux_ch)` (intel_dp_aux_regs.h)
pub const fn PCH_DP_AUX_CH_CTL(aux_ch: u32) -> I915RegT {
    _MMIO_PORT(((aux_ch) - (AUX_CH_B as u32)), _PCH_DPB_AUX_CH_CTL, _PCH_DPC_AUX_CH_CTL)
}

/// `_XELPDP_USBC1_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _XELPDP_USBC1_AUX_CH_CTL: u32 = 0x16f210;

/// `_XELPDP_USBC2_AUX_CH_CTL` (intel_dp_aux_regs.h)
pub const _XELPDP_USBC2_AUX_CH_CTL: u32 = 0x16f410;

/// `_XELPDP_DP_AUX_CH_CTL(aux_ch)` (intel_dp_aux_regs.h)
pub const fn _XELPDP_DP_AUX_CH_CTL(aux_ch: u32) -> I915RegT {
    mmio(pick_even_2ranges(aux_ch, (AUX_CH_USBC1 as u32), _DPA_AUX_CH_CTL, _DPB_AUX_CH_CTL, _XELPDP_USBC1_AUX_CH_CTL, _XELPDP_USBC2_AUX_CH_CTL))
}

/// `DP_AUX_CH_CTL_SEND_BUSY` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_SEND_BUSY: u32 = reg_bit(31);

/// `DP_AUX_CH_CTL_DONE` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_DONE: u32 = reg_bit(30);

/// `DP_AUX_CH_CTL_INTERRUPT` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_INTERRUPT: u32 = reg_bit(29);

/// `DP_AUX_CH_CTL_TIME_OUT_ERROR` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_ERROR: u32 = reg_bit(28);

/// `DP_AUX_CH_CTL_TIME_OUT_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_MASK: u32 = reg_genmask(27, 26);

/// `DP_AUX_CH_CTL_TIME_OUT_400us` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_400us: u32 = reg_field_prep(DP_AUX_CH_CTL_TIME_OUT_MASK, 0);

/// `DP_AUX_CH_CTL_TIME_OUT_600us` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_600us: u32 = reg_field_prep(DP_AUX_CH_CTL_TIME_OUT_MASK, 1);

/// `DP_AUX_CH_CTL_TIME_OUT_800us` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_800us: u32 = reg_field_prep(DP_AUX_CH_CTL_TIME_OUT_MASK, 2);

/// `DP_AUX_CH_CTL_TIME_OUT_MAX` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TIME_OUT_MAX: u32 = reg_field_prep(DP_AUX_CH_CTL_TIME_OUT_MASK, 3);

/// `DP_AUX_CH_CTL_RECEIVE_ERROR` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_RECEIVE_ERROR: u32 = reg_bit(25);

/// `DP_AUX_CH_CTL_MESSAGE_SIZE_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_MESSAGE_SIZE_MASK: u32 = reg_genmask(24, 20);

/// `DP_AUX_CH_CTL_MESSAGE_SIZE(x)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL_MESSAGE_SIZE(x: u32) -> u32 {
    reg_field_prep(DP_AUX_CH_CTL_MESSAGE_SIZE_MASK, (x))
}

/// `DP_AUX_CH_CTL_PRECHARGE_2US_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_PRECHARGE_2US_MASK: u32 = reg_genmask(19, 16);

/// `DP_AUX_CH_CTL_PRECHARGE_2US(x)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL_PRECHARGE_2US(x: u32) -> u32 {
    reg_field_prep(DP_AUX_CH_CTL_PRECHARGE_2US_MASK, (x))
}

/// `XELPDP_DP_AUX_CH_CTL_POWER_REQUEST` (intel_dp_aux_regs.h)
pub const XELPDP_DP_AUX_CH_CTL_POWER_REQUEST: u32 = reg_bit(19);

/// `XELPDP_DP_AUX_CH_CTL_POWER_STATUS` (intel_dp_aux_regs.h)
pub const XELPDP_DP_AUX_CH_CTL_POWER_STATUS: u32 = reg_bit(18);

/// `DP_AUX_CH_CTL_AUX_AKSV_SELECT` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_AUX_AKSV_SELECT: u32 = reg_bit(15);

/// `DP_AUX_CH_CTL_MANCHESTER_TEST` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_MANCHESTER_TEST: u32 = reg_bit(14);

/// `DP_AUX_CH_CTL_PSR_DATA_AUX_REG_SKL` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_PSR_DATA_AUX_REG_SKL: u32 = reg_bit(14);

/// `DP_AUX_CH_CTL_SYNC_TEST` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_SYNC_TEST: u32 = reg_bit(13);

/// `DP_AUX_CH_CTL_FS_DATA_AUX_REG_SKL` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_FS_DATA_AUX_REG_SKL: u32 = reg_bit(13);

/// `DP_AUX_CH_CTL_DEGLITCH_TEST` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_DEGLITCH_TEST: u32 = reg_bit(12);

/// `DP_AUX_CH_CTL_GTC_DATA_AUX_REG_SKL` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_GTC_DATA_AUX_REG_SKL: u32 = reg_bit(12);

/// `DP_AUX_CH_CTL_PRECHARGE_TEST` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_PRECHARGE_TEST: u32 = reg_bit(11);

/// `DP_AUX_CH_CTL_TBT_IO` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_TBT_IO: u32 = reg_bit(11);

/// `DP_AUX_CH_CTL_BIT_CLOCK_2X_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_BIT_CLOCK_2X_MASK: u32 = reg_genmask(10, 0);

/// `DP_AUX_CH_CTL_BIT_CLOCK_2X(x)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL_BIT_CLOCK_2X(x: u32) -> u32 {
    reg_field_prep(DP_AUX_CH_CTL_BIT_CLOCK_2X_MASK, (x))
}

/// `DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL_MASK: u32 = reg_genmask(9, 5);

/// `DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL(c)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL(c: u32) -> u32 {
    reg_field_prep(DP_AUX_CH_CTL_FW_SYNC_PULSE_SKL_MASK, ((c) - 1))
}

/// `DP_AUX_CH_CTL_SYNC_PULSE_SKL_MASK` (intel_dp_aux_regs.h)
pub const DP_AUX_CH_CTL_SYNC_PULSE_SKL_MASK: u32 = reg_genmask(4, 0);

/// `DP_AUX_CH_CTL_SYNC_PULSE_SKL(c)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_CTL_SYNC_PULSE_SKL(c: u32) -> u32 {
    reg_field_prep(DP_AUX_CH_CTL_SYNC_PULSE_SKL_MASK, ((c) - 1))
}

/// `_DPA_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _DPA_AUX_CH_DATA1: u32 = 0x64014;

/// `_DPB_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _DPB_AUX_CH_DATA1: u32 = 0x64114;

/// `DP_AUX_CH_DATA(aux_ch, i)` (intel_dp_aux_regs.h)
pub const fn DP_AUX_CH_DATA(aux_ch: u32, i: u32) -> I915RegT {
    mmio((_PORT(aux_ch, _DPA_AUX_CH_DATA1, _DPB_AUX_CH_DATA1) + ((i) * 4)))
}

/// `VLV_DP_AUX_CH_DATA(aux_ch, i)` (intel_dp_aux_regs.h)
pub const fn VLV_DP_AUX_CH_DATA(aux_ch: u32, i: u32) -> I915RegT {
    mmio(((VLV_DISPLAY_BASE + _PORT(aux_ch, _DPA_AUX_CH_DATA1, _DPB_AUX_CH_DATA1)) + ((i) * 4)))
}

/// `_PCH_DPB_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _PCH_DPB_AUX_CH_DATA1: u32 = 0xe4114;

/// `_PCH_DPC_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _PCH_DPC_AUX_CH_DATA1: u32 = 0xe4214;

/// `PCH_DP_AUX_CH_DATA(aux_ch, i)` (intel_dp_aux_regs.h)
pub const fn PCH_DP_AUX_CH_DATA(aux_ch: u32, i: u32) -> I915RegT {
    mmio((_PORT(((aux_ch) - (AUX_CH_B as u32)), _PCH_DPB_AUX_CH_DATA1, _PCH_DPC_AUX_CH_DATA1) + ((i) * 4)))
}

/// `_XELPDP_USBC1_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _XELPDP_USBC1_AUX_CH_DATA1: u32 = 0x16f214;

/// `_XELPDP_USBC2_AUX_CH_DATA1` (intel_dp_aux_regs.h)
pub const _XELPDP_USBC2_AUX_CH_DATA1: u32 = 0x16f414;

/// `_XELPDP_DP_AUX_CH_DATA(aux_ch, i)` (intel_dp_aux_regs.h)
pub const fn _XELPDP_DP_AUX_CH_DATA(aux_ch: u32, i: u32) -> I915RegT {
    mmio((pick_even_2ranges(aux_ch, (AUX_CH_USBC1 as u32), _DPA_AUX_CH_DATA1, _DPB_AUX_CH_DATA1, _XELPDP_USBC1_AUX_CH_DATA1, _XELPDP_USBC2_AUX_CH_DATA1) + ((i) * 4)))
}

/// `XE2LPD_PICA_PW_CTL` (intel_dp_aux_regs.h)
pub const XE2LPD_PICA_PW_CTL: I915RegT = mmio(0x16fe04);

/// `XE2LPD_PICA_CTL_POWER_REQUEST` (intel_dp_aux_regs.h)
pub const XE2LPD_PICA_CTL_POWER_REQUEST: u32 = reg_bit(31);

/// `XE2LPD_PICA_CTL_POWER_STATUS` (intel_dp_aux_regs.h)
pub const XE2LPD_PICA_CTL_POWER_STATUS: u32 = reg_bit(30);
