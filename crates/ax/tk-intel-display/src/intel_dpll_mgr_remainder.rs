// SPDX-License-Identifier: MIT
/*
 * Copyright © 2006-2016 Intel Corporation
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */

//! Additional Linux 7.2.3 `intel_dpll_mgr.c` functions omitted from the
//! display-12/13 translation.  DRM state, encoder iteration, CX0/LT PHY
//! programming, and diagnostics are explicit adapter operations.  The
//! transaction ordering and source-side choices remain in these functions.
#![allow(dead_code, non_camel_case_types, non_snake_case, clippy::too_many_arguments)]

pub const DPLL_ID_ICL_DPLL0: u8 = 0;
pub const DPLL_ID_ICL_DPLL1: u8 = 1;
pub const DPLL_ID_ICL_TBTPLL: u8 = 2;
pub const DPLL_ID_ICL_MGPLL1: u8 = 3;
pub const DPLL_ID_ICL_MGPLL2: u8 = 4;
pub const DPLL_ID_ICL_MGPLL3: u8 = 5;
pub const DPLL_ID_ICL_MGPLL4: u8 = 6;
pub const ICL_PORT_DPLL_DEFAULT: usize = 0;
pub const ICL_PORT_DPLL_MG_PHY: usize = 1;
pub const PCH_DPLL_A: u8 = 0;
pub const PCH_DPLL_B: u8 = 1;
pub const DPLL_VCO_ENABLE: u32 = 1 << 31;
const DREF_SSC_SOURCE_MASK: u32 = 3 << 11;
const DREF_NONSPREAD_SOURCE_MASK: u32 = 3 << 9;
const DREF_SUPERSPREAD_SOURCE_MASK: u32 = 3 << 7;
const HSW_PLL_ENABLE: u32 = 1 << 31;
const HSW_WRPLL_REF_PCH_SSC: u32 = 1 << 28;
const HSW_WRPLL_REF_SPECIAL: u32 = 2 << 28;
const HSW_WRPLL_REF_LCPLL: u32 = 3 << 28;
const HSW_WRPLL_REF_MASK: u32 = 3 << 28;
const HSW_WRPLL_REF_DIV_MASK: u32 = 0xff;
const HSW_WRPLL_POST_DIV_MASK: u32 = 0x3f << 8;
const HSW_WRPLL_POST_DIV_SHIFT: u32 = 8;
const HSW_WRPLL_FB_DIV_MASK: u32 = 0xff << 16;
const HSW_WRPLL_FB_DIV_SHIFT: u32 = 16;
const HSW_SPLL_FREQ_MASK: u32 = 3 << 26;
const HSW_SPLL_FREQ_810: u32 = 0 << 26;
const HSW_SPLL_FREQ_1350: u32 = 1 << 26;
const HSW_SPLL_FREQ_2700: u32 = 2 << 26;
const HSW_SPLL_REF_MUXED_SSC: u32 = 1 << 28;
const HSW_REF_CLK_SELECT: u32 = 1 << 1;
const SKL_PLL_ENABLE: u32 = 1 << 31;
const SKL_CTRL1_HDMI_MODE: u32 = 1 << 5;
const SKL_CTRL1_SSC: u32 = 1 << 4;
const SKL_CTRL1_LINK_RATE_MASK: u32 = 7 << 1;
const SKL_CTRL1_OVERRIDE: u32 = 1;
const SKL_DCO_FREQ_ENABLE: u32 = 1 << 31;
const SKL_DCO_FRACTION_MASK: u32 = 0x7fff << 9;
const SKL_DCO_INTEGER_MASK: u32 = 0x1ff;
const SKL_CFGCR2_QDIV_RATIO_MASK: u32 = 0xff << 8;
const SKL_CFGCR2_QDIV_MODE: u32 = 1 << 7;
const SKL_CFGCR2_KDIV_MASK: u32 = 3 << 5;
const SKL_CFGCR2_PDIV_MASK: u32 = 7 << 2;
const SKL_CFGCR2_CENTRAL_FREQ_MASK: u32 = 3;
const SKL_LINK_RATE_2700: u32 = 0;
const SKL_LINK_RATE_1350: u32 = 1;
const SKL_LINK_RATE_810: u32 = 2;
const SKL_LINK_RATE_1620: u32 = 3;
const SKL_LINK_RATE_1080: u32 = 4;
const SKL_LINK_RATE_2160: u32 = 5;
const BXT_PLL_ENABLE: u32 = 1 << 31;
const BXT_PLL_LOCK: u32 = 1 << 30;
const BXT_PLL_REF_SEL: u32 = 1 << 27;
const BXT_PLL_POWER_ENABLE: u32 = 1 << 26;
const BXT_PLL_POWER_STATE: u32 = 1 << 25;
const BXT_PLL_P1_MASK: u32 = 7 << 13;
const BXT_PLL_P2_MASK: u32 = 0x1f << 8;
const BXT_PLL_RECALIBRATE: u32 = 1 << 14;
const BXT_PLL_10BIT_CLK_ENABLE: u32 = 1 << 13;
const BXT_PLL_M2_INT_MASK: u32 = 0xff;
const BXT_PLL_N_MASK: u32 = 0xf << 8;
const BXT_PLL_M2_FRAC_MASK: u32 = (1 << 22) - 1;
const BXT_PLL_M2_FRAC_ENABLE: u32 = 1 << 16;
const BXT_PLL_GAIN_CTL_MASK: u32 = 7 << 16;
const BXT_PLL_INT_COEFF_MASK: u32 = 0x1f << 8;
const BXT_PLL_PROP_COEFF_MASK: u32 = 0xf;
const BXT_PLL_TARGET_CNT_MASK: u32 = 0x3ff;
const BXT_PLL_LOCK_THRESHOLD_MASK: u32 = 7 << 1;
const BXT_PLL_DCO_AMP_OVR_EN_H: u32 = 1 << 27;
const BXT_PLL_DCO_AMP_MASK: u32 = 0xf << 10;
const BXT_LANESTAGGER_STRAP_OVRD: u32 = 1 << 6;
const BXT_LANE_STAGGER_MASK: u32 = 0x1f;
const BXT_DCC_DELAY_RANGE_2: u32 = 1 << 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cx0PllState {
    pub words: [u32; 12],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LtPllState {
    pub words: [u32; 12],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MtlPllHwState {
    pub cx0pll: Cx0PllState,
    pub ltpll: LtPllState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MtlPortDpll {
    pub pll: Option<u8>,
    pub hw_state: MtlPllHwState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MtlCrtcState {
    pub port_dplls: [MtlPortDpll; 2],
    pub port_clock: i32,
    pub active_port_dpll: usize,
    pub old_dpll_id: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MtlEncoder {
    pub port: u8,
    pub type_c: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MtlDisplay<'a> {
    pub display_id: usize,
    /// The first Type-C port in the platform's `enum port` numbering.
    pub first_tc_port: u8,
    pub encoders: &'a [MtlEncoder],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I9xxDpllHwState {
    pub dpll: u32,
    pub dpll_md: u32,
    pub fp0: u32,
    pub fp1: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IbXDisplay {
    pub display_id: usize,
    pub has_pch_ibx: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IbXCrtcState {
    pub pipe: u8,
    pub dpll_hw_state: I9xxDpllHwState,
    pub intel_dpll: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HswDpllHwState {
    pub wrpll: u32,
    pub spll: u32,
    pub lcpll: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HswDisplay {
    pub display_id: usize,
    pub haswell: bool,
    pub haswell_ult: bool,
    pub nssc_refclk_khz: i32,
    pub ssc_refclk_khz: i32,
    pub pch_ssc_use: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HswOutput {
    Hdmi,
    DisplayPort,
    Analog,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HswCrtcState {
    pub port_clock: i32,
    pub output: HswOutput,
    pub dpll_hw_state: HswDpllHwState,
    pub intel_dpll: Option<u8>,
}

impl Default for HswCrtcState {
    fn default() -> Self {
        Self {
            port_clock: 0,
            output: HswOutput::Other,
            dpll_hw_state: HswDpllHwState::default(),
            intel_dpll: None,
        }
    }
}

pub trait HswDpllHooks {
    fn read32(&mut self, register: LegacyRegister) -> u32;
    fn write32(&mut self, register: LegacyRegister, value: u32);
    fn rmw32(&mut self, register: LegacyRegister, clear: u32, set: u32);
    fn posting_read32(&mut self, register: LegacyRegister);
    fn delay_us(&mut self, delay: u32);
    fn display_power_get_if_enabled(&mut self, display_id: usize) -> Option<u64>;
    fn display_power_put(&mut self, display_id: usize, cookie: u64);
    fn init_pch_refclk(&mut self, display_id: usize);
    fn missing_case(&mut self, display_id: usize, value: u32);
    fn drm_warn(&mut self, display_id: usize, message: &'static str);
    fn drm_warn_on(&mut self, display_id: usize, condition: bool, message: &'static str) -> bool;
    fn dpll_by_id(&mut self, display_id: usize, id: u8) -> Option<u8>;
    fn find_dpll(&mut self, display_id: usize, crtc: usize, state: &HswDpllHwState, mask: u32) -> Option<u8>;
    fn reference_dpll(&mut self, display_id: usize, crtc: usize, pll: u8, state: &HswDpllHwState);
    fn log_hw_state(&mut self, display_id: usize, message: &'static str, a: u32, b: u32);
}

pub trait SklDpllHooks {
    fn read32(&mut self, register: LegacyRegister) -> u32;
    fn write32(&mut self, register: LegacyRegister, value: u32);
    fn rmw32(&mut self, register: LegacyRegister, clear: u32, set: u32);
    fn posting_read32(&mut self, register: LegacyRegister);
    fn wait_for_set(&mut self, display_id: usize, register: LegacyRegister, mask: u32, timeout_ms: u32) -> bool;
    fn display_power_get_if_enabled(&mut self, display_id: usize) -> Option<u64>;
    fn display_power_put(&mut self, display_id: usize, cookie: u64);
    fn drm_err(&mut self, display_id: usize, message: &'static str, pll_id: u8);
    fn drm_warn(&mut self, display_id: usize, message: &'static str, value: u32);
    fn drm_warn_on(&mut self, display_id: usize, condition: bool, message: &'static str) -> bool;
    fn drm_debug(&mut self, display_id: usize, message: &'static str, value: u32);
    fn missing_case(&mut self, display_id: usize, value: u32);
    fn find_dpll(&mut self, display_id: usize, crtc: usize, state: &SklDpllHwState, mask: u32) -> Option<u8>;
    fn reference_dpll(&mut self, display_id: usize, crtc: usize, pll: u8, state: &SklDpllHwState);
    fn log_hw_state(&mut self, display_id: usize, message: &'static str, a: u32, b: u32, c: u32);
}

pub trait BxtDpllHooks {
    fn read32(&mut self, register: LegacyRegister) -> u32;
    fn write32(&mut self, register: LegacyRegister, value: u32);
    fn rmw32(&mut self, register: LegacyRegister, clear: u32, set: u32);
    fn posting_read32(&mut self, register: LegacyRegister);
    fn wait_for_set_us(&mut self, display_id: usize, register: LegacyRegister, mask: u32, timeout_us: u32) -> bool;
    fn wait_for_clear_us(&mut self, display_id: usize, register: LegacyRegister, mask: u32, timeout_us: u32) -> bool;
    fn display_power_get_if_enabled(&mut self, display_id: usize) -> Option<u64>;
    fn display_power_put(&mut self, display_id: usize, cookie: u64);
    fn bxt_port_to_phy_channel(&mut self, display_id: usize, port: u8) -> (u8, u8);
    fn bxt_find_best_dpll(&mut self, port_clock: i32) -> Option<BxtDpllClock>;
    fn chv_calc_dpll_params(&mut self, refclk_khz: i32, clock: &mut BxtDpllClock) -> i32;
    fn dpll_by_id(&mut self, display_id: usize, id: u8) -> Option<u8>;
    fn reference_dpll(&mut self, display_id: usize, crtc: usize, pll: u8, state: &BxtDpllHwState);
    fn drm_err(&mut self, display_id: usize, message: &'static str, port: u8);
    fn drm_warn_on(&mut self, display_id: usize, condition: bool, message: &'static str) -> bool;
    fn drm_debug(&mut self, display_id: usize, message: &'static str, a: u32, b: u32);
    fn log_hw_state(&mut self, display_id: usize, message: &'static str, values: &[u32]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyRegister {
    PchDpll(u8),
    PchFp0(u8),
    PchFp1(u8),
    PchDrefControl,
    WrpllCtl(u8),
    SpllCtl,
    FuseStrap3,
    DpllCtrl1,
    DpllCfgcr1(u8),
    DpllCfgcr2(u8),
    DpllCtl(u8),
    LcpllCtl(u8),
    DpllStatus,
    BxtPllEnable(u8),
    BxtEbb0(u8, u8),
    BxtEbb4(u8, u8),
    BxtPll(u8, u8, u8),
    BxtPcsDw12Lane(u8, u8, u8),
    BxtPcsDw12Group(u8, u8),
    BxtTxDw5Lane(u8, u8, u8),
    BxtTxDw5Group(u8, u8),
}

pub trait IbXDpllHooks {
    fn read32(&mut self, register: LegacyRegister) -> u32;
    fn write32(&mut self, register: LegacyRegister, value: u32);
    fn posting_read32(&mut self, register: LegacyRegister);
    fn delay_us(&mut self, delay: u32);
    fn display_power_get_if_enabled(&mut self, display_id: usize) -> Option<u64>;
    fn display_power_put(&mut self, display_id: usize, cookie: u64);
    fn display_state_warn(&mut self, display_id: usize, condition: bool, message: &'static str);
    fn find_dpll(&mut self, display_id: usize, crtc: usize, hw_state: &I9xxDpllHwState, mask: u32) -> Option<u8>;
    fn dpll_by_id(&mut self, display_id: usize, id: u8) -> Option<u8>;
    fn reference_dpll(&mut self, display_id: usize, crtc: usize, pll: u8, hw_state: &I9xxDpllHwState);
    fn drm_debug(&mut self, display_id: usize, crtc: usize, message: &'static str, pll: u8);
}

/// The APIs below correspond to DRM atomic helpers and to the separately
/// implemented CX0/LT PHY driver.  They are intentionally narrow: DPLL
/// selection, old-vs-new TBT reuse, CRTC clock publication, and failure
/// return ordering stay in the translated manager code.
pub trait IntelDpllRemainderHooks {
    fn port_to_tc(&mut self, port: u8) -> Option<u8>;
    fn missing_case(&mut self, display_id: usize, value: u32);
    fn drm_warn_on(&mut self, display_id: usize, condition: bool, message: &'static str) -> bool;

    fn find_dpll(
        &mut self,
        display_id: usize,
        crtc: usize,
        hw_state: &MtlPllHwState,
        mask: u32,
    ) -> Option<u8>;
    fn reference_dpll(
        &mut self,
        display_id: usize,
        crtc: usize,
        pll: u8,
        hw_state: &MtlPllHwState,
    );
    fn icl_update_active_dpll(&mut self, display_id: usize, crtc: usize, encoder: usize);
    fn icl_get_tc_phy_dplls(&mut self, display_id: usize, crtc: usize, encoder: usize) -> i32;
    fn icl_set_active_port_dpll(&mut self, crtc: usize, index: usize);

    fn intel_cx0pll_readout_hw_state(&mut self, encoder: usize, state: &mut Cx0PllState) -> bool;
    fn intel_cx0pll_calc_port_clock(&mut self, encoder: usize, state: &Cx0PllState) -> i32;
    fn intel_cx0pll_calc_state(
        &mut self,
        crtc: usize,
        encoder: usize,
        state: &mut MtlPllHwState,
    ) -> i32;
    fn intel_mtl_tbt_pll_calc_state(&mut self, state: &mut MtlPllHwState);
    fn intel_mtl_pll_enable(&mut self, encoder: usize, pll: u8, state: &MtlPllHwState);
    fn intel_mtl_pll_disable(&mut self, encoder: usize);
    fn intel_mtl_tbt_pll_readout_hw_state(&mut self, pll: u8, state: &mut MtlPllHwState) -> bool;
    fn intel_cx0pll_dump_hw_state(&mut self, state: &Cx0PllState);
    fn intel_cx0pll_compare_hw_state(&mut self, a: &Cx0PllState, b: &Cx0PllState) -> bool;

    fn intel_lt_phy_pll_readout_hw_state(&mut self, encoder: usize, state: &mut LtPllState) -> bool;
    fn intel_lt_phy_calc_port_clock(&mut self, display_id: usize, state: &LtPllState) -> i32;
    fn intel_lt_phy_pll_calc_state(
        &mut self,
        crtc: usize,
        encoder: usize,
        state: &mut MtlPllHwState,
    ) -> i32;
    fn intel_lt_phy_tbt_pll_calc_state(&mut self, state: &mut MtlPllHwState);
    fn intel_xe3plpd_pll_enable(&mut self, encoder: usize, pll: u8, state: &MtlPllHwState);
    fn intel_xe3plpd_pll_disable(&mut self, encoder: usize);
    fn intel_lt_phy_tbt_pll_readout_hw_state(&mut self, pll: u8, state: &mut MtlPllHwState) -> bool;
    fn intel_lt_phy_dump_hw_state(&mut self, state: &LtPllState);
    fn intel_lt_phy_pll_compare_hw_state(&mut self, a: &LtPllState, b: &LtPllState) -> bool;
}

#[inline]
const fn tc_pll_id(tc_port: u8) -> u8 {
    tc_port + DPLL_ID_ICL_MGPLL1
}

// upstream: intel_dpll_mgr.c mtl_port_to_pll_id()
pub fn mtl_port_to_pll_id<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    port: u8,
) -> u8 {
    if port >= display.first_tc_port {
        return hooks
            .port_to_tc(port)
            .map(tc_pll_id)
            .unwrap_or_else(|| {
                hooks.missing_case(display.display_id, u32::from(port));
                DPLL_ID_ICL_DPLL0
            });
    }

    match port {
        0 => DPLL_ID_ICL_DPLL0, // PORT_A
        1 => DPLL_ID_ICL_DPLL1, // PORT_B
        other => {
            hooks.missing_case(display.display_id, u32::from(other));
            DPLL_ID_ICL_DPLL0
        }
    }
}

// upstream: intel_dpll_mgr.c ibx_pch_dpll_get_hw_state()
pub fn ibx_pch_dpll_get_hw_state<H: IbXDpllHooks>(
    hooks: &mut H,
    display: &IbXDisplay,
    pll_id: u8,
    state: &mut I9xxDpllHwState,
) -> bool {
    let Some(wakeref) = hooks.display_power_get_if_enabled(display.display_id) else {
        return false;
    };

    let register = LegacyRegister::PchDpll(pll_id);
    let value = hooks.read32(register);
    state.dpll = value;
    state.fp0 = hooks.read32(LegacyRegister::PchFp0(pll_id));
    state.fp1 = hooks.read32(LegacyRegister::PchFp1(pll_id));
    hooks.display_power_put(display.display_id, wakeref);
    value & DPLL_VCO_ENABLE != 0
}

// upstream: intel_dpll_mgr.c ibx_assert_pch_refclk_enabled()
pub fn ibx_assert_pch_refclk_enabled<H: IbXDpllHooks>(hooks: &mut H, display: &IbXDisplay) {
    let value = hooks.read32(LegacyRegister::PchDrefControl);
    let enabled = value
        & (DREF_SSC_SOURCE_MASK | DREF_NONSPREAD_SOURCE_MASK | DREF_SUPERSPREAD_SOURCE_MASK)
        != 0;
    hooks.display_state_warn(
        display.display_id,
        !enabled,
        "PCH refclk assertion failure, should be active but is disabled",
    );
}

// upstream: intel_dpll_mgr.c ibx_pch_dpll_enable()
pub fn ibx_pch_dpll_enable<H: IbXDpllHooks>(
    hooks: &mut H,
    display: &IbXDisplay,
    pll_id: u8,
    state: &I9xxDpllHwState,
) {
    // PCH refclock must be enabled first.
    ibx_assert_pch_refclk_enabled(hooks, display);

    hooks.write32(LegacyRegister::PchFp0(pll_id), state.fp0);
    hooks.write32(LegacyRegister::PchFp1(pll_id), state.fp1);
    let register = LegacyRegister::PchDpll(pll_id);
    hooks.write32(register, state.dpll);

    // Wait for clocks to stabilize, then rewrite the DPLL so the pixel
    // multiplier is latched only after the VCO is running.
    hooks.posting_read32(register);
    hooks.delay_us(150);
    hooks.write32(register, state.dpll);
    hooks.posting_read32(register);
    hooks.delay_us(200);
}

// upstream: intel_dpll_mgr.c ibx_pch_dpll_disable()
pub fn ibx_pch_dpll_disable<H: IbXDpllHooks>(hooks: &mut H, _display: &IbXDisplay, pll_id: u8) {
    let register = LegacyRegister::PchDpll(pll_id);
    hooks.write32(register, 0);
    hooks.posting_read32(register);
    hooks.delay_us(200);
}

// upstream: intel_dpll_mgr.c ibx_compute_dpll()
pub fn ibx_compute_dpll() -> i32 {
    // PCH DPLLs use the precomputed legacy clock state.
    0
}

// upstream: intel_dpll_mgr.c ibx_get_dpll()
pub fn ibx_get_dpll<H: IbXDpllHooks>(
    hooks: &mut H,
    display: &IbXDisplay,
    crtc_index: usize,
    state: &mut IbXCrtcState,
) -> i32 {
    let pll = if display.has_pch_ibx {
        // Ironlake PCH has a fixed PLL-to-pipe mapping.
        let Some(pll) = hooks.dpll_by_id(display.display_id, state.pipe) else {
            return -22;
        };
        hooks.drm_debug(display.display_id, crtc_index, "using pre-allocated PCH DPLL", pll);
        pll
    } else {
        let mask = (1_u32 << PCH_DPLL_A) | (1_u32 << PCH_DPLL_B);
        let Some(pll) = hooks.find_dpll(display.display_id, crtc_index, &state.dpll_hw_state, mask) else {
            return -22;
        };
        pll
    };

    hooks.reference_dpll(display.display_id, crtc_index, pll, &state.dpll_hw_state);
    state.intel_dpll = Some(pll);
    0
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_enable()
pub fn hsw_ddi_wrpll_enable<H: HswDpllHooks>(
    hooks: &mut H,
    pll_id: u8,
    state: &HswDpllHwState,
) {
    let register = LegacyRegister::WrpllCtl(pll_id);
    hooks.write32(register, state.wrpll);
    hooks.posting_read32(register);
    hooks.delay_us(20);
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_enable()
pub fn hsw_ddi_spll_enable<H: HswDpllHooks>(hooks: &mut H, state: &HswDpllHwState) {
    let register = LegacyRegister::SpllCtl;
    hooks.write32(register, state.spll);
    hooks.posting_read32(register);
    hooks.delay_us(20);
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_disable()
pub fn hsw_ddi_wrpll_disable<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    pll_id: u8,
) {
    let register = LegacyRegister::WrpllCtl(pll_id);
    hooks.rmw32(register, HSW_PLL_ENABLE, 0);
    hooks.posting_read32(register);
    if display.pch_ssc_use & (1_u32 << pll_id) != 0 {
        hooks.init_pch_refclk(display.display_id);
    }
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_disable()
pub fn hsw_ddi_spll_disable<H: HswDpllHooks>(hooks: &mut H, display: &HswDisplay, pll_id: u8) {
    let register = LegacyRegister::SpllCtl;
    hooks.rmw32(register, HSW_PLL_ENABLE, 0);
    hooks.posting_read32(register);
    if display.pch_ssc_use & (1_u32 << pll_id) != 0 {
        hooks.init_pch_refclk(display.display_id);
    }
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_get_hw_state()
pub fn hsw_ddi_wrpll_get_hw_state<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    pll_id: u8,
    state: &mut HswDpllHwState,
) -> bool {
    let Some(wakeref) = hooks.display_power_get_if_enabled(display.display_id) else {
        return false;
    };
    let value = hooks.read32(LegacyRegister::WrpllCtl(pll_id));
    state.wrpll = value;
    hooks.display_power_put(display.display_id, wakeref);
    value & HSW_PLL_ENABLE != 0
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_get_hw_state()
pub fn hsw_ddi_spll_get_hw_state<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    state: &mut HswDpllHwState,
) -> bool {
    let Some(wakeref) = hooks.display_power_get_if_enabled(display.display_id) else {
        return false;
    };
    let value = hooks.read32(LegacyRegister::SpllCtl);
    state.spll = value;
    hooks.display_power_put(display.display_id, wakeref);
    value & HSW_PLL_ENABLE != 0
}

const HSW_LC_FREQ_2K: u64 = 2700 * 2000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HswWrpllRnp {
    pub p: u32,
    pub n2: u32,
    pub r2: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SklDpllHwState {
    pub ctrl1: u32,
    pub cfgcr1: u32,
    pub cfgcr2: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SklDisplay {
    pub display_id: usize,
    pub nssc_refclk_khz: i32,
    pub cdclk_hw_ref_khz: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SklCrtcState {
    pub port_clock: i32,
    pub output: HswOutput,
    pub is_edp: bool,
    pub dpll_hw_state: SklDpllHwState,
    pub intel_dpll: Option<u8>,
}

impl Default for SklCrtcState {
    fn default() -> Self {
        Self { port_clock: 0, output: HswOutput::Other, is_edp: false,
            dpll_hw_state: SklDpllHwState::default(), intel_dpll: None }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SklWrpllParams {
    pub dco_fraction: u32,
    pub dco_integer: u32,
    pub qdiv_ratio: u32,
    pub qdiv_mode: u32,
    pub kdiv: u32,
    pub pdiv: u32,
    pub central_freq: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SklWrpllContext {
    pub min_deviation: u64,
    pub central_freq: u64,
    pub dco_freq: u64,
    pub p: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BxtDpllHwState {
    pub ebb0: u32,
    pub ebb4: u32,
    pub pll0: u32,
    pub pll1: u32,
    pub pll2: u32,
    pub pll3: u32,
    pub pll6: u32,
    pub pll8: u32,
    pub pll9: u32,
    pub pll10: u32,
    pub pcsdw12: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BxtDisplay {
    pub display_id: usize,
    pub geminilake: bool,
    pub nssc_refclk_khz: i32,
    pub ssc_refclk_khz: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BxtDpllClock {
    pub dot: i32,
    pub p1: u32,
    pub p2: u32,
    pub n: u32,
    pub m1: u32,
    pub m2: u32,
    pub vco: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BxtCrtcState {
    pub port_clock: i32,
    pub output: HswOutput,
    pub dpll_hw_state: BxtDpllHwState,
    pub intel_dpll: Option<u8>,
}

impl Default for BxtCrtcState {
    fn default() -> Self {
        Self { port_clock: 0, output: HswOutput::Other, dpll_hw_state: BxtDpllHwState::default(), intel_dpll: None }
    }
}

// upstream: intel_dpll_mgr.c hsw_wrpll_get_budget_for_freq()
pub fn hsw_wrpll_get_budget_for_freq(clock: i32) -> u32 {
    match clock {
        25_175_000 | 25_200_000 | 27_000_000 | 27_027_000 | 37_762_500 | 37_800_000
        | 40_500_000 | 40_541_000 | 54_000_000 | 54_054_000 | 59_341_000 | 59_400_000
        | 72_000_000 | 74_176_000 | 74_250_000 | 81_000_000 | 81_081_000 | 89_012_000
        | 89_100_000 | 108_000_000 | 108_108_000 | 111_264_000 | 111_375_000
        | 148_352_000 | 148_500_000 | 162_000_000 | 162_162_000 | 222_525_000
        | 222_750_000 | 296_703_000 | 297_000_000 => 0,
        233_500_000 | 245_250_000 | 247_750_000 | 253_250_000 | 298_000_000 => 1500,
        169_128_000 | 169_500_000 | 179_500_000 | 202_000_000 => 2000,
        256_250_000 | 262_500_000 | 270_000_000 | 272_500_000 | 273_750_000
        | 280_750_000 | 281_250_000 | 286_000_000 | 291_750_000 => 4000,
        267_250_000 | 268_500_000 => 5000,
        _ => 1000,
    }
}

// upstream: intel_dpll_mgr.c hsw_wrpll_update_rnp()
pub fn hsw_wrpll_update_rnp(
    freq2k: u64,
    budget: u32,
    r2: u32,
    n2: u32,
    p: u32,
    best: &mut HswWrpllRnp,
) {
    if best.p == 0 {
        best.p = p;
        best.n2 = n2;
        best.r2 = r2;
        return;
    }

    let a = freq2k * u64::from(budget) * u64::from(p) * u64::from(r2);
    let b = freq2k * u64::from(budget) * u64::from(best.p) * u64::from(best.r2);
    let diff = (freq2k * u64::from(p) * u64::from(r2))
        .abs_diff(HSW_LC_FREQ_2K * u64::from(n2));
    let diff_best = (freq2k * u64::from(best.p) * u64::from(best.r2))
        .abs_diff(HSW_LC_FREQ_2K * u64::from(best.n2));
    let c = 1_000_000 * diff;
    let d = 1_000_000 * diff_best;

    if a < c && b < d {
        if u64::from(best.p) * u64::from(best.r2) * diff
            < u64::from(p) * u64::from(r2) * diff_best
        {
            *best = HswWrpllRnp { p, n2, r2 };
        }
    } else if a >= c && b < d {
        *best = HswWrpllRnp { p, n2, r2 };
    } else if a >= c && b >= d {
        if u64::from(n2) * u64::from(best.r2) * u64::from(best.r2)
            > u64::from(best.n2) * u64::from(r2) * u64::from(r2)
        {
            *best = HswWrpllRnp { p, n2, r2 };
        }
    }
}

// upstream: intel_dpll_mgr.c hsw_ddi_calculate_wrpll()
pub fn hsw_ddi_calculate_wrpll(clock_hz: u32) -> HswWrpllRnp {
    let freq2k = u64::from(clock_hz / 100);
    let budget = hsw_wrpll_get_budget_for_freq(clock_hz as i32);
    if freq2k == 5_400_000 {
        return HswWrpllRnp { p: 1, n2: 2, r2: 2 };
    }

    let mut best = HswWrpllRnp::default();
    let r2_min = 2700 * 2 / 400 + 1;
    let r2_max = 2700 * 2 / 48;
    for r2 in r2_min..=r2_max {
        let n2_min = (2400 * r2) / 2700 + 1;
        let n2_max = (4800 * r2) / 2700;
        for n2 in n2_min..=n2_max {
            for p in (2..=64).step_by(2) {
                hsw_wrpll_update_rnp(freq2k, budget, r2, n2, p, &mut best);
            }
        }
    }
    best
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_get_freq()
pub fn hsw_ddi_wrpll_get_freq<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    state: &HswDpllHwState,
) -> i32 {
    let wrpll = state.wrpll;
    let reference = match wrpll & HSW_WRPLL_REF_MASK {
        HSW_WRPLL_REF_SPECIAL if display.haswell && !display.haswell_ult => display.nssc_refclk_khz,
        HSW_WRPLL_REF_SPECIAL | HSW_WRPLL_REF_PCH_SSC => display.ssc_refclk_khz,
        HSW_WRPLL_REF_LCPLL => 2_700_000,
        other => {
            hooks.missing_case(display.display_id, other);
            return 0;
        }
    };

    let r = (wrpll & HSW_WRPLL_REF_DIV_MASK) as i32;
    let p = ((wrpll & HSW_WRPLL_POST_DIV_MASK) >> HSW_WRPLL_POST_DIV_SHIFT) as i32;
    let n = ((wrpll & HSW_WRPLL_FB_DIV_MASK) >> HSW_WRPLL_FB_DIV_SHIFT) as i32;
    let Some(denominator) = p.checked_mul(r).filter(|v| *v != 0) else {
        hooks.missing_case(display.display_id, wrpll);
        return 0;
    };

    // Convert to kHz; p and r have a fixed-point portion.
    ((reference * n / 10) / denominator) * 2
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_compute_dpll()
pub fn hsw_ddi_wrpll_compute_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc_state: &mut HswCrtcState,
) -> i32 {
    let params = hsw_ddi_calculate_wrpll((crtc_state.port_clock * 1000) as u32);
    crtc_state.dpll_hw_state.wrpll = HSW_PLL_ENABLE
        | HSW_WRPLL_REF_LCPLL
        | params.r2
        | (params.n2 << HSW_WRPLL_FB_DIV_SHIFT)
        | (params.p << HSW_WRPLL_POST_DIV_SHIFT);
    crtc_state.port_clock = hsw_ddi_wrpll_get_freq(hooks, display, &crtc_state.dpll_hw_state);
    0
}

// upstream: intel_dpll_mgr.c hsw_ddi_wrpll_get_dpll()
pub fn hsw_ddi_wrpll_get_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc: usize,
    state: &HswCrtcState,
) -> Option<u8> {
    hooks.find_dpll(
        display.display_id,
        crtc,
        &state.dpll_hw_state,
        (1_u32 << 1) | (1_u32 << 0),
    )
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_compute_dpll()
pub fn hsw_ddi_lcpll_compute_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    clock: i32,
) -> i32 {
    match clock / 2 {
        81_000 | 135_000 | 270_000 => 0,
        _ => {
            hooks.drm_warn(display.display_id, "Invalid clock for DP");
            -22
        }
    }
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_get_dpll()
pub fn hsw_ddi_lcpll_get_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    clock: i32,
) -> Option<u8> {
    let pll_id = match clock / 2 {
        81_000 => 3,
        135_000 => 4,
        270_000 => 5,
        other => {
            hooks.missing_case(display.display_id, other as u32);
            return None;
        }
    };
    hooks.dpll_by_id(display.display_id, pll_id)
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_get_freq()
pub fn hsw_ddi_lcpll_get_freq<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    pll_id: u8,
) -> i32 {
    let link_clock = match pll_id {
        3 => 81_000,
        4 => 135_000,
        5 => 270_000,
        _ => {
            hooks.drm_warn(display.display_id, "bad port clock sel");
            0
        }
    };
    link_clock * 2
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_compute_dpll()
pub fn hsw_ddi_spll_compute_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc_state: &mut HswCrtcState,
) -> i32 {
    if hooks.drm_warn_on(
        display.display_id,
        crtc_state.port_clock / 2 != 135_000,
        "SPLL only supports the 1350 MHz DP link clock",
    ) {
        return -22;
    }
    crtc_state.dpll_hw_state.spll = HSW_PLL_ENABLE | HSW_SPLL_FREQ_1350 | HSW_SPLL_REF_MUXED_SSC;
    0
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_get_dpll()
pub fn hsw_ddi_spll_get_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc: usize,
    state: &HswCrtcState,
) -> Option<u8> {
    hooks.find_dpll(display.display_id, crtc, &state.dpll_hw_state, 1_u32 << 2)
}

// upstream: intel_dpll_mgr.c hsw_ddi_spll_get_freq()
pub fn hsw_ddi_spll_get_freq<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    state: &HswDpllHwState,
) -> i32 {
    let link_clock = match state.spll & HSW_SPLL_FREQ_MASK {
        HSW_SPLL_FREQ_810 => 81_000,
        HSW_SPLL_FREQ_1350 => 135_000,
        HSW_SPLL_FREQ_2700 => 270_000,
        _ => {
            hooks.drm_warn(display.display_id, "bad spll freq");
            0
        }
    };
    link_clock * 2
}

// upstream: intel_dpll_mgr.c hsw_compute_dpll()
pub fn hsw_compute_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc_state: &mut HswCrtcState,
) -> i32 {
    match crtc_state.output {
        HswOutput::Hdmi => hsw_ddi_wrpll_compute_dpll(hooks, display, crtc_state),
        HswOutput::DisplayPort => hsw_ddi_lcpll_compute_dpll(hooks, display, crtc_state.port_clock),
        HswOutput::Analog => hsw_ddi_spll_compute_dpll(hooks, display, crtc_state),
        HswOutput::Other => -22,
    }
}

// upstream: intel_dpll_mgr.c hsw_get_dpll()
pub fn hsw_get_dpll<H: HswDpllHooks>(
    hooks: &mut H,
    display: &HswDisplay,
    crtc: usize,
    crtc_state: &mut HswCrtcState,
) -> i32 {
    let pll = match crtc_state.output {
        HswOutput::Hdmi => hsw_ddi_wrpll_get_dpll(hooks, display, crtc, crtc_state),
        HswOutput::DisplayPort => hsw_ddi_lcpll_get_dpll(hooks, display, crtc_state.port_clock),
        HswOutput::Analog => hsw_ddi_spll_get_dpll(hooks, display, crtc, crtc_state),
        HswOutput::Other => None,
    };
    let Some(pll) = pll else {
        return -22;
    };
    hooks.reference_dpll(display.display_id, crtc, pll, &crtc_state.dpll_hw_state);
    crtc_state.intel_dpll = Some(pll);
    0
}

// upstream: intel_dpll_mgr.c hsw_update_dpll_ref_clks()
pub fn hsw_update_dpll_ref_clks<H: HswDpllHooks>(hooks: &mut H, display: &mut HswDisplay) {
    display.ssc_refclk_khz = 135_000;
    // Non-SSC is only used on non-ULT HSW.
    if hooks.read32(LegacyRegister::FuseStrap3) & HSW_REF_CLK_SELECT != 0 {
        display.nssc_refclk_khz = 24_000;
    } else {
        display.nssc_refclk_khz = 135_000;
    }
}

// upstream: intel_dpll_mgr.c hsw_dump_hw_state()
pub fn hsw_dump_hw_state<H: HswDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    state: &HswDpllHwState,
) {
    hooks.log_hw_state(display_id, "dpll_hw_state: wrpll/spll", state.wrpll, state.spll);
}

// upstream: intel_dpll_mgr.c hsw_compare_hw_state()
pub fn hsw_compare_hw_state(a: &HswDpllHwState, b: &HswDpllHwState) -> bool {
    a.wrpll == b.wrpll && a.spll == b.spll
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_enable()
pub fn hsw_ddi_lcpll_enable(_hooks: &mut impl HswDpllHooks, _pll_id: u8) {
    // The HSW LCPLLs are fixed-rate and always-on.
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_disable()
pub fn hsw_ddi_lcpll_disable(_hooks: &mut impl HswDpllHooks, _pll_id: u8) {
    // The HSW LCPLLs are fixed-rate and always-on.
}

// upstream: intel_dpll_mgr.c hsw_ddi_lcpll_get_hw_state()
pub fn hsw_ddi_lcpll_get_hw_state(_hooks: &mut impl HswDpllHooks) -> bool {
    true
}

fn skl_dpll_ctl_register(id: u8) -> LegacyRegister {
    match id {
        0 | 1 => LegacyRegister::LcpllCtl(id),
        2 => LegacyRegister::WrpllCtl(0),
        _ => LegacyRegister::WrpllCtl(1),
    }
}

// upstream: intel_dpll_mgr.c skl_ddi_pll_write_ctrl1()
pub fn skl_ddi_pll_write_ctrl1<H: SklDpllHooks>(
    hooks: &mut H,
    pll_id: u8,
    state: &SklDpllHwState,
) {
    let shift = u32::from(pll_id) * 6;
    let clear = (SKL_CTRL1_HDMI_MODE | SKL_CTRL1_SSC | SKL_CTRL1_LINK_RATE_MASK) << shift;
    hooks.rmw32(LegacyRegister::DpllCtrl1, clear, state.ctrl1 << shift);
    hooks.posting_read32(LegacyRegister::DpllCtrl1);
}

// upstream: intel_dpll_mgr.c skl_ddi_pll_enable()
pub fn skl_ddi_pll_enable<H: SklDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    pll_id: u8,
    state: &SklDpllHwState,
) {
    let ctl = skl_dpll_ctl_register(pll_id);
    skl_ddi_pll_write_ctrl1(hooks, pll_id, state);
    let cfgcr1 = LegacyRegister::DpllCfgcr1(pll_id);
    let cfgcr2 = LegacyRegister::DpllCfgcr2(pll_id);
    hooks.write32(cfgcr1, state.cfgcr1);
    hooks.write32(cfgcr2, state.cfgcr2);
    hooks.posting_read32(cfgcr1);
    hooks.posting_read32(cfgcr2);
    hooks.rmw32(ctl, 0, SKL_PLL_ENABLE);

    if hooks.wait_for_set(display_id, LegacyRegister::DpllStatus, 1_u32 << (pll_id * 8), 5) {
        hooks.drm_err(display_id, "DPLL not locked", pll_id);
    }
}

// upstream: intel_dpll_mgr.c skl_ddi_dpll0_enable()
pub fn skl_ddi_dpll0_enable<H: SklDpllHooks>(
    hooks: &mut H,
    pll_id: u8,
    state: &SklDpllHwState,
) {
    skl_ddi_pll_write_ctrl1(hooks, pll_id, state);
}

// upstream: intel_dpll_mgr.c skl_ddi_pll_disable()
pub fn skl_ddi_pll_disable<H: SklDpllHooks>(hooks: &mut H, pll_id: u8) {
    let ctl = skl_dpll_ctl_register(pll_id);
    hooks.rmw32(ctl, SKL_PLL_ENABLE, 0);
    hooks.posting_read32(ctl);
}

// upstream: intel_dpll_mgr.c skl_ddi_dpll0_disable()
pub fn skl_ddi_dpll0_disable(_hooks: &mut impl SklDpllHooks, _pll_id: u8) {
    // DPLL0 is the always-on CDCLK source; upstream does not disable it.
}

// upstream: intel_dpll_mgr.c skl_ddi_pll_get_hw_state()
pub fn skl_ddi_pll_get_hw_state<H: SklDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    pll_id: u8,
    state: &mut SklDpllHwState,
) -> bool {
    let Some(wakeref) = hooks.display_power_get_if_enabled(display_id) else {
        return false;
    };

    let mut enabled = false;
    if hooks.read32(skl_dpll_ctl_register(pll_id)) & SKL_PLL_ENABLE != 0 {
        let ctrl1 = hooks.read32(LegacyRegister::DpllCtrl1);
        state.ctrl1 = (ctrl1 >> (u32::from(pll_id) * 6)) & 0x3f;
        // Avoid reading stale configuration when HDMI mode is off.
        if ctrl1 & (SKL_CTRL1_HDMI_MODE << (u32::from(pll_id) * 6)) != 0 {
            state.cfgcr1 = hooks.read32(LegacyRegister::DpllCfgcr1(pll_id));
            state.cfgcr2 = hooks.read32(LegacyRegister::DpllCfgcr2(pll_id));
        }
        enabled = true;
    }
    hooks.display_power_put(display_id, wakeref);
    enabled
}

// upstream: intel_dpll_mgr.c skl_ddi_dpll0_get_hw_state()
pub fn skl_ddi_dpll0_get_hw_state<H: SklDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    pll_id: u8,
    state: &mut SklDpllHwState,
) -> bool {
    let Some(wakeref) = hooks.display_power_get_if_enabled(display_id) else {
        return false;
    };

    let enabled = hooks.read32(skl_dpll_ctl_register(pll_id)) & SKL_PLL_ENABLE != 0;
    if hooks.drm_warn_on(display_id, !enabled, "DPLL0 is not enabled" ) {
        hooks.display_power_put(display_id, wakeref);
        return false;
    }
    let ctrl1 = hooks.read32(LegacyRegister::DpllCtrl1);
    state.ctrl1 = (ctrl1 >> (u32::from(pll_id) * 6)) & 0x3f;
    hooks.display_power_put(display_id, wakeref);
    true
}

// upstream: intel_dpll_mgr.c skl_wrpll_try_divider()
pub fn skl_wrpll_try_divider(
    context: &mut SklWrpllContext,
    central_freq: u64,
    dco_freq: u64,
    divider: u32,
) {
    let deviation = 10_000 * dco_freq.abs_diff(central_freq) / central_freq;
    if dco_freq >= central_freq {
        if deviation < 100 && deviation < context.min_deviation {
            context.min_deviation = deviation;
            context.central_freq = central_freq;
            context.dco_freq = dco_freq;
            context.p = divider;
        }
    } else if deviation < 600 && deviation < context.min_deviation {
        context.min_deviation = deviation;
        context.central_freq = central_freq;
        context.dco_freq = dco_freq;
        context.p = divider;
    }
}

// upstream: intel_dpll_mgr.c skl_wrpll_get_multipliers()
pub fn skl_wrpll_get_multipliers(p: u32) -> (u32, u32, u32) {
    if p % 2 == 0 {
        let half = p / 2;
        if [1, 2, 3, 5].contains(&half) {
            (2, 1, half)
        } else if half % 2 == 0 {
            (2, half / 2, 2)
        } else if half % 3 == 0 {
            (3, half / 3, 2)
        } else if half % 7 == 0 {
            (7, half / 7, 2)
        } else {
            (0, 0, 0)
        }
    } else if p == 3 || p == 9 {
        (3, 1, p / 3)
    } else if p == 5 || p == 7 {
        (p, 1, 1)
    } else if p == 15 {
        (3, 1, 5)
    } else if p == 21 {
        (7, 1, 3)
    } else if p == 35 {
        (7, 1, 5)
    } else {
        (0, 0, 0)
    }
}

// upstream: intel_dpll_mgr.c skl_wrpll_params_populate()
pub fn skl_wrpll_params_populate<H: SklDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    params: &mut SklWrpllParams,
    afe_clock: u64,
    ref_clock_khz: i32,
    central_freq: u64,
    p0: u32,
    p1: u32,
    p2: u32,
) {
    params.central_freq = match central_freq {
        9_600_000_000 => 0,
        9_000_000_000 => 1,
        8_400_000_000 => 3,
        _ => params.central_freq,
    };
    params.pdiv = match p0 {
        1 => 0,
        2 => 1,
        3 => 2,
        7 => 4,
        _ => {
            hooks.drm_warn(display_id, "Incorrect PDiv", p0);
            params.pdiv
        }
    };
    params.kdiv = match p2 {
        5 => 0,
        2 => 1,
        3 => 2,
        1 => 3,
        _ => {
            hooks.drm_warn(display_id, "Incorrect KDiv", p2);
            params.kdiv
        }
    };
    params.qdiv_ratio = p1;
    params.qdiv_mode = u32::from(params.qdiv_ratio != 1);

    let dco_freq = u64::from(p0) * u64::from(p1) * u64::from(p2) * afe_clock;
    let reference_hz = i64::from(ref_clock_khz) * 1000;
    if reference_hz <= 0 {
        hooks.drm_warn(display_id, "Invalid WRPLL reference clock", ref_clock_khz as u32);
        return;
    }
    params.dco_integer = (dco_freq / reference_hz as u64) as u32;
    let ref_mhz = ref_clock_khz / 1000;
    if ref_mhz <= 0 {
        hooks.drm_warn(display_id, "Invalid WRPLL reference clock", ref_clock_khz as u32);
        return;
    }
    let dco_mhz = dco_freq / ref_mhz as u64;
    params.dco_fraction = (((dco_mhz - u64::from(params.dco_integer) * 1_000_000) * 0x8000)
        / 1_000_000) as u32;
}

// upstream: intel_dpll_mgr.c skl_ddi_calculate_wrpll()
pub fn skl_ddi_calculate_wrpll<H: SklDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    clock_khz: i32,
    ref_clock_khz: i32,
    params: &mut SklWrpllParams,
) -> i32 {
    const CENTRAL: [u64; 3] = [8_400_000_000, 9_000_000_000, 9_600_000_000];
    const EVEN: [u32; 36] = [
        4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 52, 54, 56,
        60, 64, 66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98,
    ];
    const ODD: [u32; 7] = [3, 5, 7, 9, 15, 21, 35];
    if clock_khz <= 0 || ref_clock_khz <= 0 {
        return -22;
    }
    let afe_clock = clock_khz as u64 * 1000 * 5;
    let mut context = SklWrpllContext { min_deviation: u64::MAX, ..Default::default() };
    for (group, dividers) in [&EVEN[..], &ODD[..]].into_iter().enumerate() {
        for central in CENTRAL {
            for &divider in dividers {
                skl_wrpll_try_divider(&mut context, central, u64::from(divider) * afe_clock, divider);
                if context.min_deviation == 0 {
                    break;
                }
            }
            if context.min_deviation == 0 {
                break;
            }
        }
        // Prefer any valid even divider over an odd one.
        if group == 0 && context.p != 0 {
            break;
        }
    }
    if context.p == 0 {
        return -22;
    }
    let (p0, p1, p2) = skl_wrpll_get_multipliers(context.p);
    skl_wrpll_params_populate(hooks, display_id, params, afe_clock, ref_clock_khz,
                              context.central_freq, p0, p1, p2);
    0
}

// upstream: intel_dpll_mgr.c skl_ddi_wrpll_get_freq()
pub fn skl_ddi_wrpll_get_freq<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    state: &SklDpllHwState,
) -> i32 {
    let mut p0 = state.cfgcr2 & SKL_CFGCR2_PDIV_MASK;
    let p2 = state.cfgcr2 & SKL_CFGCR2_KDIV_MASK;
    let p1 = if state.cfgcr2 & SKL_CFGCR2_QDIV_MODE != 0 {
        (state.cfgcr2 & SKL_CFGCR2_QDIV_RATIO_MASK) >> 8
    } else {
        1
    };

    p0 = match p0 {
        0 => 1,
        4 => 2,
        8 => 3,
        20 => {
            // Work around the known ASUS-Z170M invalid encoding: hardware
            // ignores the low bit and behaves as PDIV=7.
            hooks.drm_debug(display.display_id, "Invalid WRPLL PDIV divider, fixing it", p0);
            7
        }
        16 => 7,
        other => {
            hooks.missing_case(display.display_id, other);
            return 0;
        }
    };
    let p2 = match p2 {
        0 => 5,
        32 => 2,
        64 => 3,
        96 => 1,
        other => {
            hooks.missing_case(display.display_id, other);
            return 0;
        }
    };
    if hooks.drm_warn_on(display.display_id, p0 == 0 || p1 == 0 || p2 == 0,
                         "Invalid WRPLL divider") {
        return 0;
    }

    let dco_integer = state.cfgcr1 & SKL_DCO_INTEGER_MASK;
    let dco_fraction = (state.cfgcr1 & SKL_DCO_FRACTION_MASK) >> 9;
    let dco_freq = u64::from(dco_integer) * display.nssc_refclk_khz as u64
        + u64::from(dco_fraction) * display.nssc_refclk_khz as u64 / 0x8000;
    (dco_freq / (u64::from(p0) * u64::from(p1) * p2 as u64 * 5)) as i32
}

// upstream: intel_dpll_mgr.c skl_ddi_hdmi_pll_dividers()
pub fn skl_ddi_hdmi_pll_dividers<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    crtc_state: &mut SklCrtcState,
) -> i32 {
    let mut params = SklWrpllParams::default();
    let ret = skl_ddi_calculate_wrpll(hooks, display.display_id, crtc_state.port_clock,
                                       display.nssc_refclk_khz, &mut params);
    if ret != 0 {
        return ret;
    }
    let hw = &mut crtc_state.dpll_hw_state;
    hw.ctrl1 = SKL_CTRL1_OVERRIDE | SKL_CTRL1_HDMI_MODE;
    hw.cfgcr1 = SKL_DCO_FREQ_ENABLE | (params.dco_fraction << 9) | params.dco_integer;
    hw.cfgcr2 = (params.qdiv_ratio << 8)
        | (params.qdiv_mode << 7)
        | (params.kdiv << 5)
        | (params.pdiv << 2)
        | params.central_freq;
    crtc_state.port_clock = skl_ddi_wrpll_get_freq(hooks, display, &crtc_state.dpll_hw_state);
    0
}

// upstream: intel_dpll_mgr.c skl_ddi_dp_set_dpll_hw_state()
pub fn skl_ddi_dp_set_dpll_hw_state(crtc_state: &mut SklCrtcState) -> i32 {
    let rate = match crtc_state.port_clock / 2 {
        81_000 => Some(SKL_LINK_RATE_810),
        135_000 => Some(SKL_LINK_RATE_1350),
        270_000 => Some(SKL_LINK_RATE_2700),
        162_000 => Some(SKL_LINK_RATE_1620),
        108_000 => Some(SKL_LINK_RATE_1080),
        216_000 => Some(SKL_LINK_RATE_2160),
        _ => None,
    };
    crtc_state.dpll_hw_state.ctrl1 = SKL_CTRL1_OVERRIDE | rate.map_or(0, |v| v << 1);
    0
}

// upstream: intel_dpll_mgr.c skl_ddi_lcpll_get_freq()
pub fn skl_ddi_lcpll_get_freq<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    state: &SklDpllHwState,
) -> i32 {
    let link_clock = match (state.ctrl1 & SKL_CTRL1_LINK_RATE_MASK) >> 1 {
        SKL_LINK_RATE_810 => 81_000,
        SKL_LINK_RATE_1080 => 108_000,
        SKL_LINK_RATE_1350 => 135_000,
        SKL_LINK_RATE_1620 => 162_000,
        SKL_LINK_RATE_2160 => 216_000,
        SKL_LINK_RATE_2700 => 270_000,
        other => {
            hooks.drm_warn(display.display_id, "Unsupported link rate", other);
            0
        }
    };
    link_clock * 2
}

// upstream: intel_dpll_mgr.c skl_compute_dpll()
pub fn skl_compute_dpll<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    crtc_state: &mut SklCrtcState,
) -> i32 {
    if crtc_state.output == HswOutput::Hdmi {
        skl_ddi_hdmi_pll_dividers(hooks, display, crtc_state)
    } else if crtc_state.output == HswOutput::DisplayPort {
        skl_ddi_dp_set_dpll_hw_state(crtc_state)
    } else {
        -22
    }
}

// upstream: intel_dpll_mgr.c skl_get_dpll()
pub fn skl_get_dpll<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    crtc: usize,
    crtc_state: &mut SklCrtcState,
) -> i32 {
    let mask = if crtc_state.is_edp { 1 } else { (1 << 3) | (1 << 2) | (1 << 1) };
    let Some(pll) = hooks.find_dpll(display.display_id, crtc, &crtc_state.dpll_hw_state, mask) else {
        return -22;
    };
    hooks.reference_dpll(display.display_id, crtc, pll, &crtc_state.dpll_hw_state);
    crtc_state.intel_dpll = Some(pll);
    0
}

// upstream: intel_dpll_mgr.c skl_ddi_pll_get_freq()
pub fn skl_ddi_pll_get_freq<H: SklDpllHooks>(
    hooks: &mut H,
    display: &SklDisplay,
    state: &SklDpllHwState,
) -> i32 {
    if state.ctrl1 & SKL_CTRL1_HDMI_MODE != 0 {
        skl_ddi_wrpll_get_freq(hooks, display, state)
    } else {
        skl_ddi_lcpll_get_freq(hooks, display, state)
    }
}

// upstream: intel_dpll_mgr.c skl_update_dpll_ref_clks()
pub fn skl_update_dpll_ref_clks(display: &mut SklDisplay) {
    // No SSC reference clock: DPLL reference follows the active CDCLK source.
    display.nssc_refclk_khz = display.cdclk_hw_ref_khz;
}

// upstream: intel_dpll_mgr.c skl_dump_hw_state()
pub fn skl_dump_hw_state<H: SklDpllHooks>(hooks: &mut H, display_id: usize, state: &SklDpllHwState) {
    hooks.log_hw_state(display_id, "dpll_hw_state: ctrl1/cfgcr1/cfgcr2", state.ctrl1, state.cfgcr1, state.cfgcr2);
}

// upstream: intel_dpll_mgr.c skl_compare_hw_state()
pub fn skl_compare_hw_state(a: &SklDpllHwState, b: &SklDpllHwState) -> bool {
    a.ctrl1 == b.ctrl1 && a.cfgcr1 == b.cfgcr1 && a.cfgcr2 == b.cfgcr2
}

// upstream: intel_dpll_mgr.c bxt_ddi_pll_enable()
pub fn bxt_ddi_pll_enable<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port: u8,
    state: &BxtDpllHwState,
) {
    let (phy, channel) = hooks.bxt_port_to_phy_channel(display.display_id, port);
    let enable = LegacyRegister::BxtPllEnable(port);

    // Select the non-SSC reference before powering the PLL.
    hooks.rmw32(enable, 0, BXT_PLL_REF_SEL);
    if display.geminilake {
        hooks.rmw32(enable, 0, BXT_PLL_POWER_ENABLE);
        if hooks.wait_for_set_us(display.display_id, enable, BXT_PLL_POWER_STATE, 200) {
            hooks.drm_err(display.display_id, "Power state not set for PLL", port);
        }
    }

    let ebb4 = LegacyRegister::BxtEbb4(phy, channel);
    let ebb0 = LegacyRegister::BxtEbb0(phy, channel);
    hooks.rmw32(ebb4, BXT_PLL_10BIT_CLK_ENABLE, 0);
    hooks.rmw32(ebb0, BXT_PLL_P1_MASK | BXT_PLL_P2_MASK, state.ebb0);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 0), BXT_PLL_M2_INT_MASK, state.pll0);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 1), BXT_PLL_N_MASK, state.pll1);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 2), BXT_PLL_M2_FRAC_MASK, state.pll2);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 3), BXT_PLL_M2_FRAC_ENABLE, state.pll3);

    let pll6 = LegacyRegister::BxtPll(phy, channel, 6);
    let mut value = hooks.read32(pll6);
    value &= !(BXT_PLL_PROP_COEFF_MASK | BXT_PLL_INT_COEFF_MASK | BXT_PLL_GAIN_CTL_MASK);
    value |= state.pll6;
    hooks.write32(pll6, value);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 8), BXT_PLL_TARGET_CNT_MASK, state.pll8);
    hooks.rmw32(LegacyRegister::BxtPll(phy, channel, 9), BXT_PLL_LOCK_THRESHOLD_MASK, state.pll9);

    let pll10 = LegacyRegister::BxtPll(phy, channel, 10);
    value = hooks.read32(pll10);
    value &= !BXT_PLL_DCO_AMP_OVR_EN_H;
    value &= !BXT_PLL_DCO_AMP_MASK;
    value |= state.pll10;
    hooks.write32(pll10, value);

    // Retrigger calibration with the programmed values, then restore the
    // requested 10-bit clock setting.
    value = hooks.read32(ebb4);
    value |= BXT_PLL_RECALIBRATE;
    hooks.write32(ebb4, value);
    value &= !BXT_PLL_10BIT_CLK_ENABLE;
    value |= state.ebb4;
    hooks.write32(ebb4, value);

    hooks.rmw32(enable, 0, BXT_PLL_ENABLE);
    hooks.posting_read32(enable);
    if hooks.wait_for_set_us(display.display_id, enable, BXT_PLL_LOCK, 200) {
        hooks.drm_err(display.display_id, "PLL not locked", port);
    }

    if display.geminilake {
        let tx_dw5 = LegacyRegister::BxtTxDw5Lane(phy, channel, 0);
        value = hooks.read32(tx_dw5);
        value |= BXT_DCC_DELAY_RANGE_2;
        hooks.write32(LegacyRegister::BxtTxDw5Group(phy, channel), value);
    }

    // Group writes program all lanes; lane 0/1 is the canonical readout.
    let pcs_lane01 = LegacyRegister::BxtPcsDw12Lane(phy, channel, 0);
    value = hooks.read32(pcs_lane01);
    value &= !BXT_LANE_STAGGER_MASK;
    value &= !BXT_LANESTAGGER_STRAP_OVRD;
    value |= state.pcsdw12;
    hooks.write32(LegacyRegister::BxtPcsDw12Group(phy, channel), value);
}

// upstream: intel_dpll_mgr.c bxt_ddi_pll_disable()
pub fn bxt_ddi_pll_disable<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port: u8,
) {
    let enable = LegacyRegister::BxtPllEnable(port);
    hooks.rmw32(enable, BXT_PLL_ENABLE, 0);
    hooks.posting_read32(enable);
    if display.geminilake {
        hooks.rmw32(enable, BXT_PLL_POWER_ENABLE, 0);
        if hooks.wait_for_clear_us(display.display_id, enable, BXT_PLL_POWER_STATE, 200) {
            hooks.drm_err(display.display_id, "Power state not reset for PLL", port);
        }
    }
}

// upstream: intel_dpll_mgr.c bxt_ddi_pll_get_hw_state()
pub fn bxt_ddi_pll_get_hw_state<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port: u8,
    state: &mut BxtDpllHwState,
) -> bool {
    let (phy, channel) = hooks.bxt_port_to_phy_channel(display.display_id, port);
    let Some(wakeref) = hooks.display_power_get_if_enabled(display.display_id) else {
        return false;
    };
    let enabled = hooks.read32(LegacyRegister::BxtPllEnable(port)) & BXT_PLL_ENABLE != 0;
    if !enabled {
        hooks.display_power_put(display.display_id, wakeref);
        return false;
    }

    state.ebb0 = hooks.read32(LegacyRegister::BxtEbb0(phy, channel)) & (BXT_PLL_P1_MASK | BXT_PLL_P2_MASK);
    state.ebb4 = hooks.read32(LegacyRegister::BxtEbb4(phy, channel)) & BXT_PLL_10BIT_CLK_ENABLE;
    state.pll0 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 0)) & BXT_PLL_M2_INT_MASK;
    state.pll1 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 1)) & BXT_PLL_N_MASK;
    state.pll2 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 2)) & BXT_PLL_M2_FRAC_MASK;
    state.pll3 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 3)) & BXT_PLL_M2_FRAC_ENABLE;
    state.pll6 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 6))
        & (BXT_PLL_PROP_COEFF_MASK | BXT_PLL_INT_COEFF_MASK | BXT_PLL_GAIN_CTL_MASK);
    state.pll8 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 8)) & BXT_PLL_TARGET_CNT_MASK;
    state.pll9 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 9)) & BXT_PLL_LOCK_THRESHOLD_MASK;
    state.pll10 = hooks.read32(LegacyRegister::BxtPll(phy, channel, 10))
        & (BXT_PLL_DCO_AMP_OVR_EN_H | BXT_PLL_DCO_AMP_MASK);

    let lane01 = hooks.read32(LegacyRegister::BxtPcsDw12Lane(phy, channel, 0));
    let lane23 = hooks.read32(LegacyRegister::BxtPcsDw12Lane(phy, channel, 1));
    state.pcsdw12 = lane01;
    if lane23 != lane01 {
        hooks.drm_debug(display.display_id,
            "lane stagger config differs for lane 01 and 23", lane01, lane23);
    }
    state.pcsdw12 &= BXT_LANE_STAGGER_MASK | BXT_LANESTAGGER_STRAP_OVRD;
    hooks.display_power_put(display.display_id, wakeref);
    true
}

