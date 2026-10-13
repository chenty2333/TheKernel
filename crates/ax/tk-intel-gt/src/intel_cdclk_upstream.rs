// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/gpu/drm/i915/display/intel_cdclk.c (Linux v7.2.3).
//
// The ADL-P/TGL CDCLK path is shared across the BXT-family PLL implementation.
// Legacy platform implementations are intentionally not selected here; this
// port's configured display target is ADL-P (including RPL-U subplatforms).
#![allow(non_snake_case, dead_code, unsafe_code, clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::ptr;

use crate::intel_cdclk_types_upstream::IntelCdclkConfig;
use crate::intel_display_core_types_upstream::IntelDisplay;
use crate::intel_display_limits_types_upstream::{I915_MAX_PIPES, INVALID_PIPE, Pipe};
use crate::intel_display_regs_upstream::*;
use crate::intel_display_wa_upstream::{
    __intel_display_wa, DISPLAY_VER, INTEL_PCH_TYPE, IntelDisplayWa,
    platform_alderlake_p, platform_dg2,
};
use crate::intel_global_state_types_upstream::{
    IntelGlobalObj, IntelGlobalState, IntelGlobalStateFuncs,
};
use crate::intel_display_types_upstream::{IntelAtomicState, IntelCrtc};
use crate::linux_i915_private::DrmI915Private;
use crate::intel_pch_types_upstream::{PCH_CNP, PCH_DG1, PCH_ICP, PCH_MTL};
use crate::intel_de_upstream::{
    intel_de_read, intel_de_rmw, intel_de_wait_for_clear_ms, intel_de_wait_for_set_ms,
    intel_de_write,
};
use crate::intel_parent_upstream::{
    intel_parent_pcode_request, intel_parent_pcode_write, intel_parent_pcode_write_timeout,
};

const SKL_PCODE_CDCLK_CONTROL: u32 = 0x7;
const SKL_CDCLK_PREPARE_FOR_CHANGE: u32 = 0x3;
const SKL_CDCLK_READY_FOR_CHANGE: u32 = 0x1;
const HSW_PCODE_DE_WRITE_FREQ_REQ: u32 = 0x17;
const CDCLK_SQUASH_LEN: i32 = 16;

