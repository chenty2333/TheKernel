// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_params.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IntelDisplayParams {
    pub dmc_firmware_path: *mut c_char,
    pub vbt_firmware: *mut c_char,
    pub lvds_channel_mode: i32,
    pub panel_use_ssc: i32,
    pub vbt_sdvo_panel_type: i32,
    pub enable_dc: i32,
    pub enable_dpt: bool,
    pub enable_dsb: bool,
    pub enable_flipq: bool,
    pub enable_sagv: bool,
    pub disable_power_well: i32,
    pub enable_ips: bool,
    pub _pad_41: [u8; 3],
    pub invert_brightness: i32,
    pub edp_vswing: i32,
    pub enable_dpcd_backlight: i32,
    pub load_detect_test: bool,
    pub force_reset_modeset_test: bool,
    pub disable_display: bool,
    pub verbose_state_checks: bool,
    pub nuclear_pageflip: bool,
    pub enable_dp_mst: bool,
    pub _pad_62: [u8; 2],
    pub enable_fbc: i32,
    pub enable_psr: i32,
    pub enable_panel_replay: i32,
    pub psr_safest_params: bool,
    pub enable_psr2_sel_fetch: bool,
    pub _pad_78: [u8; 2],
    pub enable_dmc_wl: i32,
    pub _pad_84: [u8; 4],
}

const _: () = assert!(size_of::<IntelDisplayParams>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, dmc_firmware_path) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, vbt_firmware) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, lvds_channel_mode) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, panel_use_ssc) == 20);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, vbt_sdvo_panel_type) == 24);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dc) == 28);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dpt) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dsb) == 33);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_flipq) == 34);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_sagv) == 35);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, disable_power_well) == 36);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_ips) == 40);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, invert_brightness) == 44);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, edp_vswing) == 48);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dpcd_backlight) == 52);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, load_detect_test) == 56);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, force_reset_modeset_test) == 57);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, disable_display) == 58);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, verbose_state_checks) == 59);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, nuclear_pageflip) == 60);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dp_mst) == 61);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_fbc) == 64);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_psr) == 68);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_panel_replay) == 72);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, psr_safest_params) == 76);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_psr2_sel_fetch) == 77);
const _: () = assert!(core::mem::offset_of!(IntelDisplayParams, enable_dmc_wl) == 80);
