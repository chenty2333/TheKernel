// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dpll_mgr.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::intel_display_power_types_upstream::{IntelDisplayPowerDomain};

#[repr(C)]
pub struct I9xxDpllHwState {
    pub dpll: u32,
    pub dpll_md: u32,
    pub fp0: u32,
    pub fp1: u32,
}

#[repr(C)]
pub struct HswDpllHwState {
    pub wrpll: u32,
    pub spll: u32,
}

#[repr(C)]
pub struct SklDpllHwState {
    pub ctrl1: u32,
    pub cfgcr1: u32,
    pub cfgcr2: u32,
}

#[repr(C)]
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

#[repr(C)]
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

#[repr(C)]
pub struct IntelMpllbState {
    pub clock: u32,
    pub ref_control: u32,
    pub mpllb_cp: u32,
    pub mpllb_div: u32,
    pub mpllb_div2: u32,
    pub mpllb_fracn1: u32,
    pub mpllb_fracn2: u32,
    pub mpllb_sscen: u32,
    pub mpllb_sscstep: u32,
}

#[repr(C)]
pub struct IntelC10pllState {
    pub tx: u8,
    pub cmn: u8,
    pub pll: [u8; 20],
}

#[repr(C)]
pub union IntelC20pllStateAnon14x2 {
    pub mplla: [u16; 10],
    pub mpllb: [u16; 11],
}

#[repr(C)]
pub struct IntelC20pllState {
    pub tx: [u16; 3],
    pub cmn: [u16; 4],
    pub _anon_14_2: IntelC20pllStateAnon14x2,
    pub vdr: IntelC20pllVdrState,
    pub _pad_39: [u8; 1],
}

#[repr(C)]
pub struct IntelC20pllVdrState {
    pub custom_width: u8,
    pub serdes_rate: u8,
    pub hdmi_rate: u8,
}

#[repr(C)]
pub union IntelCx0pllStateAnon0x0 {
    pub c10: ManuallyDrop<IntelC10pllState>,
    pub c20: ManuallyDrop<IntelC20pllState>,
}

#[repr(C)]
pub struct IntelCx0pllState {
    pub _anon_0_0: IntelCx0pllStateAnon0x0,
    pub lane_count: i32,
    pub ssc_enabled: bool,
    pub use_c10: bool,
    pub tbt_mode: bool,
    pub _pad_47: [u8; 1],
}

#[repr(C)]
pub struct IntelLtPhyPllState {
    pub addr_msb: [u8; 13],
    pub addr_lsb: [u8; 13],
    pub data: [[u8; 4]; 13],
    pub config: [u8; 3],
    pub ssc_enabled: bool,
    pub tbt_mode: bool,
    pub _pad_83: [u8; 1],
    pub lane_count: i32,
}

#[repr(C)]
pub union IntelDpllHwStateAnon0x0 {
    pub i9xx: ManuallyDrop<I9xxDpllHwState>,
    pub hsw: ManuallyDrop<HswDpllHwState>,
    pub skl: ManuallyDrop<SklDpllHwState>,
    pub bxt: ManuallyDrop<BxtDpllHwState>,
    pub icl: ManuallyDrop<IclDpllHwState>,
    pub mpllb: ManuallyDrop<IntelMpllbState>,
    pub cx0pll: ManuallyDrop<IntelCx0pllState>,
    pub ltpll: ManuallyDrop<IntelLtPhyPllState>,
}

#[repr(C)]
pub struct IntelDpllHwState {
    pub _anon_0_0: IntelDpllHwStateAnon0x0,
}

#[repr(C)]
pub struct IntelDpllState {
    pub pipe_mask: u8,
    pub _pad_1: [u8; 3],
    pub hw_state: IntelDpllHwState,
}

#[repr(C)]
pub struct DpllInfo {
    pub name: *mut c_char,
    pub funcs: *mut c_void,
    pub id: IntelDpllId,
    pub power_domain: IntelDisplayPowerDomain,
    pub always_on: bool,
    pub is_alt_port_dpll: bool,
    pub _pad_26: [u8; 6],
}

#[repr(C)]
pub struct IntelDpll {
    pub state: IntelDpllState,
    pub index: u8,
    pub active_mask: u8,
    pub on: bool,
    pub _pad_95: [u8; 1],
    pub info: *mut DpllInfo,
    pub wakeref: *mut c_void,
}

pub type IclPortDpllId = u32;
pub const ICL_PORT_DPLL_DEFAULT: IclPortDpllId = 0;
pub const ICL_PORT_DPLL_MG_PHY: IclPortDpllId = 1;
pub const ICL_PORT_DPLL_COUNT: IclPortDpllId = 2;