unsafe extern "C" {
    // Implemented by B3d global-state code and B4e atomic code, respectively.
    fn intel_atomic_get_global_obj_state(
        state: *mut IntelAtomicState,
        obj: *mut IntelGlobalObj,
    ) -> *mut IntelGlobalState;
    fn intel_atomic_lock_global_state(state: *mut IntelGlobalState) -> i32;
    fn intel_any_crtc_needs_modeset(state: *mut IntelAtomicState) -> bool;
    fn intel_crtc_wait_for_next_vblank(crtc: *mut IntelCrtc);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct IntelCdclkVals {
    refclk: u32,
    cdclk: u32,
    ratio: u32,
    waveform: u16,
    _pad: [u8; 2],
}

#[repr(C)]
struct IntelCdclkFuncs {
    get_cdclk: Option<unsafe extern "C" fn(*mut IntelDisplay, *mut IntelCdclkConfig)>,
    set_cdclk: Option<unsafe extern "C" fn(*mut IntelDisplay, *const IntelCdclkConfig, Pipe)>,
    calc_voltage_level: Option<unsafe extern "C" fn(i32) -> u8>,
}

#[repr(C)]
struct IntelCdclkState {
    base: IntelGlobalState,
    logical: IntelCdclkConfig,
    actual: IntelCdclkConfig,
    dbuf_bw_min_cdclk: i32,
    min_cdclk: [i32; I915_MAX_PIPES as usize],
    min_voltage_level: [u8; I915_MAX_PIPES as usize],
    pipe: Pipe,
    force_min_cdclk: i32,
    enabled_pipes: u8,
    active_pipes: u8,
    disable_pipes: bool,
}

const ADLP_A_STEP_CDCLK_TABLE: [IntelCdclkVals; 10] = [
    vals(19_200, 307_200, 32),
    vals(19_200, 556_800, 58),
    vals(19_200, 652_800, 68),
    vals(24_000, 312_000, 26),
    vals(24_000, 552_000, 46),
    // Keep the upstream v7.2.3 entry exactly: it is spelled 24400 there.
    vals(24_400, 648_000, 54),
    vals(38_400, 307_200, 16),
    vals(38_400, 556_800, 29),
    vals(38_400, 652_800, 34),
    IntelCdclkVals { refclk: 0, ..IntelCdclkVals::ZERO },
];

const ADLP_CDCLK_TABLE: [IntelCdclkVals; 16] = [
    vals(19_200, 172_800, 27), vals(19_200, 192_000, 20),
    vals(19_200, 307_200, 32), vals(19_200, 556_800, 58),
    vals(19_200, 652_800, 68), vals(24_000, 176_000, 22),
    vals(24_000, 192_000, 16), vals(24_000, 312_000, 26),
    vals(24_000, 552_000, 46), vals(24_000, 648_000, 54),
    vals(38_400, 179_200, 14), vals(38_400, 192_000, 10),
    vals(38_400, 307_200, 16), vals(38_400, 556_800, 29),
    vals(38_400, 652_800, 34),
    IntelCdclkVals { refclk: 0, ..IntelCdclkVals::ZERO },
];

const RPLU_CDCLK_TABLE: [IntelCdclkVals; 19] = [
    vals(19_200, 172_800, 27), vals(19_200, 192_000, 20),
    vals(19_200, 307_200, 32), vals(19_200, 480_000, 50),
    vals(19_200, 556_800, 58), vals(19_200, 652_800, 68),
    vals(24_000, 176_000, 22), vals(24_000, 192_000, 16),
    vals(24_000, 312_000, 26), vals(24_000, 480_000, 40),
    vals(24_000, 552_000, 46), vals(24_000, 648_000, 54),
    vals(38_400, 179_200, 14), vals(38_400, 192_000, 10),
    vals(38_400, 307_200, 16), vals(38_400, 480_000, 25),
    vals(38_400, 556_800, 29), vals(38_400, 652_800, 34),
    IntelCdclkVals { refclk: 0, ..IntelCdclkVals::ZERO },
];

impl IntelCdclkVals {
    const ZERO: Self = Self { refclk: 0, cdclk: 0, ratio: 0, waveform: 0, _pad: [0; 2] };
}

const fn vals(refclk: u32, cdclk: u32, ratio: u32) -> IntelCdclkVals {
    IntelCdclkVals { refclk, cdclk, ratio, waveform: 0, _pad: [0; 2] }
}

unsafe extern "C" fn tgl_calc_voltage_level(cdclk: i32) -> u8 {
    tgl_voltage_level(cdclk)
}

unsafe extern "C" fn rplu_calc_voltage_level(cdclk: i32) -> u8 {
    rplu_voltage_level(cdclk)
}

fn tgl_voltage_level(cdclk: i32) -> u8 {
    voltage_level(cdclk, &[312_000, 326_400, 556_800, 652_800])
}

fn rplu_voltage_level(cdclk: i32) -> u8 {
    voltage_level(cdclk, &[312_000, 480_000, 556_800, 652_800])
}

fn voltage_level(cdclk: i32, max: &[i32]) -> u8 {
    max.iter().position(|limit| cdclk <= *limit).unwrap_or(max.len() - 1) as u8
}

static TGL_CDCLK_FUNCS: IntelCdclkFuncs = IntelCdclkFuncs {
    get_cdclk: Some(bxt_get_cdclk),
    set_cdclk: Some(bxt_set_cdclk),
    calc_voltage_level: Some(tgl_calc_voltage_level),
};
static RPLU_CDCLK_FUNCS: IntelCdclkFuncs = IntelCdclkFuncs {
    get_cdclk: Some(bxt_get_cdclk),
    set_cdclk: Some(bxt_set_cdclk),
    calc_voltage_level: Some(rplu_calc_voltage_level),
};

#[inline]
unsafe fn display_feature(display: *const IntelDisplay, bit: u8) -> bool {
    let info = (*display).info.__device_info;
    !info.is_null() && ((*info)._bitfields_46[0] & (1 << bit)) != 0
}

#[inline]
unsafe fn has_cdclk_crawl(display: *const IntelDisplay) -> bool {
    display_feature(display, 1)
}

#[inline]
unsafe fn has_cdclk_squash(display: *const IntelDisplay) -> bool {
    display_feature(display, 2)
}

#[inline]
unsafe fn table(display: *const IntelDisplay) -> &'static [IntelCdclkVals] {
    let ptr = (*display).cdclk.table;
    if ptr == ADLP_A_STEP_CDCLK_TABLE.as_ptr() as *mut c_void {
        &ADLP_A_STEP_CDCLK_TABLE
    } else if ptr == RPLU_CDCLK_TABLE.as_ptr() as *mut c_void {
        &RPLU_CDCLK_TABLE
    } else if ptr == ADLP_CDCLK_TABLE.as_ptr() as *mut c_void {
        &ADLP_CDCLK_TABLE
    } else {
        &[]
    }
}

#[inline]
fn table_entries(entries: &[IntelCdclkVals]) -> impl Iterator<Item = &IntelCdclkVals> {
    entries.iter().take_while(|entry| entry.refclk != 0)
}

fn cdclk_table_calc(entries: &[IntelCdclkVals], refclk: u32, min_cdclk: i32) -> Option<u32> {
    table_entries(entries)
        .find(|entry| entry.refclk == refclk && entry.cdclk >= min_cdclk as u32)
        .map(|entry| entry.cdclk)
}

fn cdclk_table_vco(entries: &[IntelCdclkVals], refclk: u32, cdclk: u32) -> Option<u32> {
    table_entries(entries)
        .find(|entry| entry.refclk == refclk && entry.cdclk == cdclk)
        .map(|entry| refclk.saturating_mul(entry.ratio))
}

fn round_closest(numerator: u32, denominator: u32) -> u32 {
    if denominator == 0 { 0 } else { (numerator + denominator / 2) / denominator }
}

// upstream: intel_cdclk.c bxt_calc_cdclk()
unsafe fn bxt_calc_cdclk(display: *mut IntelDisplay, min_cdclk: i32) -> u32 {
    let config = &(*display).cdclk.hw;
    cdclk_table_calc(table(display), config.r#ref, min_cdclk).unwrap_or_else(|| {
        drm_WARN_ON!((*display).drm, true);
        (*display).cdclk.max_cdclk_freq
    })
}

