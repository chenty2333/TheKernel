// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_power_well.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::intel_display_power_types_upstream::{IntelPowerDomainMask};
use crate::intel_dpio_phy_types_upstream::{DpioPhy};

#[repr(C)]
pub struct I915PowerWellInstanceAnon20x3Vlv {
    pub idx: u8,
}

#[repr(C)]
pub struct I915PowerWellInstanceAnon20x3Bxt {
    pub phy: DpioPhy,
}

#[repr(C)]
pub struct I915PowerWellInstanceAnon20x3Hsw {
    pub idx: u8,
}

#[repr(C)]
pub struct I915PowerWellInstanceAnon20x3Xelpdp {
    pub aux_ch: u8,
}

#[repr(C)]
pub union I915PowerWellInstanceAnon20x3 {
    pub vlv: ManuallyDrop<I915PowerWellInstanceAnon20x3Vlv>,
    pub bxt: ManuallyDrop<I915PowerWellInstanceAnon20x3Bxt>,
    pub hsw: ManuallyDrop<I915PowerWellInstanceAnon20x3Hsw>,
    pub xelpdp: ManuallyDrop<I915PowerWellInstanceAnon20x3Xelpdp>,
}

#[repr(C)]
pub struct I915PowerWellInstance {
    pub name: *mut c_char,
    pub domain_list: *mut c_void,
    pub id: I915PowerWellId,
    pub _anon_20_3: I915PowerWellInstanceAnon20x3,
}

#[repr(C)]
pub struct I915PowerWellDesc {
    pub ops: *mut c_void,
    pub instances: *mut c_void,
    /// bit-fields irq_pipe_mask:4, always_on:1, fixed_enable_delay:1, has_fuses:1, is_tc_tbt:1
    pub _bitfields_16: [u8; 1],
    pub _pad_17: [u8; 1],
    pub enable_timeout: u16,
    pub _pad_20: [u8; 4],
}

#[repr(C)]
pub struct I915PowerWell {
    pub desc: *mut I915PowerWellDesc,
    pub domains: IntelPowerDomainMask,
    pub count: i32,
    pub hw_enabled: bool,
    pub instance_idx: u8,
    pub _pad_30: [u8; 2],
}

pub type I915PowerWellId = u32;
pub const DISP_PW_ID_NONE: I915PowerWellId = 0;
pub const VLV_DISP_PW_DISP2D: I915PowerWellId = 1;
pub const BXT_DISP_PW_DPIO_CMN_A: I915PowerWellId = 2;
pub const VLV_DISP_PW_DPIO_CMN_BC: I915PowerWellId = 3;
pub const GLK_DISP_PW_DPIO_CMN_C: I915PowerWellId = 4;
pub const CHV_DISP_PW_DPIO_CMN_D: I915PowerWellId = 5;
pub const HSW_DISP_PW_GLOBAL: I915PowerWellId = 6;
pub const SKL_DISP_PW_MISC_IO: I915PowerWellId = 7;
pub const SKL_DISP_PW_1: I915PowerWellId = 8;
pub const SKL_DISP_PW_2: I915PowerWellId = 9;
pub const ICL_DISP_PW_3: I915PowerWellId = 10;
pub const SKL_DISP_DC_OFF: I915PowerWellId = 11;
pub const TGL_DISP_PW_TC_COLD_OFF: I915PowerWellId = 12;

const _: () = assert!(size_of::<I915PowerWellInstanceAnon20x3Vlv>() == 1);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstanceAnon20x3Vlv, idx) == 0);
const _: () = assert!(size_of::<I915PowerWellInstanceAnon20x3Bxt>() == 4);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstanceAnon20x3Bxt, phy) == 0);
const _: () = assert!(size_of::<DpioPhy>() == 4);
const _: () = assert!(size_of::<I915PowerWellInstanceAnon20x3Hsw>() == 1);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstanceAnon20x3Hsw, idx) == 0);
const _: () = assert!(size_of::<I915PowerWellInstanceAnon20x3Xelpdp>() == 1);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstanceAnon20x3Xelpdp, aux_ch) == 0);
const _: () = assert!(size_of::<I915PowerWellInstanceAnon20x3>() == 4);
const _: () = assert!(size_of::<I915PowerWellInstance>() == 24);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstance, name) == 0);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstance, domain_list) == 8);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstance, id) == 16);
const _: () = assert!(size_of::<I915PowerWellId>() == 4);
const _: () = assert!(core::mem::offset_of!(I915PowerWellInstance, _anon_20_3) == 20);
const _: () = assert!(size_of::<I915PowerWellDesc>() == 24);
const _: () = assert!(core::mem::offset_of!(I915PowerWellDesc, ops) == 0);
const _: () = assert!(core::mem::offset_of!(I915PowerWellDesc, instances) == 8);
const _: () = assert!(core::mem::offset_of!(I915PowerWellDesc, _bitfields_16) == 16);
const _: () = assert!(core::mem::offset_of!(I915PowerWellDesc, enable_timeout) == 18);
const _: () = assert!(size_of::<I915PowerWell>() == 32);
const _: () = assert!(core::mem::offset_of!(I915PowerWell, desc) == 0);
const _: () = assert!(core::mem::offset_of!(I915PowerWell, domains) == 8);
const _: () = assert!(size_of::<IntelPowerDomainMask>() == 16);
const _: () = assert!(core::mem::offset_of!(I915PowerWell, count) == 24);
const _: () = assert!(core::mem::offset_of!(I915PowerWell, hw_enabled) == 28);
const _: () = assert!(core::mem::offset_of!(I915PowerWell, instance_idx) == 29);
