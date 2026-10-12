// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_psr_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;
use crate::intel_dp_aux_regs_upstream::*;

/// `_TRANS_EXITLINE_A` (intel_psr_regs.h)
pub const _TRANS_EXITLINE_A: u32 = 0x60018;

/// `EXITLINE_ENABLE` (intel_psr_regs.h)
pub const EXITLINE_ENABLE: u32 = reg_bit(31);

/// `EXITLINE_MASK` (intel_psr_regs.h)
pub const EXITLINE_MASK: u32 = reg_genmask(12, 0);

/// `EXITLINE_SHIFT` (intel_psr_regs.h)
pub const EXITLINE_SHIFT: u32 = 0;

/// `HSW_SRD_CTL` (intel_psr_regs.h)
pub const HSW_SRD_CTL: I915RegT = mmio(0x64800);

/// `_SRD_CTL_A` (intel_psr_regs.h)
pub const _SRD_CTL_A: u32 = 0x60800;

/// `_SRD_CTL_EDP` (intel_psr_regs.h)
pub const _SRD_CTL_EDP: u32 = 0x6f800;

/// `EDP_PSR_ENABLE` (intel_psr_regs.h)
pub const EDP_PSR_ENABLE: u32 = reg_bit(31);

/// `BDW_PSR_SINGLE_FRAME` (intel_psr_regs.h)
pub const BDW_PSR_SINGLE_FRAME: u32 = reg_bit(30);

/// `EDP_PSR_RESTORE_PSR_ACTIVE_CTX_MASK` (intel_psr_regs.h)
pub const EDP_PSR_RESTORE_PSR_ACTIVE_CTX_MASK: u32 = reg_bit(29);

/// `EDP_PSR_LINK_STANDBY` (intel_psr_regs.h)
pub const EDP_PSR_LINK_STANDBY: u32 = reg_bit(27);

/// `EDP_PSR_MIN_LINK_ENTRY_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR_MIN_LINK_ENTRY_TIME_MASK: u32 = reg_genmask(26, 25);

/// `EDP_PSR_MIN_LINK_ENTRY_TIME_8_LINES` (intel_psr_regs.h)
pub const EDP_PSR_MIN_LINK_ENTRY_TIME_8_LINES: u32 = reg_field_prep(EDP_PSR_MIN_LINK_ENTRY_TIME_MASK, 0);

/// `EDP_PSR_MIN_LINK_ENTRY_TIME_4_LINES` (intel_psr_regs.h)
pub const EDP_PSR_MIN_LINK_ENTRY_TIME_4_LINES: u32 = reg_field_prep(EDP_PSR_MIN_LINK_ENTRY_TIME_MASK, 1);

/// `EDP_PSR_MIN_LINK_ENTRY_TIME_2_LINES` (intel_psr_regs.h)
pub const EDP_PSR_MIN_LINK_ENTRY_TIME_2_LINES: u32 = reg_field_prep(EDP_PSR_MIN_LINK_ENTRY_TIME_MASK, 2);

/// `EDP_PSR_MIN_LINK_ENTRY_TIME_0_LINES` (intel_psr_regs.h)
pub const EDP_PSR_MIN_LINK_ENTRY_TIME_0_LINES: u32 = reg_field_prep(EDP_PSR_MIN_LINK_ENTRY_TIME_MASK, 3);

/// `EDP_PSR_MAX_SLEEP_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR_MAX_SLEEP_TIME_MASK: u32 = reg_genmask(24, 20);

/// `EDP_PSR_MAX_SLEEP_TIME(x)` (intel_psr_regs.h)
pub const fn EDP_PSR_MAX_SLEEP_TIME(x: u32) -> u32 {
    reg_field_prep(EDP_PSR_MAX_SLEEP_TIME_MASK, (x))
}

/// `LNL_EDP_PSR_ENTRY_SETUP_FRAMES_MASK` (intel_psr_regs.h)
pub const LNL_EDP_PSR_ENTRY_SETUP_FRAMES_MASK: u32 = reg_genmask(17, 16);

/// `LNL_EDP_PSR_ENTRY_SETUP_FRAMES(x)` (intel_psr_regs.h)
pub const fn LNL_EDP_PSR_ENTRY_SETUP_FRAMES(x: u32) -> u32 {
    reg_field_prep(LNL_EDP_PSR_ENTRY_SETUP_FRAMES_MASK, (x))
}

/// `EDP_PSR_SKIP_AUX_EXIT` (intel_psr_regs.h)
pub const EDP_PSR_SKIP_AUX_EXIT: u32 = reg_bit(12);

/// `EDP_PSR_TP_MASK` (intel_psr_regs.h)
pub const EDP_PSR_TP_MASK: u32 = reg_bit(11);

/// `EDP_PSR_TP_TP1_TP2` (intel_psr_regs.h)
pub const EDP_PSR_TP_TP1_TP2: u32 = reg_field_prep(EDP_PSR_TP_MASK, 0);

/// `EDP_PSR_TP_TP1_TP3` (intel_psr_regs.h)
pub const EDP_PSR_TP_TP1_TP3: u32 = reg_field_prep(EDP_PSR_TP_MASK, 1);

/// `EDP_PSR_CRC_ENABLE` (intel_psr_regs.h)
pub const EDP_PSR_CRC_ENABLE: u32 = reg_bit(10);

/// `EDP_PSR_TP2_TP3_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR_TP2_TP3_TIME_MASK: u32 = reg_genmask(9, 8);

/// `EDP_PSR_TP2_TP3_TIME_500us` (intel_psr_regs.h)
pub const EDP_PSR_TP2_TP3_TIME_500us: u32 = reg_field_prep(EDP_PSR_TP2_TP3_TIME_MASK, 0);

/// `EDP_PSR_TP2_TP3_TIME_100us` (intel_psr_regs.h)
pub const EDP_PSR_TP2_TP3_TIME_100us: u32 = reg_field_prep(EDP_PSR_TP2_TP3_TIME_MASK, 1);

/// `EDP_PSR_TP2_TP3_TIME_2500us` (intel_psr_regs.h)
pub const EDP_PSR_TP2_TP3_TIME_2500us: u32 = reg_field_prep(EDP_PSR_TP2_TP3_TIME_MASK, 2);

/// `EDP_PSR_TP2_TP3_TIME_0us` (intel_psr_regs.h)
pub const EDP_PSR_TP2_TP3_TIME_0us: u32 = reg_field_prep(EDP_PSR_TP2_TP3_TIME_MASK, 3);