// upstream: intel_cdclk.c bxt_calc_cdclk_pll_vco()
unsafe fn bxt_calc_cdclk_pll_vco(display: *mut IntelDisplay, cdclk: u32) -> u32 {
    let config = &(*display).cdclk.hw;
    if cdclk == config.bypass { return 0; }
    cdclk_table_vco(table(display), config.r#ref, cdclk).unwrap_or_else(|| {
        drm_WARN_ON!((*display).drm, true);
        0
    })
}

// upstream: intel_cdclk.c intel_cdclk_calc_voltage_level()
unsafe fn intel_cdclk_calc_voltage_level(display: *mut IntelDisplay, cdclk: u32) -> u8 {
    let funcs = (*display).cdclk.funcs.cast::<IntelCdclkFuncs>();
    if funcs.is_null() { return 0; }
    (*funcs).calc_voltage_level.map(|f| f(cdclk as i32)).unwrap_or(0)
}

// upstream: intel_cdclk.c icl_readout_refclk()
unsafe fn icl_readout_refclk(display: *mut IntelDisplay, config: *mut IntelCdclkConfig) {
    let dssm = intel_de_read(display, SKL_DSSM) & ICL_DSSM_CDCLK_PLL_REFCLK_MASK;
    (*config).r#ref = match dssm {
        ICL_DSSM_CDCLK_PLL_REFCLK_19_2MHz => 19_200,
        ICL_DSSM_CDCLK_PLL_REFCLK_38_4MHz => 38_400,
        _ => 24_000,
    };
}

// upstream: intel_cdclk.c bxt_de_pll_readout()
unsafe fn bxt_de_pll_readout(display: *mut IntelDisplay, config: *mut IntelCdclkConfig) {
    if platform_dg2(display) {
        (*config).r#ref = 38_400;
    } else if DISPLAY_VER(display) >= 11 {
        icl_readout_refclk(display, config);
    } else {
        (*config).r#ref = 19_200;
    }

    let val = intel_de_read(display, BXT_DE_PLL_ENABLE);
    if val & (BXT_DE_PLL_PLL_ENABLE | BXT_DE_PLL_LOCK)
        != (BXT_DE_PLL_PLL_ENABLE | BXT_DE_PLL_LOCK)
    {
        (*config).vco = 0;
        return;
    }

    let ratio = if DISPLAY_VER(display) >= 11 {
        val & ICL_CDCLK_PLL_RATIO_MASK
    } else {
        intel_de_read(display, BXT_DE_PLL_CTL) & BXT_DE_PLL_RATIO_MASK
    };
    (*config).vco = ratio.saturating_mul((*config).r#ref);
}

// upstream: intel_cdclk.c bxt_get_cdclk()
unsafe extern "C" fn bxt_get_cdclk(display: *mut IntelDisplay, config: *mut IntelCdclkConfig) {
    if display.is_null() || config.is_null() { return; }
    bxt_de_pll_readout(display, config);
    (*config).bypass = if DISPLAY_VER(display) >= 12 {
        (*config).r#ref / 2
    } else if DISPLAY_VER(display) >= 11 {
        50_000
    } else {
        (*config).r#ref
    };

    if (*config).vco == 0 {
        (*config).cdclk = (*config).bypass;
    } else {
        let divider = intel_de_read(display, CDCLK_CTL) & BXT_CDCLK_CD2X_DIV_SEL_MASK;
        let div = match divider {
            BXT_CDCLK_CD2X_DIV_SEL_1 => 2,
            BXT_CDCLK_CD2X_DIV_SEL_1_5 => 3,
            BXT_CDCLK_CD2X_DIV_SEL_2 => 4,
            BXT_CDCLK_CD2X_DIV_SEL_4 => 8,
            _ => {
                drm_WARN_ON!((*display).drm, true);
                (*config).cdclk = (*config).bypass;
                (*config).voltage_level = intel_cdclk_calc_voltage_level(display, (*config).cdclk);
                return;
            }
        };
        if has_cdclk_squash(display) {
            let squash = intel_de_read(display, CDCLK_SQUASH_CTL);
            if squash & CDCLK_SQUASH_ENABLE != 0 {
                let size = ((squash & CDCLK_SQUASH_WINDOW_SIZE_MASK) >> 24) + 1;
                let waveform = (squash & CDCLK_SQUASH_WAVEFORM_MASK) >> (16 - size);
                (*config).cdclk = round_closest(
                    (waveform.count_ones() * (*config).vco) as u32,
                    size * div,
                );
            } else {
                (*config).cdclk = round_closest((*config).vco, div);
            }
        } else {
            (*config).cdclk = round_closest((*config).vco, div);
        }
    }
    (*config).voltage_level = intel_cdclk_calc_voltage_level(display, (*config).cdclk);
}

// upstream: intel_cdclk.c intel_cdclk_get_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_get_cdclk(
    display: *mut IntelDisplay,
    config: *mut IntelCdclkConfig,
) {
    if display.is_null() || config.is_null() { return; }
    let funcs = (*display).cdclk.funcs.cast::<IntelCdclkFuncs>();
    if !funcs.is_null() {
        if let Some(get) = (*funcs).get_cdclk { get(display, config); }
    }
}