const BXT_DP_CLOCKS: [BxtDpllClock; 7] = [
    BxtDpllClock { dot: 162_000, p1: 4, p2: 2, n: 1, m1: 2, m2: 0x0819_999a, vco: 0 },
    BxtDpllClock { dot: 270_000, p1: 4, p2: 1, n: 1, m1: 2, m2: 0x06c0_0000, vco: 0 },
    BxtDpllClock { dot: 540_000, p1: 2, p2: 1, n: 1, m1: 2, m2: 0x06c0_0000, vco: 0 },
    BxtDpllClock { dot: 216_000, p1: 3, p2: 2, n: 1, m1: 2, m2: 0x0819_999a, vco: 0 },
    BxtDpllClock { dot: 243_000, p1: 4, p2: 1, n: 1, m1: 2, m2: 0x0613_3333, vco: 0 },
    BxtDpllClock { dot: 324_000, p1: 4, p2: 1, n: 1, m1: 2, m2: 0x0819_999a, vco: 0 },
    BxtDpllClock { dot: 432_000, p1: 3, p2: 1, n: 1, m1: 2, m2: 0x0819_999a, vco: 0 },
];

// upstream: intel_dpll_mgr.c bxt_ddi_hdmi_pll_dividers()
pub fn bxt_ddi_hdmi_pll_dividers<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port_clock: i32,
) -> Result<BxtDpllClock, i32> {
    let Some(clock) = hooks.bxt_find_best_dpll(port_clock) else {
        return Err(-22);
    };
    hooks.drm_warn_on(display.display_id, clock.m1 != 2, "BXT HDMI DPLL M1 must be 2");
    Ok(clock)
}