/// `EDP_PSR_TP4_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR_TP4_TIME_MASK: u32 = reg_genmask(7, 6);

/// `EDP_PSR_TP4_TIME_0us` (intel_psr_regs.h)
pub const EDP_PSR_TP4_TIME_0us: u32 = reg_field_prep(EDP_PSR_TP4_TIME_MASK, 3);

/// `EDP_PSR_TP1_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR_TP1_TIME_MASK: u32 = reg_genmask(5, 4);

/// `EDP_PSR_TP1_TIME_500us` (intel_psr_regs.h)
pub const EDP_PSR_TP1_TIME_500us: u32 = reg_field_prep(EDP_PSR_TP1_TIME_MASK, 0);

/// `EDP_PSR_TP1_TIME_100us` (intel_psr_regs.h)
pub const EDP_PSR_TP1_TIME_100us: u32 = reg_field_prep(EDP_PSR_TP1_TIME_MASK, 1);

/// `EDP_PSR_TP1_TIME_2500us` (intel_psr_regs.h)
pub const EDP_PSR_TP1_TIME_2500us: u32 = reg_field_prep(EDP_PSR_TP1_TIME_MASK, 2);

/// `EDP_PSR_TP1_TIME_0us` (intel_psr_regs.h)
pub const EDP_PSR_TP1_TIME_0us: u32 = reg_field_prep(EDP_PSR_TP1_TIME_MASK, 3);

/// `EDP_PSR_IDLE_FRAMES_MASK` (intel_psr_regs.h)
pub const EDP_PSR_IDLE_FRAMES_MASK: u32 = reg_genmask(3, 0);

/// `EDP_PSR_IDLE_FRAMES(x)` (intel_psr_regs.h)
pub const fn EDP_PSR_IDLE_FRAMES(x: u32) -> u32 {
    reg_field_prep(EDP_PSR_IDLE_FRAMES_MASK, (x))
}

/// `EDP_PSR_IMR` (intel_psr_regs.h)
pub const EDP_PSR_IMR: I915RegT = mmio(0x64834);

/// `EDP_PSR_IIR` (intel_psr_regs.h)
pub const EDP_PSR_IIR: I915RegT = mmio(0x64838);

/// `_PSR_IMR_A` (intel_psr_regs.h)
pub const _PSR_IMR_A: u32 = 0x60814;

/// `_PSR_IIR_A` (intel_psr_regs.h)
pub const _PSR_IIR_A: u32 = 0x60818;

/// `TGL_PSR_MASK` (intel_psr_regs.h)
pub const TGL_PSR_MASK: u32 = reg_genmask(2, 0);

/// `TGL_PSR_ERROR` (intel_psr_regs.h)
pub const TGL_PSR_ERROR: u32 = reg_bit(2);

/// `TGL_PSR_POST_EXIT` (intel_psr_regs.h)
pub const TGL_PSR_POST_EXIT: u32 = reg_bit(1);

/// `TGL_PSR_PRE_ENTRY` (intel_psr_regs.h)
pub const TGL_PSR_PRE_ENTRY: u32 = reg_bit(0);

/// `HSW_SRD_AUX_CTL` (intel_psr_regs.h)
pub const HSW_SRD_AUX_CTL: I915RegT = mmio(0x64810);

/// `_SRD_AUX_CTL_A` (intel_psr_regs.h)
pub const _SRD_AUX_CTL_A: u32 = 0x60810;

/// `_SRD_AUX_CTL_EDP` (intel_psr_regs.h)
pub const _SRD_AUX_CTL_EDP: u32 = 0x6f810;

/// `EDP_PSR_AUX_CTL_TIME_OUT_MASK` (intel_psr_regs.h)
pub const EDP_PSR_AUX_CTL_TIME_OUT_MASK: u32 = DP_AUX_CH_CTL_TIME_OUT_MASK;

/// `EDP_PSR_AUX_CTL_MESSAGE_SIZE_MASK` (intel_psr_regs.h)
pub const EDP_PSR_AUX_CTL_MESSAGE_SIZE_MASK: u32 = DP_AUX_CH_CTL_MESSAGE_SIZE_MASK;

/// `EDP_PSR_AUX_CTL_PRECHARGE_2US_MASK` (intel_psr_regs.h)
pub const EDP_PSR_AUX_CTL_PRECHARGE_2US_MASK: u32 = DP_AUX_CH_CTL_PRECHARGE_2US_MASK;

/// `EDP_PSR_AUX_CTL_ERROR_INTERRUPT` (intel_psr_regs.h)
pub const EDP_PSR_AUX_CTL_ERROR_INTERRUPT: u32 = reg_bit(11);

/// `EDP_PSR_AUX_CTL_BIT_CLOCK_2X_MASK` (intel_psr_regs.h)
pub const EDP_PSR_AUX_CTL_BIT_CLOCK_2X_MASK: u32 = DP_AUX_CH_CTL_BIT_CLOCK_2X_MASK;

/// `HSW_SRD_AUX_DATA(i)` (intel_psr_regs.h)
pub const fn HSW_SRD_AUX_DATA(i: u32) -> I915RegT {
    mmio((0x64814 + ((i) * 4)))
}

/// `_SRD_AUX_DATA_A` (intel_psr_regs.h)
pub const _SRD_AUX_DATA_A: u32 = 0x60814;

/// `_SRD_AUX_DATA_EDP` (intel_psr_regs.h)
pub const _SRD_AUX_DATA_EDP: u32 = 0x6f814;

/// `HSW_SRD_STATUS` (intel_psr_regs.h)
pub const HSW_SRD_STATUS: I915RegT = mmio(0x64840);

/// `_SRD_STATUS_A` (intel_psr_regs.h)
pub const _SRD_STATUS_A: u32 = 0x60840;

/// `_SRD_STATUS_EDP` (intel_psr_regs.h)
pub const _SRD_STATUS_EDP: u32 = 0x6f840;

/// `EDP_PSR_STATUS_STATE_MASK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_MASK: u32 = reg_genmask(31, 29);

/// `EDP_PSR_STATUS_STATE_IDLE` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_IDLE: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 0);

/// `EDP_PSR_STATUS_STATE_SRDONACK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_SRDONACK: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 1);

/// `EDP_PSR_STATUS_STATE_SRDENT` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_SRDENT: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 2);

