// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux v7.2.3 SKL watermark prefill accounting (`skl_prefill.c`).
#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::{
    intel_display_types_upstream::IntelCrtcState, skl_prefill_types_upstream::SklPrefillCtx,
};

const Q16_ONE: u32 = 1 << 16;

unsafe extern "C" {
    fn skl_wm0_prefill_lines_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_wm0_prefill_lines(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_1st_prefill_lines_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_2nd_prefill_lines_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_1st_prefill_adjustment_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_2nd_prefill_adjustment_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_1st_prefill_lines(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_2nd_prefill_lines(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_1st_prefill_adjustment(crtc_state: *const IntelCrtcState) -> u32;
    fn skl_scaler_2nd_prefill_adjustment(crtc_state: *const IntelCrtcState) -> u32;
    fn intel_cdclk_prefill_adjustment(crtc_state: *const IntelCrtcState) -> u32;
    fn intel_cdclk_prefill_adjustment_worst(crtc_state: *const IntelCrtcState) -> u32;
    fn intel_cdclk_min_cdclk_for_prefill(
        crtc_state: *const IntelCrtcState,
        prefill_lines_unadjusted: u32,
        prefill_lines_available: u32,
    ) -> i32;
    fn intel_crtc_vblank_length(crtc_state: *const IntelCrtcState) -> i32;
}

#[inline]
fn div_round_up_u64(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    numerator
        .saturating_add(denominator - 1)
        .checked_div(denominator)
        .unwrap_or(0)
}

/// Convert microseconds to Q16 scanlines with the Linux pixel-clock formula.
fn usecs_to_lines(clock_khz: u32, htotal: u32, usecs: u32) -> u32 {
    if htotal == 0 {
        return 0;
    }
    let numerator = u64::from(clock_khz).saturating_mul(u64::from(usecs) << 16);
    let denominator = u64::from(htotal).saturating_mul(1000);
    div_round_up_u64(numerator, denominator).min(u64::from(u32::MAX)) as u32
}

/// Linux `prefill_adjust()`: multiply a Q16 line count by a Q16 factor,
/// rounding up so latency reservations never underestimate scanlines.
fn prefill_adjust(value: u32, factor: u32) -> u32 {
    div_round_up_u64(u64::from(value) * u64::from(factor), u64::from(Q16_ONE))
        .min(u64::from(u32::MAX)) as u32
}

#[inline]
fn prefill_lines_nocdclk(ctx: &SklPrefillCtx) -> u32 {
    let mut lines = ctx.prefill.dsc;
    lines = prefill_adjust(lines, ctx.adj.scaler_2nd);
    lines = lines.saturating_add(ctx.prefill.scaler_2nd);
    lines = prefill_adjust(lines, ctx.adj.scaler_1st);
    lines
        .saturating_add(ctx.prefill.scaler_1st)
        .saturating_add(ctx.prefill.wm0)
}

#[inline]
fn prefill_lines_cdclk(ctx: &SklPrefillCtx) -> u32 {
    prefill_adjust(prefill_lines_nocdclk(ctx), ctx.adj.cdclk)
}

#[inline]
fn prefill_lines_full(ctx: &SklPrefillCtx) -> u32 {
    ctx.prefill.fixed.saturating_add(prefill_lines_cdclk(ctx))
}

unsafe fn init_base(ctx: *mut SklPrefillCtx, crtc_state: *const IntelCrtcState) {
    if ctx.is_null() || crtc_state.is_null() {
        return;
    }
    let state = unsafe { &*crtc_state };
    let mode = &state.hw.pipe_mode;
    unsafe { core::ptr::write_bytes(ctx, 0, 1) };
    let fixed = u32::from(state.framestart_delay).saturating_mul(Q16_ONE);
    let translation_walks = usecs_to_lines(mode.crtc_clock as u32, mode.crtc_htotal as u32, 20);
    // Linux's `intel_vdsc_prefill_lines()` is a fixed 1.5 scanline reservation
    // when DSC is active; that helper is kept local until the optional VDSC
    // module is brought into the configured build.
    let dsc = if state.dsc.compression_enable {
        0x18000
    } else {
        0
    };
    unsafe {
        (*ctx).prefill.fixed = fixed.saturating_add(translation_walks);
        (*ctx).prefill.dsc = dsc;
    }
}

unsafe fn init_without_cdclk_worst(ctx: *mut SklPrefillCtx, crtc_state: *const IntelCrtcState) {
    unsafe { init_base(ctx, crtc_state) };
    if ctx.is_null() || crtc_state.is_null() {
        return;
    }
    unsafe {
        (*ctx).prefill.wm0 = skl_wm0_prefill_lines_worst(crtc_state);
        (*ctx).prefill.scaler_1st = skl_scaler_1st_prefill_lines_worst(crtc_state);
        (*ctx).prefill.scaler_2nd = skl_scaler_2nd_prefill_lines_worst(crtc_state);
        (*ctx).adj.scaler_1st = skl_scaler_1st_prefill_adjustment_worst(crtc_state);
        (*ctx).adj.scaler_2nd = skl_scaler_2nd_prefill_adjustment_worst(crtc_state);
    }
}

unsafe fn init_without_cdclk(ctx: *mut SklPrefillCtx, crtc_state: *const IntelCrtcState) {
    unsafe { init_base(ctx, crtc_state) };
    if ctx.is_null() || crtc_state.is_null() {
        return;
    }
    unsafe {
        (*ctx).prefill.wm0 = skl_wm0_prefill_lines(crtc_state);
        (*ctx).prefill.scaler_1st = skl_scaler_1st_prefill_lines(crtc_state);
        (*ctx).prefill.scaler_2nd = skl_scaler_2nd_prefill_lines(crtc_state);
        (*ctx).adj.scaler_1st = skl_scaler_1st_prefill_adjustment(crtc_state);
        (*ctx).adj.scaler_2nd = skl_scaler_2nd_prefill_adjustment(crtc_state);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_prefill_init_worst(
    ctx: *mut SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
) {
    unsafe { init_without_cdclk_worst(ctx, crtc_state) };
    if ctx.is_null() || crtc_state.is_null() {
        return;
    }
    unsafe {
        (*ctx).adj.cdclk = intel_cdclk_prefill_adjustment_worst(crtc_state);
        (*ctx).prefill.full = prefill_lines_full(&*ctx);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_prefill_init(
    ctx: *mut SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
) {
    unsafe { init_without_cdclk(ctx, crtc_state) };
    if ctx.is_null() || crtc_state.is_null() {
        return;
    }
    unsafe {
        (*ctx).adj.cdclk = intel_cdclk_prefill_adjustment(crtc_state);
        (*ctx).prefill.full = prefill_lines_full(&*ctx);
    }
}

#[inline]
fn prefill_with_latency(
    ctx: &SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
    latency_us: u32,
) -> u32 {
    if crtc_state.is_null() {
        return u32::MAX;
    }
    let mode = unsafe { &(*crtc_state).hw.pipe_mode };
    ctx.prefill.full.saturating_add(usecs_to_lines(
        mode.crtc_clock as u32,
        mode.crtc_htotal as u32,
        latency_us,
    ))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_prefill_min_guardband(
    ctx: *const SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
    latency_us: u32,
) -> i32 {
    if ctx.is_null() || crtc_state.is_null() {
        return 0;
    }
    let prefill = prefill_with_latency(unsafe { &*ctx }, crtc_state, latency_us);
    div_round_up_u64(u64::from(prefill), u64::from(Q16_ONE)).min(i32::MAX as u64) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_prefill_vblank_too_short(
    ctx: *const SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
    latency_us: u32,
) -> bool {
    if ctx.is_null() || crtc_state.is_null() {
        return true;
    }
    let guardband = unsafe { intel_crtc_vblank_length(crtc_state) }.max(0) as u32;
    (guardband << 16) < prefill_with_latency(unsafe { &*ctx }, crtc_state, latency_us)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn skl_prefill_min_cdclk(
    ctx: *const SklPrefillCtx,
    crtc_state: *const IntelCrtcState,
) -> i32 {
    if ctx.is_null() || crtc_state.is_null() {
        return 0;
    }
    let ctx = unsafe { &*ctx };
    let unadjusted = prefill_lines_nocdclk(ctx);
    let guardband = unsafe { intel_crtc_vblank_length(crtc_state) }.max(0) as u32;
    let available = (guardband << 16).saturating_sub(ctx.prefill.fixed);
    unsafe { intel_cdclk_min_cdclk_for_prefill(crtc_state, unadjusted, available) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usec_conversion_uses_rounded_up_q16_scanlines() {
        assert_eq!(usecs_to_lines(148_500, 2_200, 20), 88_474);
        assert_eq!(usecs_to_lines(148_500, 0, 20), 0);
    }

    #[test]
    fn prefill_adjustment_rounds_up_at_q16_boundary() {
        assert_eq!(prefill_adjust(0x10001, 0x8000), 0x8001);
        assert_eq!(prefill_adjust(0x20000, 0x10000), 0x20000);
    }

    #[test]
    fn scaler_chain_adjustment_order_matches_linux() {
        let ctx = SklPrefillCtx {
            prefill: crate::skl_prefill_types_upstream::SklPrefillCtxPrefill {
                fixed: 0,
                wm0: 0x10000,
                scaler_1st: 0x20000,
                scaler_2nd: 0x30000,
                dsc: 0x10000,
                full: 0,
            },
            adj: crate::skl_prefill_types_upstream::SklPrefillCtxAdj {
                cdclk: 0x10000,
                scaler_1st: 0x20000,
                scaler_2nd: 0x30000,
            },
        };
        // ((DSC * scaler2) + scaler2 lines) * scaler1 + scaler1 + WM0.
        assert_eq!(prefill_lines_nocdclk(&ctx), 0x0f0000);
        assert_eq!(prefill_lines_full(&ctx), 0x0f0000);
    }
}
