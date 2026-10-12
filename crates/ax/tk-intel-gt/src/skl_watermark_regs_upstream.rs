// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! i915 display register definitions from `drivers/gpu/drm/i915/display/skl_watermark_regs.h`.
#![allow(non_snake_case, non_upper_case_globals, dead_code, unused_imports)]
// Macro bodies keep C's parenthesization; the redundant parens are expected.
#![allow(unused_parens, unused_braces)]

use crate::intel_workarounds_types_upstream::I915RegT;
use crate::intel_display_reg_defs_upstream::*;

/// `_PIPEA_MBUS_DBOX_CTL` (skl_watermark_regs.h)
pub const _PIPEA_MBUS_DBOX_CTL: u32 = 0x7003C;

/// `_PIPEB_MBUS_DBOX_CTL` (skl_watermark_regs.h)
pub const _PIPEB_MBUS_DBOX_CTL: u32 = 0x7103C;

/// `PIPE_MBUS_DBOX_CTL(pipe)` (skl_watermark_regs.h)
pub const fn PIPE_MBUS_DBOX_CTL(pipe: u32) -> I915RegT {
    _MMIO_PIPE(pipe, _PIPEA_MBUS_DBOX_CTL, _PIPEB_MBUS_DBOX_CTL)
}

/// `MBUS_DBOX_B2B_TRANSACTIONS_MAX_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_B2B_TRANSACTIONS_MAX_MASK: u32 = reg_genmask(24, 20);

/// `MBUS_DBOX_B2B_TRANSACTIONS_MAX(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_B2B_TRANSACTIONS_MAX(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_B2B_TRANSACTIONS_MAX_MASK, x)
}

/// `MBUS_DBOX_B2B_TRANSACTIONS_DELAY_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_B2B_TRANSACTIONS_DELAY_MASK: u32 = reg_genmask(19, 17);

/// `MBUS_DBOX_B2B_TRANSACTIONS_DELAY(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_B2B_TRANSACTIONS_DELAY(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_B2B_TRANSACTIONS_DELAY_MASK, x)
}

/// `MBUS_DBOX_REGULATE_B2B_TRANSACTIONS_EN` (skl_watermark_regs.h)
pub const MBUS_DBOX_REGULATE_B2B_TRANSACTIONS_EN: u32 = reg_bit(16);

/// `MBUS_DBOX_BW_CREDIT_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_BW_CREDIT_MASK: u32 = reg_genmask(15, 14);

/// `MBUS_DBOX_BW_CREDIT(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_BW_CREDIT(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_BW_CREDIT_MASK, x)
}

/// `MBUS_DBOX_BW_4CREDITS_MTL` (skl_watermark_regs.h)
pub const MBUS_DBOX_BW_4CREDITS_MTL: u32 = reg_field_prep(MBUS_DBOX_BW_CREDIT_MASK, 0x2);

/// `MBUS_DBOX_BW_8CREDITS_MTL` (skl_watermark_regs.h)
pub const MBUS_DBOX_BW_8CREDITS_MTL: u32 = reg_field_prep(MBUS_DBOX_BW_CREDIT_MASK, 0x3);

/// `MBUS_DBOX_B_CREDIT_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_B_CREDIT_MASK: u32 = reg_genmask(12, 8);

/// `MBUS_DBOX_B_CREDIT(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_B_CREDIT(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_B_CREDIT_MASK, x)
}

/// `MBUS_DBOX_I_CREDIT_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_I_CREDIT_MASK: u32 = reg_genmask(7, 5);

/// `MBUS_DBOX_I_CREDIT(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_I_CREDIT(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_I_CREDIT_MASK, x)
}

/// `MBUS_DBOX_A_CREDIT_MASK` (skl_watermark_regs.h)
pub const MBUS_DBOX_A_CREDIT_MASK: u32 = reg_genmask(3, 0);

/// `MBUS_DBOX_A_CREDIT(x)` (skl_watermark_regs.h)
pub const fn MBUS_DBOX_A_CREDIT(x: u32) -> u32 {
    reg_field_prep(MBUS_DBOX_A_CREDIT_MASK, x)
}