/// `EDP_PSR_STATUS_STATE_BUFOFF` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_BUFOFF: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 3);

/// `EDP_PSR_STATUS_STATE_BUFON` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_BUFON: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 4);

/// `EDP_PSR_STATUS_STATE_AUXACK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_AUXACK: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 5);

/// `EDP_PSR_STATUS_STATE_SRDOFFACK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_STATE_SRDOFFACK: u32 = reg_field_prep(EDP_PSR_STATUS_STATE_MASK, 6);

/// `EDP_PSR_STATUS_LINK_MASK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_LINK_MASK: u32 = reg_genmask(27, 26);

/// `EDP_PSR_STATUS_LINK_FULL_OFF` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_LINK_FULL_OFF: u32 = reg_field_prep(EDP_PSR_STATUS_LINK_MASK, 0);

/// `EDP_PSR_STATUS_LINK_FULL_ON` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_LINK_FULL_ON: u32 = reg_field_prep(EDP_PSR_STATUS_LINK_MASK, 1);

/// `EDP_PSR_STATUS_LINK_STANDBY` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_LINK_STANDBY: u32 = reg_field_prep(EDP_PSR_STATUS_LINK_MASK, 2);

/// `EDP_PSR_STATUS_MAX_SLEEP_TIMER_MASK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_MAX_SLEEP_TIMER_MASK: u32 = reg_genmask(24, 20);

/// `EDP_PSR_STATUS_COUNT_MASK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_COUNT_MASK: u32 = reg_genmask(19, 16);

/// `EDP_PSR_STATUS_AUX_ERROR` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_AUX_ERROR: u32 = reg_bit(15);

/// `EDP_PSR_STATUS_AUX_SENDING` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_AUX_SENDING: u32 = reg_bit(12);

/// `EDP_PSR_STATUS_SENDING_IDLE` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_SENDING_IDLE: u32 = reg_bit(9);

/// `EDP_PSR_STATUS_SENDING_TP2_TP3` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_SENDING_TP2_TP3: u32 = reg_bit(8);

/// `EDP_PSR_STATUS_SENDING_TP1` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_SENDING_TP1: u32 = reg_bit(4);

/// `EDP_PSR_STATUS_IDLE_MASK` (intel_psr_regs.h)
pub const EDP_PSR_STATUS_IDLE_MASK: u32 = reg_genmask(3, 0);

/// `HSW_SRD_PERF_CNT` (intel_psr_regs.h)
pub const HSW_SRD_PERF_CNT: I915RegT = mmio(0x64844);

/// `_SRD_PERF_CNT_A` (intel_psr_regs.h)
pub const _SRD_PERF_CNT_A: u32 = 0x60844;

/// `_SRD_PERF_CNT_EDP` (intel_psr_regs.h)
pub const _SRD_PERF_CNT_EDP: u32 = 0x6f844;

/// `EDP_PSR_PERF_CNT_MASK` (intel_psr_regs.h)
pub const EDP_PSR_PERF_CNT_MASK: u32 = reg_genmask(23, 0);

/// `HSW_SRD_DEBUG` (intel_psr_regs.h)
pub const HSW_SRD_DEBUG: I915RegT = mmio(0x64860);

/// `_SRD_DEBUG_A` (intel_psr_regs.h)
pub const _SRD_DEBUG_A: u32 = 0x60860;

/// `_SRD_DEBUG_EDP` (intel_psr_regs.h)
pub const _SRD_DEBUG_EDP: u32 = 0x6f860;

/// `EDP_PSR_DEBUG_MASK_MAX_SLEEP` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_MAX_SLEEP: u32 = reg_bit(28);

/// `EDP_PSR_DEBUG_MASK_LPSP` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_LPSP: u32 = reg_bit(27);

/// `EDP_PSR_DEBUG_MASK_MEMUP` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_MEMUP: u32 = reg_bit(26);

/// `EDP_PSR_DEBUG_MASK_HPD` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_HPD: u32 = reg_bit(25);

/// `EDP_PSR_DEBUG_MASK_FBC_MODIFY` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_FBC_MODIFY: u32 = reg_bit(24);

/// `EDP_PSR_DEBUG_MASK_PRIMARY_FLIP` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_PRIMARY_FLIP: u32 = reg_bit(23);

/// `EDP_PSR_DEBUG_MASK_HDCP_ENABLE` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_HDCP_ENABLE: u32 = reg_bit(22);

/// `EDP_PSR_DEBUG_MASK_SPRITE_ENABLE` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_SPRITE_ENABLE: u32 = reg_bit(21);

/// `EDP_PSR_DEBUG_MASK_CURSOR_MOVE` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_CURSOR_MOVE: u32 = reg_bit(20);

/// `EDP_PSR_DEBUG_MASK_VBLANK_VSYNC_INT` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_VBLANK_VSYNC_INT: u32 = reg_bit(19);

/// `EDP_PSR_DEBUG_MASK_DPST_PHASE_IN` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_DPST_PHASE_IN: u32 = reg_bit(18);

/// `EDP_PSR_DEBUG_MASK_KVMR_SESSION_EN` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_KVMR_SESSION_EN: u32 = reg_bit(17);

/// `EDP_PSR_DEBUG_MASK_DISP_REG_WRITE` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_MASK_DISP_REG_WRITE: u32 = reg_bit(16);

/// `EDP_PSR_DEBUG_EXIT_ON_PIXEL_UNDERRUN` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_EXIT_ON_PIXEL_UNDERRUN: u32 = reg_bit(15);

/// `EDP_PSR_DEBUG_RFB_UPDATE_SENT` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_RFB_UPDATE_SENT: u32 = reg_bit(2);

/// `EDP_PSR_DEBUG_ENTRY_COMPLETION` (intel_psr_regs.h)
pub const EDP_PSR_DEBUG_ENTRY_COMPLETION: u32 = reg_bit(1);

/// `_PSR2_CTL_A` (intel_psr_regs.h)
pub const _PSR2_CTL_A: u32 = 0x60900;

/// `_PSR2_CTL_EDP` (intel_psr_regs.h)
pub const _PSR2_CTL_EDP: u32 = 0x6f900;

/// `EDP_PSR2_ENABLE` (intel_psr_regs.h)
pub const EDP_PSR2_ENABLE: u32 = reg_bit(31);

/// `EDP_SU_TRACK_ENABLE` (intel_psr_regs.h)
pub const EDP_SU_TRACK_ENABLE: u32 = reg_bit(30);