// upstream: intel_dpll_mgr.c bxt_ddi_dp_pll_dividers()
pub fn bxt_ddi_dp_pll_dividers<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port_clock: i32,
) -> BxtDpllClock {
    let mut clock = BXT_DP_CLOCKS[0];
    if let Some(value) = BXT_DP_CLOCKS.iter().find(|value| value.dot == port_clock) {
        clock = *value;
    }
    hooks.chv_calc_dpll_params(display.nssc_refclk_khz, &mut clock);
    hooks.drm_warn_on(display.display_id,
        clock.vco == 0 || clock.dot != port_clock,
        "Invalid BXT DP DPLL divider selection");
    clock
}

// upstream: intel_dpll_mgr.c bxt_ddi_set_dpll_hw_state()
pub fn bxt_ddi_set_dpll_hw_state<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    port_clock: i32,
    clock: &BxtDpllClock,
    hw: &mut BxtDpllHwState,
) -> i32 {
    let (prop, integ, gain, target) = if (6_200_000..=6_700_000).contains(&clock.vco) {
        (4, 9, 3, 8)
    } else if (clock.vco > 5_400_000 && clock.vco < 6_200_000)
        || (clock.vco >= 4_800_000 && clock.vco < 5_400_000)
    {
        (5, 11, 3, 9)
    } else if clock.vco == 5_400_000 {
        (3, 8, 1, 9)
    } else {
        hooks.drm_err(display.display_id, "Invalid VCO", 0);
        return -22;
    };
    let stagger = if port_clock > 270_000 { 0x18 }
        else if port_clock > 135_000 { 0x0d }
        else if port_clock > 67_000 { 0x07 }
        else if port_clock > 33_000 { 0x04 }
        else { 0x02 };

    hw.ebb0 = (clock.p1 << 13) | (clock.p2 << 8);
    hw.pll0 = (clock.m2 >> 22) & BXT_PLL_M2_INT_MASK;
    hw.pll1 = (clock.n << 8) & BXT_PLL_N_MASK;
    hw.pll2 = clock.m2 & BXT_PLL_M2_FRAC_MASK;
    if clock.m2 & BXT_PLL_M2_FRAC_MASK != 0 {
        hw.pll3 = BXT_PLL_M2_FRAC_ENABLE;
    }
    hw.pll6 = (gain << 16) | (integ << 8) | prop;
    hw.pll8 = target;
    hw.pll9 = 5 << 1;
    hw.pll10 = (15 << 10) | BXT_PLL_DCO_AMP_OVR_EN_H;
    hw.ebb4 = BXT_PLL_10BIT_CLK_ENABLE;
    hw.pcsdw12 = BXT_LANESTAGGER_STRAP_OVRD | stagger;
    0
}

