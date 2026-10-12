// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/intel_dsb_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_DSBSL_INSTANCE_BASE` (intel_dsb_regs.h)
pub const _DSBSL_INSTANCE_BASE: u32 = 0x70B00;

/// `DSBSL_INSTANCE(pipe, id)` (intel_dsb_regs.h)
pub const fn DSBSL_INSTANCE(pipe: u32, id: u32) -> u32 {
    (((_DSBSL_INSTANCE_BASE + ((pipe) * 0x1000)) + ((id) * 0x100)))
}

/// `DSB_HEAD(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_HEAD(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x0))
}

/// `DSB_TAIL(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_TAIL(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x4))
}

/// `DSB_CTRL(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_CTRL(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x8))
}

/// `DSB_ENABLE` (intel_dsb_regs.h)
pub const DSB_ENABLE: u32 = reg_bit(31);

/// `DSB_BUF_REITERATE` (intel_dsb_regs.h)
pub const DSB_BUF_REITERATE: u32 = reg_bit(29);

/// `DSB_WAIT_FOR_VBLANK` (intel_dsb_regs.h)
pub const DSB_WAIT_FOR_VBLANK: u32 = reg_bit(28);

/// `DSB_WAIT_FOR_LINE_IN` (intel_dsb_regs.h)
pub const DSB_WAIT_FOR_LINE_IN: u32 = reg_bit(27);

/// `DSB_HALT` (intel_dsb_regs.h)
pub const DSB_HALT: u32 = reg_bit(16);

/// `DSB_NON_POSTED` (intel_dsb_regs.h)
pub const DSB_NON_POSTED: u32 = reg_bit(8);

/// `DSB_STATUS_BUSY` (intel_dsb_regs.h)
pub const DSB_STATUS_BUSY: u32 = reg_bit(0);

/// `DSB_MMIOCTRL(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_MMIOCTRL(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0xc))
}

/// `DSB_MMIO_DEAD_CLOCKS_ENABLE` (intel_dsb_regs.h)
pub const DSB_MMIO_DEAD_CLOCKS_ENABLE: u32 = reg_bit(31);

/// `DSB_MMIO_DEAD_CLOCKS_COUNT_MASK` (intel_dsb_regs.h)
pub const DSB_MMIO_DEAD_CLOCKS_COUNT_MASK: u32 = reg_genmask(15, 8);

/// `DSB_MMIO_CYCLES_MASK` (intel_dsb_regs.h)
pub const DSB_MMIO_CYCLES_MASK: u32 = reg_genmask(7, 0);

/// `DSB_MMIO_CYCLES(x)` (intel_dsb_regs.h)
pub const fn DSB_MMIO_CYCLES(x: u32) -> u32 {
    reg_field_prep(DSB_MMIO_CYCLES_MASK, (x))
}

/// `DSB_POLLFUNC(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_POLLFUNC(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x10))
}

/// `DSB_POLL_ENABLE` (intel_dsb_regs.h)
pub const DSB_POLL_ENABLE: u32 = reg_bit(31);

/// `DSB_POLL_WAIT_MASK` (intel_dsb_regs.h)
pub const DSB_POLL_WAIT_MASK: u32 = reg_genmask(30, 23);

/// `DSB_POLL_WAIT(x)` (intel_dsb_regs.h)
pub const fn DSB_POLL_WAIT(x: u32) -> u32 {
    reg_field_prep(DSB_POLL_WAIT_MASK, (x))
}

/// `DSB_POLL_COUNT_MASK` (intel_dsb_regs.h)
pub const DSB_POLL_COUNT_MASK: u32 = reg_genmask(22, 15);

/// `DSB_POLL_COUNT(x)` (intel_dsb_regs.h)
pub const fn DSB_POLL_COUNT(x: u32) -> u32 {
    reg_field_prep(DSB_POLL_COUNT_MASK, (x))
}

/// `DSB_DEBUG(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_DEBUG(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x14))
}

/// `DSB_POLLMASK(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_POLLMASK(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x1c))
}

/// `DSB_STATUS(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_STATUS(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x24))
}

/// `DSB_HP_IDLE_STATUS` (intel_dsb_regs.h)
pub const DSB_HP_IDLE_STATUS: u32 = reg_bit(31);

/// `DSB_DEWAKE_STATUS` (intel_dsb_regs.h)
pub const DSB_DEWAKE_STATUS: u32 = reg_bit(30);

/// `DSB_REQARB_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_REQARB_SM_STATE_MASK: u32 = reg_genmask(29, 27);