fn cdclk_squash_divider(waveform: u16) -> u32 {
    if waveform == 0 { u16::MAX.count_ones() } else { waveform.count_ones() }
}

fn cdclk_divider(cdclk: u32, vco: u32, waveform: u16) -> u32 {
    round_closest(
        vco.saturating_mul(cdclk_squash_divider(waveform)),
        cdclk.saturating_mul(CDCLK_SQUASH_LEN as u32),
    )
}

unsafe fn cdclk_squash_waveform(display: *mut IntelDisplay, cdclk: u32) -> u16 {
    if cdclk == (*display).cdclk.hw.bypass { return 0; }
    table_entries(table(display))
        .find(|entry| entry.refclk == (*display).cdclk.hw.r#ref && entry.cdclk == cdclk)
        .map(|entry| entry.waveform)
        .unwrap_or(u16::MAX)
}

unsafe fn cdclk_pll_is_unknown(vco: u32) -> bool { vco == u32::MAX }

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CdclkTransitionStep { PreparePcode, UpdatePll, ProgramCdclk, SetVoltage, Readback }

fn transition_order() -> [CdclkTransitionStep; 5] {
    [
        CdclkTransitionStep::PreparePcode,
        CdclkTransitionStep::UpdatePll,
        CdclkTransitionStep::ProgramCdclk,
        CdclkTransitionStep::SetVoltage,
        CdclkTransitionStep::Readback,
    ]
}

// upstream: intel_cdclk.c adlp_cdclk_pll_crawl()
unsafe fn adlp_cdclk_pll_crawl(display: *mut IntelDisplay, vco: u32) {
    let ratio = round_closest(vco, (*display).cdclk.hw.r#ref);
    let mut val = ICL_CDCLK_PLL_RATIO(ratio) | BXT_DE_PLL_PLL_ENABLE;
    intel_de_write(display, BXT_DE_PLL_ENABLE, val);
    val |= BXT_DE_PLL_FREQ_REQ;
    intel_de_write(display, BXT_DE_PLL_ENABLE, val);
    if intel_de_wait_for_set_ms(
        display,
        BXT_DE_PLL_ENABLE,
        BXT_DE_PLL_LOCK | BXT_DE_PLL_FREQ_REQ_ACK,
        1,
    ) != 0 {
        drm_err!(&(*display).drm, "timeout waiting for FREQ change request ack\n");
    }
    val &= !BXT_DE_PLL_FREQ_REQ;
    intel_de_write(display, BXT_DE_PLL_ENABLE, val);
    (*display).cdclk.hw.vco = vco;
}

unsafe fn icl_cdclk_pll_disable(display: *mut IntelDisplay) {
    // Wa_13012396614 changes MDCLK to CD2XCLK before disabling the PLL.
    if __intel_display_wa(display, IntelDisplayWa::Wa13012396614, ptr::null()) {
        intel_de_rmw(display, CDCLK_CTL, MDCLK_SOURCE_SEL_MASK, MDCLK_SOURCE_SEL_CD2XCLK);
    }
    intel_de_rmw(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_PLL_ENABLE, 0);
    if intel_de_wait_for_clear_ms(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_LOCK, 1) != 0 {
        drm_err!(&(*display).drm, "timeout waiting for CDCLK PLL unlock\n");
    }
    (*display).cdclk.hw.vco = 0;
}

unsafe fn icl_cdclk_pll_enable(display: *mut IntelDisplay, vco: u32) {
    let ratio = round_closest(vco, (*display).cdclk.hw.r#ref);
    let mut val = ICL_CDCLK_PLL_RATIO(ratio);
    intel_de_write(display, BXT_DE_PLL_ENABLE, val);
    val |= BXT_DE_PLL_PLL_ENABLE;
    intel_de_write(display, BXT_DE_PLL_ENABLE, val);
    if intel_de_wait_for_set_ms(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_LOCK, 1) != 0 {
        drm_err!(&(*display).drm, "timeout waiting for CDCLK PLL lock\n");
    }
    (*display).cdclk.hw.vco = vco;
}

unsafe fn icl_cdclk_pll_update(display: *mut IntelDisplay, vco: u32) {
    if (*display).cdclk.hw.vco != 0 && (*display).cdclk.hw.vco != vco {
        icl_cdclk_pll_disable(display);
    }
    if (*display).cdclk.hw.vco != vco {
        icl_cdclk_pll_enable(display, vco);
    }
}

unsafe fn bxt_cdclk_pll_disable(display: *mut IntelDisplay) {
    intel_de_write(display, BXT_DE_PLL_ENABLE, 0);
    if intel_de_wait_for_clear_ms(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_LOCK, 1) != 0 {
        drm_err!(&(*display).drm, "timeout waiting for DE PLL unlock\n");
    }
    (*display).cdclk.hw.vco = 0;
}

unsafe fn bxt_de_pll_enable(display: *mut IntelDisplay, vco: u32) {
    let ratio = round_closest(vco, (*display).cdclk.hw.r#ref);
    intel_de_rmw(display, BXT_DE_PLL_CTL, BXT_DE_PLL_RATIO_MASK, BXT_DE_PLL_RATIO(ratio));
    intel_de_write(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_PLL_ENABLE);
    if intel_de_wait_for_set_ms(display, BXT_DE_PLL_ENABLE, BXT_DE_PLL_LOCK, 1) != 0 {
        drm_err!(&(*display).drm, "timeout waiting for DE PLL lock\n");
    }
    (*display).cdclk.hw.vco = vco;
}

unsafe fn bxt_cdclk_pll_update(display: *mut IntelDisplay, vco: u32) {
    if (*display).cdclk.hw.vco != 0 && (*display).cdclk.hw.vco != vco {
        bxt_cdclk_pll_disable(display);
    }
    if (*display).cdclk.hw.vco != vco {
        bxt_de_pll_enable(display, vco);
    }
}

unsafe fn bxt_cdclk_cd2x_div_sel(display: *mut IntelDisplay, cdclk: u32, vco: u32) -> u32 {
    let divider = cdclk_divider(cdclk, vco, cdclk_squash_waveform(display, cdclk));
    match divider {
        2 => BXT_CDCLK_CD2X_DIV_SEL_1,
        3 => BXT_CDCLK_CD2X_DIV_SEL_1_5,
        4 => BXT_CDCLK_CD2X_DIV_SEL_2,
        8 => BXT_CDCLK_CD2X_DIV_SEL_4,
        _ => {
            drm_WARN_ON!((*display).drm, cdclk != (*display).cdclk.hw.bypass || vco != 0);
            BXT_CDCLK_CD2X_DIV_SEL_1
        }
    }
}

unsafe fn cdclk_ctl(display: *mut IntelDisplay, config: &IntelCdclkConfig, pipe: Pipe) -> u32 {
    let mut val = bxt_cdclk_cd2x_div_sel(display, config.cdclk, config.vco);
    if DISPLAY_VER(display) >= 12 {
        val |= if pipe == INVALID_PIPE { TGL_CDCLK_CD2X_PIPE_NONE } else { TGL_CDCLK_CD2X_PIPE(pipe as u32) };
    } else if DISPLAY_VER(display) >= 11 {
        val |= if pipe == INVALID_PIPE { ICL_CDCLK_CD2X_PIPE_NONE } else { ICL_CDCLK_CD2X_PIPE(pipe as u32) };
    } else {
        val |= if pipe == INVALID_PIPE { BXT_CDCLK_CD2X_PIPE_NONE } else { BXT_CDCLK_CD2X_PIPE(pipe as u32) };
    }
    if DISPLAY_VER(display) < 20 {
        val |= (((config.cdclk as i32 - 1000 + 250) / 500).max(0) as u32) & CDCLK_FREQ_DECIMAL_MASK;
    }
    val
}

unsafe fn cdclk_pipe_none(display: *mut IntelDisplay) -> u32 {
    if DISPLAY_VER(display) >= 12 {
        TGL_CDCLK_CD2X_PIPE_NONE
    } else if DISPLAY_VER(display) >= 11 {
        ICL_CDCLK_CD2X_PIPE_NONE
    } else {
        BXT_CDCLK_CD2X_PIPE_NONE
    }
}

unsafe fn bxt_update_pll(display: *mut IntelDisplay, config: &IntelCdclkConfig) {
    let current_vco = (*display).cdclk.hw.vco;
    if has_cdclk_crawl(display)
        && current_vco != 0
        && config.vco != 0
        && !cdclk_pll_is_unknown(current_vco)
    {
        if current_vco != config.vco { adlp_cdclk_pll_crawl(display, config.vco); }
    } else if DISPLAY_VER(display) >= 11 {
        icl_cdclk_pll_update(display, config.vco);
    } else {
        bxt_cdclk_pll_update(display, config.vco);
    }

}

unsafe fn bxt_program_cdclk(display: *mut IntelDisplay, config: &IntelCdclkConfig, pipe: Pipe) {
    if has_cdclk_squash(display) {
        let waveform = cdclk_squash_waveform(display, config.cdclk);
        let squash = if waveform != 0 {
            CDCLK_SQUASH_ENABLE | CDCLK_SQUASH_WINDOW_SIZE(15) | waveform as u32
        } else { 0 };
        intel_de_write(display, CDCLK_SQUASH_CTL, squash);
    }
    intel_de_write(display, CDCLK_CTL, cdclk_ctl(display, config, pipe));
    if pipe != INVALID_PIPE {
        let crtc = crate::display_pending_stubs::b4::intel_crtc_for_pipe(display, pipe);
        if !crtc.is_null() { intel_crtc_wait_for_next_vblank(crtc); }
    }
}

unsafe extern "C" fn bxt_set_cdclk(display: *mut IntelDisplay, config: *const IntelCdclkConfig, pipe: Pipe) {
    if display.is_null() || config.is_null() { return; }
    let config = &*config;
    let version = DISPLAY_VER(display);

    // Keep source ordering explicit. Tests below exercise this exact sequence.
    for step in transition_order() {
        match step {
            CdclkTransitionStep::PreparePcode => {
                // BSpec requires prepare/ready before any PLL or CDCLK writes.
                let ret = if version >= 14 || platform_dg2(display) {
                    0
                } else if version >= 11 {
                    intel_parent_pcode_request(
                        display, SKL_PCODE_CDCLK_CONTROL, SKL_CDCLK_PREPARE_FOR_CHANGE,
                        SKL_CDCLK_READY_FOR_CHANGE, SKL_CDCLK_READY_FOR_CHANGE, 3,
                    )
                } else {
                    intel_parent_pcode_write_timeout(
                        display, HSW_PCODE_DE_WRITE_FREQ_REQ, 0x8000_0000, 2,
                    )
                };
                if ret != 0 {
                    drm_err!(&(*display).drm, "Failed to inform PCU about cdclk change (err {}, freq {})\n", ret, config.cdclk);
                    return;
                }
            }
            CdclkTransitionStep::UpdatePll => bxt_update_pll(display, config),
            CdclkTransitionStep::ProgramCdclk => bxt_program_cdclk(display, config, pipe),
            CdclkTransitionStep::SetVoltage => {
                if version >= 11 && version < 14 && !platform_dg2(display) {
                    let ret = intel_parent_pcode_write(display, SKL_PCODE_CDCLK_CONTROL, config.voltage_level as u32);
                    if ret != 0 {
                        drm_err!(&(*display).drm, "PCode CDCLK freq set failed (err {}, freq {})\n", ret, config.cdclk);
                        return;
                    }
                } else if version < 11 {
                    let ret = intel_parent_pcode_write_timeout(display, HSW_PCODE_DE_WRITE_FREQ_REQ, config.voltage_level as u32, 2);
                    if ret != 0 {
                        drm_err!(&(*display).drm, "PCode CDCLK freq set failed (err {}, freq {})\n", ret, config.cdclk);
                        return;
                    }
                }
            }
            CdclkTransitionStep::Readback => {
                intel_update_cdclk(display);
                if version >= 11 { (*display).cdclk.hw.voltage_level = config.voltage_level; }
            }
        }
    }
}

// upstream: intel_cdclk.c intel_update_cdclk()
unsafe fn intel_update_cdclk(display: *mut IntelDisplay) {
    intel_cdclk_get_cdclk(display, ptr::addr_of_mut!((*display).cdclk.hw));
    if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 20) != 0
        || (*display).platform._anon_0_0.bitmap[0] & (1u64 << 27) != 0
    {
        intel_de_write(display, GMBUSFREQ_VLV, ((*display).cdclk.hw.cdclk + 999) / 1000);
    }
}

unsafe fn bxt_sanitize_cdclk(display: *mut IntelDisplay) {
    intel_update_cdclk(display);
    let hw = (*display).cdclk.hw;
    if hw.vco == 0 || hw.cdclk == hw.bypass {
        (*display).cdclk.hw.cdclk = 0;
        (*display).cdclk.hw.vco = u32::MAX;
        return;
    }
    if bxt_calc_cdclk(display, hw.cdclk as i32) != hw.cdclk
        || bxt_calc_cdclk_pll_vco(display, hw.cdclk) != hw.vco
    {
        (*display).cdclk.hw.cdclk = 0;
        (*display).cdclk.hw.vco = u32::MAX;
        return;
    }

    let mut cdctl = intel_de_read(display, CDCLK_CTL);
    let expected = cdclk_ctl(display, &hw, INVALID_PIPE);
    // Normalize pipe selection to PIPE_NONE. The firmware may have selected
    // an active pipe; Linux accepts either before applying this comparison.
    cdctl &= !cdclk_pipe_none(display);
    cdctl |= cdclk_pipe_none(display);
    if cdctl != expected {
        if DISPLAY_VER(display) < 20 {
            cdctl &= !CDCLK_FREQ_DECIMAL_MASK;
            cdctl |= expected & CDCLK_FREQ_DECIMAL_MASK;
        }
        if cdctl != expected {
            (*display).cdclk.hw.cdclk = 0;
            (*display).cdclk.hw.vco = u32::MAX;
            return;
        }
        intel_de_write(display, CDCLK_CTL, expected);
    }
}

unsafe fn bxt_cdclk_init_hw(display: *mut IntelDisplay) {
    bxt_sanitize_cdclk(display);
    if (*display).cdclk.hw.cdclk != 0 && (*display).cdclk.hw.vco != 0 { return; }
    let mut config = (*display).cdclk.hw;
    config.cdclk = bxt_calc_cdclk(display, 0);
    config.vco = bxt_calc_cdclk_pll_vco(display, config.cdclk);
    config.voltage_level = intel_cdclk_calc_voltage_level(display, config.cdclk);
    bxt_set_cdclk(display, &config, INVALID_PIPE);
}

unsafe fn bxt_cdclk_uninit_hw(display: *mut IntelDisplay) {
    let mut config = (*display).cdclk.hw;
    config.cdclk = config.bypass;
    config.vco = 0;
    config.voltage_level = intel_cdclk_calc_voltage_level(display, config.cdclk);
    bxt_set_cdclk(display, &config, INVALID_PIPE);
}

// upstream: intel_cdclk.c intel_cdclk_init_hw()/intel_cdclk_uninit_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_init_hw(display: *mut IntelDisplay) {
    if !display.is_null() && platform_alderlake_p(display) { bxt_cdclk_init_hw(display); }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_uninit_hw(display: *mut IntelDisplay) {
    if !display.is_null() && platform_alderlake_p(display) { bxt_cdclk_uninit_hw(display); }
}

// upstream: intel_cdclk.c intel_init_cdclk_hooks()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_init_cdclk_hooks(display: *mut IntelDisplay) {
    if display.is_null() { return; }
    if platform_alderlake_p(display) {
        let (funcs, vals) = if __intel_display_wa(display, IntelDisplayWa::Wa22011320316, ptr::null()) {
            (&TGL_CDCLK_FUNCS, ADLP_A_STEP_CDCLK_TABLE.as_ptr())
        } else if (*display).platform._anon_0_0.bitmap[0] & (1u64 << 55) != 0 {
            (&RPLU_CDCLK_FUNCS, RPLU_CDCLK_TABLE.as_ptr())
        } else {
            (&TGL_CDCLK_FUNCS, ADLP_CDCLK_TABLE.as_ptr())
        };
        (*display).cdclk.funcs = funcs as *const _ as *mut c_void;
        (*display).cdclk.table = vals as *mut c_void;
    } else if DISPLAY_VER(display) >= 11 && DISPLAY_VER(display) <= 13 {
        // TGL/RKL use their own table, not the ADL-P table. This exact
        // upstream family is supported only when the reference-clock-specific
        // rows below match; don't fabricate a valid table for other IPs.
        (*display).cdclk.funcs = ptr::null_mut();
        (*display).cdclk.table = ptr::null_mut();
    } else {
        (*display).cdclk.funcs = ptr::null_mut();
        (*display).cdclk.table = ptr::null_mut();
    }
}

// upstream: intel_cdclk.c intel_cdclk_read_hw()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_read_hw(display: *mut IntelDisplay) {
    if display.is_null() { return; }
    intel_update_cdclk(display);
    let state = (*display).cdclk.obj.state.cast::<IntelCdclkState>();
    if !state.is_null() {
        (*state).actual = (*display).cdclk.hw;
        (*state).logical = (*display).cdclk.hw;
    }
}

// upstream: intel_cdclk.c intel_update_max_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_update_max_cdclk(display: *mut IntelDisplay) {
    if display.is_null() || !platform_alderlake_p(display) { return; }
    (*display).cdclk.max_cdclk_freq = if (*display).cdclk.hw.r#ref == 24_000 { 648_000 } else { 652_800 };
    // ADL-P is DISPLAY_VER >= 10: two pixels per CDCLK and no guardband.
    (*display).cdclk.max_dotclk_freq = 2 * (*display).cdclk.max_cdclk_freq;
}