// upstream: intel_dpll_mgr.c bxt_ddi_pll_get_freq()
pub fn bxt_ddi_pll_get_freq<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    hw: &BxtDpllHwState,
) -> i32 {
    let mut clock = BxtDpllClock {
        m1: 2,
        m2: ((hw.pll0 & BXT_PLL_M2_INT_MASK) << 22)
            | if hw.pll3 & BXT_PLL_M2_FRAC_ENABLE != 0 { hw.pll2 & BXT_PLL_M2_FRAC_MASK } else { 0 },
        n: (hw.pll1 & BXT_PLL_N_MASK) >> 8,
        p1: (hw.ebb0 & BXT_PLL_P1_MASK) >> 13,
        p2: (hw.ebb0 & BXT_PLL_P2_MASK) >> 8,
        ..Default::default()
    };
    hooks.chv_calc_dpll_params(display.nssc_refclk_khz, &mut clock)
}

// upstream: intel_dpll_mgr.c bxt_ddi_dp_set_dpll_hw_state()
pub fn bxt_ddi_dp_set_dpll_hw_state<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    crtc_state: &mut BxtCrtcState,
) -> i32 {
    let clock = bxt_ddi_dp_pll_dividers(hooks, display, crtc_state.port_clock);
    bxt_ddi_set_dpll_hw_state(hooks, display, crtc_state.port_clock, &clock, &mut crtc_state.dpll_hw_state)
}