/// `DSB_SAFE_WINDOW_LIVE` (intel_dsb_regs.h)
pub const DSB_SAFE_WINDOW_LIVE: u32 = reg_bit(26);

/// `DSB_VTDFAULT_ARB_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_VTDFAULT_ARB_SM_STATE_MASK: u32 = reg_genmask(25, 23);

/// `DSB_TLBTRANS_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_TLBTRANS_SM_STATE_MASK: u32 = reg_genmask(21, 20);

/// `DSB_SAFE_WINDOW` (intel_dsb_regs.h)
pub const DSB_SAFE_WINDOW: u32 = reg_bit(19);

/// `DSB_POINTERS_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_POINTERS_SM_STATE_MASK: u32 = reg_genmask(18, 17);

/// `DSB_BUSY_DURING_DELAYED_VBLANK` (intel_dsb_regs.h)
pub const DSB_BUSY_DURING_DELAYED_VBLANK: u32 = reg_bit(16);

/// `DSB_MMIO_ARB_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_MMIO_ARB_SM_STATE_MASK: u32 = reg_genmask(15, 13);

/// `DSB_MMIO_INST_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_MMIO_INST_SM_STATE_MASK: u32 = reg_genmask(11, 7);

/// `DSB_RESET_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_RESET_SM_STATE_MASK: u32 = reg_genmask(5, 4);

/// `DSB_RUN_SM_STATE_MASK` (intel_dsb_regs.h)
pub const DSB_RUN_SM_STATE_MASK: u32 = reg_genmask(2, 0);

/// `DSB_INTERRUPT(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_INTERRUPT(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x28))
}

/// `DSB_GOSUB_INT_EN` (intel_dsb_regs.h)
pub const DSB_GOSUB_INT_EN: u32 = reg_bit(21);

/// `DSB_ATS_FAULT_INT_EN` (intel_dsb_regs.h)
pub const DSB_ATS_FAULT_INT_EN: u32 = reg_bit(20);

/// `DSB_GTT_FAULT_INT_EN` (intel_dsb_regs.h)
pub const DSB_GTT_FAULT_INT_EN: u32 = reg_bit(19);

/// `DSB_RSPTIMEOUT_INT_EN` (intel_dsb_regs.h)
pub const DSB_RSPTIMEOUT_INT_EN: u32 = reg_bit(18);

/// `DSB_POLL_ERR_INT_EN` (intel_dsb_regs.h)
pub const DSB_POLL_ERR_INT_EN: u32 = reg_bit(17);

/// `DSB_PROG_INT_EN` (intel_dsb_regs.h)
pub const DSB_PROG_INT_EN: u32 = reg_bit(16);

/// `DSB_GOSUB_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_GOSUB_INT_STATUS: u32 = reg_bit(5);

/// `DSB_ATS_FAULT_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_ATS_FAULT_INT_STATUS: u32 = reg_bit(4);

/// `DSB_GTT_FAULT_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_GTT_FAULT_INT_STATUS: u32 = reg_bit(3);

/// `DSB_RSPTIMEOUT_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_RSPTIMEOUT_INT_STATUS: u32 = reg_bit(2);

/// `DSB_POLL_ERR_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_POLL_ERR_INT_STATUS: u32 = reg_bit(1);

/// `DSB_PROG_INT_STATUS` (intel_dsb_regs.h)
pub const DSB_PROG_INT_STATUS: u32 = reg_bit(0);

/// `DSB_CURRENT_HEAD(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_CURRENT_HEAD(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x2c))
}

/// `DSB_RM_TIMEOUT(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_RM_TIMEOUT(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x30))
}

/// `DSB_RM_CLAIM_TIMEOUT` (intel_dsb_regs.h)
pub const DSB_RM_CLAIM_TIMEOUT: u32 = reg_bit(31);

/// `DSB_RM_READY_TIMEOUT` (intel_dsb_regs.h)
pub const DSB_RM_READY_TIMEOUT: u32 = reg_bit(30);

/// `DSB_RM_CLAIM_TIMEOUT_COUNT_MASK` (intel_dsb_regs.h)
pub const DSB_RM_CLAIM_TIMEOUT_COUNT_MASK: u32 = reg_genmask(23, 16);

/// `DSB_RM_CLAIM_TIMEOUT_COUNT(x)` (intel_dsb_regs.h)
pub const fn DSB_RM_CLAIM_TIMEOUT_COUNT(x: u32) -> u32 {
    reg_field_prep(DSB_RM_CLAIM_TIMEOUT_COUNT_MASK, (x))
}

/// `DSB_RM_READY_TIMEOUT_VALUE_MASK` (intel_dsb_regs.h)
pub const DSB_RM_READY_TIMEOUT_VALUE_MASK: u32 = reg_genmask(15, 0);

