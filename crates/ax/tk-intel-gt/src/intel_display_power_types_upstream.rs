// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_power.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::DelayedWork;
use tk_linux_kapi::types::Mutex;
use crate::intel_display_power_well_types_upstream::{I915PowerWell};

#[repr(C)]
pub struct IntelPowerDomainMask {
    pub bits: [u64; 2],
}

#[repr(C, align(8))]
pub struct I915PowerDomains {
    pub initializing: bool,
    pub display_core_suspended: bool,
    pub _pad_2: [u8; 2],
    pub power_well_count: i32,
    pub dc_state: u32,
    pub target_dc_state: u32,
    pub allowed_dc_mask: u32,
    pub _pad_20: [u8; 4],
    pub init_wakeref: *mut c_void,
    pub disable_wakeref: *mut c_void,
    pub lock: Mutex,
    pub domain_use_count: [i32; 76],
    pub async_put_work: DelayedWork,
    pub async_put_wakeref: *mut c_void,
    pub async_put_domains: [IntelPowerDomainMask; 2],
    pub async_put_next_delay: i32,
    pub _pad_500: [u8; 4],
    pub power_wells: *mut I915PowerWell,
}

#[repr(C)]
pub struct IntelDisplayPowerDomainSet {
    pub mask: IntelPowerDomainMask,
}