unsafe fn cnp_rawclk(display: *mut IntelDisplay) -> u32 {
    let (divider, fraction) = if intel_de_read(display, SFUSE_STRAP) & SFUSE_STRAP_RAW_FREQUENCY != 0 {
        (24_000, 0)
    } else {
        (19_000, 200)
    };
    let mut rawclk = CNP_RAWCLK_DIV(divider / 1000);
    if fraction != 0 {
        let den = (1000 + fraction / 2) / fraction - 1;
        rawclk |= CNP_RAWCLK_DEN(den);
        if INTEL_PCH_TYPE(display) >= crate::intel_pch_types_upstream::PCH_ICP {
            rawclk |= ICP_RAWCLK_NUM(1);
        }
    }
    intel_de_write(display, PCH_RAWCLK_FREQ, rawclk);
    divider + fraction
}

// upstream: intel_cdclk.c intel_read_rawclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_read_rawclk(display: *mut IntelDisplay) -> u32 {
    if display.is_null() { return 0; }
    let pch = INTEL_PCH_TYPE(display);
    if pch >= PCH_MTL {
        38_400
    } else if pch >= PCH_DG1 {
        intel_de_write(display, PCH_RAWCLK_FREQ, CNP_RAWCLK_DEN(4) | CNP_RAWCLK_DIV(37) | ICP_RAWCLK_NUM(2));
        38_400
    } else if pch >= PCH_CNP {
        cnp_rawclk(display)
    } else if pch > crate::intel_pch_types_upstream::PCH_NONE {
        (intel_de_read(display, PCH_RAWCLK_FREQ) & RAWCLK_FREQ_MASK) * 1000
    } else { 0 }
}