/// `MBUS_UBOX_CTL` (skl_watermark_regs.h)
pub const MBUS_UBOX_CTL: I915RegT = mmio(0x4503C);

/// `MBUS_BBOX_CTL_S1` (skl_watermark_regs.h)
pub const MBUS_BBOX_CTL_S1: I915RegT = mmio(0x45040);

/// `MBUS_BBOX_CTL_S2` (skl_watermark_regs.h)
pub const MBUS_BBOX_CTL_S2: I915RegT = mmio(0x45044);

/// `MBUS_CTL` (skl_watermark_regs.h)
pub const MBUS_CTL: I915RegT = mmio(0x4438C);

/// `MBUS_JOIN` (skl_watermark_regs.h)
pub const MBUS_JOIN: u32 = reg_bit(31);

/// `MBUS_HASHING_MODE_MASK` (skl_watermark_regs.h)
pub const MBUS_HASHING_MODE_MASK: u32 = reg_bit(30);

/// `MBUS_HASHING_MODE_2x2` (skl_watermark_regs.h)
pub const MBUS_HASHING_MODE_2x2: u32 = reg_field_prep(MBUS_HASHING_MODE_MASK, 0);

/// `MBUS_HASHING_MODE_1x4` (skl_watermark_regs.h)
pub const MBUS_HASHING_MODE_1x4: u32 = reg_field_prep(MBUS_HASHING_MODE_MASK, 1);

/// `MBUS_JOIN_PIPE_SELECT_MASK` (skl_watermark_regs.h)
pub const MBUS_JOIN_PIPE_SELECT_MASK: u32 = reg_genmask(28, 26);

/// `MBUS_JOIN_PIPE_SELECT(pipe)` (skl_watermark_regs.h)
pub const fn MBUS_JOIN_PIPE_SELECT(pipe: u32) -> u32 {
    reg_field_prep(MBUS_JOIN_PIPE_SELECT_MASK, pipe)
}

/// `MBUS_JOIN_PIPE_SELECT_NONE` (skl_watermark_regs.h)
pub const MBUS_JOIN_PIPE_SELECT_NONE: u32 = MBUS_JOIN_PIPE_SELECT(7);

/// `XE3P_MBUS_TRANSLATION_THROTTLE_MIN_MASK` (skl_watermark_regs.h)
pub const XE3P_MBUS_TRANSLATION_THROTTLE_MIN_MASK: u32 = reg_genmask(16, 13);

/// `XE3P_MBUS_TRANSLATION_THROTTLE_MIN(val)` (skl_watermark_regs.h)
pub const fn XE3P_MBUS_TRANSLATION_THROTTLE_MIN(val: u32) -> u32 {
    reg_field_prep(XE3P_MBUS_TRANSLATION_THROTTLE_MIN_MASK, val)
}

/// `MBUS_TRANSLATION_THROTTLE_MIN_MASK` (skl_watermark_regs.h)
pub const MBUS_TRANSLATION_THROTTLE_MIN_MASK: u32 = reg_genmask(15, 13);

/// `MBUS_TRANSLATION_THROTTLE_MIN(val)` (skl_watermark_regs.h)
pub const fn MBUS_TRANSLATION_THROTTLE_MIN(val: u32) -> u32 {
    reg_field_prep(MBUS_TRANSLATION_THROTTLE_MIN_MASK, val)
}

/// `_DBUF_CTL_S0` (skl_watermark_regs.h)
pub const _DBUF_CTL_S0: u32 = 0x45008;

/// `_DBUF_CTL_S1` (skl_watermark_regs.h)
pub const _DBUF_CTL_S1: u32 = 0x44FE8;

/// `_DBUF_CTL_S2` (skl_watermark_regs.h)
pub const _DBUF_CTL_S2: u32 = 0x44300;

/// `_DBUF_CTL_S3` (skl_watermark_regs.h)
pub const _DBUF_CTL_S3: u32 = 0x44304;