pub type IntelDpllId = i32;
pub const DPLL_ID_PRIVATE: IntelDpllId = -1;
pub const DPLL_ID_PCH_PLL_A: IntelDpllId = 0;
pub const DPLL_ID_PCH_PLL_B: IntelDpllId = 1;
pub const DPLL_ID_WRPLL1: IntelDpllId = 0;
pub const DPLL_ID_WRPLL2: IntelDpllId = 1;
pub const DPLL_ID_SPLL: IntelDpllId = 2;
pub const DPLL_ID_LCPLL_810: IntelDpllId = 3;
pub const DPLL_ID_LCPLL_1350: IntelDpllId = 4;
pub const DPLL_ID_LCPLL_2700: IntelDpllId = 5;
pub const DPLL_ID_SKL_DPLL0: IntelDpllId = 0;
pub const DPLL_ID_SKL_DPLL1: IntelDpllId = 1;
pub const DPLL_ID_SKL_DPLL2: IntelDpllId = 2;
pub const DPLL_ID_SKL_DPLL3: IntelDpllId = 3;
pub const DPLL_ID_ICL_DPLL0: IntelDpllId = 0;
pub const DPLL_ID_ICL_DPLL1: IntelDpllId = 1;
pub const DPLL_ID_EHL_DPLL4: IntelDpllId = 2;
pub const DPLL_ID_ICL_TBTPLL: IntelDpllId = 2;
pub const DPLL_ID_ICL_MGPLL1: IntelDpllId = 3;
pub const DPLL_ID_ICL_MGPLL2: IntelDpllId = 4;
pub const DPLL_ID_ICL_MGPLL3: IntelDpllId = 5;
pub const DPLL_ID_ICL_MGPLL4: IntelDpllId = 6;
pub const DPLL_ID_TGL_MGPLL5: IntelDpllId = 7;
pub const DPLL_ID_TGL_MGPLL6: IntelDpllId = 8;
pub const DPLL_ID_DG1_DPLL0: IntelDpllId = 0;
pub const DPLL_ID_DG1_DPLL1: IntelDpllId = 1;
pub const DPLL_ID_DG1_DPLL2: IntelDpllId = 2;
pub const DPLL_ID_DG1_DPLL3: IntelDpllId = 3;