// upstream: intel_cdclk.c intel_cdclk_clock_changed()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_clock_changed(a: *const IntelCdclkConfig, b: *const IntelCdclkConfig) -> bool {
    !a.is_null() && !b.is_null()
        && ((*a).cdclk != (*b).cdclk || (*a).vco != (*b).vco || (*a).r#ref != (*b).r#ref)
}

// upstream: intel_cdclk.c intel_cdclk_update_dbuf_bw_min_cdclk()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_update_dbuf_bw_min_cdclk(
    state: *mut IntelAtomicState,
    old_min_cdclk: i32,
    new_min_cdclk: i32,
    need_cdclk_calc: *mut bool,
) -> i32 {
    if state.is_null() || need_cdclk_calc.is_null() { return -crate::linux_config::EINVAL; }
    let drm = (*state).base.dev;
    if drm.is_null() || (*drm).dev_private.is_null() { return -crate::linux_config::EINVAL; }
    let i915 = (*drm).dev_private.cast::<DrmI915Private>();
    let display = (*i915).display.cast::<IntelDisplay>();
    if display.is_null() { return -crate::linux_config::EINVAL; }
    let allow_decrease = intel_any_crtc_needs_modeset(state);
    if new_min_cdclk == old_min_cdclk || (!allow_decrease && new_min_cdclk < old_min_cdclk) {
        return 0;
    }
    let global = intel_atomic_get_global_obj_state(state, ptr::addr_of_mut!((*display).cdclk.obj));
    if crate::linux_config::IS_ERR(global) { return crate::linux_config::PTR_ERR(global); }
    if global.is_null() { return -crate::linux_config::ENOMEM; }
    let cdclk_state = global.cast::<IntelCdclkState>();
    let actual_old_min = (*cdclk_state).dbuf_bw_min_cdclk;
    if new_min_cdclk == actual_old_min || (!allow_decrease && new_min_cdclk < actual_old_min) {
        return 0;
    }
    (*cdclk_state).dbuf_bw_min_cdclk = new_min_cdclk;
    let ret = intel_atomic_lock_global_state(global);
    if ret != 0 { return ret; }
    *need_cdclk_calc = true;
    0
}

