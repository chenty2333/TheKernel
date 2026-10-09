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
 * The above copyright notice and this permission notice (including the next
 * paragraph) shall be included in all copies or substantial portions of the
 * Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */

//! Linux 7.2.3 `intel_dpll_mgr.c` display-12/13 shared-DPLL translation.
//! Kernel atomic, framework, power, MMIO, and indexed PHY access is explicit
//! through `IntelDpllHooks`; no register access is performed implicitly.
#![allow(dead_code, non_camel_case_types, non_snake_case, clippy::too_many_arguments)]

use core::mem::swap;

pub const MAX_DPLLS: usize = 9;
pub const MAX_PIPES: usize = 8;
pub const DPLL_ID_PRIVATE: i32 = -1;
pub const DPLL_ID_ICL_DPLL0: DpllId = DpllId(0);
pub const DPLL_ID_ICL_DPLL1: DpllId = DpllId(1);
pub const DPLL_ID_ICL_TBTPLL: DpllId = DpllId(2);
pub const DPLL_ID_ICL_MGPLL1: DpllId = DpllId(3);
pub const DPLL_ID_ICL_MGPLL2: DpllId = DpllId(4);
pub const DPLL_ID_ICL_MGPLL3: DpllId = DpllId(5);
pub const DPLL_ID_ICL_MGPLL4: DpllId = DpllId(6);
pub const DPLL_ID_TGL_MGPLL5: DpllId = DpllId(7);
pub const DPLL_ID_TGL_MGPLL6: DpllId = DpllId(8);
pub const DPLL_ID_DG1_DPLL0: DpllId = DpllId(0);
pub const DPLL_ID_DG1_DPLL1: DpllId = DpllId(1);
pub const DPLL_ID_DG1_DPLL2: DpllId = DpllId(2);
pub const DPLL_ID_DG1_DPLL3: DpllId = DpllId(3);
pub const DPLL_ID_EHL_DPLL4: DpllId = DpllId(2);