pub type IntelDisplayPowerDomain = u32;
pub const POWER_DOMAIN_DISPLAY_CORE: IntelDisplayPowerDomain = 0;
pub const POWER_DOMAIN_PIPE_A: IntelDisplayPowerDomain = 1;
pub const POWER_DOMAIN_PIPE_B: IntelDisplayPowerDomain = 2;
pub const POWER_DOMAIN_PIPE_C: IntelDisplayPowerDomain = 3;
pub const POWER_DOMAIN_PIPE_D: IntelDisplayPowerDomain = 4;
pub const POWER_DOMAIN_PIPE_PANEL_FITTER_A: IntelDisplayPowerDomain = 5;
pub const POWER_DOMAIN_PIPE_PANEL_FITTER_B: IntelDisplayPowerDomain = 6;
pub const POWER_DOMAIN_PIPE_PANEL_FITTER_C: IntelDisplayPowerDomain = 7;
pub const POWER_DOMAIN_PIPE_PANEL_FITTER_D: IntelDisplayPowerDomain = 8;
pub const POWER_DOMAIN_TRANSCODER_A: IntelDisplayPowerDomain = 9;
pub const POWER_DOMAIN_TRANSCODER_B: IntelDisplayPowerDomain = 10;
pub const POWER_DOMAIN_TRANSCODER_C: IntelDisplayPowerDomain = 11;
pub const POWER_DOMAIN_TRANSCODER_D: IntelDisplayPowerDomain = 12;
pub const POWER_DOMAIN_TRANSCODER_EDP: IntelDisplayPowerDomain = 13;
pub const POWER_DOMAIN_TRANSCODER_DSI_A: IntelDisplayPowerDomain = 14;
pub const POWER_DOMAIN_TRANSCODER_DSI_C: IntelDisplayPowerDomain = 15;
pub const POWER_DOMAIN_TRANSCODER_VDSC_PW2: IntelDisplayPowerDomain = 16;
pub const POWER_DOMAIN_PORT_DDI_LANES_A: IntelDisplayPowerDomain = 17;
pub const POWER_DOMAIN_PORT_DDI_LANES_B: IntelDisplayPowerDomain = 18;
pub const POWER_DOMAIN_PORT_DDI_LANES_C: IntelDisplayPowerDomain = 19;
pub const POWER_DOMAIN_PORT_DDI_LANES_D: IntelDisplayPowerDomain = 20;
pub const POWER_DOMAIN_PORT_DDI_LANES_E: IntelDisplayPowerDomain = 21;
pub const POWER_DOMAIN_PORT_DDI_LANES_F: IntelDisplayPowerDomain = 22;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC1: IntelDisplayPowerDomain = 23;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC2: IntelDisplayPowerDomain = 24;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC3: IntelDisplayPowerDomain = 25;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC4: IntelDisplayPowerDomain = 26;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC5: IntelDisplayPowerDomain = 27;
pub const POWER_DOMAIN_PORT_DDI_LANES_TC6: IntelDisplayPowerDomain = 28;
pub const POWER_DOMAIN_PORT_DDI_IO_A: IntelDisplayPowerDomain = 29;
pub const POWER_DOMAIN_PORT_DDI_IO_B: IntelDisplayPowerDomain = 30;
pub const POWER_DOMAIN_PORT_DDI_IO_C: IntelDisplayPowerDomain = 31;
pub const POWER_DOMAIN_PORT_DDI_IO_D: IntelDisplayPowerDomain = 32;
pub const POWER_DOMAIN_PORT_DDI_IO_E: IntelDisplayPowerDomain = 33;
pub const POWER_DOMAIN_PORT_DDI_IO_F: IntelDisplayPowerDomain = 34;
pub const POWER_DOMAIN_PORT_DDI_IO_TC1: IntelDisplayPowerDomain = 35;
pub const POWER_DOMAIN_PORT_DDI_IO_TC2: IntelDisplayPowerDomain = 36;
pub const POWER_DOMAIN_PORT_DDI_IO_TC3: IntelDisplayPowerDomain = 37;
pub const POWER_DOMAIN_PORT_DDI_IO_TC4: IntelDisplayPowerDomain = 38;
pub const POWER_DOMAIN_PORT_DDI_IO_TC5: IntelDisplayPowerDomain = 39;
pub const POWER_DOMAIN_PORT_DDI_IO_TC6: IntelDisplayPowerDomain = 40;
pub const POWER_DOMAIN_PORT_DSI: IntelDisplayPowerDomain = 41;
pub const POWER_DOMAIN_PORT_CRT: IntelDisplayPowerDomain = 42;
pub const POWER_DOMAIN_PORT_OTHER: IntelDisplayPowerDomain = 43;
pub const POWER_DOMAIN_VGA: IntelDisplayPowerDomain = 44;
pub const POWER_DOMAIN_AUDIO_MMIO: IntelDisplayPowerDomain = 45;
pub const POWER_DOMAIN_AUDIO_PLAYBACK: IntelDisplayPowerDomain = 46;
pub const POWER_DOMAIN_AUX_IO_A: IntelDisplayPowerDomain = 47;
pub const POWER_DOMAIN_AUX_IO_B: IntelDisplayPowerDomain = 48;
pub const POWER_DOMAIN_AUX_IO_C: IntelDisplayPowerDomain = 49;
pub const POWER_DOMAIN_AUX_IO_D: IntelDisplayPowerDomain = 50;
pub const POWER_DOMAIN_AUX_IO_E: IntelDisplayPowerDomain = 51;
pub const POWER_DOMAIN_AUX_IO_F: IntelDisplayPowerDomain = 52;
pub const POWER_DOMAIN_AUX_A: IntelDisplayPowerDomain = 53;
pub const POWER_DOMAIN_AUX_B: IntelDisplayPowerDomain = 54;
pub const POWER_DOMAIN_AUX_C: IntelDisplayPowerDomain = 55;
pub const POWER_DOMAIN_AUX_D: IntelDisplayPowerDomain = 56;
pub const POWER_DOMAIN_AUX_E: IntelDisplayPowerDomain = 57;
pub const POWER_DOMAIN_AUX_F: IntelDisplayPowerDomain = 58;
pub const POWER_DOMAIN_AUX_USBC1: IntelDisplayPowerDomain = 59;
pub const POWER_DOMAIN_AUX_USBC2: IntelDisplayPowerDomain = 60;
pub const POWER_DOMAIN_AUX_USBC3: IntelDisplayPowerDomain = 61;
pub const POWER_DOMAIN_AUX_USBC4: IntelDisplayPowerDomain = 62;
pub const POWER_DOMAIN_AUX_USBC5: IntelDisplayPowerDomain = 63;
pub const POWER_DOMAIN_AUX_USBC6: IntelDisplayPowerDomain = 64;
pub const POWER_DOMAIN_AUX_TBT1: IntelDisplayPowerDomain = 65;
pub const POWER_DOMAIN_AUX_TBT2: IntelDisplayPowerDomain = 66;
pub const POWER_DOMAIN_AUX_TBT3: IntelDisplayPowerDomain = 67;
pub const POWER_DOMAIN_AUX_TBT4: IntelDisplayPowerDomain = 68;
pub const POWER_DOMAIN_AUX_TBT5: IntelDisplayPowerDomain = 69;
pub const POWER_DOMAIN_AUX_TBT6: IntelDisplayPowerDomain = 70;
pub const POWER_DOMAIN_GMBUS: IntelDisplayPowerDomain = 71;
pub const POWER_DOMAIN_GT_IRQ: IntelDisplayPowerDomain = 72;
pub const POWER_DOMAIN_DC_OFF: IntelDisplayPowerDomain = 73;
pub const POWER_DOMAIN_TC_COLD_OFF: IntelDisplayPowerDomain = 74;
pub const POWER_DOMAIN_INIT: IntelDisplayPowerDomain = 75;
pub const POWER_DOMAIN_NUM: IntelDisplayPowerDomain = 76;
pub const POWER_DOMAIN_INVALID: IntelDisplayPowerDomain = 76;

const _: () = assert!(size_of::<IntelPowerDomainMask>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelPowerDomainMask, bits) == 0);
const _: () = assert!(size_of::<[u64; 2]>() == 16);
const _: () = assert!(size_of::<I915PowerDomains>() == 512);
const _: () = assert!(align_of::<I915PowerDomains>() == 8);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, initializing) == 0);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, display_core_suspended) == 1);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, power_well_count) == 4);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, dc_state) == 8);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, target_dc_state) == 12);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, allowed_dc_mask) == 16);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, init_wakeref) == 24);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, disable_wakeref) == 32);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, lock) == 40);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, domain_use_count) == 64);
const _: () = assert!(size_of::<[i32; 76]>() == 304);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, async_put_work) == 368);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, async_put_wakeref) == 456);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, async_put_domains) == 464);
const _: () = assert!(size_of::<[IntelPowerDomainMask; 2]>() == 32);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, async_put_next_delay) == 496);
const _: () = assert!(core::mem::offset_of!(I915PowerDomains, power_wells) == 504);
const _: () = assert!(size_of::<IntelDisplayPowerDomainSet>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPowerDomainSet, mask) == 0);
const _: () = assert!(size_of::<IntelPowerDomainMask>() == 16);