/// `DBUF_CTL_S(slice)` (skl_watermark_regs.h)
pub const fn DBUF_CTL_S(slice: u32) -> I915RegT {
    mmio(pick(slice, &[_DBUF_CTL_S0, _DBUF_CTL_S1, _DBUF_CTL_S2, _DBUF_CTL_S3]))
}

/// `DBUF_POWER_REQUEST` (skl_watermark_regs.h)
pub const DBUF_POWER_REQUEST: u32 = reg_bit(31);

/// `DBUF_POWER_STATE` (skl_watermark_regs.h)
pub const DBUF_POWER_STATE: u32 = reg_bit(30);

/// `DBUF_TRACKER_STATE_SERVICE_MASK` (skl_watermark_regs.h)
pub const DBUF_TRACKER_STATE_SERVICE_MASK: u32 = reg_genmask(23, 19);

/// `DBUF_TRACKER_STATE_SERVICE(x)` (skl_watermark_regs.h)
pub const fn DBUF_TRACKER_STATE_SERVICE(x: u32) -> u32 {
    reg_field_prep(DBUF_TRACKER_STATE_SERVICE_MASK, x)
}

/// `XE3P_DBUF_MIN_TRACKER_STATE_SERVICE_MASK` (skl_watermark_regs.h)
pub const XE3P_DBUF_MIN_TRACKER_STATE_SERVICE_MASK: u32 = reg_genmask(20, 16);

/// `XE3P_DBUF_MIN_TRACKER_STATE_SERVICE(x)` (skl_watermark_regs.h)
pub const fn XE3P_DBUF_MIN_TRACKER_STATE_SERVICE(x: u32) -> u32 {
    reg_field_prep(XE3P_DBUF_MIN_TRACKER_STATE_SERVICE_MASK, x)
}

/// `DBUF_MIN_TRACKER_STATE_SERVICE_MASK` (skl_watermark_regs.h)
pub const DBUF_MIN_TRACKER_STATE_SERVICE_MASK: u32 = reg_genmask(18, 16);

/// `DBUF_MIN_TRACKER_STATE_SERVICE(x)` (skl_watermark_regs.h)
pub const fn DBUF_MIN_TRACKER_STATE_SERVICE(x: u32) -> u32 {
    reg_field_prep(DBUF_MIN_TRACKER_STATE_SERVICE_MASK, x)
}

/// `MTL_LATENCY_LP0_LP1` (skl_watermark_regs.h)
pub const MTL_LATENCY_LP0_LP1: I915RegT = mmio(0x45780);

/// `MTL_LATENCY_LP2_LP3` (skl_watermark_regs.h)
pub const MTL_LATENCY_LP2_LP3: I915RegT = mmio(0x45784);

/// `MTL_LATENCY_LP4_LP5` (skl_watermark_regs.h)
pub const MTL_LATENCY_LP4_LP5: I915RegT = mmio(0x45788);

/// `MTL_LATENCY_LEVEL_EVEN_MASK` (skl_watermark_regs.h)
pub const MTL_LATENCY_LEVEL_EVEN_MASK: u32 = reg_genmask(12, 0);

/// `MTL_LATENCY_LEVEL_ODD_MASK` (skl_watermark_regs.h)
pub const MTL_LATENCY_LEVEL_ODD_MASK: u32 = reg_genmask(28, 16);

/// `MTL_LATENCY_SAGV` (skl_watermark_regs.h)
pub const MTL_LATENCY_SAGV: I915RegT = mmio(0x4578c);

/// `MTL_LATENCY_QCLK_SAGV` (skl_watermark_regs.h)
pub const MTL_LATENCY_QCLK_SAGV: u32 = reg_genmask(12, 0);

/// `LNL_PKG_C_LATENCY` (skl_watermark_regs.h)
pub const LNL_PKG_C_LATENCY: I915RegT = mmio(0x46460);

/// `LNL_ADDED_WAKE_TIME_MASK` (skl_watermark_regs.h)
pub const LNL_ADDED_WAKE_TIME_MASK: u32 = reg_genmask(28, 16);

/// `LNL_PKG_C_LATENCY_MASK` (skl_watermark_regs.h)
pub const LNL_PKG_C_LATENCY_MASK: u32 = reg_genmask(12, 0);
