// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dsi_vbt_defs.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct MipiConfigAnon2x1 {
    /// bit-fields enable_dithering:1, rsvd1:1, is_bridge:1, panel_arch_type:2, is_cmd_mode:1, video_transfer_mode:2, cabc_supported:1, pwm_blc:1, videomode_color_format:4, rotation:2, bta_disable:1, rsvd2:15
    pub _bitfields_2: [u8; 4],
}

#[repr(C)]
pub struct MipiConfigAnon6x2 {
    /// bit-fields dual_link:2, lane_cnt:2, pixel_overlap:3, rgb_flip:1, dl_dcs_cabc_ports:2, dl_dcs_backlight_ports:2, port_sync:1, rsvd3:3
    pub _bitfields_6: [u8; 2],
}

#[repr(C)]
pub struct MipiConfigAnon8x3 {
    /// bit-fields dsi_usage:1, rsvd4:15
    pub _bitfields_8: [u8; 2],
}

#[repr(C)]
pub struct MipiConfigAnon23x8 {
    /// bit-fields byte_clk_sel:2, rsvd6:6
    pub _bitfields_23: [u8; 1],
}

#[repr(C)]
pub struct MipiConfigAnon24x9 {
    /// bit-fields dphy_param_valid:1, eot_pkt_disabled:1, enable_clk_stop:1, blanking_packets_during_bllp:1, lp_clock_during_lpm:1, rsvd7:11
    pub _bitfields_24: [u8; 2],
}

#[repr(C)]
pub struct MipiConfigAnon54x17 {
    /// bit-fields prepare_cnt:6, rsvd8:2, clk_zero_cnt:8, trail_cnt:5, rsvd9:3, exit_zero_cnt:6, rsvd10:2
    pub _bitfields_54: [u8; 4],
}

#[repr(C, packed)]
pub struct MipiConfig {
    pub panel_id: u16,
    pub _anon_2_1: MipiConfigAnon2x1,
    pub _anon_6_2: MipiConfigAnon6x2,
    pub _anon_8_3: MipiConfigAnon8x3,
    pub rsvd5: u8,
    pub target_burst_mode_freq: u32,
    pub dsi_ddr_clk: u32,
    pub bridge_ref_clk: u32,
    pub _anon_23_8: MipiConfigAnon23x8,
    pub _anon_24_9: MipiConfigAnon24x9,
    pub hs_tx_timeout: u32,
    pub lp_rx_timeout: u32,
    pub turn_around_timeout: u32,
    pub device_reset_timer: u32,
    pub master_init_timer: u32,
    pub dbi_bw_timer: u32,
    pub lp_byte_clk_val: u32,
    pub _anon_54_17: MipiConfigAnon54x17,
    pub clk_lane_switch_cnt: u32,
    pub hl_switch_cnt: u32,
    pub rsvd11: [u32; 6],
    pub tclk_miss: u8,
    pub tclk_post: u8,
    pub rsvd12: u8,
    pub tclk_pre: u8,
    pub tclk_prepare: u8,
    pub tclk_settle: u8,
    pub tclk_term_enable: u8,
    pub tclk_trail: u8,
    pub tclk_prepare_clkzero: u16,
    pub rsvd13: u8,
    pub td_term_enable: u8,
    pub teot: u8,
    pub ths_exit: u8,
    pub ths_prepare: u8,
    pub ths_prepare_hszero: u16,
    pub rsvd14: u8,
    pub ths_settle: u8,
    pub ths_skip: u8,
    pub ths_trail: u8,
    pub tinit: u8,
    pub tlpx: u8,
    pub rsvd15: [u8; 3],
    pub panel_enable: u8,
    pub bl_enable: u8,
    pub pwm_enable: u8,
    pub reset_r_n: u8,
    pub pwr_down_r: u8,
    pub stdby_r_n: u8,
}

#[repr(C)]
pub struct MipiPpsData {
    pub panel_on_delay: u16,
    pub bl_enable_delay: u16,
    pub bl_disable_delay: u16,
    pub panel_off_delay: u16,
    pub panel_power_cycle_delay: u16,
}