/// `TGL_EDP_PSR2_BLOCK_COUNT_MASK` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_BLOCK_COUNT_MASK: u32 = reg_bit(28);

/// `TGL_EDP_PSR2_BLOCK_COUNT_NUM_2` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_BLOCK_COUNT_NUM_2: u32 = reg_field_prep(TGL_EDP_PSR2_BLOCK_COUNT_MASK, 0);

/// `TGL_EDP_PSR2_BLOCK_COUNT_NUM_3` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_BLOCK_COUNT_NUM_3: u32 = reg_field_prep(TGL_EDP_PSR2_BLOCK_COUNT_MASK, 1);

/// `LNL_EDP_PSR2_SU_REGION_ET_ENABLE` (intel_psr_regs.h)
pub const LNL_EDP_PSR2_SU_REGION_ET_ENABLE: u32 = reg_bit(27);

/// `EDP_Y_COORDINATE_ENABLE` (intel_psr_regs.h)
pub const EDP_Y_COORDINATE_ENABLE: u32 = reg_bit(25);

/// `EDP_PSR2_SU_SDP_SCANLINE` (intel_psr_regs.h)
pub const EDP_PSR2_SU_SDP_SCANLINE: u32 = reg_bit(25);

/// `EDP_MAX_SU_DISABLE_TIME_MASK` (intel_psr_regs.h)
pub const EDP_MAX_SU_DISABLE_TIME_MASK: u32 = reg_genmask(24, 20);

/// `EDP_PSR2_IO_BUFFER_WAKE_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = reg_genmask(14, 13);

/// `EDP_PSR2_IO_BUFFER_WAKE_MAX_LINES` (intel_psr_regs.h)
pub const EDP_PSR2_IO_BUFFER_WAKE_MAX_LINES: u32 = 8;

/// `EDP_PSR2_IO_BUFFER_WAKE(lines)` (intel_psr_regs.h)
pub const fn EDP_PSR2_IO_BUFFER_WAKE(lines: u32) -> u32 {
    reg_field_prep(EDP_PSR2_IO_BUFFER_WAKE_MASK, (EDP_PSR2_IO_BUFFER_WAKE_MAX_LINES - (lines)))
}

/// `TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = reg_genmask(15, 13);

/// `TGL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES: u32 = 5;

/// `TGL_EDP_PSR2_IO_BUFFER_WAKE(lines)` (intel_psr_regs.h)
pub const fn TGL_EDP_PSR2_IO_BUFFER_WAKE(lines: u32) -> u32 {
    reg_field_prep(TGL_EDP_PSR2_IO_BUFFER_WAKE_MASK, ((lines) - TGL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES))
}

/// `LNL_EDP_PSR2_IO_BUFFER_WAKE_MASK` (intel_psr_regs.h)
pub const LNL_EDP_PSR2_IO_BUFFER_WAKE_MASK: u32 = reg_genmask(18, 13);

/// `LNL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES` (intel_psr_regs.h)
pub const LNL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES: u32 = 5;

/// `LNL_EDP_PSR2_IO_BUFFER_WAKE(lines)` (intel_psr_regs.h)
pub const fn LNL_EDP_PSR2_IO_BUFFER_WAKE(lines: u32) -> u32 {
    reg_field_prep(LNL_EDP_PSR2_IO_BUFFER_WAKE_MASK, ((lines) - LNL_EDP_PSR2_IO_BUFFER_WAKE_MIN_LINES))
}

/// `EDP_PSR2_FAST_WAKE_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_FAST_WAKE_MASK: u32 = reg_genmask(12, 11);

/// `EDP_PSR2_FAST_WAKE_MAX_LINES` (intel_psr_regs.h)
pub const EDP_PSR2_FAST_WAKE_MAX_LINES: u32 = 8;

/// `EDP_PSR2_FAST_WAKE(lines)` (intel_psr_regs.h)
pub const fn EDP_PSR2_FAST_WAKE(lines: u32) -> u32 {
    reg_field_prep(EDP_PSR2_FAST_WAKE_MASK, (EDP_PSR2_FAST_WAKE_MAX_LINES - (lines)))
}

/// `TGL_EDP_PSR2_FAST_WAKE_MASK` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_FAST_WAKE_MASK: u32 = reg_genmask(12, 10);

/// `TGL_EDP_PSR2_FAST_WAKE_MIN_LINES` (intel_psr_regs.h)
pub const TGL_EDP_PSR2_FAST_WAKE_MIN_LINES: u32 = 5;

/// `TGL_EDP_PSR2_FAST_WAKE(lines)` (intel_psr_regs.h)
pub const fn TGL_EDP_PSR2_FAST_WAKE(lines: u32) -> u32 {
    reg_field_prep(TGL_EDP_PSR2_FAST_WAKE_MASK, ((lines) - TGL_EDP_PSR2_FAST_WAKE_MIN_LINES))
}

/// `EDP_PSR2_TP2_TIME_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_TP2_TIME_MASK: u32 = reg_genmask(9, 8);

/// `EDP_PSR2_TP2_TIME_500us` (intel_psr_regs.h)
pub const EDP_PSR2_TP2_TIME_500us: u32 = reg_field_prep(EDP_PSR2_TP2_TIME_MASK, 0);

/// `EDP_PSR2_TP2_TIME_100us` (intel_psr_regs.h)
pub const EDP_PSR2_TP2_TIME_100us: u32 = reg_field_prep(EDP_PSR2_TP2_TIME_MASK, 1);

/// `EDP_PSR2_TP2_TIME_2500us` (intel_psr_regs.h)
pub const EDP_PSR2_TP2_TIME_2500us: u32 = reg_field_prep(EDP_PSR2_TP2_TIME_MASK, 2);

/// `EDP_PSR2_TP2_TIME_50us` (intel_psr_regs.h)
pub const EDP_PSR2_TP2_TIME_50us: u32 = reg_field_prep(EDP_PSR2_TP2_TIME_MASK, 3);

/// `EDP_PSR2_FRAME_BEFORE_SU_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_FRAME_BEFORE_SU_MASK: u32 = reg_genmask(7, 4);

/// `EDP_PSR2_FRAME_BEFORE_SU(a)` (intel_psr_regs.h)
pub const fn EDP_PSR2_FRAME_BEFORE_SU(a: u32) -> u32 {
    reg_field_prep(EDP_PSR2_FRAME_BEFORE_SU_MASK, (a))
}