// upstream: intel_dpll_mgr.c bxt_ddi_hdmi_set_dpll_hw_state()
pub fn bxt_ddi_hdmi_set_dpll_hw_state<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    crtc_state: &mut BxtCrtcState,
) -> i32 {
    let clock = bxt_ddi_hdmi_pll_dividers(hooks, display, crtc_state.port_clock).unwrap_or_default();
    let ret = bxt_ddi_set_dpll_hw_state(hooks, display, crtc_state.port_clock, &clock,
                                        &mut crtc_state.dpll_hw_state);
    if ret != 0 { return ret; }
    crtc_state.port_clock = bxt_ddi_pll_get_freq(hooks, display, &crtc_state.dpll_hw_state);
    0
}

// upstream: intel_dpll_mgr.c bxt_compute_dpll()
pub fn bxt_compute_dpll<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    crtc_state: &mut BxtCrtcState,
) -> i32 {
    if crtc_state.output == HswOutput::Hdmi {
        bxt_ddi_hdmi_set_dpll_hw_state(hooks, display, crtc_state)
    } else if crtc_state.output == HswOutput::DisplayPort {
        bxt_ddi_dp_set_dpll_hw_state(hooks, display, crtc_state)
    } else {
        -22
    }
}

// upstream: intel_dpll_mgr.c bxt_get_dpll()
pub fn bxt_get_dpll<H: BxtDpllHooks>(
    hooks: &mut H,
    display: &BxtDisplay,
    crtc: usize,
    port: u8,
    crtc_state: &mut BxtCrtcState,
) -> i32 {
    // BXT maps each output port to its own PLL.
    let Some(pll) = hooks.dpll_by_id(display.display_id, port) else { return -22; };
    hooks.reference_dpll(display.display_id, crtc, pll, &crtc_state.dpll_hw_state);
    crtc_state.intel_dpll = Some(pll);
    0
}