/// `DSB_RMTIMEOUTREG_CAPTURE(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_RMTIMEOUTREG_CAPTURE(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x34))
}

/// `DSB_PMCTRL(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_PMCTRL(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x38))
}

/// `DSB_ENABLE_DEWAKE` (intel_dsb_regs.h)
pub const DSB_ENABLE_DEWAKE: u32 = reg_bit(31);

/// `DSB_SCANLINE_FOR_DEWAKE_MASK` (intel_dsb_regs.h)
pub const DSB_SCANLINE_FOR_DEWAKE_MASK: u32 = reg_genmask(30, 0);

/// `DSB_SCANLINE_FOR_DEWAKE(x)` (intel_dsb_regs.h)
pub const fn DSB_SCANLINE_FOR_DEWAKE(x: u32) -> u32 {
    reg_field_prep(DSB_SCANLINE_FOR_DEWAKE_MASK, (x))
}

/// `DSB_PMCTRL_2(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_PMCTRL_2(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x3c))
}

/// `DSB_MMIOGEN_DEWAKE_DIS` (intel_dsb_regs.h)
pub const DSB_MMIOGEN_DEWAKE_DIS: u32 = reg_bit(31);

/// `DSB_FORCE_DEWAKE` (intel_dsb_regs.h)
pub const DSB_FORCE_DEWAKE: u32 = reg_bit(23);

/// `DSB_BLOCK_DEWAKE_EXTENSION` (intel_dsb_regs.h)
pub const DSB_BLOCK_DEWAKE_EXTENSION: u32 = reg_bit(15);

/// `DSB_OVERRIDE_DC5_DC6_OK` (intel_dsb_regs.h)
pub const DSB_OVERRIDE_DC5_DC6_OK: u32 = reg_bit(7);

/// `DSB_PF_LN_LOWER(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_PF_LN_LOWER(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x40))
}

/// `DSB_PF_LN_UPPER(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_PF_LN_UPPER(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x44))
}

/// `DSB_BUFRPT_CNT(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_BUFRPT_CNT(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0x48))
}

/// `DSB_CHICKEN(pipe, id)` (intel_dsb_regs.h)
pub const fn DSB_CHICKEN(pipe: u32, id: u32) -> I915RegT {
    mmio((DSBSL_INSTANCE(pipe, id) + 0xf0))
}

/// `DSB_FORCE_DMA_SYNC_RESET` (intel_dsb_regs.h)
pub const DSB_FORCE_DMA_SYNC_RESET: u32 = reg_bit(31);

/// `DSB_FORCE_VTD_ENGIE_RESET` (intel_dsb_regs.h)
pub const DSB_FORCE_VTD_ENGIE_RESET: u32 = reg_bit(30);

/// `DSB_DISABLE_IPC_DEMOTE` (intel_dsb_regs.h)
pub const DSB_DISABLE_IPC_DEMOTE: u32 = reg_bit(29);

/// `DSB_SKIP_WAITS_EN` (intel_dsb_regs.h)
pub const DSB_SKIP_WAITS_EN: u32 = reg_bit(23);

/// `DSB_EXTEND_HP_IDLE` (intel_dsb_regs.h)
pub const DSB_EXTEND_HP_IDLE: u32 = reg_bit(16);

/// `DSB_CTRL_WAIT_SAFE_WINDOW` (intel_dsb_regs.h)
pub const DSB_CTRL_WAIT_SAFE_WINDOW: u32 = reg_bit(15);

/// `DSB_CTRL_NO_WAIT_VBLANK` (intel_dsb_regs.h)
pub const DSB_CTRL_NO_WAIT_VBLANK: u32 = reg_bit(14);

/// `DSB_INST_WAIT_SAFE_WINDOW` (intel_dsb_regs.h)
pub const DSB_INST_WAIT_SAFE_WINDOW: u32 = reg_bit(7);

/// `DSB_INST_NO_WAIT_VBLANK` (intel_dsb_regs.h)
pub const DSB_INST_NO_WAIT_VBLANK: u32 = reg_bit(6);

/// `DSB_MMIOGEN_DEWAKE_DIS_CHICKEN` (intel_dsb_regs.h)
pub const DSB_MMIOGEN_DEWAKE_DIS_CHICKEN: u32 = reg_bit(2);

/// `DSB_DISABLE_MMIO_COUNT_FOR_INDEXED` (intel_dsb_regs.h)
pub const DSB_DISABLE_MMIO_COUNT_FOR_INDEXED: u32 = reg_bit(0);