/// `EDP_PSR2_IDLE_FRAMES_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_IDLE_FRAMES_MASK: u32 = reg_genmask(3, 0);

/// `EDP_PSR2_IDLE_FRAMES(x)` (intel_psr_regs.h)
pub const fn EDP_PSR2_IDLE_FRAMES(x: u32) -> u32 {
    reg_field_prep(EDP_PSR2_IDLE_FRAMES_MASK, (x))
}

/// `_PSR_EVENT_TRANS_A` (intel_psr_regs.h)
pub const _PSR_EVENT_TRANS_A: u32 = 0x60848;

/// `_PSR_EVENT_TRANS_B` (intel_psr_regs.h)
pub const _PSR_EVENT_TRANS_B: u32 = 0x61848;

/// `_PSR_EVENT_TRANS_C` (intel_psr_regs.h)
pub const _PSR_EVENT_TRANS_C: u32 = 0x62848;

/// `_PSR_EVENT_TRANS_D` (intel_psr_regs.h)
pub const _PSR_EVENT_TRANS_D: u32 = 0x63848;

/// `_PSR_EVENT_TRANS_EDP` (intel_psr_regs.h)
pub const _PSR_EVENT_TRANS_EDP: u32 = 0x6f848;

/// `PSR_EVENT_PSR2_WD_TIMER_EXPIRE` (intel_psr_regs.h)
pub const PSR_EVENT_PSR2_WD_TIMER_EXPIRE: u32 = reg_bit(17);

/// `PSR_EVENT_PSR2_DISABLED` (intel_psr_regs.h)
pub const PSR_EVENT_PSR2_DISABLED: u32 = reg_bit(16);

/// `PSR_EVENT_SU_DIRTY_FIFO_UNDERRUN` (intel_psr_regs.h)
pub const PSR_EVENT_SU_DIRTY_FIFO_UNDERRUN: u32 = reg_bit(15);

/// `PSR_EVENT_SU_CRC_FIFO_UNDERRUN` (intel_psr_regs.h)
pub const PSR_EVENT_SU_CRC_FIFO_UNDERRUN: u32 = reg_bit(14);

/// `PSR_EVENT_GRAPHICS_RESET` (intel_psr_regs.h)
pub const PSR_EVENT_GRAPHICS_RESET: u32 = reg_bit(12);

/// `PSR_EVENT_PCH_INTERRUPT` (intel_psr_regs.h)
pub const PSR_EVENT_PCH_INTERRUPT: u32 = reg_bit(11);

/// `PSR_EVENT_MEMORY_UP` (intel_psr_regs.h)
pub const PSR_EVENT_MEMORY_UP: u32 = reg_bit(10);

/// `PSR_EVENT_FRONT_BUFFER_MODIFY` (intel_psr_regs.h)
pub const PSR_EVENT_FRONT_BUFFER_MODIFY: u32 = reg_bit(9);

/// `PSR_EVENT_WD_TIMER_EXPIRE` (intel_psr_regs.h)
pub const PSR_EVENT_WD_TIMER_EXPIRE: u32 = reg_bit(8);

/// `PSR_EVENT_PIPE_REGISTERS_UPDATE` (intel_psr_regs.h)
pub const PSR_EVENT_PIPE_REGISTERS_UPDATE: u32 = reg_bit(6);

/// `PSR_EVENT_REGISTER_UPDATE` (intel_psr_regs.h)
pub const PSR_EVENT_REGISTER_UPDATE: u32 = reg_bit(5);

/// `PSR_EVENT_HDCP_ENABLE` (intel_psr_regs.h)
pub const PSR_EVENT_HDCP_ENABLE: u32 = reg_bit(4);

/// `PSR_EVENT_KVMR_SESSION_ENABLE` (intel_psr_regs.h)
pub const PSR_EVENT_KVMR_SESSION_ENABLE: u32 = reg_bit(3);

/// `PSR_EVENT_VBI_ENABLE` (intel_psr_regs.h)
pub const PSR_EVENT_VBI_ENABLE: u32 = reg_bit(2);

/// `PSR_EVENT_LPSP_MODE_EXIT` (intel_psr_regs.h)
pub const PSR_EVENT_LPSP_MODE_EXIT: u32 = reg_bit(1);

/// `PSR_EVENT_PSR_DISABLE` (intel_psr_regs.h)
pub const PSR_EVENT_PSR_DISABLE: u32 = reg_bit(0);

/// `_PSR2_STATUS_A` (intel_psr_regs.h)
pub const _PSR2_STATUS_A: u32 = 0x60940;

/// `_PSR2_STATUS_EDP` (intel_psr_regs.h)
pub const _PSR2_STATUS_EDP: u32 = 0x6f940;

/// `EDP_PSR2_STATUS_STATE_MASK` (intel_psr_regs.h)
pub const EDP_PSR2_STATUS_STATE_MASK: u32 = reg_genmask(31, 28);

/// `EDP_PSR2_STATUS_STATE_DEEP_SLEEP` (intel_psr_regs.h)
pub const EDP_PSR2_STATUS_STATE_DEEP_SLEEP: u32 = reg_field_prep(EDP_PSR2_STATUS_STATE_MASK, 0x8);

/// `_PSR2_SU_STATUS_A` (intel_psr_regs.h)
pub const _PSR2_SU_STATUS_A: u32 = 0x60914;

/// `_PSR2_SU_STATUS_EDP` (intel_psr_regs.h)
pub const _PSR2_SU_STATUS_EDP: u32 = 0x6f914;

/// `PSR2_SU_STATUS_SHIFT(frame)` (intel_psr_regs.h)
pub const fn PSR2_SU_STATUS_SHIFT(frame: u32) -> u32 {
    (((((frame) % 3)) * 10))
}

/// `PSR2_SU_STATUS_MASK(frame)` (intel_psr_regs.h)
pub const fn PSR2_SU_STATUS_MASK(frame: u32) -> u32 {
    ((0x3ff << PSR2_SU_STATUS_SHIFT(frame)))
}

/// `PSR2_SU_STATUS_FRAMES` (intel_psr_regs.h)
pub const PSR2_SU_STATUS_FRAMES: u32 = 8;

/// `_PSR2_MAN_TRK_CTL_A` (intel_psr_regs.h)
pub const _PSR2_MAN_TRK_CTL_A: u32 = 0x60910;

