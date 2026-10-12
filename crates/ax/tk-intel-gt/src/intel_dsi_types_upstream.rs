// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dsi.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::i915_external_placeholder_types_upstream::{MipiDsiDevice, MipiDsiHost, MipiDsiPixelFormat};
use crate::intel_display_limits_types_upstream::{Port};
use crate::intel_display_types_upstream::{IntelConnector, IntelEncoder};

#[repr(C)]
pub union IntelDsiAnon672x6 {
    pub ports: u16,
    pub phys: u16,
}

#[repr(C, align(8))]
pub struct IntelDsi {
    pub base: IntelEncoder,
    pub dsi_hosts: [*mut IntelDsiHost; 9],
    pub io_wakeref: [*mut c_void; 9],
    pub gpio_panel: *mut c_void,
    pub gpio_backlight: *mut c_void,
    pub attached_connector: *mut IntelConnector,
    pub _anon_672_6: IntelDsiAnon672x6,
    pub _pad_674: [u8; 2],
    pub channel: i32,
    pub operation_mode: u16,
    pub _pad_682: [u8; 2],
    pub lane_count: u32,
    pub i2c_bus_num: i32,
    pub pixel_format: MipiDsiPixelFormat,
    pub video_mode: i32,
    pub lp_clock_during_lpm: bool,
    pub blanking_pkt: bool,
    pub eot_pkt: bool,
    pub clock_stop: bool,
    pub escape_clk_div: u8,
    pub dual_link: u8,
    pub bgr_enabled: bool,
    pub pixel_overlap: u8,
    pub bw_timer: u32,
    pub dphy_reg: u32,
    pub dphy_data_lane_reg: u32,
    pub video_frmt_cfg_bits: u32,
    pub lp_byte_clk: u16,
    pub hs_tx_timeout: u16,
    pub lp_rx_timeout: u16,
    pub turn_arnd_val: u16,
    pub rst_timer_val: u16,
    pub hs_to_lp_count: u16,
    pub clk_lp_to_hs_count: u16,
    pub clk_hs_to_lp_count: u16,
    pub init_count: u16,
    pub _pad_742: [u8; 2],
    pub pclk: u32,
    pub burst_mode_ratio: u16,
    pub backlight_off_delay: u16,
    pub backlight_on_delay: u16,
    pub panel_on_delay: u16,
    pub panel_off_delay: u16,
    pub panel_pwr_cycle_delay: u16,
    pub panel_power_off_time: i64,
}

#[repr(C)]
pub struct IntelDsiHost {
    pub base: MipiDsiHost,
    pub intel_dsi: *mut IntelDsi,
    pub port: Port,
    pub _pad_44: [u8; 4],
    pub device: *mut MipiDsiDevice,
}

const _: () = assert!(size_of::<IntelDsiAnon672x6>() == 2);
const _: () = assert!(size_of::<IntelDsi>() == 768);
const _: () = assert!(align_of::<IntelDsi>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDsi, base) == 0);
const _: () = assert!(size_of::<IntelEncoder>() == 504);
const _: () = assert!(core::mem::offset_of!(IntelDsi, dsi_hosts) == 504);
const _: () = assert!(size_of::<[*mut IntelDsiHost; 9]>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelDsi, io_wakeref) == 576);
const _: () = assert!(size_of::<[*mut c_void; 9]>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelDsi, gpio_panel) == 648);
const _: () = assert!(core::mem::offset_of!(IntelDsi, gpio_backlight) == 656);
const _: () = assert!(core::mem::offset_of!(IntelDsi, attached_connector) == 664);
const _: () = assert!(core::mem::offset_of!(IntelDsi, _anon_672_6) == 672);
const _: () = assert!(core::mem::offset_of!(IntelDsi, channel) == 676);
const _: () = assert!(core::mem::offset_of!(IntelDsi, operation_mode) == 680);
const _: () = assert!(core::mem::offset_of!(IntelDsi, lane_count) == 684);
const _: () = assert!(core::mem::offset_of!(IntelDsi, i2c_bus_num) == 688);
const _: () = assert!(core::mem::offset_of!(IntelDsi, pixel_format) == 692);
const _: () = assert!(size_of::<MipiDsiPixelFormat>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDsi, video_mode) == 696);
const _: () = assert!(core::mem::offset_of!(IntelDsi, lp_clock_during_lpm) == 700);
const _: () = assert!(core::mem::offset_of!(IntelDsi, blanking_pkt) == 701);
const _: () = assert!(core::mem::offset_of!(IntelDsi, eot_pkt) == 702);
const _: () = assert!(core::mem::offset_of!(IntelDsi, clock_stop) == 703);
const _: () = assert!(core::mem::offset_of!(IntelDsi, escape_clk_div) == 704);
const _: () = assert!(core::mem::offset_of!(IntelDsi, dual_link) == 705);
const _: () = assert!(core::mem::offset_of!(IntelDsi, bgr_enabled) == 706);
const _: () = assert!(core::mem::offset_of!(IntelDsi, pixel_overlap) == 707);
const _: () = assert!(core::mem::offset_of!(IntelDsi, bw_timer) == 708);
const _: () = assert!(core::mem::offset_of!(IntelDsi, dphy_reg) == 712);
const _: () = assert!(core::mem::offset_of!(IntelDsi, dphy_data_lane_reg) == 716);
const _: () = assert!(core::mem::offset_of!(IntelDsi, video_frmt_cfg_bits) == 720);
const _: () = assert!(core::mem::offset_of!(IntelDsi, lp_byte_clk) == 724);
const _: () = assert!(core::mem::offset_of!(IntelDsi, hs_tx_timeout) == 726);
const _: () = assert!(core::mem::offset_of!(IntelDsi, lp_rx_timeout) == 728);
const _: () = assert!(core::mem::offset_of!(IntelDsi, turn_arnd_val) == 730);
const _: () = assert!(core::mem::offset_of!(IntelDsi, rst_timer_val) == 732);
const _: () = assert!(core::mem::offset_of!(IntelDsi, hs_to_lp_count) == 734);
const _: () = assert!(core::mem::offset_of!(IntelDsi, clk_lp_to_hs_count) == 736);
const _: () = assert!(core::mem::offset_of!(IntelDsi, clk_hs_to_lp_count) == 738);
const _: () = assert!(core::mem::offset_of!(IntelDsi, init_count) == 740);
const _: () = assert!(core::mem::offset_of!(IntelDsi, pclk) == 744);
const _: () = assert!(core::mem::offset_of!(IntelDsi, burst_mode_ratio) == 748);
const _: () = assert!(core::mem::offset_of!(IntelDsi, backlight_off_delay) == 750);
const _: () = assert!(core::mem::offset_of!(IntelDsi, backlight_on_delay) == 752);
const _: () = assert!(core::mem::offset_of!(IntelDsi, panel_on_delay) == 754);
const _: () = assert!(core::mem::offset_of!(IntelDsi, panel_off_delay) == 756);
const _: () = assert!(core::mem::offset_of!(IntelDsi, panel_pwr_cycle_delay) == 758);
const _: () = assert!(core::mem::offset_of!(IntelDsi, panel_power_off_time) == 760);
const _: () = assert!(size_of::<IntelDsiHost>() == 56);
const _: () = assert!(core::mem::offset_of!(IntelDsiHost, base) == 0);
const _: () = assert!(size_of::<MipiDsiHost>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDsiHost, intel_dsi) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDsiHost, port) == 40);
const _: () = assert!(size_of::<Port>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDsiHost, device) == 48);