const PLL_ENABLE: u32 = 1 << 31;
const PLL_LOCK: u32 = 1 << 30;
const PLL_POWER_ENABLE: u32 = 1 << 27;
const PLL_POWER_STATE: u32 = 1 << 26;
const TGL_DPLL0_DIV0_AFC_STARTUP_MASK: u32 = 0x7 << 25;
const DPLL_CFGCR0_DCO_FRACTION_MASK: u32 = 0x7fff << 10;
const DPLL_CFGCR0_DCO_FRACTION_SHIFT: u32 = 10;
const DPLL_CFGCR0_DCO_INTEGER_MASK: u32 = 0x3ff;
const DPLL_CFGCR1_QDIV_RATIO_MASK: u32 = 0xff << 10;
const DPLL_CFGCR1_QDIV_RATIO_SHIFT: u32 = 10;
const DPLL_CFGCR1_QDIV_MODE_SHIFT: u32 = 9;
const DPLL_CFGCR1_KDIV_MASK: u32 = 7 << 6;
const DPLL_CFGCR1_KDIV_SHIFT: u32 = 6;
const DPLL_CFGCR1_PDIV_MASK: u32 = 0xf << 2;
const DPLL_CFGCR1_PDIV_SHIFT: u32 = 2;
const DPLL_CFGCR1_KDIV_1: u32 = 1 << 6;
const DPLL_CFGCR1_KDIV_2: u32 = 2 << 6;
const DPLL_CFGCR1_KDIV_3: u32 = 4 << 6;
const DPLL_CFGCR1_PDIV_2: u32 = 1 << 2;
const DPLL_CFGCR1_PDIV_3: u32 = 2 << 2;
const DPLL_CFGCR1_PDIV_5: u32 = 4 << 2;
const DPLL_CFGCR1_PDIV_7: u32 = 8 << 2;
const DPLL_CFGCR1_CENTRAL_FREQ_8400: u32 = 3;
const TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL: u32 = 0;
const DPLL_CFGCR1_QDIV_MODE_MASK: u32 = 1 << DPLL_CFGCR1_QDIV_MODE_SHIFT;
const fn dpll_cfgcr1_qdiv_ratio(v: u32) -> u32 {
    v << DPLL_CFGCR1_QDIV_RATIO_SHIFT
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpllId(pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcPort {
    Tc1 = 0,
    Tc2 = 1,
    Tc3 = 2,
    Tc4 = 3,
    Tc5 = 4,
    Tc6 = 5,
}

impl TcPort {
    pub const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Port {
    A,
    B,
    C,
    D,
    E,
    F,
    Tc(TcPort),
    Other(u8),
}

impl Port {
    const fn as_tc_port(self) -> Option<TcPort> {
        match self {
            Self::Tc(port) => Some(port),
            Self::C => Some(TcPort::Tc1),
            Self::D => Some(TcPort::Tc2),
            Self::E => Some(TcPort::Tc3),
            Self::F => Some(TcPort::Tc4),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputType {
    DisplayPort,
    DisplayPortMst,
    Hdmi,
    Dsi,
    Other,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IclDpllHwState {
    pub cfgcr0: u32,
    pub cfgcr1: u32,
    pub div0: u32,
    pub mg_refclkin_ctl: u32,
    pub mg_clktop2_coreclkctl1: u32,
    pub mg_clktop2_hsclkctl: u32,
    pub mg_pll_div0: u32,
    pub mg_pll_div1: u32,
    pub mg_pll_lf: u32,
    pub mg_pll_frac_lock: u32,
    pub mg_pll_ssc: u32,
    pub mg_pll_bias: u32,
    pub mg_pll_tdc_coldst_bias: u32,
    pub mg_pll_bias_mask: u32,
    pub mg_pll_tdc_coldst_bias_mask: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelDpllHwState {
    pub icl: IclDpllHwState,
    pub i9xx: I9xxDpllHwState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct I9xxDpllHwState {
    pub dpll: u32,
    pub dpll_md: u32,
    pub fp0: u32,
    pub fp1: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelDpllState {
    pub pipe_mask: u8,
    pub hw_state: IntelDpllHwState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpllFunction {
    Combo,
    Tbt,
    Mg,
    Dkl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerDomain {
    DisplayCore,
    DcOff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DpllInfo {
    pub name: &'static str,
    pub funcs: DpllFunction,
    pub id: DpllId,
    pub power_domain: Option<PowerDomain>,
    pub always_on: bool,
    pub is_alt_port_dpll: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelDpll {
    pub state: IntelDpllState,
    pub index: u8,
    pub active_mask: u8,
    pub on: bool,
    pub info: Option<DpllInfo>,
    pub wakeref: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortDpllId {
    Default = 0,
    MgPhy = 1,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IclPortDpll {
    pub pll: Option<usize>,
    pub hw_state: IntelDpllHwState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcState {
    pub id: i32,
    pub name: &'static str,
    pub pipe: u8,
    pub joined_pipe_mask: u8,
    pub hw_active: bool,
    pub intel_dpll: Option<usize>,
    pub dpll_hw_state: IntelDpllHwState,
    pub icl_port_dplls: [IclPortDpll; 2],
    pub port_clock: u32,
    pub output: OutputType,
    pub port: Port,
    pub dsc: bool,
}

impl Default for CrtcState {
    fn default() -> Self {
        Self {
            id: -1,
            name: "",
            pipe: 0,
            joined_pipe_mask: 1,
            hw_active: false,
            intel_dpll: None,
            dpll_hw_state: IntelDpllHwState::default(),
            icl_port_dplls: [IclPortDpll::default(); 2],
            port_clock: 0,
            output: OutputType::Other,
            port: Port::A,
            dsc: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntelCrtc {
    pub id: i32,
    pub name: &'static str,
    pub pipe: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelEncoder {
    pub output: OutputType,
    pub port: Port,
    pub is_combo_phy: bool,
    pub is_tc_phy: bool,
    pub primary_port: Option<Port>,
    pub tc_dp_alt_mode: bool,
    pub tc_legacy_mode: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelAtomicState {
    pub dpll_set: bool,
    pub dpll_state: [IntelDpllState; MAX_DPLLS],
    pub old_crtcs: [CrtcState; MAX_PIPES],
    pub new_crtcs: [CrtcState; MAX_PIPES],
}

impl Default for IntelAtomicState {
    fn default() -> Self {
        Self {
            dpll_set: false,
            dpll_state: [IntelDpllState::default(); MAX_DPLLS],
            old_crtcs: [CrtcState::default(); MAX_PIPES],
            new_crtcs: [CrtcState::default(); MAX_PIPES],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayPlatform {
    pub dg2: bool,
    pub alderlake_p: bool,
    pub alderlake_s: bool,
    pub dg1: bool,
    pub rocketlake: bool,
    pub jasperlake: bool,
    pub elkhartlake: bool,
    pub geminilake: bool,
    pub broxton: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefClocks {
    pub nssc: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VbtOverrides {
    pub override_afc_startup: bool,
    pub override_afc_startup_val: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpllManagerKind {
    Tgl,
    RocketLake,
    Dg1,
    AlderLakeS,
    AlderLakeP,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntelDpllMgr {
    pub kind: DpllManagerKind,
    pub dpll_info: &'static [DpllInfo],
    pub has_update_active_dpll: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntelDpllDisplay {
    pub display_id: usize,
    pub display_ver: u8,
    pub platform: DisplayPlatform,
    pub dplls: [IntelDpll; MAX_DPLLS],
    pub num_dpll: usize,
    pub mgr: Option<&'static IntelDpllMgr>,
    pub ref_clks: RefClocks,
    pub cdclk_ref: u32,
    pub vbt: VbtOverrides,
    pub pipe_active: [bool; MAX_PIPES],
    pub crtc_states: [CrtcState; MAX_PIPES],
}

impl Default for IntelDpllDisplay {
    fn default() -> Self {
        Self {
            display_id: 0,
            display_ver: 0,
            platform: DisplayPlatform::default(),
            dplls: [IntelDpll::default(); MAX_DPLLS],
            num_dpll: 0,
            mgr: None,
            ref_clks: RefClocks::default(),
            cdclk_ref: 0,
            vbt: VbtOverrides::default(),
            pipe_active: [false; MAX_PIPES],
            crtc_states: [CrtcState::default(); MAX_PIPES],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComboEnableFamily {
    Icl,
    Dg1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CfgRegisterFamily {
    Icl,
    Dg1,
    AlderLakeS,
    RocketLake,
    TigerLake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MgRegister {
    RefclkInCtl,
    CoreClkCtl1,
    HsClkCtl,
    PllDiv0,
    PllDiv1,
    PllLf,
    PllFracLock,
    PllSsc,
    PllBias,
    PllTdcColdStartBias,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpllRegister {
    ComboEnable(ComboEnableFamily, DpllId),
    MgEnable(TcPort),
    AdlpTcEnable(TcPort),
    TbtEnable,
    CfgCr0(CfgRegisterFamily, DpllId),
    CfgCr1(CfgRegisterFamily, DpllId),
    TglDpllDiv0(DpllId),
    Mg(TcPort, MgRegister),
    TransCmtgChicken,
}

/// Kernel/DRM/framework boundary used by the source-order translation.
/// MMIO, DKL indexed PHY access, power references, atomic locking and state
/// diagnostics are separate operations so adapters cannot hide sequencing.
pub trait IntelDpllHooks {
    fn read32(&mut self, register: DpllRegister) -> u32;
    fn write32(&mut self, register: DpllRegister, value: u32);
    fn rmw32(&mut self, register: DpllRegister, clear: u32, set: u32) -> u32 {
        let old = self.read32(register);
        let value = (old & !clear) | set;
        self.write32(register, value);
        value
    }
    fn posting_read32(&mut self, register: DpllRegister);
    fn read_dkl(&mut self, port: TcPort, offset: u16) -> u32;
    fn write_dkl(&mut self, port: TcPort, offset: u16, value: u32);
    fn posting_read_dkl(&mut self, port: TcPort, offset: u16);
    fn wait_for_set(&mut self, register: DpllRegister, mask: u32, timeout_ms: u32) -> bool;
    fn wait_for_clear(&mut self, register: DpllRegister, mask: u32, timeout_ms: u32) -> bool;
    fn power_get(&mut self, display_id: usize, domain: PowerDomain) -> u64;
    fn power_get_if_enabled(&mut self, display_id: usize, domain: PowerDomain) -> Option<u64>;
    fn power_put(&mut self, display_id: usize, domain: PowerDomain, cookie: u64);

    fn dpll_mutex_init(&mut self, display_id: usize);
    fn dpll_mutex_lock(&mut self, display_id: usize);
    fn dpll_mutex_unlock(&mut self, display_id: usize);
    fn connection_mutex_is_locked(&mut self, display_id: usize) -> bool;
    fn drm_debug(&mut self, display_id: usize, message: &str);
    fn drm_error(&mut self, display_id: usize, message: &str);
    fn drm_warn(&mut self, display_id: usize, message: &str);
    fn drm_warn_on(&mut self, display_id: usize, condition: bool, message: &str) -> bool;
    fn display_state_warn(&mut self, display_id: usize, condition: bool, message: &str) -> bool;
    fn missing_case(&mut self, display_id: usize, value: u32);
    fn hti_dpll_mask(&mut self, display_id: usize) -> u32;
    fn is_adlp_step_a0_to_b0(&mut self, display_id: usize) -> bool;
    fn intel_cx0pll_verify_plls(&mut self, display_id: usize);
    fn intel_lt_phy_verify_plls(&mut self, display_id: usize);
    fn intel_cx0_pll_power_save_wa(&mut self, display_id: usize);
    fn log_hw_state(&mut self, display_id: usize, title: &str, state: &IntelDpllHwState);
}

const MG_REFCLKIN_CTL_OD_2_MUX_MASK: u32 = 0x7 << 8;
const MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK: u32 = 0xff << 8;
const MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK: u32 = 1 << 16;
const MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK: u32 = 3 << 14;
const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK: u32 = 3 << 12;
const MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK: u32 = 0xf << 8;
const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2: u32 = 0 << 12;
const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3: u32 = 1 << 12;
const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5: u32 = 2 << 12;
const MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7: u32 = 3 << 12;
const MG_PLL_DIV0_FRACNEN_H: u32 = 1 << 30;
const MG_PLL_DIV0_FBDIV_FRAC_MASK: u32 = 0x3f_ffff << 8;
const MG_PLL_DIV0_FBDIV_FRAC_SHIFT: u32 = 8;
const MG_PLL_DIV0_FBDIV_INT_MASK: u32 = 0xff;
const MG_PLL_DIV1_FBPREDIV_MASK: u32 = 0xf;
const fn MG_PLL_DIV1_IREF_NDIVRATIO(v: u32) -> u32 { v << 16 }
const MG_PLL_DIV1_DITHER_DIV_2: u32 = 1 << 12;
const fn MG_PLL_DIV1_NDIVRATIO(v: u32) -> u32 { v << 4 }
const fn MG_PLL_LF_TDCTARGETCNT(v: u32) -> u32 { v << 24 }
const MG_PLL_LF_AFCCNTSEL_512: u32 = 1 << 20;
const fn MG_PLL_LF_GAINCTRL(v: u32) -> u32 { v << 16 }
const fn MG_PLL_LF_INT_COEFF(v: u32) -> u32 { v << 8 }
const fn MG_PLL_LF_PROP_COEFF(v: u32) -> u32 { v }
const MG_PLL_FRAC_LOCK_TRUELOCK_CRIT_32: u32 = 1 << 18;
const MG_PLL_FRAC_LOCK_EARLYLOCK_CRIT_32: u32 = 1 << 16;
const fn MG_PLL_FRAC_LOCK_LOCKTHRESH(v: u32) -> u32 { v << 11 }
const MG_PLL_FRAC_LOCK_DCODITHEREN: u32 = 1 << 10;
const MG_PLL_FRAC_LOCK_FEEDFWRDCAL_EN: u32 = 1 << 8;
const fn MG_PLL_FRAC_LOCK_FEEDFWRDGAIN(v: u32) -> u32 { v }
const MG_PLL_SSC_EN: u32 = 1 << 28;
const fn MG_PLL_SSC_TYPE(v: u32) -> u32 { v << 26 }
const fn MG_PLL_SSC_STEPLENGTH(v: u64) -> u32 { (v as u32) << 16 }
const fn MG_PLL_SSC_STEPNUM(v: u64) -> u32 { (v as u32) << 10 }
const MG_PLL_SSC_FLLEN: u32 = 1 << 9;
const fn MG_PLL_SSC_STEPSIZE(v: u64) -> u32 { v as u32 }
const fn MG_PLL_BIAS_BIAS_GB_SEL(v: u32) -> u32 { v << 30 }
const fn MG_PLL_BIAS_INIT_DCOAMP(v: u32) -> u32 { v << 24 }
const fn MG_PLL_BIAS_BIAS_BONUS(v: u32) -> u32 { v << 16 }
const MG_PLL_BIAS_BIASCAL_EN: u32 = 1 << 15;
const fn MG_PLL_BIAS_CTRIM(v: u32) -> u32 { v << 8 }
const fn MG_PLL_BIAS_VREF_RDAC(v: u32) -> u32 { v << 5 }
const fn MG_PLL_BIAS_IREFTRIM(v: u32) -> u32 { v }
const MG_PLL_TDC_COLDST_IREFINT_EN: u32 = 1 << 27;
const fn MG_PLL_TDC_COLDST_REFBIAS_START_PULSE_W(v: u32) -> u32 { v << 17 }
const MG_PLL_TDC_COLDST_COLDSTART: u32 = 1 << 16;
const MG_PLL_TDC_TDCOVCCORR_EN: u32 = 1 << 2;
const fn MG_PLL_TDC_TDCSEL(v: u32) -> u32 { v }
const DKL_PLL_DIV0_AFC_STARTUP_MASK: u32 = 7 << 25;
const fn DKL_PLL_DIV0_INTEG_COEFF(v: u32) -> u32 { v << 16 }
const fn DKL_PLL_DIV0_PROP_COEFF(v: u32) -> u32 { v << 12 }
const fn DKL_PLL_DIV0_FBPREDIV(v: u32) -> u32 { v << 8 }
const fn DKL_PLL_DIV0_FBDIV_INT(v: u32) -> u32 { v }
const DKL_PLL_DIV0_MASK: u32 = (0x1f << 16) | (0xf << 12) | (0xf << 8) | 0xff;
const DKL_PLL_DIV0_FBPREDIV_MASK: u32 = 0xf << 8;
const DKL_PLL_DIV0_FBDIV_INT_MASK: u32 = 0xff;
const fn DKL_PLL_DIV1_IREF_TRIM(v: u32) -> u32 { v << 16 }
const fn DKL_PLL_DIV1_TDC_TARGET_CNT(v: u32) -> u32 { v }
const fn DKL_PLL_SSC_IREF_NDIV_RATIO(v: u32) -> u32 { v << 29 }
const fn DKL_PLL_SSC_STEP_LEN(v: u64) -> u32 { (v as u32) << 16 }
const fn DKL_PLL_SSC_STEP_NUM(v: u64) -> u32 { (v as u32) << 11 }
const DKL_PLL_SSC_EN: u32 = 1 << 9;
const DKL_PLL_BIAS_FRAC_EN_H: u32 = 1 << 30;
const DKL_PLL_BIAS_FBDIV_FRAC_MASK: u32 = 0x3f_ffff << 8;
const DKL_PLL_BIAS_FBDIV_SHIFT: u32 = 8;
const fn DKL_PLL_BIAS_FBDIV_FRAC(v: u32) -> u32 { v << 8 }
const fn DKL_PLL_TDC_SSC_STEP_SIZE(v: u64) -> u32 { (v as u32) << 8 }
const fn DKL_PLL_TDC_FEED_FWD_GAIN(v: u32) -> u32 { v }

// upstream: intel_dpll_mgr.c intel_atomic_duplicate_dpll_state()
pub fn intel_atomic_duplicate_dpll_state(
    display: &IntelDpllDisplay,
    dpll_state: &mut [IntelDpllState; MAX_DPLLS],
) {
    for pll in display.dplls.iter().take(display.num_dpll) {
        dpll_state[usize::from(pll.index)] = pll.state;
    }
}

// upstream: intel_dpll_mgr.c intel_atomic_get_dpll_state()
pub fn intel_atomic_get_dpll_state<'a, H: IntelDpllHooks>(
    display: &IntelDpllDisplay,
    state: &'a mut IntelAtomicState,
    hooks: &mut H,
) -> &'a mut [IntelDpllState; MAX_DPLLS] {
    let connection_locked = hooks.connection_mutex_is_locked(display.display_id);
    hooks.drm_warn_on(
        display.display_id,
        !connection_locked,
        "DPLL atomic state requested without connection mutex",
    );
    if !state.dpll_set {
        state.dpll_set = true;
        intel_atomic_duplicate_dpll_state(display, &mut state.dpll_state);
    }
    &mut state.dpll_state
}

// upstream: intel_dpll_mgr.c intel_get_dpll_by_id()
pub fn intel_get_dpll_by_id(display: &IntelDpllDisplay, id: DpllId) -> Option<usize> {
    display
        .dplls
        .iter()
        .take(display.num_dpll)
        .position(|pll| pll.info.is_some_and(|info| info.id == id))
}

// upstream: intel_dpll_mgr.c assert_dpll()
pub fn assert_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: Option<usize>,
    expected_state: bool,
) {
    let Some(index) = pll_index else {
        hooks.drm_warn(display.display_id, "asserting DPLL with no DPLL");
        return;
    };
    let mut hw_state = IntelDpllHwState::default();
    let current_state = intel_dpll_get_hw_state(hooks, display, index, &mut hw_state);
    let pll_name = display.dplls[index].info.map_or("unknown", |info| info.name);
    hooks.display_state_warn(
        display.display_id,
        current_state != expected_state,
        if expected_state {
            "DPLL enabled assertion failure"
        } else {
            "DPLL disabled assertion failure"
        },
    );
    if current_state != expected_state {
        hooks.drm_debug(display.display_id, pll_name);
    }
}

// upstream: intel_dpll_mgr.c icl_pll_id_to_tc_port()
fn icl_pll_id_to_tc_port(id: DpllId) -> Option<TcPort> {
    id.0.checked_sub(DPLL_ID_ICL_MGPLL1.0).and_then(|index| match index {
        0 => Some(TcPort::Tc1),
        1 => Some(TcPort::Tc2),
        2 => Some(TcPort::Tc3),
        3 => Some(TcPort::Tc4),
        4 => Some(TcPort::Tc5),
        5 => Some(TcPort::Tc6),
        _ => None,
    })
}

// upstream: intel_dpll_mgr.c icl_tc_port_to_pll_id()
pub const fn icl_tc_port_to_pll_id(tc_port: TcPort) -> DpllId {
    DpllId(DPLL_ID_ICL_MGPLL1.0 + tc_port as u8)
}

// upstream: intel_dpll_mgr.c intel_combo_pll_enable_reg()
fn intel_combo_pll_enable_reg(display: &IntelDpllDisplay, pll: &IntelDpll) -> DpllRegister {
    let id = pll.info.map_or(DPLL_ID_ICL_DPLL0, |info| info.id);
    if display.platform.dg1 {
        DpllRegister::ComboEnable(ComboEnableFamily::Dg1, id)
    } else if (display.platform.jasperlake || display.platform.elkhartlake)
        && id == DPLL_ID_EHL_DPLL4
    {
        DpllRegister::MgEnable(TcPort::Tc1)
    } else {
        DpllRegister::ComboEnable(ComboEnableFamily::Icl, id)
    }
}

// upstream: intel_dpll_mgr.c intel_tc_pll_enable_reg()
fn intel_tc_pll_enable_reg(display: &IntelDpllDisplay, pll: &IntelDpll) -> DpllRegister {
    let id = pll.info.map_or(DPLL_ID_ICL_MGPLL1, |info| info.id);
    let tc_port = icl_pll_id_to_tc_port(id).unwrap_or(TcPort::Tc1);
    if display.platform.alderlake_p {
        DpllRegister::AdlpTcEnable(tc_port)
    } else {
        DpllRegister::MgEnable(tc_port)
    }
}

// upstream: intel_dpll_mgr.c _intel_enable_shared_dpll()
fn _intel_enable_shared_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
) {
    let pll_info = display.dplls[pll_index].info;
    if let Some(info) = pll_info {
        if let Some(domain) = info.power_domain {
            display.dplls[pll_index].wakeref = hooks.power_get(display.display_id, domain);
        }
    }
    let hw_state = display.dplls[pll_index].state.hw_state;
    enable_dpll_function(hooks, display, pll_index, &hw_state);
    display.dplls[pll_index].on = true;
}

// upstream: intel_dpll_mgr.c _intel_disable_shared_dpll()
fn _intel_disable_shared_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
) {
    let pll_info = display.dplls[pll_index].info;
    disable_dpll_function(hooks, display, pll_index);
    display.dplls[pll_index].on = false;
    if let Some(info) = pll_info {
        if let Some(domain) = info.power_domain {
            hooks.power_put(display.display_id, domain, display.dplls[pll_index].wakeref);
        }
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_enable()
pub fn intel_dpll_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    crtc_state: &CrtcState,
) {
    let Some(pll_index) = crtc_state.intel_dpll else {
        hooks.drm_warn(display.display_id, "CRTC has no shared DPLL to enable");
        return;
    };
    let pipe_mask = crtc_state.joined_pipe_mask;
    hooks.dpll_mutex_lock(display.display_id);
    let old_mask = display.dplls[pll_index].active_mask;
    let referenced = display.dplls[pll_index].state.pipe_mask & pipe_mask != 0;
    let already_active = display.dplls[pll_index].active_mask & pipe_mask != 0;
    if hooks.drm_warn_on(display.display_id, !referenced, "DPLL not reserved for CRTC")
        || hooks.drm_warn_on(display.display_id, already_active, "DPLL already active for CRTC")
    {
        hooks.dpll_mutex_unlock(display.display_id);
        return;
    }
    display.dplls[pll_index].active_mask |= pipe_mask;
    let info = display.dplls[pll_index].info;
    hooks.drm_debug(
        display.display_id,
        info.map_or("enable unknown PLL", |item| item.name),
    );
    if old_mask != 0 {
        hooks.drm_warn_on(
            display.display_id,
            !display.dplls[pll_index].on,
            "shared DPLL active but not enabled",
        );
        assert_dpll(hooks, display, Some(pll_index), true);
        hooks.dpll_mutex_unlock(display.display_id);
        return;
    }
    hooks.drm_warn_on(
        display.display_id,
        display.dplls[pll_index].on,
        "DPLL unexpectedly on before first active user",
    );
    _intel_enable_shared_dpll(hooks, display, pll_index);
    hooks.dpll_mutex_unlock(display.display_id);
}

// upstream: intel_dpll_mgr.c intel_dpll_disable()
pub fn intel_dpll_disable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    crtc_state: &CrtcState,
) {
    if display.display_ver < 5 {
        return;
    }
    let Some(pll_index) = crtc_state.intel_dpll else {
        return;
    };
    let pipe_mask = crtc_state.joined_pipe_mask;
    hooks.dpll_mutex_lock(display.display_id);
    if hooks.drm_warn_on(
        display.display_id,
        display.dplls[pll_index].active_mask & pipe_mask == 0,
        "DPLL not active for CRTC disable",
    ) {
        hooks.dpll_mutex_unlock(display.display_id);
        return;
    }
    hooks.drm_debug(
        display.display_id,
        display.dplls[pll_index].info.map_or("disable unknown PLL", |item| item.name),
    );
    assert_dpll(hooks, display, Some(pll_index), true);
    hooks.drm_warn_on(
        display.display_id,
        !display.dplls[pll_index].on,
        "DPLL tracked active but not on",
    );
    display.dplls[pll_index].active_mask &= !pipe_mask;
    if display.dplls[pll_index].active_mask == 0 {
        _intel_disable_shared_dpll(hooks, display, pll_index);
    }
    hooks.dpll_mutex_unlock(display.display_id);
}

// upstream: intel_dpll_mgr.c intel_dpll_mask_all()
fn intel_dpll_mask_all<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
) -> u32 {
    let mut dpll_mask = 0_u32;
    for pll in display.dplls.iter().take(display.num_dpll) {
        if let Some(info) = pll.info {
            hooks.drm_warn_on(
                display.display_id,
                dpll_mask & (1_u32 << info.id.0) != 0,
                "duplicate shared DPLL id",
            );
            dpll_mask |= 1_u32 << info.id.0;
        }
    }
    dpll_mask
}

// upstream: intel_dpll_mgr.c intel_find_dpll()
fn intel_find_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    dpll_hw_state: &IntelDpllHwState,
    dpll_mask: u32,
) -> Option<usize> {
    let dpll_mask_all = intel_dpll_mask_all(hooks, display);
    intel_atomic_get_dpll_state(display, state, hooks);
    hooks.drm_warn_on(
        display.display_id,
        dpll_mask & !dpll_mask_all != 0,
        "DPLL candidate mask includes nonexistent PLL",
    );
    let max_id = 32_u32.saturating_sub(dpll_mask_all.leading_zeros());
    let mut unused_pll = None;
    for id in 0..max_id {
        if dpll_mask & (1_u32 << id) == 0 {
            continue;
        }
        let Some(pll_index) = intel_get_dpll_by_id(display, DpllId(id as u8)) else {
            continue;
        };
        let selected = state.dpll_state[pll_index];
        if selected.pipe_mask == 0 {
            if unused_pll.is_none() {
                unused_pll = Some(pll_index);
            }
            continue;
        }
        if *dpll_hw_state == selected.hw_state {
            hooks.drm_debug(display.display_id, "sharing existing matching DPLL");
            let _ = crtc;
            return Some(pll_index);
        }
    }
    if let Some(pll_index) = unused_pll {
        hooks.drm_debug(display.display_id, "allocated an unused DPLL");
        return Some(pll_index);
    }
    None
}

// upstream: intel_dpll_mgr.c intel_dpll_crtc_get()
pub fn intel_dpll_crtc_get<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    crtc: &IntelCrtc,
    pll: &IntelDpll,
    dpll_state: &mut IntelDpllState,
) {
    hooks.drm_warn_on(
        display.display_id,
        dpll_state.pipe_mask & (1 << crtc.pipe) != 0,
        "duplicate CRTC reference to DPLL",
    );
    dpll_state.pipe_mask |= 1 << crtc.pipe;
    hooks.drm_debug(display.display_id, pll.info.map_or("reserve unknown DPLL", |info| info.name));
}

// upstream: intel_dpll_mgr.c intel_reference_dpll()
fn intel_reference_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    pll_index: usize,
    dpll_hw_state: &IntelDpllHwState,
) {
    let dpll_state = intel_atomic_get_dpll_state(display, state, hooks);
    if dpll_state[pll_index].pipe_mask == 0 {
        dpll_state[pll_index].hw_state = *dpll_hw_state;
    }
    let pll = display.dplls[pll_index];
    intel_dpll_crtc_get(hooks, display, crtc, &pll, &mut dpll_state[pll_index]);
}

// upstream: intel_dpll_mgr.c intel_dpll_crtc_put()
pub fn intel_dpll_crtc_put<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    crtc: &IntelCrtc,
    pll: &IntelDpll,
    dpll_state: &mut IntelDpllState,
) {
    hooks.drm_warn_on(
        display.display_id,
        dpll_state.pipe_mask & (1 << crtc.pipe) == 0,
        "dropping nonexistent DPLL CRTC reference",
    );
    dpll_state.pipe_mask &= !(1 << crtc.pipe);
    hooks.drm_debug(display.display_id, pll.info.map_or("release unknown DPLL", |info| info.name));
}

// upstream: intel_dpll_mgr.c intel_unreference_dpll()
fn intel_unreference_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    pll_index: usize,
) {
    let dpll_state = intel_atomic_get_dpll_state(display, state, hooks);
    let pll = display.dplls[pll_index];
    intel_dpll_crtc_put(hooks, display, crtc, &pll, &mut dpll_state[pll_index]);
}

// upstream: intel_dpll_mgr.c intel_put_dpll()
fn intel_put_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
) {
    let crtc_index = usize::from(crtc.pipe);
    let old_pll = state.old_crtcs[crtc_index].intel_dpll;
    state.new_crtcs[crtc_index].intel_dpll = None;
    if let Some(pll_index) = old_pll {
        intel_unreference_dpll(hooks, display, state, crtc, pll_index);
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_swap_state()
pub fn intel_dpll_swap_state(display: &mut IntelDpllDisplay, state: &mut IntelAtomicState) {
    if !state.dpll_set {
        return;
    }
    for pll in display.dplls.iter_mut().take(display.num_dpll) {
        swap(&mut pll.state, &mut state.dpll_state[usize::from(pll.index)]);
    }
}

// upstream: intel_dpll_mgr.c ibx_dump_hw_state()
fn ibx_dump_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display_id: usize,
    dpll_hw_state: &IntelDpllHwState,
) {
    hooks.log_hw_state(
        display_id,
        "i9xx/PCH DPLL hardware state",
        &IntelDpllHwState {
            i9xx: dpll_hw_state.i9xx,
            ..IntelDpllHwState::default()
        },
    );
}

// upstream: intel_dpll_mgr.c ibx_compare_hw_state()
fn ibx_compare_hw_state(a: &IntelDpllHwState, b: &IntelDpllHwState) -> bool {
    a.i9xx.dpll == b.i9xx.dpll
        && a.i9xx.dpll_md == b.i9xx.dpll_md
        && a.i9xx.fp0 == b.i9xx.fp0
        && a.i9xx.fp1 == b.i9xx.fp1
}

const EINVAL: i32 = -22;
const U32_MAX: u32 = u32::MAX;
const fn MG_REFCLKIN_CTL_OD_2_MUX(v: u32) -> u32 { v << 8 }
const fn MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO(v: u32) -> u32 { v << 8 }
const fn MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL(v: u32) -> u32 { v << 14 }
const fn MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL(v: u32) -> u32 { v << 16 }
const fn MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO(v: u32) -> u32 { v << 8 }
const fn MG_PLL_DIV0_FBDIV_FRAC(v: u32) -> u32 { v << 8 }
const fn MG_PLL_DIV0_FBDIV_INT(v: u32) -> u32 { v }
const fn MG_PLL_DIV1_FBPREDIV(v: u32) -> u32 { v }
const fn DPLL_CFGCR0_DCO_FRACTION(v: u32) -> u32 { v << 10 }
const fn DPLL_CFGCR1_QDIV_MODE(v: u32) -> u32 { v << 9 }
const fn DPLL_CFGCR1_KDIV(v: u32) -> u32 { v << DPLL_CFGCR1_KDIV_SHIFT }
const fn DPLL_CFGCR1_PDIV(v: u32) -> u32 { v << DPLL_CFGCR1_PDIV_SHIFT }
const fn TGL_DPLL0_DIV0_AFC_STARTUP(v: u32) -> u32 { (v & 7) << 25 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SklWrpllParams {
    pub dco_integer: u32,
    pub dco_fraction: u32,
    pub pdiv: u32,
    pub kdiv: u32,
    pub qdiv_mode: u32,
    pub qdiv_ratio: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct IclComboPllParams {
    clock: u32,
    wrpll: SklWrpllParams,
}

const ICL_DP_COMBO_PLL_24MHZ: [IclComboPllParams; 8] = [
    IclComboPllParams { clock: 540_000, wrpll: SklWrpllParams { dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 2, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 270_000, wrpll: SklWrpllParams { dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 2, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 162_000, wrpll: SklWrpllParams { dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 4, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 324_000, wrpll: SklWrpllParams { dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 4, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 216_000, wrpll: SklWrpllParams { dco_integer: 0x168, dco_fraction: 0, pdiv: 1, kdiv: 2, qdiv_mode: 1, qdiv_ratio: 2 } },
    IclComboPllParams { clock: 432_000, wrpll: SklWrpllParams { dco_integer: 0x168, dco_fraction: 0, pdiv: 1, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 648_000, wrpll: SklWrpllParams { dco_integer: 0x195, dco_fraction: 0, pdiv: 2, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 810_000, wrpll: SklWrpllParams { dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 1, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
];
const ICL_DP_COMBO_PLL_19_2MHZ: [IclComboPllParams; 8] = [
    IclComboPllParams { clock: 540_000, wrpll: SklWrpllParams { dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 2, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 270_000, wrpll: SklWrpllParams { dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 2, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 162_000, wrpll: SklWrpllParams { dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 4, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 324_000, wrpll: SklWrpllParams { dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 4, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 216_000, wrpll: SklWrpllParams { dco_integer: 0x1c2, dco_fraction: 0, pdiv: 1, kdiv: 2, qdiv_mode: 1, qdiv_ratio: 2 } },
    IclComboPllParams { clock: 432_000, wrpll: SklWrpllParams { dco_integer: 0x1c2, dco_fraction: 0, pdiv: 1, kdiv: 2, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 648_000, wrpll: SklWrpllParams { dco_integer: 0x1fa, dco_fraction: 0x2000, pdiv: 2, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
    IclComboPllParams { clock: 810_000, wrpll: SklWrpllParams { dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 1, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0 } },
];
const ICL_TBT_PLL_24MHZ: SklWrpllParams = SklWrpllParams {
    dco_integer: 0x151, dco_fraction: 0x4000, pdiv: 4, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0,
};
const ICL_TBT_PLL_19_2MHZ: SklWrpllParams = SklWrpllParams {
    dco_integer: 0x1a5, dco_fraction: 0x7000, pdiv: 4, kdiv: 1, qdiv_mode: 0, qdiv_ratio: 0,
};
const TGL_TBT_PLL_19_2MHZ: SklWrpllParams = SklWrpllParams {
    dco_integer: 0x54, dco_fraction: 0x3000, pdiv: 0, kdiv: 0, qdiv_mode: 0, qdiv_ratio: 0,
};
const TGL_TBT_PLL_24MHZ: SklWrpllParams = SklWrpllParams {
    dco_integer: 0x43, dco_fraction: 0x4000, pdiv: 0, kdiv: 0, qdiv_mode: 0, qdiv_ratio: 0,
};

// upstream: intel_dpll_mgr.c icl_wrpll_get_multipliers()
fn icl_wrpll_get_multipliers(bestdiv: i32) -> (i32, i32, i32) {
    let (mut pdiv, mut qdiv, mut kdiv) = (0, 0, 0);
    if bestdiv % 2 == 0 {
        if bestdiv == 2 {
            (pdiv, qdiv, kdiv) = (2, 1, 1);
        } else if bestdiv % 4 == 0 {
            (pdiv, qdiv, kdiv) = (2, bestdiv / 4, 2);
        } else if bestdiv % 6 == 0 {
            (pdiv, qdiv, kdiv) = (3, bestdiv / 6, 2);
        } else if bestdiv % 5 == 0 {
            (pdiv, qdiv, kdiv) = (5, bestdiv / 10, 2);
        } else if bestdiv % 14 == 0 {
            (pdiv, qdiv, kdiv) = (7, bestdiv / 14, 2);
        }
    } else if [3, 5, 7].contains(&bestdiv) {
        (pdiv, qdiv, kdiv) = (bestdiv, 1, 1);
    } else {
        // Odd supported divisors are 9, 15, and 21.
        (pdiv, qdiv, kdiv) = (bestdiv / 3, 1, 3);
    }
    (pdiv, qdiv, kdiv)
}

// upstream: intel_dpll_mgr.c icl_wrpll_params_populate()
fn icl_wrpll_params_populate(
    params: &mut SklWrpllParams,
    dco_freq: u32,
    ref_freq: u32,
    pdiv: i32,
    qdiv: i32,
    kdiv: i32,
) {
    params.kdiv = match kdiv {
        1 => 1,
        2 => 2,
        3 => 4,
        _ => params.kdiv,
    };
    params.pdiv = match pdiv {
        2 => 1,
        3 => 2,
        5 => 4,
        7 => 8,
        _ => params.pdiv,
    };
    params.qdiv_ratio = qdiv as u32;
    params.qdiv_mode = u32::from(qdiv != 1);
    let dco = ((u64::from(dco_freq) << 15) / u64::from(ref_freq)) as u32;
    params.dco_integer = dco >> 15;
    params.dco_fraction = dco & 0x7fff;
}

// upstream: intel_dpll_mgr.c ehl_combo_pll_div_frac_wa_needed()
fn ehl_combo_pll_div_frac_wa_needed(display: &IntelDpllDisplay) -> bool {
    ((display.platform.elkhartlake && display.display_ver >= 12) || display.display_ver >= 12)
        && display.ref_clks.nssc == 38_400
}

// upstream: intel_dpll_mgr.c icl_calc_dp_combo_pll()
fn icl_calc_dp_combo_pll(
    display: &IntelDpllDisplay,
    clock: u32,
    pll_params: &mut SklWrpllParams,
    hooks: &mut impl IntelDpllHooks,
) -> i32 {
    let params = if display.ref_clks.nssc == 24_000 {
        &ICL_DP_COMBO_PLL_24MHZ
    } else {
        &ICL_DP_COMBO_PLL_19_2MHZ
    };
    for value in params {
        if clock == value.clock {
            *pll_params = value.wrpll;
            return 0;
        }
    }
    hooks.missing_case(display.display_id, clock);
    EINVAL
}

// upstream: intel_dpll_mgr.c icl_calc_tbt_pll()
fn icl_calc_tbt_pll(
    display: &IntelDpllDisplay,
    pll_params: &mut SklWrpllParams,
    hooks: &mut impl IntelDpllHooks,
) -> i32 {
    if display.display_ver >= 12 {
        *pll_params = match display.ref_clks.nssc {
            19_200 | 38_400 => TGL_TBT_PLL_19_2MHZ,
            24_000 => TGL_TBT_PLL_24MHZ,
            other => {
                hooks.missing_case(display.display_id, other);
                TGL_TBT_PLL_19_2MHZ
            }
        };
    } else {
        *pll_params = match display.ref_clks.nssc {
            19_200 | 38_400 => ICL_TBT_PLL_19_2MHZ,
            24_000 => ICL_TBT_PLL_24MHZ,
            other => {
                hooks.missing_case(display.display_id, other);
                ICL_TBT_PLL_19_2MHZ
            }
        };
    }
    0
}

// upstream: intel_dpll_mgr.c icl_ddi_tbt_pll_get_freq()
fn icl_ddi_tbt_pll_get_freq(display: &IntelDpllDisplay, hooks: &mut impl IntelDpllHooks) -> i32 {
    hooks.drm_warn(display.display_id, "TBT PLL exposes multiple clocks selected by DDI mux");
    0
}

// upstream: intel_dpll_mgr.c icl_wrpll_ref_clock()
fn icl_wrpll_ref_clock(display: &IntelDpllDisplay) -> u32 {
    if display.ref_clks.nssc == 38_400 {
        19_200
    } else {
        display.ref_clks.nssc
    }
}

// upstream: intel_dpll_mgr.c icl_calc_wrpll()
fn icl_calc_wrpll(
    display: &IntelDpllDisplay,
    port_clock: u32,
    wrpll_params: &mut SklWrpllParams,
) -> i32 {
    let ref_clock = icl_wrpll_ref_clock(display);
    let afe_clock = port_clock * 5;
    let dco_min = 7_998_000_u32;
    let dco_max = 10_000_000_u32;
    let dco_mid = (dco_min + dco_max) / 2;
    const DIVIDERS: [u32; 46] = [
        2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54,
        56, 60, 64, 66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9,
        15, 21,
    ];
    let (mut best_dco, mut best_centrality, mut best_div) = (0_u32, U32_MAX, 0_u32);
    for divider in DIVIDERS {
        let dco = afe_clock * divider;
        if (dco_min..=dco_max).contains(&dco) {
            let centrality = dco.abs_diff(dco_mid);
            if centrality < best_centrality {
                best_centrality = centrality;
                best_div = divider;
                best_dco = dco;
            }
        }
    }
    if best_div == 0 {
        return EINVAL;
    }
    let (pdiv, qdiv, kdiv) = icl_wrpll_get_multipliers(best_div as i32);
    icl_wrpll_params_populate(wrpll_params, best_dco, ref_clock, pdiv, qdiv, kdiv);
    0
}

// upstream: intel_dpll_mgr.c icl_ddi_combo_pll_get_freq()
fn icl_ddi_combo_pll_get_freq(
    display: &IntelDpllDisplay,
    dpll_hw_state: &IntelDpllHwState,
    hooks: &mut impl IntelDpllHooks,
) -> u32 {
    let hw_state = &dpll_hw_state.icl;
    let ref_clock = icl_wrpll_ref_clock(display);
    let mut p0 = hw_state.cfgcr1 & DPLL_CFGCR1_PDIV_MASK;
    let mut p2 = hw_state.cfgcr1 & DPLL_CFGCR1_KDIV_MASK;
    let p1 = if hw_state.cfgcr1 & DPLL_CFGCR1_QDIV_MODE_MASK != 0 {
        (hw_state.cfgcr1 & DPLL_CFGCR1_QDIV_RATIO_MASK) >> DPLL_CFGCR1_QDIV_RATIO_SHIFT
    } else {
        1
    };

    p0 = match p0 {
        DPLL_CFGCR1_PDIV_2 => 2,
        DPLL_CFGCR1_PDIV_3 => 3,
        DPLL_CFGCR1_PDIV_5 => 5,
        DPLL_CFGCR1_PDIV_7 => 7,
        other => other,
    };
    p2 = match p2 {
        DPLL_CFGCR1_KDIV_1 => 1,
        DPLL_CFGCR1_KDIV_2 => 2,
        DPLL_CFGCR1_KDIV_3 => 3,
        other => other,
    };

    let mut dco_fraction =
        (hw_state.cfgcr0 & DPLL_CFGCR0_DCO_FRACTION_MASK) >> DPLL_CFGCR0_DCO_FRACTION_SHIFT;
    let mut dco_freq = (hw_state.cfgcr0 & DPLL_CFGCR0_DCO_INTEGER_MASK) * ref_clock;
    if ehl_combo_pll_div_frac_wa_needed(display) {
        dco_fraction *= 2;
    }
    dco_freq += (dco_fraction * ref_clock) / 0x8000;

    if p0 == 0 || p1 == 0 || p2 == 0 {
        hooks.drm_warn(display.display_id, "invalid combo PLL output divider");
        return 0;
    }
    dco_freq / (p0 * p1 * p2 * 5)
}

// upstream: intel_dpll_mgr.c icl_calc_dpll_state()
fn icl_calc_dpll_state(
    display: &IntelDpllDisplay,
    pll_params: &SklWrpllParams,
    dpll_hw_state: &mut IntelDpllHwState,
) {
    let hw_state = &mut dpll_hw_state.icl;
    let mut dco_fraction = pll_params.dco_fraction;
    if ehl_combo_pll_div_frac_wa_needed(display) {
        dco_fraction = (dco_fraction + 1) / 2;
    }
    hw_state.cfgcr0 = DPLL_CFGCR0_DCO_FRACTION(dco_fraction) | pll_params.dco_integer;
    hw_state.cfgcr1 = dpll_cfgcr1_qdiv_ratio(pll_params.qdiv_ratio)
        | DPLL_CFGCR1_QDIV_MODE(pll_params.qdiv_mode)
        | DPLL_CFGCR1_KDIV(pll_params.kdiv)
        | DPLL_CFGCR1_PDIV(pll_params.pdiv);
    if display.display_ver >= 12 {
        hw_state.cfgcr1 |= TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL;
    } else {
        hw_state.cfgcr1 |= DPLL_CFGCR1_CENTRAL_FREQ_8400;
    }
    if display.vbt.override_afc_startup {
        hw_state.div0 = TGL_DPLL0_DIV0_AFC_STARTUP(u32::from(
            display.vbt.override_afc_startup_val,
        ));
    }
}

// upstream: intel_dpll_mgr.c icl_mg_pll_find_divisors()
fn icl_mg_pll_find_divisors(
    clock_khz: u32,
    is_dp: bool,
    use_ssc: bool,
    target_dco_khz: &mut u32,
    hw_state: &mut IclDpllHwState,
    is_dkl: bool,
) -> i32 {
    const DIV1_VALUES: [u32; 4] = [7, 5, 3, 2];
    let dco_min_freq = if is_dp {
        8_100_000
    } else if use_ssc {
        8_000_000
    } else {
        7_992_000
    };
    let dco_max_freq = if is_dp { 8_100_000 } else { 10_000_000 };

    for div1 in DIV1_VALUES {
        for div2 in (1..=10_u32).rev() {
            let dco = div1 * div2 * clock_khz * 5;
            if dco < dco_min_freq || dco > dco_max_freq {
                continue;
            }
            let (a_divratio, tlinedrv) = if div2 >= 2 {
                (if is_dp { 10 } else { 5 }, if is_dkl { 1 } else { 2 })
            } else {
                (5, 0)
            };
            let inputsel = u32::from(!is_dp);
            let hsdiv = match div1 {
                2 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2,
                3 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3,
                5 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5,
                7 => MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7,
                other => {
                    // The source fallthrough maps unexpected ratios to the /2 case.
                    let _ = other;
                    MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2
                }
            };
            *target_dco_khz = dco;
            hw_state.mg_refclkin_ctl = MG_REFCLKIN_CTL_OD_2_MUX(1);
            hw_state.mg_clktop2_coreclkctl1 = MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO(a_divratio);
            hw_state.mg_clktop2_hsclkctl = MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL(tlinedrv)
                | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL(inputsel)
                | hsdiv
                | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO(div2);
            return 0;
        }
    }
    EINVAL
}

// upstream: intel_dpll_mgr.c icl_calc_mg_pll_state()
fn icl_calc_mg_pll_state(
    display: &IntelDpllDisplay,
    crtc_state: &CrtcState,
    dpll_hw_state: &mut IntelDpllHwState,
) -> i32 {
    let hw_state = &mut dpll_hw_state.icl;
    let refclk_khz = display.ref_clks.nssc;
    let clock = crtc_state.port_clock;
    let mut dco_khz = 0_u32;
    let (mut ssc_stepsize, mut ssc_steplen) = (0_u64, 0_u64);
    let ssc_steplog = 4_u64;
    let use_ssc = false;
    let is_dp = crtc_state.output != OutputType::Hdmi;
    let is_dkl = display.display_ver >= 12;

    let ret = icl_mg_pll_find_divisors(
        clock,
        is_dp,
        use_ssc,
        &mut dco_khz,
        hw_state,
        is_dkl,
    );
    if ret != 0 {
        return ret;
    }

    let mut m1div = 2;
    let mut m2div_int = dco_khz / (refclk_khz * m1div);
    if m2div_int > 255 {
        if !is_dkl {
            m1div = 4;
            m2div_int = dco_khz / (refclk_khz * m1div);
        }
        if m2div_int > 255 {
            return EINVAL;
        }
    }
    let m2div_rem = dco_khz % (refclk_khz * m1div);
    let m2div_frac = ((u64::from(m2div_rem) * (1_u64 << 22))
        / u64::from(refclk_khz * m1div)) as u32;

    let (iref_ndiv, iref_trim, iref_pulse_w) = match refclk_khz {
        19_200 => (1, 28, 1),
        24_000 => (1, 25, 2),
        38_400 => (2, 28, 1),
        _ => return EINVAL,
    };

    // Integer rearrangement of BSpec's rounded TDC target count.
    let tdc_targetcnt = (2 * 1000 * 100_000 * 10 / (132 * refclk_khz) + 5) / 10;
    let feedfwgain = if use_ssc || m2div_rem > 0 {
        m1div * 1_000_000 * 100 / (dco_khz * 3 / 10)
    } else {
        0
    };
    let (prop_coeff, int_coeff) = if dco_khz >= 9_000_000 { (5, 10) } else { (4, 8) };
    if use_ssc {
        ssc_stepsize = (u64::from(dco_khz) * 47 * 32)
            / (u64::from(refclk_khz * m1div) * 10_000);
        ssc_steplen = div_round_up_u64(u64::from(dco_khz) * 1000, 32 * 2 * 32);
    }

    if is_dkl {
        hw_state.mg_pll_div0 = DKL_PLL_DIV0_INTEG_COEFF(int_coeff)
            | DKL_PLL_DIV0_PROP_COEFF(prop_coeff)
            | DKL_PLL_DIV0_FBPREDIV(m1div)
            | DKL_PLL_DIV0_FBDIV_INT(m2div_int);
        if display.vbt.override_afc_startup {
            hw_state.mg_pll_div0 |= TGL_DPLL0_DIV0_AFC_STARTUP(u32::from(
                display.vbt.override_afc_startup_val,
            ));
        }
        hw_state.mg_pll_div1 = DKL_PLL_DIV1_IREF_TRIM(iref_trim)
            | DKL_PLL_DIV1_TDC_TARGET_CNT(tdc_targetcnt);
        hw_state.mg_pll_ssc = DKL_PLL_SSC_IREF_NDIV_RATIO(iref_ndiv)
            | DKL_PLL_SSC_STEP_LEN(ssc_steplen)
            | DKL_PLL_SSC_STEP_NUM(ssc_steplog)
            | if use_ssc { DKL_PLL_SSC_EN } else { 0 };
        hw_state.mg_pll_bias = if m2div_frac != 0 { DKL_PLL_BIAS_FRAC_EN_H } else { 0 }
            | DKL_PLL_BIAS_FBDIV_FRAC(m2div_frac);
        hw_state.mg_pll_tdc_coldst_bias = DKL_PLL_TDC_SSC_STEP_SIZE(ssc_stepsize)
            | DKL_PLL_TDC_FEED_FWD_GAIN(feedfwgain);
    } else {
        hw_state.mg_pll_div0 = if m2div_rem > 0 { MG_PLL_DIV0_FRACNEN_H } else { 0 }
            | MG_PLL_DIV0_FBDIV_FRAC(m2div_frac)
            | MG_PLL_DIV0_FBDIV_INT(m2div_int);
        hw_state.mg_pll_div1 = MG_PLL_DIV1_IREF_NDIVRATIO(iref_ndiv)
            | MG_PLL_DIV1_DITHER_DIV_2
            | MG_PLL_DIV1_NDIVRATIO(1)
            | MG_PLL_DIV1_FBPREDIV(m1div);
        hw_state.mg_pll_lf = MG_PLL_LF_TDCTARGETCNT(tdc_targetcnt)
            | MG_PLL_LF_AFCCNTSEL_512
            | MG_PLL_LF_GAINCTRL(1)
            | MG_PLL_LF_INT_COEFF(int_coeff)
            | MG_PLL_LF_PROP_COEFF(prop_coeff);
        hw_state.mg_pll_frac_lock = MG_PLL_FRAC_LOCK_TRUELOCK_CRIT_32
            | MG_PLL_FRAC_LOCK_EARLYLOCK_CRIT_32
            | MG_PLL_FRAC_LOCK_LOCKTHRESH(10)
            | MG_PLL_FRAC_LOCK_DCODITHEREN
            | MG_PLL_FRAC_LOCK_FEEDFWRDGAIN(feedfwgain);
        if use_ssc || m2div_rem > 0 {
            hw_state.mg_pll_frac_lock |= MG_PLL_FRAC_LOCK_FEEDFWRDCAL_EN;
        }
        hw_state.mg_pll_ssc = if use_ssc { MG_PLL_SSC_EN } else { 0 }
            | MG_PLL_SSC_TYPE(2)
            | MG_PLL_SSC_STEPLENGTH(ssc_steplen)
            | MG_PLL_SSC_STEPNUM(ssc_steplog)
            | MG_PLL_SSC_FLLEN
            | MG_PLL_SSC_STEPSIZE(ssc_stepsize);
        hw_state.mg_pll_tdc_coldst_bias = MG_PLL_TDC_COLDST_COLDSTART
            | MG_PLL_TDC_COLDST_IREFINT_EN
            | MG_PLL_TDC_COLDST_REFBIAS_START_PULSE_W(iref_pulse_w)
            | MG_PLL_TDC_TDCOVCCORR_EN
            | MG_PLL_TDC_TDCSEL(3);
        hw_state.mg_pll_bias = MG_PLL_BIAS_BIAS_GB_SEL(3)
            | MG_PLL_BIAS_INIT_DCOAMP(0x3f)
            | MG_PLL_BIAS_BIAS_BONUS(10)
            | MG_PLL_BIAS_BIASCAL_EN
            | MG_PLL_BIAS_CTRIM(12)
            | MG_PLL_BIAS_VREF_RDAC(4)
            | MG_PLL_BIAS_IREFTRIM(iref_trim);
        if refclk_khz == 38_400 {
            hw_state.mg_pll_tdc_coldst_bias_mask = 1 << 16;
            hw_state.mg_pll_bias_mask = 0;
        } else {
            hw_state.mg_pll_tdc_coldst_bias_mask = U32_MAX;
            hw_state.mg_pll_bias_mask = U32_MAX;
        }
        hw_state.mg_pll_tdc_coldst_bias &= hw_state.mg_pll_tdc_coldst_bias_mask;
        hw_state.mg_pll_bias &= hw_state.mg_pll_bias_mask;
    }
    0
}

// upstream: intel_dpll_mgr.c icl_ddi_mg_pll_get_freq()
fn icl_ddi_mg_pll_get_freq(
    display: &IntelDpllDisplay,
    dpll_hw_state: &IntelDpllHwState,
    hooks: &mut impl IntelDpllHooks,
) -> u32 {
    let hw_state = &dpll_hw_state.icl;
    let mut ref_clock = display.ref_clks.nssc;
    let (m1, m2_int, m2_frac) = if display.display_ver >= 12 {
        let m1 = (hw_state.mg_pll_div0 & DKL_PLL_DIV0_FBPREDIV_MASK) >> 8;
        let m2_int = hw_state.mg_pll_div0 & DKL_PLL_DIV0_FBDIV_INT_MASK;
        let m2_frac = if hw_state.mg_pll_bias & DKL_PLL_BIAS_FRAC_EN_H != 0 {
            (hw_state.mg_pll_bias & DKL_PLL_BIAS_FBDIV_FRAC_MASK) >> DKL_PLL_BIAS_FBDIV_SHIFT
        } else {
            0
        };
        (m1, m2_int, m2_frac)
    } else {
        let m1 = hw_state.mg_pll_div1 & MG_PLL_DIV1_FBPREDIV_MASK;
        let m2_int = hw_state.mg_pll_div0 & MG_PLL_DIV0_FBDIV_INT_MASK;
        let m2_frac = if hw_state.mg_pll_div0 & MG_PLL_DIV0_FRACNEN_H != 0 {
            (hw_state.mg_pll_div0 & MG_PLL_DIV0_FBDIV_FRAC_MASK)
                >> MG_PLL_DIV0_FBDIV_FRAC_SHIFT
        } else {
            0
        };
        (m1, m2_int, m2_frac)
    };
    let div1 = match hw_state.mg_clktop2_hsclkctl & MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK {
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_2 => 2,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_3 => 3,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_5 => 5,
        MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_7 => 7,
        invalid => {
            hooks.missing_case(display.display_id, invalid);
            return 0;
        }
    };
    let mut div2 = (hw_state.mg_clktop2_hsclkctl & MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK) >> 8;
    if div2 == 0 {
        div2 = 1;
    }
    if m1 == 0 || div1 == 0 || div2 == 0 {
        hooks.drm_warn(display.display_id, "invalid MG PLL divider in frequency readout");
        return 0;
    }
    // Delay the 2^22 division to minimize rounding error.
    let mut tmp = u64::from(m1) * u64::from(m2_int) * u64::from(ref_clock)
        + ((u64::from(m1) * u64::from(m2_frac) * u64::from(ref_clock)) >> 22);
    tmp /= u64::from(5 * div1 * div2);
    ref_clock = 0; // source does not reuse the ref-clock local after the formula.
    let _ = ref_clock;
    tmp.min(u64::from(u32::MAX)) as u32
}

// upstream: intel_dpll_mgr.c icl_set_active_port_dpll()
pub fn icl_set_active_port_dpll(crtc_state: &mut CrtcState, port_dpll_id: PortDpllId) {
    let port_dpll = crtc_state.icl_port_dplls[port_dpll_id as usize];
    crtc_state.intel_dpll = port_dpll.pll;
    crtc_state.dpll_hw_state = port_dpll.hw_state;
}

// upstream: intel_dpll_mgr.c icl_update_active_dpll()
fn icl_update_active_dpll(
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) {
    let crtc_state = &mut state.new_crtcs[usize::from(crtc.pipe)];
    let primary_port = if encoder.output == OutputType::DisplayPortMst {
        encoder.primary_port
    } else {
        Some(encoder.port)
    };
    let port_dpll_id = if primary_port.is_some()
        && (encoder.tc_dp_alt_mode || encoder.tc_legacy_mode)
    {
        PortDpllId::MgPhy
    } else {
        PortDpllId::Default
    };
    icl_set_active_port_dpll(crtc_state, port_dpll_id);
}

// upstream: intel_dpll_mgr.c icl_compute_combo_phy_dpll()
fn icl_compute_combo_phy_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
) -> i32 {
    let crtc_state = &mut state.new_crtcs[usize::from(crtc.pipe)];
    let port_index = PortDpllId::Default as usize;
    let mut port_hw_state = crtc_state.icl_port_dplls[port_index].hw_state;
    let mut pll_params = SklWrpllParams::default();
    let ret = if crtc_state.output == OutputType::Hdmi || crtc_state.output == OutputType::Dsi {
        icl_calc_wrpll(display, crtc_state.port_clock, &mut pll_params)
    } else {
        icl_calc_dp_combo_pll(display, crtc_state.port_clock, &mut pll_params, hooks)
    };
    if ret != 0 {
        return ret;
    }
    icl_calc_dpll_state(display, &pll_params, &mut port_hw_state);
    crtc_state.icl_port_dplls[port_index].hw_state = port_hw_state;
    // Needed for the fastset check.
    icl_set_active_port_dpll(crtc_state, PortDpllId::Default);
    crtc_state.port_clock = icl_ddi_combo_pll_get_freq(display, &port_hw_state, hooks);
    0
}

// upstream: intel_dpll_mgr.c icl_get_combo_phy_dpll()
fn icl_get_combo_phy_dpll<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    let crtc_state = &mut state.new_crtcs[usize::from(crtc.pipe)];
    let port_dpll_hw_state = crtc_state.icl_port_dplls[PortDpllId::Default as usize].hw_state;
    let mut dpll_mask: u32;
    if display.platform.alderlake_s {
        dpll_mask = (1 << DPLL_ID_DG1_DPLL3.0)
            | (1 << DPLL_ID_DG1_DPLL2.0)
            | (1 << DPLL_ID_ICL_DPLL1.0)
            | (1 << DPLL_ID_ICL_DPLL0.0);
    } else if display.platform.dg1 {
        if encoder.port == Port::D || encoder.port == Port::E {
            dpll_mask = (1 << DPLL_ID_DG1_DPLL2.0) | (1 << DPLL_ID_DG1_DPLL3.0);
        } else {
            dpll_mask = (1 << DPLL_ID_DG1_DPLL0.0) | (1 << DPLL_ID_DG1_DPLL1.0);
        }
    } else if display.platform.rocketlake {
        dpll_mask = (1 << DPLL_ID_EHL_DPLL4.0)
            | (1 << DPLL_ID_ICL_DPLL1.0)
            | (1 << DPLL_ID_ICL_DPLL0.0);
    } else if (display.platform.jasperlake || display.platform.elkhartlake)
        && encoder.port != Port::A
    {
        dpll_mask = (1 << DPLL_ID_EHL_DPLL4.0)
            | (1 << DPLL_ID_ICL_DPLL1.0)
            | (1 << DPLL_ID_ICL_DPLL0.0);
    } else {
        dpll_mask = (1 << DPLL_ID_ICL_DPLL1.0) | (1 << DPLL_ID_ICL_DPLL0.0);
    }
    // Eliminate DPLLs reserved by HTI.
    dpll_mask &= !hooks.hti_dpll_mask(display.display_id);

    let Some(pll_index) = intel_find_dpll(
        hooks,
        display,
        state,
        crtc,
        &port_dpll_hw_state,
        dpll_mask,
    ) else {
        return EINVAL;
    };
    state.new_crtcs[usize::from(crtc.pipe)].icl_port_dplls[PortDpllId::Default as usize].pll =
        Some(pll_index);
    intel_reference_dpll(
        hooks,
        display,
        state,
        crtc,
        pll_index,
        &port_dpll_hw_state,
    );
    icl_update_active_dpll(state, crtc, encoder);
    0
}

// upstream: intel_dpll_mgr.c icl_compute_tc_phy_dplls()
fn icl_compute_tc_phy_dplls<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
) -> i32 {
    let crtc_index = usize::from(crtc.pipe);
    let old_state = state.old_crtcs[crtc_index];
    let crtc_state = &mut state.new_crtcs[crtc_index];
    let mut pll_params = SklWrpllParams::default();
    let ret = icl_calc_tbt_pll(display, &mut pll_params, hooks);
    if ret != 0 {
        return ret;
    }
    let mut default_port_dpll = crtc_state.icl_port_dplls[PortDpllId::Default as usize];
    icl_calc_dpll_state(display, &pll_params, &mut default_port_dpll.hw_state);
    crtc_state.icl_port_dplls[PortDpllId::Default as usize] = default_port_dpll;

    let mg_index = PortDpllId::MgPhy as usize;
    let mut mg_port_dpll = crtc_state.icl_port_dplls[mg_index];
    let ret = icl_calc_mg_pll_state(display, crtc_state, &mut mg_port_dpll.hw_state);
    if ret != 0 {
        return ret;
    }
    crtc_state.icl_port_dplls[mg_index] = mg_port_dpll;

    let old_is_tbt = old_state
        .intel_dpll
        .and_then(|index| display.dplls[index].info)
        .is_some_and(|info| info.id == DPLL_ID_ICL_TBTPLL);
    let active = if old_is_tbt {
        PortDpllId::Default
    } else {
        PortDpllId::MgPhy
    };
    icl_set_active_port_dpll(crtc_state, active);
    crtc_state.port_clock = icl_ddi_mg_pll_get_freq(display, &mg_port_dpll.hw_state, hooks);
    0
}

// upstream: intel_dpll_mgr.c icl_get_tc_phy_dplls()
fn icl_get_tc_phy_dplls<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    let crtc_index = usize::from(crtc.pipe);
    let mut port_dpll = state.new_crtcs[crtc_index].icl_port_dplls[PortDpllId::Default as usize];
    let Some(tbt_index) = intel_find_dpll(
        hooks,
        display,
        state,
        crtc,
        &port_dpll.hw_state,
        1 << DPLL_ID_ICL_TBTPLL.0,
    ) else {
        return EINVAL;
    };
    port_dpll.pll = Some(tbt_index);
    state.new_crtcs[crtc_index].icl_port_dplls[PortDpllId::Default as usize] = port_dpll;
    intel_reference_dpll(hooks, display, state, crtc, tbt_index, &port_dpll.hw_state);

    let Some(tc_port) = encoder.port.as_tc_port() else {
        intel_unreference_dpll(hooks, display, state, crtc, tbt_index);
        return EINVAL;
    };
    let dpll_id = icl_tc_port_to_pll_id(tc_port);
    let mg_hw_state = state.new_crtcs[crtc_index].icl_port_dplls[PortDpllId::MgPhy as usize].hw_state;
    let Some(mg_index) = intel_find_dpll(
        hooks,
        display,
        state,
        crtc,
        &mg_hw_state,
        1 << dpll_id.0,
    ) else {
        intel_unreference_dpll(hooks, display, state, crtc, tbt_index);
        return EINVAL;
    };
    let mut mg_port_dpll = state.new_crtcs[crtc_index].icl_port_dplls[PortDpllId::MgPhy as usize];
    mg_port_dpll.pll = Some(mg_index);
    state.new_crtcs[crtc_index].icl_port_dplls[PortDpllId::MgPhy as usize] = mg_port_dpll;
    intel_reference_dpll(hooks, display, state, crtc, mg_index, &mg_hw_state);
    icl_update_active_dpll(state, crtc, encoder);
    0
}

// upstream: intel_dpll_mgr.c icl_compute_dplls()
fn icl_compute_dplls<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    if encoder.is_combo_phy {
        icl_compute_combo_phy_dpll(hooks, display, state, crtc)
    } else if encoder.is_tc_phy {
        icl_compute_tc_phy_dplls(hooks, display, state, crtc)
    } else {
        hooks.missing_case(display.display_id, port_code(encoder.port));
        0
    }
}

// upstream: intel_dpll_mgr.c icl_get_dplls()
fn icl_get_dplls<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    if encoder.is_combo_phy {
        icl_get_combo_phy_dpll(hooks, display, state, crtc, encoder)
    } else if encoder.is_tc_phy {
        icl_get_tc_phy_dplls(hooks, display, state, crtc, encoder)
    } else {
        hooks.missing_case(display.display_id, port_code(encoder.port));
        EINVAL
    }
}

fn port_code(port: Port) -> u32 {
    match port {
        Port::A => 0,
        Port::B => 1,
        Port::C => 2,
        Port::D => 3,
        Port::E => 4,
        Port::F => 5,
        Port::Tc(tc_port) => 6 + tc_port as u32,
        Port::Other(value) => u32::from(value),
    }
}

// upstream: intel_dpll_mgr.c icl_put_dplls()
fn icl_put_dplls<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
) {
    let crtc_index = usize::from(crtc.pipe);
    let old_crtc_state = state.old_crtcs[crtc_index];
    state.new_crtcs[crtc_index].intel_dpll = None;
    for id in [PortDpllId::Default, PortDpllId::MgPhy] {
        let old_port_dpll = old_crtc_state.icl_port_dplls[id as usize];
        state.new_crtcs[crtc_index].icl_port_dplls[id as usize].pll = None;
        if let Some(pll_index) = old_port_dpll.pll {
            intel_unreference_dpll(hooks, display, state, crtc, pll_index);
        }
    }
}

// upstream: intel_dpll_mgr.c mg_pll_get_hw_state()
fn mg_pll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &mut IntelDpllHwState,
) -> bool {
    let Some(info) = pll.info else { return false };
    let Some(tc_port) = icl_pll_id_to_tc_port(info.id) else { return false };
    let Some(wakeref) = hooks.power_get_if_enabled(display.display_id, PowerDomain::DisplayCore) else {
        return false;
    };
    let enable_reg = intel_tc_pll_enable_reg(display, pll);
    if hooks.read32(enable_reg) & PLL_ENABLE == 0 {
        hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
        return false;
    }

    let hw_state = &mut dpll_hw_state.icl;
    hw_state.mg_refclkin_ctl = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::RefclkInCtl))
        & MG_REFCLKIN_CTL_OD_2_MUX_MASK;
    hw_state.mg_clktop2_coreclkctl1 =
        hooks.read32(DpllRegister::Mg(tc_port, MgRegister::CoreClkCtl1))
            & MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK;
    hw_state.mg_clktop2_hsclkctl = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::HsClkCtl))
        & (MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
            | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK);
    hw_state.mg_pll_div0 = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllDiv0));
    hw_state.mg_pll_div1 = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllDiv1));
    hw_state.mg_pll_lf = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllLf));
    hw_state.mg_pll_frac_lock = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllFracLock));
    hw_state.mg_pll_ssc = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllSsc));
    hw_state.mg_pll_bias = hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllBias));
    hw_state.mg_pll_tdc_coldst_bias =
        hooks.read32(DpllRegister::Mg(tc_port, MgRegister::PllTdcColdStartBias));
    if display.ref_clks.nssc == 38_400 {
        hw_state.mg_pll_tdc_coldst_bias_mask = MG_PLL_TDC_COLDST_COLDSTART;
        hw_state.mg_pll_bias_mask = 0;
    } else {
        hw_state.mg_pll_tdc_coldst_bias_mask = U32_MAX;
        hw_state.mg_pll_bias_mask = U32_MAX;
    }
    hw_state.mg_pll_tdc_coldst_bias &= hw_state.mg_pll_tdc_coldst_bias_mask;
    hw_state.mg_pll_bias &= hw_state.mg_pll_bias_mask;
    hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
    true
}

const DKL_PLL_DIV1_IREF_TRIM_MASK: u32 = 0x1f << 16;
const DKL_PLL_DIV1_TDC_TARGET_CNT_MASK: u32 = 0xff;
const DKL_PLL_SSC_IREF_NDIV_RATIO_MASK: u32 = 7 << 29;
const DKL_PLL_SSC_STEP_LEN_MASK: u32 = 0xff << 16;
const DKL_PLL_SSC_STEP_NUM_MASK: u32 = 7 << 11;
const DKL_PLL_TDC_SSC_STEP_SIZE_MASK: u32 = 0xff << 8;
const DKL_PLL_TDC_FEED_FWD_GAIN_MASK: u32 = 0xff;

// upstream: intel_dpll_mgr.c dkl_pll_get_hw_state()
fn dkl_pll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &mut IntelDpllHwState,
) -> bool {
    let Some(info) = pll.info else { return false };
    let Some(tc_port) = icl_pll_id_to_tc_port(info.id) else { return false };
    let Some(wakeref) = hooks.power_get_if_enabled(display.display_id, PowerDomain::DisplayCore) else {
        return false;
    };
    if hooks.read32(intel_tc_pll_enable_reg(display, pll)) & PLL_ENABLE == 0 {
        hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
        return false;
    }
    let hw_state = &mut dpll_hw_state.icl;
    hw_state.mg_refclkin_ctl = hooks.read_dkl(tc_port, 0x212c) & MG_REFCLKIN_CTL_OD_2_MUX_MASK;
    hw_state.mg_clktop2_hsclkctl = hooks.read_dkl(tc_port, 0x20d4)
        & (MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
            | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK);
    hw_state.mg_clktop2_coreclkctl1 = hooks.read_dkl(tc_port, 0x20d8)
        & MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK;
    hw_state.mg_pll_div0 = hooks.read_dkl(tc_port, 0x2200);
    let mut div0_mask = DKL_PLL_DIV0_MASK;
    if display.vbt.override_afc_startup {
        div0_mask |= DKL_PLL_DIV0_AFC_STARTUP_MASK;
    }
    hw_state.mg_pll_div0 &= div0_mask;
    hw_state.mg_pll_div1 = hooks.read_dkl(tc_port, 0x2204)
        & (DKL_PLL_DIV1_IREF_TRIM_MASK | DKL_PLL_DIV1_TDC_TARGET_CNT_MASK);
    hw_state.mg_pll_ssc = hooks.read_dkl(tc_port, 0x2210)
        & (DKL_PLL_SSC_IREF_NDIV_RATIO_MASK
            | DKL_PLL_SSC_STEP_LEN_MASK
            | DKL_PLL_SSC_STEP_NUM_MASK
            | DKL_PLL_SSC_EN);
    hw_state.mg_pll_bias = hooks.read_dkl(tc_port, 0x2214)
        & (DKL_PLL_BIAS_FRAC_EN_H | DKL_PLL_BIAS_FBDIV_FRAC_MASK);
    hw_state.mg_pll_tdc_coldst_bias = hooks.read_dkl(tc_port, 0x2218)
        & (DKL_PLL_TDC_SSC_STEP_SIZE_MASK | DKL_PLL_TDC_FEED_FWD_GAIN_MASK);
    hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
    true
}

fn icl_cfg_family(display: &IntelDpllDisplay) -> CfgRegisterFamily {
    if display.platform.alderlake_s {
        CfgRegisterFamily::AlderLakeS
    } else if display.platform.dg1 {
        CfgRegisterFamily::Dg1
    } else if display.platform.rocketlake {
        CfgRegisterFamily::RocketLake
    } else if display.display_ver >= 12 {
        CfgRegisterFamily::TigerLake
    } else {
        CfgRegisterFamily::Icl
    }
}

// upstream: intel_dpll_mgr.c icl_pll_get_hw_state()
fn icl_pll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &mut IntelDpllHwState,
    enable_reg: DpllRegister,
) -> bool {
    let Some(info) = pll.info else { return false };
    let Some(wakeref) = hooks.power_get_if_enabled(display.display_id, PowerDomain::DisplayCore) else {
        return false;
    };
    if hooks.read32(enable_reg) & PLL_ENABLE == 0 {
        hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
        return false;
    }
    let family = icl_cfg_family(display);
    let hw_state = &mut dpll_hw_state.icl;
    hw_state.cfgcr0 = hooks.read32(DpllRegister::CfgCr0(family, info.id));
    hw_state.cfgcr1 = hooks.read32(DpllRegister::CfgCr1(family, info.id));
    if display.display_ver >= 12 && display.vbt.override_afc_startup {
        hw_state.div0 = hooks.read32(DpllRegister::TglDpllDiv0(info.id))
            & TGL_DPLL0_DIV0_AFC_STARTUP_MASK;
    }
    hooks.power_put(display.display_id, PowerDomain::DisplayCore, wakeref);
    true
}

// upstream: intel_dpll_mgr.c combo_pll_get_hw_state()
fn combo_pll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &mut IntelDpllHwState,
) -> bool {
    let enable_reg = intel_combo_pll_enable_reg(display, pll);
    icl_pll_get_hw_state(hooks, display, pll, dpll_hw_state, enable_reg)
}

// upstream: intel_dpll_mgr.c icl_tbt_pll_get_hw_state()
fn icl_tbt_pll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &mut IntelDpllHwState,
) -> bool {
    icl_pll_get_hw_state(hooks, display, pll, dpll_hw_state, DpllRegister::TbtEnable)
}

// upstream: intel_dpll_mgr.c icl_dpll_write()
fn icl_dpll_write<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    hw_state: &IclDpllHwState,
) {
    let id = pll.info.map_or(DPLL_ID_ICL_DPLL0, |info| info.id);
    let family = icl_cfg_family(display);
    let div0_reg = if display.display_ver >= 12 {
        Some(DpllRegister::TglDpllDiv0(id))
    } else {
        None
    };
    hooks.write32(DpllRegister::CfgCr0(family, id), hw_state.cfgcr0);
    hooks.write32(DpllRegister::CfgCr1(family, id), hw_state.cfgcr1);
    hooks.drm_warn_on(
        display.display_id,
        display.vbt.override_afc_startup && div0_reg.is_none(),
        "AFC startup override requested without DPLL DIV0 register",
    );
    if display.vbt.override_afc_startup {
        if let Some(register) = div0_reg {
            hooks.rmw32(register, TGL_DPLL0_DIV0_AFC_STARTUP_MASK, hw_state.div0);
        }
    }
    hooks.posting_read32(DpllRegister::CfgCr1(family, id));
}

// upstream: intel_dpll_mgr.c icl_mg_pll_write()
fn icl_mg_pll_write<H: IntelDpllHooks>(
    hooks: &mut H,
    _display: &IntelDpllDisplay,
    pll: &IntelDpll,
    hw_state: &IclDpllHwState,
) {
    let Some(id) = pll.info.map(|info| info.id) else { return };
    let Some(tc_port) = icl_pll_id_to_tc_port(id) else { return };
    hooks.rmw32(
        DpllRegister::Mg(tc_port, MgRegister::RefclkInCtl),
        MG_REFCLKIN_CTL_OD_2_MUX_MASK,
        hw_state.mg_refclkin_ctl,
    );
    hooks.rmw32(
        DpllRegister::Mg(tc_port, MgRegister::CoreClkCtl1),
        MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK,
        hw_state.mg_clktop2_coreclkctl1,
    );
    hooks.rmw32(
        DpllRegister::Mg(tc_port, MgRegister::HsClkCtl),
        MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
            | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
            | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK,
        hw_state.mg_clktop2_hsclkctl,
    );
    hooks.write32(DpllRegister::Mg(tc_port, MgRegister::PllDiv0), hw_state.mg_pll_div0);
    hooks.write32(DpllRegister::Mg(tc_port, MgRegister::PllDiv1), hw_state.mg_pll_div1);
    hooks.write32(DpllRegister::Mg(tc_port, MgRegister::PllLf), hw_state.mg_pll_lf);
    hooks.write32(
        DpllRegister::Mg(tc_port, MgRegister::PllFracLock),
        hw_state.mg_pll_frac_lock,
    );
    hooks.write32(DpllRegister::Mg(tc_port, MgRegister::PllSsc), hw_state.mg_pll_ssc);
    hooks.rmw32(
        DpllRegister::Mg(tc_port, MgRegister::PllBias),
        hw_state.mg_pll_bias_mask,
        hw_state.mg_pll_bias,
    );
    hooks.rmw32(
        DpllRegister::Mg(tc_port, MgRegister::PllTdcColdStartBias),
        hw_state.mg_pll_tdc_coldst_bias_mask,
        hw_state.mg_pll_tdc_coldst_bias,
    );
    hooks.posting_read32(DpllRegister::Mg(tc_port, MgRegister::PllTdcColdStartBias));
}

// upstream: intel_dpll_mgr.c dkl_pll_write()
fn dkl_pll_write<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    hw_state: &IclDpllHwState,
) {
    let Some(id) = pll.info.map(|info| info.id) else { return };
    let Some(tc_port) = icl_pll_id_to_tc_port(id) else { return };
    let mut val = hooks.read_dkl(tc_port, 0x212c);
    val = (val & !MG_REFCLKIN_CTL_OD_2_MUX_MASK) | hw_state.mg_refclkin_ctl;
    hooks.write_dkl(tc_port, 0x212c, val);

    let mut val = hooks.read_dkl(tc_port, 0x20d8);
    val = (val & !MG_CLKTOP2_CORECLKCTL1_A_DIVRATIO_MASK) | hw_state.mg_clktop2_coreclkctl1;
    hooks.write_dkl(tc_port, 0x20d8, val);

    let mut val = hooks.read_dkl(tc_port, 0x20d4);
    val &= !(MG_CLKTOP2_HSCLKCTL_TLINEDRV_CLKSEL_MASK
        | MG_CLKTOP2_HSCLKCTL_CORE_INPUTSEL_MASK
        | MG_CLKTOP2_HSCLKCTL_HSDIV_RATIO_MASK
        | MG_CLKTOP2_HSCLKCTL_DSDIV_RATIO_MASK);
    val |= hw_state.mg_clktop2_hsclkctl;
    hooks.write_dkl(tc_port, 0x20d4, val);

    let div0_mask = DKL_PLL_DIV0_MASK
        | if display.vbt.override_afc_startup {
            DKL_PLL_DIV0_AFC_STARTUP_MASK
        } else {
            0
        };
    dkl_rmw(hooks, tc_port, 0x2200, div0_mask, hw_state.mg_pll_div0);

    val = hooks.read_dkl(tc_port, 0x2204);
    val &= !(DKL_PLL_DIV1_IREF_TRIM_MASK | DKL_PLL_DIV1_TDC_TARGET_CNT_MASK);
    val |= hw_state.mg_pll_div1;
    hooks.write_dkl(tc_port, 0x2204, val);

    val = hooks.read_dkl(tc_port, 0x2210);
    val &= !(DKL_PLL_SSC_IREF_NDIV_RATIO_MASK
        | DKL_PLL_SSC_STEP_LEN_MASK
        | DKL_PLL_SSC_STEP_NUM_MASK
        | DKL_PLL_SSC_EN);
    val |= hw_state.mg_pll_ssc;
    hooks.write_dkl(tc_port, 0x2210, val);

    val = hooks.read_dkl(tc_port, 0x2214);
    val &= !(DKL_PLL_BIAS_FRAC_EN_H | DKL_PLL_BIAS_FBDIV_FRAC_MASK);
    val |= hw_state.mg_pll_bias;
    hooks.write_dkl(tc_port, 0x2214, val);

    val = hooks.read_dkl(tc_port, 0x2218);
    val &= !(DKL_PLL_TDC_SSC_STEP_SIZE_MASK | DKL_PLL_TDC_FEED_FWD_GAIN_MASK);
    val |= hw_state.mg_pll_tdc_coldst_bias;
    hooks.write_dkl(tc_port, 0x2218, val);
    hooks.posting_read_dkl(tc_port, 0x2218);
}

fn dkl_rmw<H: IntelDpllHooks>(hooks: &mut H, port: TcPort, offset: u16, clear: u32, set: u32) {
    let old = hooks.read_dkl(port, offset);
    hooks.write_dkl(port, offset, (old & !clear) | set);
}

// upstream: intel_dpll_mgr.c icl_pll_power_enable()
fn icl_pll_power_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    enable_reg: DpllRegister,
) {
    hooks.rmw32(enable_reg, 0, PLL_POWER_ENABLE);
    if hooks.wait_for_set(enable_reg, PLL_POWER_STATE, 1) {
        hooks.drm_error(
            display.display_id,
            if let Some(info) = pll.info { info.name } else { "PLL power not enabled" },
        );
    }
}

// upstream: intel_dpll_mgr.c icl_pll_enable()
fn icl_pll_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    enable_reg: DpllRegister,
) {
    hooks.rmw32(enable_reg, 0, PLL_ENABLE);
    if hooks.wait_for_set(enable_reg, PLL_LOCK, 1) {
        hooks.drm_error(
            display.display_id,
            if let Some(info) = pll.info { info.name } else { "PLL not locked" },
        );
    }
}

// upstream: intel_dpll_mgr.c adlp_cmtg_clock_gating_wa()
fn adlp_cmtg_clock_gating_wa<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
) {
    if !(display.platform.alderlake_p && hooks.is_adlp_step_a0_to_b0(display.display_id))
        || pll.info.is_none_or(|info| info.id != DPLL_ID_ICL_DPLL0)
    {
        return;
    }
    // WA_16011069516: ADL-P A0/B0, use double-read then disable CMTG gating.
    let _first_read = hooks.read32(DpllRegister::TransCmtgChicken);
    let val = hooks.rmw32(
        DpllRegister::TransCmtgChicken,
        u32::MAX,
        1 << 1,
    );
    hooks.drm_warn_on(
        display.display_id,
        val & !(1 << 1) != 0,
        "unexpected flags in TRANS_CMTG_CHICKEN",
    );
}

// upstream: intel_dpll_mgr.c combo_pll_enable()
fn combo_pll_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &IntelDpllHwState,
) {
    let enable_reg = intel_combo_pll_enable_reg(display, pll);
    icl_pll_power_enable(hooks, display, pll, enable_reg);
    icl_dpll_write(hooks, display, pll, &dpll_hw_state.icl);
    // DVFS sequencing is handled by the shared CDCLK paths.
    icl_pll_enable(hooks, display, pll, enable_reg);
    adlp_cmtg_clock_gating_wa(hooks, display, pll);
}

// upstream: intel_dpll_mgr.c icl_tbt_pll_enable()
fn icl_tbt_pll_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &IntelDpllHwState,
) {
    icl_pll_power_enable(hooks, display, pll, DpllRegister::TbtEnable);
    icl_dpll_write(hooks, display, pll, &dpll_hw_state.icl);
    icl_pll_enable(hooks, display, pll, DpllRegister::TbtEnable);
}

// upstream: intel_dpll_mgr.c mg_pll_enable()
fn mg_pll_enable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    dpll_hw_state: &IntelDpllHwState,
) {
    let enable_reg = intel_tc_pll_enable_reg(display, pll);
    icl_pll_power_enable(hooks, display, pll, enable_reg);
    if display.display_ver >= 12 {
        dkl_pll_write(hooks, display, pll, &dpll_hw_state.icl);
    } else {
        icl_mg_pll_write(hooks, display, pll, &dpll_hw_state.icl);
    }
    icl_pll_enable(hooks, display, pll, enable_reg);
}

// upstream: intel_dpll_mgr.c icl_pll_disable()
fn icl_pll_disable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
    enable_reg: DpllRegister,
) {
    hooks.rmw32(enable_reg, PLL_ENABLE, 0);
    if hooks.wait_for_clear(enable_reg, PLL_LOCK, 1) {
        hooks.drm_error(
            display.display_id,
            if let Some(info) = pll.info { info.name } else { "PLL remained locked" },
        );
    }
    hooks.rmw32(enable_reg, PLL_POWER_ENABLE, 0);
    if hooks.wait_for_clear(enable_reg, PLL_POWER_STATE, 1) {
        hooks.drm_error(
            display.display_id,
            if let Some(info) = pll.info { info.name } else { "PLL power remained enabled" },
        );
    }
}

// upstream: intel_dpll_mgr.c combo_pll_disable()
fn combo_pll_disable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
) {
    icl_pll_disable(hooks, display, pll, intel_combo_pll_enable_reg(display, pll));
}

// upstream: intel_dpll_mgr.c icl_tbt_pll_disable()
fn icl_tbt_pll_disable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
) {
    icl_pll_disable(hooks, display, pll, DpllRegister::TbtEnable);
}

// upstream: intel_dpll_mgr.c mg_pll_disable()
fn mg_pll_disable<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll: &IntelDpll,
) {
    icl_pll_disable(hooks, display, pll, intel_tc_pll_enable_reg(display, pll));
}

// upstream: intel_dpll_mgr.c icl_update_dpll_ref_clks()
pub fn icl_update_dpll_ref_clks(display: &mut IntelDpllDisplay) {
    // No SSC reference clock.
    display.ref_clks.nssc = display.cdclk_ref;
}

// upstream: intel_dpll_mgr.c icl_dump_hw_state()
pub fn icl_dump_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    dpll_hw_state: &IntelDpllHwState,
) {
    hooks.log_hw_state(display.display_id, "ICL/TGL DPLL hardware state", dpll_hw_state);
}

// upstream: intel_dpll_mgr.c icl_compare_hw_state()
pub fn icl_compare_hw_state(a: &IntelDpllHwState, b: &IntelDpllHwState) -> bool {
    let a = &a.icl;
    let b = &b.icl;
    a.cfgcr0 == b.cfgcr0
        && a.cfgcr1 == b.cfgcr1
        && a.div0 == b.div0
        && a.mg_refclkin_ctl == b.mg_refclkin_ctl
        && a.mg_clktop2_coreclkctl1 == b.mg_clktop2_coreclkctl1
        && a.mg_clktop2_hsclkctl == b.mg_clktop2_hsclkctl
        && a.mg_pll_div0 == b.mg_pll_div0
        && a.mg_pll_div1 == b.mg_pll_div1
        && a.mg_pll_lf == b.mg_pll_lf
        && a.mg_pll_frac_lock == b.mg_pll_frac_lock
        && a.mg_pll_ssc == b.mg_pll_ssc
        && a.mg_pll_bias == b.mg_pll_bias
        && a.mg_pll_tdc_coldst_bias == b.mg_pll_tdc_coldst_bias
}

const TGL_DPLLS: [DpllInfo; 9] = [
    DpllInfo { name: "DPLL 0", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL0, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 1", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TBT PLL", funcs: DpllFunction::Tbt, id: DPLL_ID_ICL_TBTPLL, power_domain: None, always_on: false, is_alt_port_dpll: true },
    DpllInfo { name: "TC PLL 1", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 2", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL2, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 3", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL3, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 4", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL4, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 5", funcs: DpllFunction::Dkl, id: DPLL_ID_TGL_MGPLL5, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 6", funcs: DpllFunction::Dkl, id: DPLL_ID_TGL_MGPLL6, power_domain: None, always_on: false, is_alt_port_dpll: false },
];
const RKL_DPLLS: [DpllInfo; 3] = [
    DpllInfo { name: "DPLL 0", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL0, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 1", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 4", funcs: DpllFunction::Combo, id: DPLL_ID_EHL_DPLL4, power_domain: None, always_on: false, is_alt_port_dpll: false },
];
const DG1_DPLLS: [DpllInfo; 4] = [
    DpllInfo { name: "DPLL 0", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL0, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 1", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 2", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL2, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 3", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL3, power_domain: None, always_on: false, is_alt_port_dpll: false },
];
const ADLS_DPLLS: [DpllInfo; 4] = [
    DpllInfo { name: "DPLL 0", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL0, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 1", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 2", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL2, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 3", funcs: DpllFunction::Combo, id: DPLL_ID_DG1_DPLL3, power_domain: None, always_on: false, is_alt_port_dpll: false },
];
const ADLP_DPLLS: [DpllInfo; 7] = [
    DpllInfo { name: "DPLL 0", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL0, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "DPLL 1", funcs: DpllFunction::Combo, id: DPLL_ID_ICL_DPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TBT PLL", funcs: DpllFunction::Tbt, id: DPLL_ID_ICL_TBTPLL, power_domain: None, always_on: false, is_alt_port_dpll: true },
    DpllInfo { name: "TC PLL 1", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL1, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 2", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL2, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 3", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL3, power_domain: None, always_on: false, is_alt_port_dpll: false },
    DpllInfo { name: "TC PLL 4", funcs: DpllFunction::Dkl, id: DPLL_ID_ICL_MGPLL4, power_domain: None, always_on: false, is_alt_port_dpll: false },
];

static INTEL_DPLL_MGR_TGL: IntelDpllMgr = IntelDpllMgr { kind: DpllManagerKind::Tgl, dpll_info: &TGL_DPLLS, has_update_active_dpll: true };
static INTEL_DPLL_MGR_RKL: IntelDpllMgr = IntelDpllMgr { kind: DpllManagerKind::RocketLake, dpll_info: &RKL_DPLLS, has_update_active_dpll: false };
static INTEL_DPLL_MGR_DG1: IntelDpllMgr = IntelDpllMgr { kind: DpllManagerKind::Dg1, dpll_info: &DG1_DPLLS, has_update_active_dpll: false };
static INTEL_DPLL_MGR_ADLS: IntelDpllMgr = IntelDpllMgr { kind: DpllManagerKind::AlderLakeS, dpll_info: &ADLS_DPLLS, has_update_active_dpll: false };
static INTEL_DPLL_MGR_ADLP: IntelDpllMgr = IntelDpllMgr { kind: DpllManagerKind::AlderLakeP, dpll_info: &ADLP_DPLLS, has_update_active_dpll: true };

fn enable_dpll_function<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
    hw_state: &IntelDpllHwState,
) {
    let pll = display.dplls[pll_index];
    match pll.info.map(|info| info.funcs) {
        Some(DpllFunction::Combo) => combo_pll_enable(hooks, display, &pll, hw_state),
        Some(DpllFunction::Tbt) => icl_tbt_pll_enable(hooks, display, &pll, hw_state),
        Some(DpllFunction::Mg | DpllFunction::Dkl) => mg_pll_enable(hooks, display, &pll, hw_state),
        None => hooks.drm_warn(display.display_id, "DPLL enable callback missing"),
    }
}

fn disable_dpll_function<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
) {
    let pll = display.dplls[pll_index];
    match pll.info.map(|info| info.funcs) {
        Some(DpllFunction::Combo) => combo_pll_disable(hooks, display, &pll),
        Some(DpllFunction::Tbt) => icl_tbt_pll_disable(hooks, display, &pll),
        Some(DpllFunction::Mg | DpllFunction::Dkl) => mg_pll_disable(hooks, display, &pll),
        None => hooks.drm_warn(display.display_id, "DPLL disable callback missing"),
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_init()
pub fn intel_dpll_init<H: IntelDpllHooks>(hooks: &mut H, display: &mut IntelDpllDisplay) {
    hooks.dpll_mutex_init(display.display_id);
    let dpll_mgr = if display.platform.dg2 {
        None
    } else if display.display_ver >= 35 || display.display_ver >= 14 {
        // Xe3/MTL managers are outside this display-12/13 translation.
        None
    } else if display.platform.alderlake_p {
        Some(&INTEL_DPLL_MGR_ADLP)
    } else if display.platform.alderlake_s {
        Some(&INTEL_DPLL_MGR_ADLS)
    } else if display.platform.dg1 {
        Some(&INTEL_DPLL_MGR_DG1)
    } else if display.platform.rocketlake {
        Some(&INTEL_DPLL_MGR_RKL)
    } else if display.display_ver >= 12 {
        Some(&INTEL_DPLL_MGR_TGL)
    } else {
        None
    };

    display.mgr = None;
    display.num_dpll = 0;
    display.dplls = [IntelDpll::default(); MAX_DPLLS];
    if let Some(manager) = dpll_mgr {
        for (index, info) in manager.dpll_info.iter().enumerate() {
            if hooks.drm_warn_on(display.display_id, index >= MAX_DPLLS, "too many shared DPLLs")
                || hooks.drm_warn_on(display.display_id, info.id.0 >= 32, "DPLL id exceeds bitmask")
            {
                break;
            }
            display.dplls[index].info = Some(*info);
            display.dplls[index].index = index as u8;
            display.num_dpll = index + 1;
        }
        display.mgr = Some(manager);
    }
    // Retain the source's PHY consistency verification during initialization.
    hooks.intel_cx0pll_verify_plls(display.display_id);
    hooks.intel_lt_phy_verify_plls(display.display_id);
}

// upstream: intel_dpll_mgr.c intel_dpll_compute()
pub fn intel_dpll_compute<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    if display.mgr.is_none() {
        hooks.drm_warn(display.display_id, "shared DPLL manager unavailable");
        return EINVAL;
    }
    icl_compute_dplls(hooks, display, state, crtc, encoder)
}

// upstream: intel_dpll_mgr.c intel_dpll_reserve()
pub fn intel_dpll_reserve<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) -> i32 {
    if display.mgr.is_none() {
        hooks.drm_warn(display.display_id, "shared DPLL manager unavailable");
        return EINVAL;
    }
    icl_get_dplls(hooks, display, state, crtc, encoder)
}

// upstream: intel_dpll_mgr.c intel_dpll_release()
pub fn intel_dpll_release<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
) {
    if display.mgr.is_none() {
        return;
    }
    icl_put_dplls(hooks, display, state, crtc);
}

// upstream: intel_dpll_mgr.c intel_dpll_update_active()
pub fn intel_dpll_update_active(
    display: &IntelDpllDisplay,
    state: &mut IntelAtomicState,
    crtc: &IntelCrtc,
    encoder: &IntelEncoder,
) {
    if display.mgr.is_some_and(|manager| manager.has_update_active_dpll) {
        icl_update_active_dpll(state, crtc, encoder);
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_get_freq()
pub fn intel_dpll_get_freq<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll_index: usize,
    dpll_hw_state: &IntelDpllHwState,
) -> i32 {
    let Some(info) = display.dplls.get(pll_index).and_then(|pll| pll.info) else {
        hooks.drm_warn(display.display_id, "DPLL frequency callback missing");
        return 0;
    };
    match info.funcs {
        DpllFunction::Combo => icl_ddi_combo_pll_get_freq(display, dpll_hw_state, hooks) as i32,
        DpllFunction::Tbt => icl_ddi_tbt_pll_get_freq(display, hooks),
        DpllFunction::Mg | DpllFunction::Dkl => {
            icl_ddi_mg_pll_get_freq(display, dpll_hw_state, hooks) as i32
        }
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_get_hw_state()
pub fn intel_dpll_get_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    pll_index: usize,
    dpll_hw_state: &mut IntelDpllHwState,
) -> bool {
    let Some(pll) = display.dplls.get(pll_index).copied() else { return false };
    let Some(info) = pll.info else { return false };
    match info.funcs {
        DpllFunction::Combo => combo_pll_get_hw_state(hooks, display, &pll, dpll_hw_state),
        DpllFunction::Tbt => icl_tbt_pll_get_hw_state(hooks, display, &pll, dpll_hw_state),
        DpllFunction::Mg => mg_pll_get_hw_state(hooks, display, &pll, dpll_hw_state),
        DpllFunction::Dkl => dkl_pll_get_hw_state(hooks, display, &pll, dpll_hw_state),
    }
}

// upstream: intel_dpll_mgr.c readout_dpll_hw_state()
fn readout_dpll_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
) {
    let mut hw_state = IntelDpllHwState::default();
    display.dplls[pll_index].on = intel_dpll_get_hw_state(hooks, display, pll_index, &mut hw_state);
    // Upstream passes `&pll->state.hw_state` directly to the hardware-state
    // callback. Retaining this decoded image is also required by
    // `intel_find_dpll()` when a later atomic state compares PLL candidates.
    display.dplls[pll_index].state.hw_state = hw_state;
    let info = display.dplls[pll_index].info;
    if display.dplls[pll_index].on {
        if let Some(domain) = info.and_then(|item| item.power_domain) {
            display.dplls[pll_index].wakeref = hooks.power_get(display.display_id, domain);
        }
    }
    display.dplls[pll_index].state.pipe_mask = 0;
    for pipe in 0..MAX_PIPES {
        let crtc_state = display.crtc_states[pipe];
        if crtc_state.hw_active && crtc_state.intel_dpll == Some(pll_index) {
            let crtc = IntelCrtc {
                id: crtc_state.id,
                name: crtc_state.name,
                pipe: crtc_state.pipe,
            };
            let pll = display.dplls[pll_index];
            let mut pll_state = display.dplls[pll_index].state;
            intel_dpll_crtc_get(
                hooks,
                display,
                &crtc,
                &pll,
                &mut pll_state,
            );
            display.dplls[pll_index].state = pll_state;
        }
    }
    display.dplls[pll_index].active_mask = display.dplls[pll_index].state.pipe_mask;
    hooks.drm_debug(display.display_id, "shared DPLL hardware state readout complete");
}

// upstream: intel_dpll_mgr.c intel_dpll_update_ref_clks()
pub fn intel_dpll_update_ref_clks(display: &mut IntelDpllDisplay) {
    if display.mgr.is_some() {
        icl_update_dpll_ref_clks(display);
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_readout_hw_state()
pub fn intel_dpll_readout_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
) {
    for pll_index in 0..display.num_dpll {
        readout_dpll_hw_state(hooks, display, pll_index);
    }
}

// upstream: intel_dpll_mgr.c sanitize_dpll_state()
fn sanitize_dpll_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
) {
    if !display.dplls[pll_index].on {
        return;
    }
    let pll = display.dplls[pll_index];
    adlp_cmtg_clock_gating_wa(hooks, display, &pll);
    if display.dplls[pll_index].active_mask != 0 {
        return;
    }
    hooks.drm_debug(display.display_id, "DPLL enabled but unused, disabling");
    _intel_disable_shared_dpll(hooks, display, pll_index);
}

// upstream: intel_dpll_mgr.c intel_dpll_sanitize_state()
pub fn intel_dpll_sanitize_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
) {
    hooks.intel_cx0_pll_power_save_wa(display.display_id);
    for pll_index in 0..display.num_dpll {
        sanitize_dpll_state(hooks, display, pll_index);
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_dump_hw_state()
pub fn intel_dpll_dump_hw_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &IntelDpllDisplay,
    dpll_hw_state: &IntelDpllHwState,
) {
    if display.mgr.is_some() {
        icl_dump_hw_state(hooks, display, dpll_hw_state);
    } else {
        // The source falls back to the i9xx/PCH state printer when no manager exists.
        ibx_dump_hw_state(hooks, display.display_id, dpll_hw_state);
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_compare_hw_state()
pub fn intel_dpll_compare_hw_state(
    display: &IntelDpllDisplay,
    a: &IntelDpllHwState,
    b: &IntelDpllHwState,
) -> bool {
    if display.mgr.is_some() {
        icl_compare_hw_state(a, b)
    } else {
        ibx_compare_hw_state(a, b)
    }
}

// upstream: intel_dpll_mgr.c verify_single_dpll_state()
fn verify_single_dpll_state<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    pll_index: usize,
    crtc: Option<IntelCrtc>,
    new_crtc_state: Option<CrtcState>,
) {
    let mut hw_state = IntelDpllHwState::default();
    let active = intel_dpll_get_hw_state(hooks, display, pll_index, &mut hw_state);
    let pll = display.dplls[pll_index];
    if !pll.info.is_some_and(|info| info.always_on) {
        hooks.display_state_warn(
            display.display_id,
            !pll.on && pll.active_mask != 0,
            "PLL is in active use but SW tracking says off",
        );
        hooks.display_state_warn(
            display.display_id,
            pll.on && pll.active_mask == 0,
            "PLL is on but no active pipe uses it",
        );
        hooks.display_state_warn(
            display.display_id,
            pll.on != active,
            "PLL on-state mismatch between software and hardware",
        );
    }

    let Some(crtc) = crtc else {
        hooks.display_state_warn(
            display.display_id,
            pll.active_mask & !pll.state.pipe_mask != 0,
            "more active PLL users than tracked references",
        );
        return;
    };
    let pipe_mask = 1_u8 << crtc.pipe;
    let hw_active = new_crtc_state.is_some_and(|state| state.hw_active);
    hooks.display_state_warn(
        display.display_id,
        hw_active && pll.active_mask & pipe_mask == 0,
        "PLL active mask misses an active CRTC pipe",
    );
    hooks.display_state_warn(
        display.display_id,
        !hw_active && pll.active_mask & pipe_mask != 0,
        "PLL active mask unexpectedly includes an inactive CRTC pipe",
    );
    hooks.display_state_warn(
        display.display_id,
        pll.state.pipe_mask & pipe_mask == 0,
        "PLL reference mask misses the CRTC pipe",
    );

    if pll.on && !icl_compare_hw_state(&pll.state.hw_state, &hw_state) {
        if hooks.display_state_warn(display.display_id, true, "PLL hardware state mismatch") {
            hooks.log_hw_state(display.display_id, "observed DPLL hardware state", &hw_state);
            hooks.log_hw_state(display.display_id, "tracked DPLL state", &pll.state.hw_state);
        }
    }
}

// upstream: intel_dpll_mgr.c has_alt_port_dpll()
fn has_alt_port_dpll(old_pll: Option<usize>, new_pll: Option<usize>, display: &IntelDpllDisplay) -> bool {
    let old_info = old_pll.and_then(|index| display.dplls[index].info);
    let new_info = new_pll.and_then(|index| display.dplls[index].info);
    matches!((old_info, new_info), (Some(old), Some(new))
        if old.id != new.id && (old.is_alt_port_dpll || new.is_alt_port_dpll))
}

// upstream: intel_dpll_mgr.c intel_dpll_state_verify()
pub fn intel_dpll_state_verify<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
    state: &IntelAtomicState,
    crtc: &IntelCrtc,
) {
    let crtc_index = usize::from(crtc.pipe);
    let old_crtc_state = state.old_crtcs[crtc_index];
    let new_crtc_state = state.new_crtcs[crtc_index];
    if let Some(pll_index) = new_crtc_state.intel_dpll {
        verify_single_dpll_state(
            hooks,
            display,
            pll_index,
            Some(*crtc),
            Some(new_crtc_state),
        );
    }
    if let Some(old_pll_index) = old_crtc_state.intel_dpll {
        if Some(old_pll_index) != new_crtc_state.intel_dpll {
            let pipe_mask = 1_u8 << crtc.pipe;
            let pll = display.dplls[old_pll_index];
            hooks.display_state_warn(
                display.display_id,
                pll.active_mask & pipe_mask != 0,
                "old PLL remains active after CRTC switch",
            );
            hooks.display_state_warn(
                display.display_id,
                !has_alt_port_dpll(
                    old_crtc_state.intel_dpll,
                    new_crtc_state.intel_dpll,
                    display,
                ) && pll.state.pipe_mask & pipe_mask != 0,
                "old PLL reference remains after CRTC switch",
            );
        }
    }
}

// upstream: intel_dpll_mgr.c intel_dpll_verify_disabled()
pub fn intel_dpll_verify_disabled<H: IntelDpllHooks>(
    hooks: &mut H,
    display: &mut IntelDpllDisplay,
) {
    for pll_index in 0..display.num_dpll {
        verify_single_dpll_state(hooks, display, pll_index, None, None);
    }
}

fn div_round_up_u64(dividend: u64, divisor: u64) -> u64 {
    if divisor == 0 { 0 } else { dividend / divisor + u64::from(dividend % divisor != 0) }
}

#[cfg(test)]
mod readout_tests {
    use super::*;

    struct ReadoutHooks {
        dkl: crate::dpll_mgr::DklPllState,
    }

    impl IntelDpllHooks for ReadoutHooks {
        fn read32(&mut self, register: DpllRegister) -> u32 {
            match register {
                DpllRegister::AdlpTcEnable(TcPort::Tc1) => 1 << 31,
                _ => 0,
            }
        }
        fn write32(&mut self, _register: DpllRegister, _value: u32) {}
        fn posting_read32(&mut self, _register: DpllRegister) {}
        fn read_dkl(&mut self, _port: TcPort, offset: u16) -> u32 {
            match offset {
                0x212c => self.dkl.refclkin_ctl,
                0x20d4 => self.dkl.hsclkctl,
                0x20d8 => self.dkl.coreclkctl1,
                0x2200 => self.dkl.div0,
                0x2204 => self.dkl.div1,
                0x2210 => self.dkl.ssc,
                0x2214 => self.dkl.bias,
                0x2218 => self.dkl.tdc_coldst_bias,
                _ => 0,
            }
        }
        fn write_dkl(&mut self, _port: TcPort, _offset: u16, _value: u32) {}
        fn posting_read_dkl(&mut self, _port: TcPort, _offset: u16) {}
        fn wait_for_set(&mut self, _register: DpllRegister, _mask: u32, _timeout_ms: u32) -> bool {
            false
        }
        fn wait_for_clear(&mut self, _register: DpllRegister, _mask: u32, _timeout_ms: u32) -> bool {
            false
        }
        fn power_get(&mut self, _display_id: usize, _domain: PowerDomain) -> u64 {
            1
        }
        fn power_get_if_enabled(
            &mut self,
            _display_id: usize,
            _domain: PowerDomain,
        ) -> Option<u64> {
            Some(1)
        }
        fn power_put(&mut self, _display_id: usize, _domain: PowerDomain, _cookie: u64) {}
        fn dpll_mutex_init(&mut self, _display_id: usize) {}
        fn dpll_mutex_lock(&mut self, _display_id: usize) {}
        fn dpll_mutex_unlock(&mut self, _display_id: usize) {}
        fn connection_mutex_is_locked(&mut self, _display_id: usize) -> bool {
            true
        }
        fn drm_debug(&mut self, _display_id: usize, _message: &str) {}
        fn drm_error(&mut self, _display_id: usize, _message: &str) {}
        fn drm_warn(&mut self, _display_id: usize, _message: &str) {}
        fn drm_warn_on(&mut self, _display_id: usize, condition: bool, _message: &str) -> bool {
            condition
        }
        fn display_state_warn(
            &mut self,
            _display_id: usize,
            condition: bool,
            _message: &str,
        ) -> bool {
            condition
        }
        fn missing_case(&mut self, _display_id: usize, _value: u32) {}
        fn hti_dpll_mask(&mut self, _display_id: usize) -> u32 {
            0
        }
        fn is_adlp_step_a0_to_b0(&mut self, _display_id: usize) -> bool {
            false
        }
        fn intel_cx0pll_verify_plls(&mut self, _display_id: usize) {}
        fn intel_lt_phy_verify_plls(&mut self, _display_id: usize) {}
        fn intel_cx0_pll_power_save_wa(&mut self, _display_id: usize) {}
        fn log_hw_state(&mut self, _display_id: usize, _title: &str, _state: &IntelDpllHwState) {}
    }

    #[test]
    fn readout_keeps_observed_dkl_state_for_future_atomic_matching() {
        let dkl = crate::dpll_mgr::icl_calc_mg_pll_state_for_output(
            148_500,
            24_000,
            crate::dpll_mgr::MgPllOutput::Hdmi,
            None,
        )
        .unwrap();
        let mut display = IntelDpllDisplay::default();
        display.display_ver = 13;
        display.platform.alderlake_p = true;
        display.ref_clks.nssc = 24_000;
        display.num_dpll = 5;
        display.dplls[3].index = 3;
        display.dplls[3].info = Some(ADLP_DPLLS[3]);
        display.crtc_states[0] = CrtcState {
            id: 0,
            name: "Pipe A",
            pipe: 0,
            joined_pipe_mask: 1,
            hw_active: true,
            intel_dpll: Some(3),
            port_clock: 148_500,
            output: OutputType::Hdmi,
            port: Port::Tc(TcPort::Tc1),
            ..CrtcState::default()
        };
        let mut hooks = ReadoutHooks { dkl };

        readout_dpll_hw_state(&mut hooks, &mut display, 3);

        assert!(display.dplls[3].on);
        assert_eq!(display.dplls[3].state.pipe_mask, 1);
        assert_eq!(display.dplls[3].active_mask, 1);
        assert_eq!(display.dplls[3].state.hw_state.icl.mg_refclkin_ctl, dkl.refclkin_ctl);
        assert_eq!(display.dplls[3].state.hw_state.icl.mg_pll_div0, dkl.div0);
        assert_eq!(display.dplls[3].state.hw_state.icl.mg_pll_div1, dkl.div1);
    }

    #[test]
    fn tc_manager_compute_matches_dkl_hdmi_planner() {
        for (clock, afc_startup) in [(148_500, None), (297_000, Some(5))] {
            let dkl = crate::dpll_mgr::icl_calc_mg_pll_state_for_output(
                clock,
                24_000,
                crate::dpll_mgr::MgPllOutput::Hdmi,
                afc_startup,
            )
            .unwrap();
            let mut display = IntelDpllDisplay::default();
            display.display_ver = 13;
            display.platform.alderlake_p = true;
            display.ref_clks.nssc = 24_000;
            display.vbt.override_afc_startup = afc_startup.is_some();
            display.vbt.override_afc_startup_val = afc_startup.unwrap_or_default();
            let mut hooks = ReadoutHooks { dkl };
            intel_dpll_init(&mut hooks, &mut display);
            display.num_dpll = 4;

            let crtc_state = CrtcState {
                id: 0,
                name: "Pipe A",
                pipe: 0,
                joined_pipe_mask: 1,
                hw_active: true,
                intel_dpll: Some(3),
                port_clock: 297_000,
                output: OutputType::Hdmi,
                port: Port::Tc(TcPort::Tc1),
                ..CrtcState::default()
            };
            let crtc = IntelCrtc { id: 0, name: "Pipe A", pipe: 0 };
            let encoder = IntelEncoder {
                output: OutputType::Hdmi,
                port: Port::Tc(TcPort::Tc1),
                is_combo_phy: false,
                is_tc_phy: true,
                primary_port: None,
                tc_dp_alt_mode: false,
                tc_legacy_mode: true,
            };
            let mut state = IntelAtomicState::default();
            state.old_crtcs[0] = crtc_state;
            state.new_crtcs[0] = CrtcState { port_clock: clock, ..crtc_state };

            assert_eq!(intel_dpll_compute(&mut hooks, &display, &mut state, &crtc, &encoder), 0);
            let computed = state.new_crtcs[0].icl_port_dplls[PortDpllId::MgPhy as usize]
                .hw_state
                .icl;
            assert_eq!(state.new_crtcs[0].port_clock, clock);
            assert_eq!(computed.mg_refclkin_ctl, dkl.refclkin_ctl);
            assert_eq!(computed.mg_clktop2_coreclkctl1, dkl.coreclkctl1);
            assert_eq!(computed.mg_clktop2_hsclkctl, dkl.hsclkctl);
            assert_eq!(computed.mg_pll_div0, dkl.div0);
            assert_eq!(computed.mg_pll_div1, dkl.div1);
            assert_eq!(computed.mg_pll_ssc, dkl.ssc);
            assert_eq!(computed.mg_pll_bias, dkl.bias);
            assert_eq!(computed.mg_pll_tdc_coldst_bias, dkl.tdc_coldst_bias);
        }
    }

    #[test]
    fn tc_manager_readout_can_reserve_the_active_tbt_and_dkl_port_slots() {
        let dkl = crate::dpll_mgr::icl_calc_mg_pll_state_for_output(
            148_500,
            24_000,
            crate::dpll_mgr::MgPllOutput::Hdmi,
            None,
        )
        .unwrap();
        let mut display = IntelDpllDisplay::default();
        display.display_ver = 13;
        display.platform.alderlake_p = true;
        display.ref_clks.nssc = 24_000;
        let mut hooks = ReadoutHooks { dkl };
        intel_dpll_init(&mut hooks, &mut display);
        // The selected-port adapter exposes combo DPLL0/1, TBT, and TC1 DKL.
        display.num_dpll = 4;
        display.crtc_states[0] = CrtcState {
            id: 0,
            name: "Pipe A",
            pipe: 0,
            joined_pipe_mask: 1,
            hw_active: true,
            intel_dpll: Some(3),
            port_clock: 148_500,
            output: OutputType::Hdmi,
            port: Port::Tc(TcPort::Tc1),
            ..CrtcState::default()
        };
        intel_dpll_readout_hw_state(&mut hooks, &mut display);
        let readout = display.dplls[3].state.hw_state;
        display.crtc_states[0].dpll_hw_state = readout;
        display.crtc_states[0].icl_port_dplls[PortDpllId::MgPhy as usize] = IclPortDpll {
            pll: Some(3),
            hw_state: readout,
        };

        let mut state = IntelAtomicState::default();
        state.old_crtcs = display.crtc_states;
        state.new_crtcs = display.crtc_states;
        let crtc = IntelCrtc { id: 0, name: "Pipe A", pipe: 0 };
        let encoder = IntelEncoder {
            output: OutputType::Hdmi,
            port: Port::Tc(TcPort::Tc1),
            is_combo_phy: false,
            is_tc_phy: true,
            primary_port: None,
            tc_dp_alt_mode: false,
            tc_legacy_mode: true,
        };
        assert_eq!(intel_dpll_compute(&mut hooks, &display, &mut state, &crtc, &encoder), 0);
        intel_dpll_release(&mut hooks, &display, &mut state, &crtc);
        assert_eq!(intel_dpll_reserve(&mut hooks, &display, &mut state, &crtc, &encoder), 0);
        let new_crtc = state.new_crtcs[0];
        assert_eq!(new_crtc.icl_port_dplls[PortDpllId::Default as usize].pll, Some(2));
        assert_eq!(new_crtc.icl_port_dplls[PortDpllId::MgPhy as usize].pll, Some(3));
        assert_eq!(new_crtc.intel_dpll, Some(3));
        assert_eq!(state.dpll_state[2].pipe_mask, 1);
        assert_eq!(state.dpll_state[3].pipe_mask, 1);

        intel_dpll_swap_state(&mut display, &mut state);
        display.crtc_states = state.new_crtcs;
        let active = display.crtc_states[0];
        intel_dpll_disable(&mut hooks, &mut display, &active);
        assert_eq!(display.dplls[3].active_mask, 0);
        assert!(!display.dplls[3].on);
        intel_dpll_enable(&mut hooks, &mut display, &active);
        assert_eq!(display.dplls[3].active_mask, 1);
        assert!(display.dplls[3].on);

        let mut switched = IntelAtomicState::default();
        switched.old_crtcs = display.crtc_states;
        switched.new_crtcs = display.crtc_states;
        switched.new_crtcs[0].port_clock = 297_000;
        assert_eq!(intel_dpll_compute(&mut hooks, &display, &mut switched, &crtc, &encoder), 0);
        intel_dpll_release(&mut hooks, &display, &mut switched, &crtc);
        assert_eq!(intel_dpll_reserve(&mut hooks, &display, &mut switched, &crtc, &encoder), 0);
        let target = switched.new_crtcs[0];
        let expected = crate::dpll_mgr::icl_calc_mg_pll_state(
            297_000,
            24_000,
            None,
        )
        .unwrap();
        assert_eq!(target.intel_dpll, Some(3));
        assert_eq!(target.icl_port_dplls[PortDpllId::Default as usize].pll, Some(2));
        assert_eq!(target.icl_port_dplls[PortDpllId::MgPhy as usize].pll, Some(3));
        assert_eq!(target.dpll_hw_state.icl.mg_pll_div0, expected.div0);
        assert_eq!(switched.dpll_state[2].pipe_mask, 1);
        assert_eq!(switched.dpll_state[3].pipe_mask, 1);
    }
}