/// `_PSR2_MAN_TRK_CTL_EDP` (intel_psr_regs.h)
pub const _PSR2_MAN_TRK_CTL_EDP: u32 = 0x6f910;

/// `PSR2_MAN_TRK_CTL_ENABLE` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_ENABLE: u32 = reg_bit(31);

/// `PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK: u32 = reg_genmask(30, 21);

/// `PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR(val)` (intel_psr_regs.h)
pub const fn PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR(val: u32) -> u32 {
    reg_field_prep(PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK, val)
}

/// `PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK: u32 = reg_genmask(20, 11);

/// `PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR(val)` (intel_psr_regs.h)
pub const fn PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR(val: u32) -> u32 {
    reg_field_prep(PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK, val)
}

/// `PSR2_MAN_TRK_CTL_SF_SINGLE_FULL_FRAME` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_SF_SINGLE_FULL_FRAME: u32 = reg_bit(3);

/// `PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME: u32 = reg_bit(2);

/// `PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE` (intel_psr_regs.h)
pub const PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE: u32 = reg_bit(1);

/// `ADLP_PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK` (intel_psr_regs.h)
pub const ADLP_PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK: u32 = reg_genmask(28, 16);

/// `ADLP_PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR(val)` (intel_psr_regs.h)
pub const fn ADLP_PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR(val: u32) -> u32 {
    reg_field_prep(ADLP_PSR2_MAN_TRK_CTL_SU_REGION_START_ADDR_MASK, val)
}

/// `ADLP_PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK` (intel_psr_regs.h)
pub const ADLP_PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK: u32 = reg_genmask(12, 0);

/// `ADLP_PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR(val)` (intel_psr_regs.h)
pub const fn ADLP_PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR(val: u32) -> u32 {
    reg_field_prep(ADLP_PSR2_MAN_TRK_CTL_SU_REGION_END_ADDR_MASK, val)
}

/// `ADLP_PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE` (intel_psr_regs.h)
pub const ADLP_PSR2_MAN_TRK_CTL_SF_PARTIAL_FRAME_UPDATE: u32 = reg_bit(31);

/// `ADLP_PSR2_MAN_TRK_CTL_SF_SINGLE_FULL_FRAME` (intel_psr_regs.h)
pub const ADLP_PSR2_MAN_TRK_CTL_SF_SINGLE_FULL_FRAME: u32 = reg_bit(14);

/// `ADLP_PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME` (intel_psr_regs.h)
pub const ADLP_PSR2_MAN_TRK_CTL_SF_CONTINUOS_FULL_FRAME: u32 = reg_bit(13);

/// `_LNL_SFF_CTL_A` (intel_psr_regs.h)
pub const _LNL_SFF_CTL_A: u32 = 0x60918;

/// `_LNL_SFF_CTL_B` (intel_psr_regs.h)
pub const _LNL_SFF_CTL_B: u32 = 0x61918;

/// `LNL_SFF_CTL(tran)` (intel_psr_regs.h)
pub const fn LNL_SFF_CTL(tran: u32) -> I915RegT {
    _MMIO_TRANS(tran, _LNL_SFF_CTL_A, _LNL_SFF_CTL_B)
}

/// `LNL_SFF_CTL_SF_SINGLE_FULL_FRAME` (intel_psr_regs.h)
pub const LNL_SFF_CTL_SF_SINGLE_FULL_FRAME: u32 = reg_bit(1);

/// `_LNL_CFF_CTL_A` (intel_psr_regs.h)
pub const _LNL_CFF_CTL_A: u32 = 0x6091c;

/// `_LNL_CFF_CTL_B` (intel_psr_regs.h)
pub const _LNL_CFF_CTL_B: u32 = 0x6191c;

/// `LNL_CFF_CTL(tran)` (intel_psr_regs.h)
pub const fn LNL_CFF_CTL(tran: u32) -> I915RegT {
    _MMIO_TRANS(tran, _LNL_CFF_CTL_A, _LNL_CFF_CTL_B)
}

/// `LNL_CFF_CTL_SF_CONTINUOUS_FULL_FRAME` (intel_psr_regs.h)
pub const LNL_CFF_CTL_SF_CONTINUOUS_FULL_FRAME: u32 = reg_bit(1);

/// `_PIPE_SRCSZ_ERLY_TPT_A` (intel_psr_regs.h)
pub const _PIPE_SRCSZ_ERLY_TPT_A: u32 = 0x70074;

/// `_PIPE_SRCSZ_ERLY_TPT_B` (intel_psr_regs.h)
pub const _PIPE_SRCSZ_ERLY_TPT_B: u32 = 0x71074;

/// `PIPE_SRCSZ_ERLY_TPT(pipe)` (intel_psr_regs.h)
pub const fn PIPE_SRCSZ_ERLY_TPT(pipe: u32) -> I915RegT {
    _MMIO_PIPE((pipe), _PIPE_SRCSZ_ERLY_TPT_A, _PIPE_SRCSZ_ERLY_TPT_B)
}

/// `_PR_ALPM_CTL_A` (intel_psr_regs.h)
pub const _PR_ALPM_CTL_A: u32 = 0x60948;

/// `PR_ALPM_CTL_ALLOW_LINK_OFF_BETWEEN_AS_SDP_AND_SU` (intel_psr_regs.h)
pub const PR_ALPM_CTL_ALLOW_LINK_OFF_BETWEEN_AS_SDP_AND_SU: u32 = reg_bit(6);

/// `PR_ALPM_CTL_RFB_UPDATE_CONTROL` (intel_psr_regs.h)
pub const PR_ALPM_CTL_RFB_UPDATE_CONTROL: u32 = reg_bit(5);

/// `PR_ALPM_CTL_AS_SDP_TRANSMISSION_IN_ACTIVE_DISABLE` (intel_psr_regs.h)
pub const PR_ALPM_CTL_AS_SDP_TRANSMISSION_IN_ACTIVE_DISABLE: u32 = reg_bit(4);

/// `PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_MASK` (intel_psr_regs.h)
pub const PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_MASK: u32 = reg_genmask(1, 0);

/// `PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T1_OR_T2` (intel_psr_regs.h)
pub const PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T1_OR_T2: u32 = reg_field_prep(PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_MASK, 0);

/// `PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T1` (intel_psr_regs.h)
pub const PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T1: u32 = reg_field_prep(PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_MASK, 1);