const _: () = assert!(size_of::<MipiConfigAnon2x1>() == 4);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon2x1, _bitfields_2) == 0);
const _: () = assert!(size_of::<MipiConfigAnon6x2>() == 2);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon6x2, _bitfields_6) == 0);
const _: () = assert!(size_of::<MipiConfigAnon8x3>() == 2);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon8x3, _bitfields_8) == 0);
const _: () = assert!(size_of::<MipiConfigAnon23x8>() == 1);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon23x8, _bitfields_23) == 0);
const _: () = assert!(size_of::<MipiConfigAnon24x9>() == 2);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon24x9, _bitfields_24) == 0);
const _: () = assert!(size_of::<MipiConfigAnon54x17>() == 4);
const _: () = assert!(core::mem::offset_of!(MipiConfigAnon54x17, _bitfields_54) == 0);
const _: () = assert!(size_of::<MipiConfig>() == 122);
const _: () = assert!(core::mem::offset_of!(MipiConfig, panel_id) == 0);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_2_1) == 2);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_6_2) == 6);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_8_3) == 8);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd5) == 10);
const _: () = assert!(core::mem::offset_of!(MipiConfig, target_burst_mode_freq) == 11);
const _: () = assert!(core::mem::offset_of!(MipiConfig, dsi_ddr_clk) == 15);
const _: () = assert!(core::mem::offset_of!(MipiConfig, bridge_ref_clk) == 19);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_23_8) == 23);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_24_9) == 24);
const _: () = assert!(core::mem::offset_of!(MipiConfig, hs_tx_timeout) == 26);
const _: () = assert!(core::mem::offset_of!(MipiConfig, lp_rx_timeout) == 30);
const _: () = assert!(core::mem::offset_of!(MipiConfig, turn_around_timeout) == 34);
const _: () = assert!(core::mem::offset_of!(MipiConfig, device_reset_timer) == 38);
const _: () = assert!(core::mem::offset_of!(MipiConfig, master_init_timer) == 42);
const _: () = assert!(core::mem::offset_of!(MipiConfig, dbi_bw_timer) == 46);
const _: () = assert!(core::mem::offset_of!(MipiConfig, lp_byte_clk_val) == 50);
const _: () = assert!(core::mem::offset_of!(MipiConfig, _anon_54_17) == 54);
const _: () = assert!(core::mem::offset_of!(MipiConfig, clk_lane_switch_cnt) == 58);
const _: () = assert!(core::mem::offset_of!(MipiConfig, hl_switch_cnt) == 62);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd11) == 66);
const _: () = assert!(size_of::<[u32; 6]>() == 24);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_miss) == 90);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_post) == 91);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd12) == 92);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_pre) == 93);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_prepare) == 94);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_settle) == 95);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_term_enable) == 96);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_trail) == 97);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tclk_prepare_clkzero) == 98);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd13) == 100);
const _: () = assert!(core::mem::offset_of!(MipiConfig, td_term_enable) == 101);
const _: () = assert!(core::mem::offset_of!(MipiConfig, teot) == 102);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_exit) == 103);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_prepare) == 104);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_prepare_hszero) == 105);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd14) == 107);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_settle) == 108);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_skip) == 109);
const _: () = assert!(core::mem::offset_of!(MipiConfig, ths_trail) == 110);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tinit) == 111);
const _: () = assert!(core::mem::offset_of!(MipiConfig, tlpx) == 112);
const _: () = assert!(core::mem::offset_of!(MipiConfig, rsvd15) == 113);
const _: () = assert!(size_of::<[u8; 3]>() == 3);
const _: () = assert!(core::mem::offset_of!(MipiConfig, panel_enable) == 116);
const _: () = assert!(core::mem::offset_of!(MipiConfig, bl_enable) == 117);
const _: () = assert!(core::mem::offset_of!(MipiConfig, pwm_enable) == 118);
const _: () = assert!(core::mem::offset_of!(MipiConfig, reset_r_n) == 119);
const _: () = assert!(core::mem::offset_of!(MipiConfig, pwr_down_r) == 120);
const _: () = assert!(core::mem::offset_of!(MipiConfig, stdby_r_n) == 121);
const _: () = assert!(size_of::<MipiPpsData>() == 10);
const _: () = assert!(core::mem::offset_of!(MipiPpsData, panel_on_delay) == 0);
const _: () = assert!(core::mem::offset_of!(MipiPpsData, bl_enable_delay) == 2);
const _: () = assert!(core::mem::offset_of!(MipiPpsData, bl_disable_delay) == 4);
const _: () = assert!(core::mem::offset_of!(MipiPpsData, panel_off_delay) == 6);
const _: () = assert!(core::mem::offset_of!(MipiPpsData, panel_power_cycle_delay) == 8);