// upstream: intel_dpll_mgr.c bxt_update_dpll_ref_clks()
pub fn bxt_update_dpll_ref_clks(display: &mut BxtDisplay) {
    display.ssc_refclk_khz = 100_000;
    display.nssc_refclk_khz = 100_000;
}

// upstream: intel_dpll_mgr.c bxt_dump_hw_state()
pub fn bxt_dump_hw_state<H: BxtDpllHooks>(hooks: &mut H, display_id: usize, hw: &BxtDpllHwState) {
    let values = [hw.ebb0, hw.ebb4, hw.pll0, hw.pll1, hw.pll2, hw.pll3,
                  hw.pll6, hw.pll8, hw.pll9, hw.pll10, hw.pcsdw12];
    hooks.log_hw_state(display_id, "dpll_hw_state: BXT PLL", &values);
}

// upstream: intel_dpll_mgr.c bxt_compare_hw_state()
pub fn bxt_compare_hw_state(a: &BxtDpllHwState, b: &BxtDpllHwState) -> bool {
    a.ebb0 == b.ebb0 && a.ebb4 == b.ebb4 && a.pll0 == b.pll0 && a.pll1 == b.pll1
        && a.pll2 == b.pll2 && a.pll3 == b.pll3 && a.pll6 == b.pll6 && a.pll8 == b.pll8
        && a.pll10 == b.pll10 && a.pcsdw12 == b.pcsdw12
}