unsafe extern "C" fn cdclk_duplicate_state(obj: *mut IntelGlobalObj) -> *mut IntelGlobalState {
    if obj.is_null() || (*obj).state.is_null() { return ptr::null_mut(); }
    let source = (*obj).state.cast::<IntelCdclkState>();
    let state = tk_linux_kapi::memory::kmalloc_obj::<IntelCdclkState>(tk_linux_kapi::config::GFP_KERNEL);
    if state.is_null() { return ptr::null_mut(); }
    ptr::copy_nonoverlapping(source, state, 1);
    (*state).base.state = ptr::null_mut();
    (*state).base.commit = ptr::null_mut();
    (*state).base.changed = false;
    (*state).base.serialized = false;
    (*state).pipe = INVALID_PIPE;
    (*state).disable_pipes = false;
    ptr::addr_of_mut!((*state).base)
}

unsafe extern "C" fn cdclk_destroy_state(_obj: *mut IntelGlobalObj, state: *mut IntelGlobalState) {
    if !state.is_null() { tk_linux_kapi::memory::kfree(state.cast::<IntelCdclkState>()); }
}

static CDCLK_GLOBAL_FUNCS: IntelGlobalStateFuncs = IntelGlobalStateFuncs {
    atomic_duplicate_state: Some(cdclk_duplicate_state),
    atomic_destroy_state: Some(cdclk_destroy_state),
};