/// `PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T2` (intel_psr_regs.h)
pub const PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_T2: u32 = reg_field_prep(PR_ALPM_CTL_ADAPTIVE_SYNC_SDP_POSITION_MASK, 2);

/// `_ALPM_CTL_A` (intel_psr_regs.h)
pub const _ALPM_CTL_A: u32 = 0x60950;

/// `ALPM_CTL_ALPM_ENABLE` (intel_psr_regs.h)
pub const ALPM_CTL_ALPM_ENABLE: u32 = reg_bit(31);

/// `ALPM_CTL_ALPM_AUX_LESS_ENABLE` (intel_psr_regs.h)
pub const ALPM_CTL_ALPM_AUX_LESS_ENABLE: u32 = reg_bit(30);

/// `ALPM_CTL_LOBF_ENABLE` (intel_psr_regs.h)
pub const ALPM_CTL_LOBF_ENABLE: u32 = reg_bit(29);

/// `ALPM_CTL_EXTENDED_FAST_WAKE_ENABLE` (intel_psr_regs.h)
pub const ALPM_CTL_EXTENDED_FAST_WAKE_ENABLE: u32 = reg_bit(28);

/// `ALPM_CTL_KEEP_FEC_ENABLE_FOR_AUX_WAKE_SLEEP` (intel_psr_regs.h)
pub const ALPM_CTL_KEEP_FEC_ENABLE_FOR_AUX_WAKE_SLEEP: u32 = reg_bit(27);

/// `ALPM_CTL_RESTORE_OCCURED` (intel_psr_regs.h)
pub const ALPM_CTL_RESTORE_OCCURED: u32 = reg_bit(26);

/// `ALPM_CTL_RESTORE_TO_SLEEP` (intel_psr_regs.h)
pub const ALPM_CTL_RESTORE_TO_SLEEP: u32 = reg_bit(25);

/// `ALPM_CTL_RESTORE_TO_DEEP_SLEEP` (intel_psr_regs.h)
pub const ALPM_CTL_RESTORE_TO_DEEP_SLEEP: u32 = reg_bit(24);

/// `ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK: u32 = reg_genmask(23, 21);

/// `ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_50_SYMBOLS` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_50_SYMBOLS: u32 = reg_field_prep(ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK, 0);

/// `ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_128_SYMBOLS` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_128_SYMBOLS: u32 = reg_field_prep(ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK, 1);

/// `ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_256_SYMBOLS` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_256_SYMBOLS: u32 = reg_field_prep(ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK, 2);

/// `ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_512_SYMBOLS` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_512_SYMBOLS: u32 = reg_field_prep(ALPM_CTL_AUX_LESS_SLEEP_HOLD_TIME_MASK, 3);

/// `ALPM_CTL_AUX_WAKE_SLEEP_HOLD_ENABLE` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_WAKE_SLEEP_HOLD_ENABLE: u32 = reg_bit(20);

/// `ALPM_CTL_ALPM_ENTRY_CHECK_MASK` (intel_psr_regs.h)
pub const ALPM_CTL_ALPM_ENTRY_CHECK_MASK: u32 = reg_genmask(19, 16);

/// `ALPM_CTL_ALPM_ENTRY_CHECK(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL_ALPM_ENTRY_CHECK(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL_ALPM_ENTRY_CHECK_MASK, val)
}

/// `ALPM_CTL_EXTENDED_FAST_WAKE_TIME_MASK` (intel_psr_regs.h)
pub const ALPM_CTL_EXTENDED_FAST_WAKE_TIME_MASK: u32 = reg_genmask(13, 8);

/// `ALPM_CTL_EXTENDED_FAST_WAKE_MIN_LINES` (intel_psr_regs.h)
pub const ALPM_CTL_EXTENDED_FAST_WAKE_MIN_LINES: u32 = 5;

/// `ALPM_CTL_EXTENDED_FAST_WAKE_TIME(lines)` (intel_psr_regs.h)
pub const fn ALPM_CTL_EXTENDED_FAST_WAKE_TIME(lines: u32) -> u32 {
    reg_field_prep(ALPM_CTL_EXTENDED_FAST_WAKE_TIME_MASK, ((lines) - ALPM_CTL_EXTENDED_FAST_WAKE_MIN_LINES))
}

/// `ALPM_CTL_AUX_LESS_WAKE_TIME_MASK` (intel_psr_regs.h)
pub const ALPM_CTL_AUX_LESS_WAKE_TIME_MASK: u32 = reg_genmask(5, 0);

/// `ALPM_CTL_AUX_LESS_WAKE_TIME(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL_AUX_LESS_WAKE_TIME(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL_AUX_LESS_WAKE_TIME_MASK, val)
}

/// `_ALPM_CTL2_A` (intel_psr_regs.h)
pub const _ALPM_CTL2_A: u32 = 0x60954;

/// `ALPM_CTL2_SWITCH_TO_ACTIVE_LATENCY_MASK` (intel_psr_regs.h)
pub const ALPM_CTL2_SWITCH_TO_ACTIVE_LATENCY_MASK: u32 = reg_genmask(28, 24);

/// `ALPM_CTL2_SWITCH_TO_ACTIVE_LATENCY(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL2_SWITCH_TO_ACTIVE_LATENCY(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL2_SWITCH_TO_ACTIVE_LATENCY_MASK, val)
}

/// `ALPM_CTL2_AUX_LESS_WAKE_TIME_EXTENSION_MASK` (intel_psr_regs.h)
pub const ALPM_CTL2_AUX_LESS_WAKE_TIME_EXTENSION_MASK: u32 = reg_genmask(19, 16);

/// `ALPM_CTL2_AUX_LESS_WAKE_TIME_EXTENSION(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL2_AUX_LESS_WAKE_TIME_EXTENSION(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL2_AUX_LESS_WAKE_TIME_EXTENSION_MASK, val)
}

/// `ALPM_CTL2_NUMBER_OF_LTTPR_MASK` (intel_psr_regs.h)
pub const ALPM_CTL2_NUMBER_OF_LTTPR_MASK: u32 = reg_genmask(15, 12);

/// `ALPM_CTL2_NUMBER_OF_LTTPR(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL2_NUMBER_OF_LTTPR(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL2_NUMBER_OF_LTTPR_MASK, val)
}

/// `ALPM_CTL2_LTTPR_AUX_LESS_SLEEP_HOLD_TIME_MASK` (intel_psr_regs.h)
pub const ALPM_CTL2_LTTPR_AUX_LESS_SLEEP_HOLD_TIME_MASK: u32 = reg_genmask(10, 8);

