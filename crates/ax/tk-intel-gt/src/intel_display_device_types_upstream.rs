// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_device.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IntelDisplayPlatformsAnon0x0Anon0x0 {
    /// bit-fields g4x:1, mobile:1, dgfx:1, i830:1, i845g:1, i85x:1, i865g:1, i915g:1, i915gm:1, i945g:1, i945gm:1, g33:1, pineview:1, i965g:1, i965gm:1, g45:1, gm45:1, ironlake:1, sandybridge:1, ivybridge:1, valleyview:1, haswell:1, haswell_ult:1, haswell_ulx:1, broadwell:1, broadwell_ult:1, broadwell_ulx:1, cherryview:1, skylake:1, skylake_ult:1, skylake_ulx:1, broxton:1, kabylake:1, kabylake_ult:1, kabylake_ulx:1, geminilake:1, coffeelake:1, coffeelake_ult:1, coffeelake_ulx:1, cometlake:1, cometlake_ult:1, cometlake_ulx:1, icelake:1, icelake_port_f:1, jasperlake:1, elkhartlake:1, tigerlake:1, tigerlake_uy:1, rocketlake:1, dg1:1, alderlake_s:1, alderlake_s_raptorlake_s:1, alderlake_p:1, alderlake_p_alderlake_n:1, alderlake_p_raptorlake_p:1, alderlake_p_raptorlake_u:1, dg2:1, dg2_g10:1, dg2_g11:1, dg2_g12:1, meteorlake:1, meteorlake_u:1, lunarlake:1, battlemage:1, pantherlake:1, pantherlake_wildcatlake:1, novalake:1
    pub _bitfields_0: [u8; 9],
    pub _pad_9: [u8; 7],
}

#[repr(C)]
pub union IntelDisplayPlatformsAnon0x0 {
    pub _anon_0_0: ManuallyDrop<IntelDisplayPlatformsAnon0x0Anon0x0>,
    pub bitmap: [u64; 2],
}

#[repr(C)]
pub struct IntelDisplayPlatforms {
    pub _anon_0_0: IntelDisplayPlatformsAnon0x0,
}

#[repr(C)]
pub struct IntelDisplayRuntimeInfo {
    pub ip: IntelDisplayIpVer,
    pub _pad_6: [u8; 2],
    pub step: i32,
    pub step_name: [c_char; 3],
    pub _pad_15: [u8; 1],
    pub rawclk_freq: u32,
    pub pipe_mask: u8,
    pub cpu_transcoder_mask: u8,
    pub port_mask: u16,
    pub num_sprites: [u8; 4],
    pub num_scalers: [u8; 4],
    pub fbc_mask: u8,
    pub has_hdcp: bool,
    pub has_dmc: bool,
    pub has_dsc: bool,
    pub edp_typec_support: bool,
    pub has_dbuf_overlap_detection: bool,
    pub _pad_38: [u8; 2],
}

#[repr(C)]
pub struct IntelDisplayIpVer {
    pub ver: u16,
    pub rel: u16,
    pub step: u16,
}

#[repr(C)]
pub struct IntelDisplayDeviceInfoDbuf {
    pub size: u16,
    pub slice_mask: u8,
    pub _pad_3: [u8; 1],
}

#[repr(C)]
pub struct IntelDisplayDeviceInfoColor {
    pub degamma_lut_size: u32,
    pub gamma_lut_size: u32,
    pub degamma_lut_tests: u32,
    pub gamma_lut_tests: u32,
}

#[repr(C)]
pub struct IntelDisplayDeviceInfo {
    pub __runtime_defaults: IntelDisplayRuntimeInfo,
    pub abox_mask: u8,
    pub _pad_41: [u8; 1],
    pub dbuf: IntelDisplayDeviceInfoDbuf,
    /// bit-fields cursor_needs_physical:1, has_cdclk_crawl:1, has_cdclk_squash:1, has_ddi:1, has_dp_mst:1, has_dsb:1, has_fpga_dbg:1, has_gmch:1, has_hotplug:1, has_hti:1, has_ipc:1, has_overlay:1, has_psr:1, has_psr_hw_tracking:1, overlay_needs_physical:1, supports_tv:1
    pub _bitfields_46: [u8; 2],
    pub mmio_offset: u32,
    pub pipe_offsets: [u32; 7],
    pub trans_offsets: [u32; 7],
    pub cursor_offsets: [u32; 4],
    pub color: IntelDisplayDeviceInfoColor,
}

const _: () = assert!(size_of::<IntelDisplayPlatformsAnon0x0Anon0x0>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPlatformsAnon0x0Anon0x0, _bitfields_0) == 0);
const _: () = assert!(size_of::<IntelDisplayPlatformsAnon0x0>() == 16);
const _: () = assert!(size_of::<IntelDisplayPlatforms>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPlatforms, _anon_0_0) == 0);
const _: () = assert!(size_of::<IntelDisplayRuntimeInfo>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, ip) == 0);
const _: () = assert!(size_of::<IntelDisplayIpVer>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, step) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, step_name) == 12);
const _: () = assert!(size_of::<[c_char; 3]>() == 3);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, rawclk_freq) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, pipe_mask) == 20);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, cpu_transcoder_mask) == 21);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, port_mask) == 22);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, num_sprites) == 24);
const _: () = assert!(size_of::<[u8; 4]>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, num_scalers) == 28);
const _: () = assert!(size_of::<[u8; 4]>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, fbc_mask) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, has_hdcp) == 33);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, has_dmc) == 34);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, has_dsc) == 35);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, edp_typec_support) == 36);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRuntimeInfo, has_dbuf_overlap_detection) == 37);
const _: () = assert!(size_of::<IntelDisplayIpVer>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIpVer, ver) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIpVer, rel) == 2);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIpVer, step) == 4);
const _: () = assert!(size_of::<IntelDisplayDeviceInfoDbuf>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoDbuf, size) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoDbuf, slice_mask) == 2);
const _: () = assert!(size_of::<IntelDisplayDeviceInfoColor>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoColor, degamma_lut_size) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoColor, gamma_lut_size) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoColor, degamma_lut_tests) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfoColor, gamma_lut_tests) == 12);
const _: () = assert!(size_of::<IntelDisplayDeviceInfo>() == 140);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, __runtime_defaults) == 0);
const _: () = assert!(size_of::<IntelDisplayRuntimeInfo>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, abox_mask) == 40);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, dbuf) == 42);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, _bitfields_46) == 46);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, mmio_offset) == 48);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, pipe_offsets) == 52);
const _: () = assert!(size_of::<[u32; 7]>() == 28);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, trans_offsets) == 80);
const _: () = assert!(size_of::<[u32; 7]>() == 28);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, cursor_offsets) == 108);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDeviceInfo, color) == 124);