const _: () = assert!(size_of::<I9xxDpllHwState>() == 16);
const _: () = assert!(core::mem::offset_of!(I9xxDpllHwState, dpll) == 0);
const _: () = assert!(core::mem::offset_of!(I9xxDpllHwState, dpll_md) == 4);
const _: () = assert!(core::mem::offset_of!(I9xxDpllHwState, fp0) == 8);
const _: () = assert!(core::mem::offset_of!(I9xxDpllHwState, fp1) == 12);
const _: () = assert!(size_of::<HswDpllHwState>() == 8);
const _: () = assert!(core::mem::offset_of!(HswDpllHwState, wrpll) == 0);
const _: () = assert!(core::mem::offset_of!(HswDpllHwState, spll) == 4);
const _: () = assert!(size_of::<SklDpllHwState>() == 12);
const _: () = assert!(core::mem::offset_of!(SklDpllHwState, ctrl1) == 0);
const _: () = assert!(core::mem::offset_of!(SklDpllHwState, cfgcr1) == 4);
const _: () = assert!(core::mem::offset_of!(SklDpllHwState, cfgcr2) == 8);
const _: () = assert!(size_of::<BxtDpllHwState>() == 44);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, ebb0) == 0);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, ebb4) == 4);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll0) == 8);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll1) == 12);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll2) == 16);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll3) == 20);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll6) == 24);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll8) == 28);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll9) == 32);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pll10) == 36);
const _: () = assert!(core::mem::offset_of!(BxtDpllHwState, pcsdw12) == 40);
const _: () = assert!(size_of::<IclDpllHwState>() == 60);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, cfgcr0) == 0);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, cfgcr1) == 4);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, div0) == 8);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_refclkin_ctl) == 12);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_clktop2_coreclkctl1) == 16);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_clktop2_hsclkctl) == 20);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_div0) == 24);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_div1) == 28);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_lf) == 32);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_frac_lock) == 36);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_ssc) == 40);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_bias) == 44);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_tdc_coldst_bias) == 48);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_bias_mask) == 52);
const _: () = assert!(core::mem::offset_of!(IclDpllHwState, mg_pll_tdc_coldst_bias_mask) == 56);
const _: () = assert!(size_of::<IntelMpllbState>() == 36);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, clock) == 0);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, ref_control) == 4);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_cp) == 8);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_div) == 12);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_div2) == 16);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_fracn1) == 20);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_fracn2) == 24);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_sscen) == 28);
const _: () = assert!(core::mem::offset_of!(IntelMpllbState, mpllb_sscstep) == 32);
const _: () = assert!(size_of::<IntelC10pllState>() == 22);
const _: () = assert!(core::mem::offset_of!(IntelC10pllState, tx) == 0);
const _: () = assert!(core::mem::offset_of!(IntelC10pllState, cmn) == 1);
const _: () = assert!(core::mem::offset_of!(IntelC10pllState, pll) == 2);
const _: () = assert!(size_of::<[u8; 20]>() == 20);
const _: () = assert!(size_of::<IntelC20pllStateAnon14x2>() == 22);
const _: () = assert!(size_of::<IntelC20pllState>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelC20pllState, tx) == 0);
const _: () = assert!(size_of::<[u16; 3]>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelC20pllState, cmn) == 6);
const _: () = assert!(size_of::<[u16; 4]>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelC20pllState, _anon_14_2) == 14);
const _: () = assert!(core::mem::offset_of!(IntelC20pllState, vdr) == 36);
const _: () = assert!(size_of::<IntelC20pllVdrState>() == 3);
const _: () = assert!(size_of::<IntelC20pllVdrState>() == 3);
const _: () = assert!(core::mem::offset_of!(IntelC20pllVdrState, custom_width) == 0);
const _: () = assert!(core::mem::offset_of!(IntelC20pllVdrState, serdes_rate) == 1);
const _: () = assert!(core::mem::offset_of!(IntelC20pllVdrState, hdmi_rate) == 2);
const _: () = assert!(size_of::<IntelCx0pllStateAnon0x0>() == 40);
const _: () = assert!(size_of::<IntelCx0pllState>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelCx0pllState, _anon_0_0) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCx0pllState, lane_count) == 40);
const _: () = assert!(core::mem::offset_of!(IntelCx0pllState, ssc_enabled) == 44);
const _: () = assert!(core::mem::offset_of!(IntelCx0pllState, use_c10) == 45);
const _: () = assert!(core::mem::offset_of!(IntelCx0pllState, tbt_mode) == 46);
const _: () = assert!(size_of::<IntelLtPhyPllState>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, addr_msb) == 0);
const _: () = assert!(size_of::<[u8; 13]>() == 13);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, addr_lsb) == 13);
const _: () = assert!(size_of::<[u8; 13]>() == 13);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, data) == 26);
const _: () = assert!(size_of::<[[u8; 4]; 13]>() == 52);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, config) == 78);
const _: () = assert!(size_of::<[u8; 3]>() == 3);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, ssc_enabled) == 81);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, tbt_mode) == 82);
const _: () = assert!(core::mem::offset_of!(IntelLtPhyPllState, lane_count) == 84);
const _: () = assert!(size_of::<IntelDpllHwStateAnon0x0>() == 88);
const _: () = assert!(size_of::<IntelDpllHwState>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelDpllHwState, _anon_0_0) == 0);
const _: () = assert!(size_of::<IntelDpllState>() == 92);
const _: () = assert!(core::mem::offset_of!(IntelDpllState, pipe_mask) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpllState, hw_state) == 4);
const _: () = assert!(size_of::<IntelDpllHwState>() == 88);
const _: () = assert!(size_of::<DpllInfo>() == 32);
const _: () = assert!(core::mem::offset_of!(DpllInfo, name) == 0);
const _: () = assert!(core::mem::offset_of!(DpllInfo, funcs) == 8);
const _: () = assert!(core::mem::offset_of!(DpllInfo, id) == 16);
const _: () = assert!(size_of::<IntelDpllId>() == 4);
const _: () = assert!(core::mem::offset_of!(DpllInfo, power_domain) == 20);
const _: () = assert!(size_of::<IntelDisplayPowerDomain>() == 4);
const _: () = assert!(core::mem::offset_of!(DpllInfo, always_on) == 24);
const _: () = assert!(core::mem::offset_of!(DpllInfo, is_alt_port_dpll) == 25);
const _: () = assert!(size_of::<IntelDpll>() == 112);
const _: () = assert!(core::mem::offset_of!(IntelDpll, state) == 0);
const _: () = assert!(size_of::<IntelDpllState>() == 92);
const _: () = assert!(core::mem::offset_of!(IntelDpll, index) == 92);
const _: () = assert!(core::mem::offset_of!(IntelDpll, active_mask) == 93);
const _: () = assert!(core::mem::offset_of!(IntelDpll, on) == 94);
const _: () = assert!(core::mem::offset_of!(IntelDpll, info) == 96);
const _: () = assert!(core::mem::offset_of!(IntelDpll, wakeref) == 104);