/// `ALPM_CTL2_LTTPR_AUX_LESS_SLEEP_HOLD_TIME(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL2_LTTPR_AUX_LESS_SLEEP_HOLD_TIME(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL2_LTTPR_AUX_LESS_SLEEP_HOLD_TIME_MASK, val)
}

/// `ALPM_CTL2_FEC_DECODE_EN_POSITION_AFTER_WAKE_SR` (intel_psr_regs.h)
pub const ALPM_CTL2_FEC_DECODE_EN_POSITION_AFTER_WAKE_SR: u32 = reg_bit(4);

/// `ALPM_CTL2_NUMBER_AUX_LESS_ML_PHY_SLEEP_SEQUENCES_MASK` (intel_psr_regs.h)
pub const ALPM_CTL2_NUMBER_AUX_LESS_ML_PHY_SLEEP_SEQUENCES_MASK: u32 = reg_genmask(2, 0);

/// `ALPM_CTL2_NUMBER_AUX_LESS_ML_PHY_SLEEP_SEQUENCES(val)` (intel_psr_regs.h)
pub const fn ALPM_CTL2_NUMBER_AUX_LESS_ML_PHY_SLEEP_SEQUENCES(val: u32) -> u32 {
    reg_field_prep(ALPM_CTL2_NUMBER_AUX_LESS_ML_PHY_SLEEP_SEQUENCES_MASK, val)
}

/// `_PORT_ALPM_CTL_A` (intel_psr_regs.h)
pub const _PORT_ALPM_CTL_A: u32 = 0x16fa2c;

/// `_PORT_ALPM_CTL_B` (intel_psr_regs.h)
pub const _PORT_ALPM_CTL_B: u32 = 0x16fc2c;

/// `PORT_ALPM_CTL(port)` (intel_psr_regs.h)
pub const fn PORT_ALPM_CTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _PORT_ALPM_CTL_A, _PORT_ALPM_CTL_B)
}

/// `PORT_ALPM_CTL_ALPM_AUX_LESS_ENABLE` (intel_psr_regs.h)
pub const PORT_ALPM_CTL_ALPM_AUX_LESS_ENABLE: u32 = reg_bit(31);

/// `PORT_ALPM_CTL_MAX_PHY_SWING_SETUP_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_CTL_MAX_PHY_SWING_SETUP_MASK: u32 = reg_genmask(25, 20);

/// `PORT_ALPM_CTL_MAX_PHY_SWING_SETUP(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_CTL_MAX_PHY_SWING_SETUP(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_CTL_MAX_PHY_SWING_SETUP_MASK, val)
}

/// `PORT_ALPM_CTL_MAX_PHY_SWING_HOLD_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_CTL_MAX_PHY_SWING_HOLD_MASK: u32 = reg_genmask(19, 16);

/// `PORT_ALPM_CTL_MAX_PHY_SWING_HOLD(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_CTL_MAX_PHY_SWING_HOLD(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_CTL_MAX_PHY_SWING_HOLD_MASK, val)
}

/// `PORT_ALPM_CTL_SILENCE_PERIOD_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_CTL_SILENCE_PERIOD_MASK: u32 = reg_genmask(7, 0);

/// `PORT_ALPM_CTL_SILENCE_PERIOD(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_CTL_SILENCE_PERIOD(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_CTL_SILENCE_PERIOD_MASK, val)
}

/// `_PORT_ALPM_LFPS_CTL_A` (intel_psr_regs.h)
pub const _PORT_ALPM_LFPS_CTL_A: u32 = 0x16fa30;

/// `_PORT_ALPM_LFPS_CTL_B` (intel_psr_regs.h)
pub const _PORT_ALPM_LFPS_CTL_B: u32 = 0x16fc30;

/// `PORT_ALPM_LFPS_CTL(port)` (intel_psr_regs.h)
pub const fn PORT_ALPM_LFPS_CTL(port: u32) -> I915RegT {
    _MMIO_PORT(port, _PORT_ALPM_LFPS_CTL_A, _PORT_ALPM_LFPS_CTL_B)
}

/// `PORT_ALPM_LFPS_CTL_LFPS_START_POLARITY` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_LFPS_START_POLARITY: u32 = reg_bit(31);

/// `PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MASK: u32 = reg_genmask(27, 24);

/// `PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MIN` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MIN: u32 = 7;

/// `PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MASK, ((val) - PORT_ALPM_LFPS_CTL_LFPS_CYCLE_COUNT_MIN))
}

/// `PORT_ALPM_LFPS_CTL_LFPS_HALF_CYCLE_DURATION_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_LFPS_HALF_CYCLE_DURATION_MASK: u32 = reg_genmask(20, 16);

/// `PORT_ALPM_LFPS_CTL_LFPS_HALF_CYCLE_DURATION(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_LFPS_CTL_LFPS_HALF_CYCLE_DURATION(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_LFPS_CTL_LFPS_HALF_CYCLE_DURATION_MASK, val)
}

/// `PORT_ALPM_LFPS_CTL_FIRST_LFPS_HALF_CYCLE_DURATION_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_FIRST_LFPS_HALF_CYCLE_DURATION_MASK: u32 = reg_genmask(12, 8);

/// `PORT_ALPM_LFPS_CTL_FIRST_LFPS_HALF_CYCLE_DURATION(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_LFPS_CTL_FIRST_LFPS_HALF_CYCLE_DURATION(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_LFPS_CTL_FIRST_LFPS_HALF_CYCLE_DURATION_MASK, val)
}

/// `PORT_ALPM_LFPS_CTL_LAST_LFPS_HALF_CYCLE_DURATION_MASK` (intel_psr_regs.h)
pub const PORT_ALPM_LFPS_CTL_LAST_LFPS_HALF_CYCLE_DURATION_MASK: u32 = reg_genmask(4, 0);

/// `PORT_ALPM_LFPS_CTL_LAST_LFPS_HALF_CYCLE_DURATION(val)` (intel_psr_regs.h)
pub const fn PORT_ALPM_LFPS_CTL_LAST_LFPS_HALF_CYCLE_DURATION(val: u32) -> u32 {
    reg_field_prep(PORT_ALPM_LFPS_CTL_LAST_LFPS_HALF_CYCLE_DURATION_MASK, val)
}