// upstream: intel_dpll_mgr.c mtl_get_non_tc_phy_dpll()
pub fn mtl_get_non_tc_phy_dpll<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    state: &mut MtlCrtcState,
) -> i32 {
    let port_dpll = state.port_dplls[ICL_PORT_DPLL_DEFAULT];
    let id = mtl_port_to_pll_id(hooks, display, display.encoders[encoder].port);
    let Some(pll) = hooks.find_dpll(
        display.display_id,
        crtc,
        &port_dpll.hw_state,
        1_u32.checked_shl(u32::from(id)).unwrap_or(0),
    ) else {
        return -22;
    };

    state.port_dplls[ICL_PORT_DPLL_DEFAULT].pll = Some(pll);
    hooks.reference_dpll(display.display_id, crtc, pll, &port_dpll.hw_state);
    hooks.icl_update_active_dpll(display.display_id, crtc, encoder);
    0
}

// upstream: intel_dpll_mgr.c get_intel_encoder()
pub fn get_intel_encoder<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
) -> Option<usize> {
    for index in 0..display.encoders.len() {
        let port = display.encoders[index].port;
        if mtl_port_to_pll_id(hooks, display, port) == pll_id {
            return Some(index);
        }
    }
    None
}

// upstream: intel_dpll_mgr.c mtl_pll_get_hw_state()
pub fn mtl_pll_get_hw_state<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &mut MtlPllHwState,
) -> bool {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        return false;
    };
    hooks.intel_cx0pll_readout_hw_state(encoder, &mut state.cx0pll)
}