// upstream: intel_cdclk.c intel_cdclk_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_cdclk_init(display: *mut IntelDisplay) -> i32 {
    if display.is_null() { return -crate::linux_config::EINVAL; }
    let state = tk_linux_kapi::memory::kzalloc_obj::<IntelCdclkState>();
    if state.is_null() { return -crate::linux_config::ENOMEM; }
    (*state).pipe = INVALID_PIPE;
    crate::display_pending_stubs::b3d::intel_atomic_global_obj_init(
        display,
        ptr::addr_of_mut!((*display).cdclk.obj),
        ptr::addr_of_mut!((*state).base),
        ptr::addr_of!(CDCLK_GLOBAL_FUNCS),
    );
    0
}

// Keep the legacy placeholder module's public ABI while replacing every B3e
// body with the real translation. This avoids churn in other display batches.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adlp_tables_and_rplu_voltage_are_source_accurate() {
        assert_eq!(cdclk_table_calc(&ADLP_CDCLK_TABLE, 24_000, 313_000), Some(552_000));
        assert_eq!(cdclk_table_vco(&ADLP_CDCLK_TABLE, 24_000, 648_000), Some(1_296_000));
        assert_eq!(cdclk_table_calc(&RPLU_CDCLK_TABLE, 24_000, 313_000), Some(480_000));
        assert_eq!(rplu_voltage_level(480_000), 1);
        assert_eq!(rplu_voltage_level(480_001), 2);
        assert_eq!(tgl_voltage_level(326_400), 1);
    }

    #[test]
    fn adlp_a_step_table_selection_rows_are_preserved() {
        assert_eq!(ADLP_A_STEP_CDCLK_TABLE[3].refclk, 24_000);
        assert_eq!(ADLP_A_STEP_CDCLK_TABLE[5].refclk, 24_400);
        assert_eq!(cdclk_table_calc(&ADLP_A_STEP_CDCLK_TABLE, 24_000, 0), Some(312_000));
        assert_eq!(cdclk_table_calc(&ADLP_A_STEP_CDCLK_TABLE, 24_400, 0), Some(648_000));
    }

    #[test]
    fn clock_divider_and_transition_stage_order_match_safe_change_path() {
        assert_eq!(cdclk_divider(648_000, 1_296_000, 0), 2);
        assert_eq!(
            transition_order(),
            [
                CdclkTransitionStep::PreparePcode,
                CdclkTransitionStep::UpdatePll,
                CdclkTransitionStep::ProgramCdclk,
                CdclkTransitionStep::SetVoltage,
                CdclkTransitionStep::Readback,
            ]
        );
    }
}