// upstream: intel_dpll_mgr.c mtl_pll_get_freq()
pub fn mtl_pll_get_freq<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &MtlPllHwState,
) -> i32 {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return -22;
    };
    hooks.intel_cx0pll_calc_port_clock(encoder, &state.cx0pll)
}

// upstream: intel_dpll_mgr.c mtl_pll_enable()
pub fn mtl_pll_enable<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &MtlPllHwState,
) {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return;
    };
    hooks.intel_mtl_pll_enable(encoder, pll_id, state);
}

// upstream: intel_dpll_mgr.c mtl_pll_disable()
pub fn mtl_pll_disable<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
) {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return;
    };
    hooks.intel_mtl_pll_disable(encoder);
}

// upstream: intel_dpll_mgr.c mtl_tbt_pll_enable()
pub fn mtl_tbt_pll_enable(_hooks: &mut impl IntelDpllRemainderHooks, _pll_id: u8) {
    // MTL TBT PLL is always-on; the upstream callback intentionally has no
    // register writes.
}

// upstream: intel_dpll_mgr.c mtl_tbt_pll_disable()
pub fn mtl_tbt_pll_disable(_hooks: &mut impl IntelDpllRemainderHooks, _pll_id: u8) {
    // MTL TBT PLL is always-on; the upstream callback intentionally has no
    // register writes.
}

// upstream: intel_dpll_mgr.c mtl_tbt_pll_get_freq()
pub fn mtl_tbt_pll_get_freq<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    _pll_id: u8,
    _state: &MtlPllHwState,
) -> i32 {
    // The TBT PLL produces several simultaneous clocks.  Selection occurs in
    // the DDI clock mux, so the source deliberately warns and returns zero.
    hooks.drm_warn_on(display.display_id, true, "TBT PLL exposes multiple mux-selected clocks");
    0
}

// upstream: intel_dpll_mgr.c mtl_compute_non_tc_phy_dpll()
pub fn mtl_compute_non_tc_phy_dpll<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    crtc: usize,
    encoder: usize,
    state: &mut MtlCrtcState,
) -> i32 {
    let port_dpll = &mut state.port_dplls[ICL_PORT_DPLL_DEFAULT];
    let ret = hooks.intel_cx0pll_calc_state(crtc, encoder, &mut port_dpll.hw_state);
    if ret != 0 {
        return ret;
    }
    // This is mainly for the fastset check in the upstream sequence.
    hooks.icl_set_active_port_dpll(crtc, ICL_PORT_DPLL_DEFAULT);
    let port_clock = hooks.intel_cx0pll_calc_port_clock(encoder, &port_dpll.hw_state.cx0pll);
    state.port_clock = port_clock;
    0
}

// upstream: intel_dpll_mgr.c mtl_compute_tc_phy_dplls()
pub fn mtl_compute_tc_phy_dplls<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    crtc: usize,
    encoder: usize,
    old_crtc: &MtlCrtcState,
    state: &mut MtlCrtcState,
) -> i32 {
    let default = &mut state.port_dplls[ICL_PORT_DPLL_DEFAULT];
    hooks.intel_mtl_tbt_pll_calc_state(&mut default.hw_state);

    let mg_phy = &mut state.port_dplls[ICL_PORT_DPLL_MG_PHY];
    let ret = hooks.intel_cx0pll_calc_state(crtc, encoder, &mut mg_phy.hw_state);
    if ret != 0 {
        return ret;
    }

    // Retain the default (TBT) PLL for fastset only when it was the old active
    // DPLL; otherwise use the newly computed MG PHY PLL.
    let index = if old_crtc.old_dpll_id == Some(DPLL_ID_ICL_TBTPLL) {
        ICL_PORT_DPLL_DEFAULT
    } else {
        ICL_PORT_DPLL_MG_PHY
    };
    hooks.icl_set_active_port_dpll(crtc, index);
    state.active_port_dpll = index;
    let port_clock = hooks.intel_cx0pll_calc_port_clock(encoder, &mg_phy.hw_state.cx0pll);
    state.port_clock = port_clock;
    0
}

// upstream: intel_dpll_mgr.c mtl_compute_dplls()
pub fn mtl_compute_dplls<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    old_crtc: &MtlCrtcState,
    state: &mut MtlCrtcState,
) -> i32 {
    if display.encoders.get(encoder).is_some_and(|e| e.type_c) {
        mtl_compute_tc_phy_dplls(hooks, crtc, encoder, old_crtc, state)
    } else {
        mtl_compute_non_tc_phy_dpll(hooks, crtc, encoder, state)
    }
}

// upstream: intel_dpll_mgr.c mtl_get_dplls()
pub fn mtl_get_dplls<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    state: &mut MtlCrtcState,
) -> i32 {
    if display.encoders.get(encoder).is_some_and(|e| e.type_c) {
        hooks.icl_get_tc_phy_dplls(display.display_id, crtc, encoder)
    } else {
        mtl_get_non_tc_phy_dpll(hooks, display, crtc, encoder, state)
    }
}

// upstream: intel_dpll_mgr.c mtl_dump_hw_state()
pub fn mtl_dump_hw_state<H: IntelDpllRemainderHooks>(hooks: &mut H, state: &MtlPllHwState) {
    hooks.intel_cx0pll_dump_hw_state(&state.cx0pll);
}

// upstream: intel_dpll_mgr.c mtl_compare_hw_state()
pub fn mtl_compare_hw_state<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    a: &MtlPllHwState,
    b: &MtlPllHwState,
) -> bool {
    hooks.intel_cx0pll_compare_hw_state(&a.cx0pll, &b.cx0pll)
}

// upstream: intel_dpll_mgr.c xe3plpd_pll_get_hw_state()
pub fn xe3plpd_pll_get_hw_state<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &mut MtlPllHwState,
) -> bool {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        return false;
    };
    hooks.intel_lt_phy_pll_readout_hw_state(encoder, &mut state.ltpll)
}

// upstream: intel_dpll_mgr.c xe3plpd_pll_get_freq()
pub fn xe3plpd_pll_get_freq<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &MtlPllHwState,
) -> i32 {
    let Some(_encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return -22;
    };
    hooks.intel_lt_phy_calc_port_clock(display.display_id, &state.ltpll)
}

// upstream: intel_dpll_mgr.c xe3plpd_pll_enable()
pub fn xe3plpd_pll_enable<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
    state: &MtlPllHwState,
) {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return;
    };
    hooks.intel_xe3plpd_pll_enable(encoder, pll_id, state);
}

// upstream: intel_dpll_mgr.c xe3plpd_pll_disable()
pub fn xe3plpd_pll_disable<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    pll_id: u8,
) {
    let Some(encoder) = get_intel_encoder(hooks, display, pll_id) else {
        hooks.drm_warn_on(display.display_id, true, "DPLL has no matching encoder");
        return;
    };
    hooks.intel_xe3plpd_pll_disable(encoder);
}

// upstream: intel_dpll_mgr.c xe3plpd_compute_non_tc_phy_dpll()
pub fn xe3plpd_compute_non_tc_phy_dpll<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    state: &mut MtlCrtcState,
) -> i32 {
    let port_dpll = &mut state.port_dplls[ICL_PORT_DPLL_DEFAULT];
    let ret = hooks.intel_lt_phy_pll_calc_state(crtc, encoder, &mut port_dpll.hw_state);
    if ret != 0 {
        return ret;
    }
    hooks.icl_set_active_port_dpll(crtc, ICL_PORT_DPLL_DEFAULT);
    let port_clock = hooks.intel_lt_phy_calc_port_clock(display.display_id, &port_dpll.hw_state.ltpll);
    state.port_clock = port_clock;
    0
}

// upstream: intel_dpll_mgr.c xe3plpd_compute_tc_phy_dplls()
pub fn xe3plpd_compute_tc_phy_dplls<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    old_crtc: &MtlCrtcState,
    state: &mut MtlCrtcState,
) -> i32 {
    let default = &mut state.port_dplls[ICL_PORT_DPLL_DEFAULT];
    hooks.intel_lt_phy_tbt_pll_calc_state(&mut default.hw_state);

    let lt_phy = &mut state.port_dplls[ICL_PORT_DPLL_MG_PHY];
    let ret = hooks.intel_lt_phy_pll_calc_state(crtc, encoder, &mut lt_phy.hw_state);
    if ret != 0 {
        return ret;
    }

    let index = if old_crtc.old_dpll_id == Some(DPLL_ID_ICL_TBTPLL) {
        ICL_PORT_DPLL_DEFAULT
    } else {
        ICL_PORT_DPLL_MG_PHY
    };
    hooks.icl_set_active_port_dpll(crtc, index);
    state.active_port_dpll = index;
    let port_clock = hooks.intel_lt_phy_calc_port_clock(display.display_id, &lt_phy.hw_state.ltpll);
    state.port_clock = port_clock;
    0
}

// upstream: intel_dpll_mgr.c xe3plpd_compute_dplls()
pub fn xe3plpd_compute_dplls<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    display: &MtlDisplay<'_>,
    crtc: usize,
    encoder: usize,
    old_crtc: &MtlCrtcState,
    state: &mut MtlCrtcState,
) -> i32 {
    if display.encoders.get(encoder).is_some_and(|e| e.type_c) {
        xe3plpd_compute_tc_phy_dplls(hooks, display, crtc, encoder, old_crtc, state)
    } else {
        xe3plpd_compute_non_tc_phy_dpll(hooks, display, crtc, encoder, state)
    }
}

// upstream: intel_dpll_mgr.c xe3plpd_dump_hw_state()
pub fn xe3plpd_dump_hw_state<H: IntelDpllRemainderHooks>(hooks: &mut H, state: &MtlPllHwState) {
    hooks.intel_lt_phy_dump_hw_state(&state.ltpll);
}

// upstream: intel_dpll_mgr.c xe3plpd_compare_hw_state()
pub fn xe3plpd_compare_hw_state<H: IntelDpllRemainderHooks>(
    hooks: &mut H,
    a: &MtlPllHwState,
    b: &MtlPllHwState,
) -> bool {
    hooks.intel_lt_phy_pll_compare_hw_state(&a.ltpll, &b.ltpll)
}
