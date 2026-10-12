// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_vbt_defs.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::intel_dsi_vbt_defs_types_upstream::{MipiConfig, MipiPpsData};

#[repr(C, packed)]
pub struct BdbEdidPnpId {
    pub mfg_name: u16,
    pub product_code: u16,
    pub serial: u32,
    pub mfg_week: u8,
    pub mfg_year: u8,
}

#[repr(C)]
pub struct BdbEdidProductName {
    pub name: [c_char; 13],
}

#[repr(C)]
pub struct BdbEdidDtd {
    pub clock: u16,
    pub hactive_lo: u8,
    pub hblank_lo: u8,
    /// bit-fields hblank_hi:4, hactive_hi:4
    pub _bitfields_4: [u8; 1],
    pub vactive_lo: u8,
    pub vblank_lo: u8,
    /// bit-fields vblank_hi:4, vactive_hi:4
    pub _bitfields_7: [u8; 1],
    pub hsync_off_lo: u8,
    pub hsync_pulse_width_lo: u8,
    /// bit-fields vsync_pulse_width_lo:4, vsync_off_lo:4, vsync_pulse_width_hi:2, vsync_off_hi:2, hsync_pulse_width_hi:2, hsync_off_hi:2
    pub _bitfields_10: [u8; 2],
    pub himage_lo: u8,
    pub vimage_lo: u8,
    /// bit-fields vimage_hi:4, himage_hi:4
    pub _bitfields_14: [u8; 1],
    pub h_border: u8,
    pub v_border: u8,
    /// bit-fields rsvd1:3, digital:2, vsync_positive:1, hsync_positive:1, non_interlaced:1
    pub _bitfields_17: [u8; 1],
}

#[repr(C)]
pub struct VbtHeader {
    pub signature: [u8; 20],
    pub version: u16,
    pub header_size: u16,
    pub vbt_size: u16,
    pub vbt_checksum: u8,
    pub reserved0: u8,
    pub bdb_offset: u32,
    pub aim_offset: [u32; 4],
}

#[repr(C)]
pub struct BdbHeader {
    pub signature: [u8; 16],
    pub version: u16,
    pub header_size: u16,
    pub bdb_size: u16,
}

#[repr(C)]
pub struct BdbGeneralFeatures {
    /// bit-fields panel_fitting:2, flexaim:1, msg_enable:1, clear_screen:3, color_flip:1, download_ext_vbt:1, enable_ssc:1, ssc_freq:1, enable_lfp_on_override:1, disable_ssc_ddt:1, underscan_vga_timings:1, display_clock_mode:1, vbios_hotplug_support:1, disable_smooth_vision:1, single_dvi:1, rotate_180:1, fdi_rx_polarity_inverted:1, vbios_extended_mode:1, copy_ilfp_dtd_to_sdvo_lvds_dtd:1, panel_best_fit_timing:1, ignore_strap_state:1
    pub _bitfields_0: [u8; 3],
    pub legacy_monitor_detect: u8,
    /// bit-fields int_crt_support:1, int_tv_support:1, int_efp_support:1, dp_ssc_enable:1, dp_ssc_freq:1, dp_ssc_dongle_supported:1, rsvd11:2, tc_hpd_retry_timeout:7, rsvd12:1, afc_startup_config:2, rsvd13:6
    pub _bitfields_4: [u8; 3],
}

#[repr(C)]
pub struct ChildDeviceConfigAnon4x2Anon4x1 {
    pub i2c_speed: u8,
    /// bit-fields dp_onboard_redriver_preemph:3, dp_onboard_redriver_vswing:3, dp_onboard_redriver_present:1, reserved0:1, dp_ondock_redriver_preemph:3, dp_ondock_redriver_vswing:3, dp_ondock_redriver_present:1, reserved1:1, hdmi_level_shifter_value:5, hdmi_max_data_rate:3
    pub _bitfields_5: [u8; 3],
    pub dtd_buf_ptr: u16,
    /// bit-fields edidless_efp:1, compression_enable:1, compression_method_cps:1, ganged_edp:1, lttpr_non_transparent:1, disable_compression_for_ext_disp:1, reserved2:2, compression_structure_index:4, reserved3:4, hdmi_max_frl_rate:4, hdmi_max_frl_rate_valid:1, reserved4:3
    pub _bitfields_10: [u8; 3],
    pub reserved5: u8,
}

#[repr(C)]
pub union ChildDeviceConfigAnon4x2 {
    pub device_id: [u8; 10],
    pub _anon_4_1: ManuallyDrop<ChildDeviceConfigAnon4x2Anon4x1>,
}

#[repr(C)]
pub struct ChildDeviceConfigAnon23x10Anon23x0 {
    pub dvo2_port: u8,
    pub i2c2_pin: u8,
    pub target2_addr: u8,
    pub ddc2_pin: u8,
}

#[repr(C)]
pub struct ChildDeviceConfigAnon23x10Anon23x1 {
    /// bit-fields efp_routed:1, lane_reversal:1, lspcon:1, iboost:1, hpd_invert:1, use_vbt_vswing:1, dp_max_lane_count:2, hdmi_support:1, dp_support:1, tmds_support:1, support_reserved:5
    pub _bitfields_23: [u8; 2],
    pub aux_channel: u8,
    pub dongle_detect: u8,
}

#[repr(C)]
pub union ChildDeviceConfigAnon23x10 {
    pub _anon_23_0: ManuallyDrop<ChildDeviceConfigAnon23x10Anon23x0>,
    pub _anon_23_1: ManuallyDrop<ChildDeviceConfigAnon23x10Anon23x1>,
}

#[repr(C)]
pub union ChildDeviceConfigAnon29x17 {
    pub dvo2_wiring: u8,
    pub mipi_bridge_type: u8,
}

#[repr(C, packed)]
pub struct ChildDeviceConfig {
    pub handle: u16,
    pub device_type: u16,
    pub _anon_4_2: ChildDeviceConfigAnon4x2,
    pub addin_offset: u16,
    pub dvo_port: u8,
    pub i2c_pin: u8,
    pub target_addr: u8,
    pub ddc_pin: u8,
    pub edid_ptr: u16,
    pub dvo_cfg: u8,
    pub _anon_23_10: ChildDeviceConfigAnon23x10,
    /// bit-fields pipe_cap:2, sdvo_stall:1, hpd_status:2, integrated_encoder:1, capabilities_reserved:2
    pub _bitfields_27: [u8; 1],
    pub dvo_wiring: u8,
    pub _anon_29_17: ChildDeviceConfigAnon29x17,
    pub extended_type: u16,
    pub dvo_function: u8,
    /// bit-fields dp_usb_type_c:1, tbt:1, dedicated_external:1, dyn_port_over_tc:1, dp_port_trace_length:4
    pub _bitfields_33: [u8; 1],
    pub dp_gpio_index: u8,
    pub dp_gpio_pin_num: u16,
    /// bit-fields dp_iboost_level:4, hdmi_iboost_level:4, dp_max_link_rate:3, dp_max_link_rate_reserved:5
    pub _bitfields_37: [u8; 2],
    pub efp_index: u8,
    /// bit-fields edp_data_rate_override:12, edp_data_rate_override_reserved:20
    pub _bitfields_40: [u8; 4],
}

#[repr(C)]
pub struct BdbGeneralDefinitions {
    pub crt_ddc_gmbus_pin: u8,
    /// bit-fields dpms_non_acpi:1, skip_boot_crt_detect:1, dpms_aim:1, rsvd1:5
    pub _bitfields_1: [u8; 1],
    pub boot_display: [u8; 2],
    pub child_dev_size: u8,
    pub devices: [u8; 0],
}

#[repr(C, packed)]
pub struct BdbDisplayToggle {
    pub feature_bits: u8,
    pub num_entries: u16,
    pub list: [u16; 0],
}

#[repr(C)]
pub struct BdbModeSupportList {
    pub intel_mode_number: [u8; 0],
    pub mode_list_length: u16,
}

#[repr(C, packed)]
pub struct GenericModeTable {
    pub x_res: u16,
    pub y_res: u16,
    pub color_depths: u8,
    pub refresh_rate: [u8; 3],
    pub reserved: u8,
    pub text_cols: u8,
    pub text_rows: u8,
    pub font_height: u8,
    pub page_size: u16,
    pub misc: u8,
}

#[repr(C)]
pub struct GenericModeTimings {
    pub dotclock_khz: u32,
    pub hdisplay: u16,
    pub htotal: u16,
    pub hblank_start: u16,
    pub hblank_end: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub vdisplay: u16,
    pub vtotal: u16,
    pub vblank_start: u16,
    pub vblank_end: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
}

#[repr(C, packed)]
pub struct GenericModeTimingsAlm {
    pub timings: GenericModeTimings,
    pub wm_8bpp: u8,
    pub burst_8bpp: u8,
    pub wm_16bpp: u8,
    pub burst_16bpp: u8,
    pub wm_32bpp: u8,
    pub burst_32bpp: u8,
}

#[repr(C, packed)]
pub struct BdbGenericModeTableAlm {
    pub table: GenericModeTable,
    pub timings: [GenericModeTimingsAlm; 3],
}

#[repr(C, packed)]
pub struct BdbGenericModeTableMgm {
    pub mode_flag: u16,
    pub table: GenericModeTable,
    pub timings: [GenericModeTimings; 3],
}

#[repr(C, packed)]
pub struct BdbRegTable {
    pub table_id: u16,
    pub data_access_size: u8,
}

#[repr(C)]
pub struct DotClockOverrideEntryGen2 {
    pub dotclock: u32,
    pub n: u8,
    pub m1: u8,
    pub m2: u8,
    /// bit-fields p1:5, p1_div_by_2:1, reserved:1, p2_div_by_4:1
    pub _bitfields_7: [u8; 1],
}

#[repr(C)]
pub struct BdbDotClockOverrideAlm {
    pub t: [DotClockOverrideEntryGen2; 0],
}

#[repr(C)]
pub struct PsrTable {
    /// bit-fields full_link:1, require_aux_to_wakeup:1, feature_bits_rsvd:6, idle_frames:4, lines_to_wait:3, wait_times_rsvd:1
    pub _bitfields_0: [u8; 2],
    pub tp1_wakeup_time: u16,
    pub tp2_tp3_wakeup_time: u16,
}

#[repr(C)]
pub struct BdbPsr {
    pub psr_table: [PsrTable; 16],
    pub psr2_tp2_tp3_wakeup_time: u32,
}

#[repr(C, packed)]
pub struct ModeRemovalTable {
    pub x_res: u16,
    pub y_res: u16,
    pub bpp: u8,
    pub refresh_rate: u16,
    pub removal_flags: u8,
    pub panel_flags: u16,
}

#[repr(C, packed)]
pub struct BdbModeRemoval {
    pub row_size: u8,
    pub modes: [ModeRemovalTable; 0],
}

#[repr(C, packed)]
pub struct BdbDriverFeatures {
    /// bit-fields boot_dev_algorithm:1, allow_display_switch_dvd:1, allow_display_switch_dos:1, hotplug_dvo:1, dual_view_zoom:1, int15h_hook:1, sprite_in_clone:1, primary_lfp_id:1
    pub _bitfields_0: [u8; 1],
    pub boot_mode_x: u16,
    pub boot_mode_y: u16,
    pub boot_mode_bpp: u8,
    pub boot_mode_refresh: u8,
    /// bit-fields enable_lfp_primary:1, selective_mode_pruning:1, dual_frequency:1, render_clock_freq:1, nt_clone_support:1, power_scheme_ui:1, sprite_display_assign:1, cui_aspect_scaling:1, preserve_aspect_ratio:1, sdvo_device_power_down:1, crt_hotplug:1, lvds_config:2, tv_hotplug:1, hdmi_config:2, static_display:1, embedded_platform:1, display_subsystem_enable:1, reserved0:5
    pub _bitfields_7: [u8; 3],
    pub legacy_crt_max_x: u16,
    pub legacy_crt_max_y: u16,
    pub legacy_crt_max_refresh: u8,
    /// bit-fields hdmi_termination:1, cea861d_hdmi_support:1, self_refresh_enable:1, reserved1:5
    pub _bitfields_15: [u8; 1],
    pub custom_vbt_version: u8,
    /// bit-fields rmpm_enabled:1, s2ddt_enabled:1, dpst_enabled:1, bltclt_enabled:1, adb_enabled:1, drrs_enabled:1, grs_enabled:1, gpmt_enabled:1, tbt_enabled:1, psr_enabled:1, ips_enabled:1, dfps_enabled:1, dmrrs_enabled:1, adt_enabled:1, hpd_wake:1, pc_feature_valid:1
    pub _bitfields_17: [u8; 2],
}

#[repr(C, packed)]
pub struct BdbDriverPersistence {
    /// bit-fields hotkey_persistent_algorithm:1, lid_switch_persistent_algorithm:1, power_management_persistent_algorithm:1, hotkey_persistent_on_mds_twin:1, hotkey_persistent_on_refresh_rate:1, hotkey_persistent_on_restore_pipe:1, hotkey_persistent_on_mode:1, edid_persistent_on_mode:1, dvo_hotplug_persistent_on_mode:1, docking_persistent_algorithm:1, rsvd:6
    pub _bitfields_0: [u8; 2],
    pub persistent_max_config: u8,
}

#[repr(C, packed)]
pub struct DotClockOverrideEntryGen3 {
    pub dotclock: u32,
    pub n: u8,
    pub m1: u8,
    pub m2: u8,
    pub p1: u8,
    pub p2: u8,
}

#[repr(C, packed)]
pub struct BdbDotClockOverride {
    pub row_size: u8,
    pub num_rows: u8,
    pub table: [DotClockOverrideEntryGen3; 0],
}

#[repr(C)]
pub struct ToggleListEntryOld {
    pub display_select_pipe_a: u8,
    pub display_select_pipe_b: u8,
    pub caps: u8,
}

#[repr(C, packed)]
pub struct ToggleListTableOld {
    pub num_entries: u16,
    pub entry_size: u8,
    pub list: [ToggleListEntryOld; 0],
}

#[repr(C)]
pub struct BdbDisplaySelectOld {
    pub tables: [ToggleListTableOld; 4],
}

#[repr(C)]
pub struct BdbSvTestFunctions {
    pub sv_bits: [u8; 8],
}

#[repr(C)]
pub struct BdbDriverRotation {
    pub rotation_enable: u8,
    pub rotation_flags_1: u8,
    pub rotation_flags_2: u16,
    pub rotation_flags_3: u32,
    pub rotation_flags_4: u32,
}

#[repr(C)]
pub struct DisplayRemoveEntryOld {
    pub display_select_pipe_a: u8,
    pub display_select_pipe_b: u8,
}

#[repr(C)]
pub struct BdbDisplayRemoveOld {
    pub num_entries: u8,
    pub entry_size: u8,
    pub table: [DisplayRemoveEntryOld; 0],
}

#[repr(C)]
pub struct OemMode {
    /// bit-fields enable_in_vbios:1, enable_in_os:1, enable_in_gop:1, reserved:5
    pub _bitfields_0: [u8; 1],
    pub display_flags: u8,
    pub x_res: u16,
    pub y_res: u16,
    pub color_depth: u8,
    pub refresh_rate: u8,
    pub dtd: BdbEdidDtd,
    pub display_flags_2: u16,
}

#[repr(C)]
pub struct BdbOemCustom {
    pub num_entries: u8,
    pub entry_size: u8,
    pub modes: [OemMode; 0],
}

#[repr(C)]
pub struct EfpEntry {
    pub mfg_name: u16,
    pub product_code: u16,
}

#[repr(C)]
pub struct BdbEfpList {
    pub num_entries: u8,
    pub entry_size: u8,
    pub efp: [EfpEntry; 0],
}

#[repr(C)]
pub struct BdbSdvoLvdsOptions {
    pub panel_backlight: u8,
    pub h40_set_panel_type: u8,
    pub panel_type: u8,
    pub ssc_clk_freq: u8,
    pub als_low_trip: u16,
    pub als_high_trip: u16,
    pub sclalarcoeff_tab_row_num: u8,
    pub sclalarcoeff_tab_row_size: u8,
    pub coefficient: [u8; 8],
    pub panel_misc_bits_1: u8,
    pub panel_misc_bits_2: u8,
    pub panel_misc_bits_3: u8,
    pub panel_misc_bits_4: u8,
}

#[repr(C)]
pub struct BdbSdvoLvdsDtd {
    pub dtd: [BdbEdidDtd; 4],
}

#[repr(C)]
pub struct BdbSdvoLvdsPnpId {
    pub pnp_id: [BdbEdidPnpId; 4],
}

#[repr(C)]
pub struct SdvoLvdsPps {
    pub t0: u16,
    pub t1: u16,
    pub t2: u16,
    pub t3: u16,
    pub t4: u16,
}

#[repr(C)]
pub struct BdbSdvoLvdsPps {
    pub pps: [SdvoLvdsPps; 4],
}

#[repr(C)]
pub struct BdbTvOptions {
    /// bit-fields underscan_overscan_hdtv_component:2, rsvd1:10, underscan_overscan_hdtv_dvi:2, add_modes_to_avoid_overscan_issue:1, d_connector_support:1
    pub _bitfields_0: [u8; 2],
}

#[repr(C)]
pub struct EdpPowerSeq {
    pub t1_t3: u16,
    pub t8: u16,
    pub t9: u16,
    pub t10: u16,
    pub t11_t12: u16,
}

#[repr(C)]
pub struct EdpFastLinkParams {
    /// bit-fields rate:4, lanes:4, preemphasis:4, vswing:4
    pub _bitfields_0: [u8; 2],
}

#[repr(C)]
pub struct EdpPwmDelays {
    pub pwm_on_to_backlight_enable: u16,
    pub backlight_disable_to_pwm_off: u16,
}

#[repr(C)]
pub struct EdpFullLinkParams {
    /// bit-fields preemphasis:4, vswing:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct EdpApicalParams {
    pub panel_oui: u32,
    pub dpcd_base_address: u32,
    pub dpcd_idridix_control_0: u32,
    pub dpcd_option_select: u32,
    pub dpcd_backlight: u32,
    pub ambient_light: u32,
    pub backlight_scale: u32,
}

#[repr(C, packed)]
pub struct BdbEdp {
    pub power_seqs: [EdpPowerSeq; 16],
    pub color_depth: u32,
    pub fast_link_params: [EdpFastLinkParams; 16],
    pub sdrrs_msa_timing_delay: u32,
    pub edp_s3d_feature: u16,
    pub edp_t3_optimization: u16,
    pub edp_vswing_preemph: u64,
    pub fast_link_training: u16,
    pub dpcd_600h_write_required: u16,
    pub pwm_delays: [EdpPwmDelays; 16],
    pub full_link_params_provided: u16,
    pub full_link_params: [EdpFullLinkParams; 16],
    pub apical_enable: u16,
    pub apical_params: [EdpApicalParams; 16],
    pub edp_fast_link_training_rate: [u16; 16],
    pub edp_max_port_link_rate: [u16; 16],
    pub edp_dsc_disable: u16,
    pub t6_delay_support: u16,
    pub link_idle_time: [u16; 16],
    pub pipe_joiner_enable: u16,
}

#[repr(C)]
pub struct BdbEfpDtd {
    pub dtd: [BdbEdidDtd; 3],
}

#[repr(C)]
pub struct ToggleListEntryIvb {
    pub display_select: u8,
}

#[repr(C, packed)]
pub struct ToggleListTableIvb {
    pub num_entries: u16,
    pub entry_size: u8,
    pub list: [ToggleListEntryIvb; 0],
}

#[repr(C)]
pub struct BdbDisplaySelectIvb {
    pub tables: [ToggleListTableIvb; 4],
}

#[repr(C)]
pub struct DisplayRemoveEntryIvb {
    pub display_select: u8,
}

#[repr(C)]
pub struct BdbDisplayRemoveIvb {
    pub num_entries: u8,
    pub entry_size: u8,
    pub table: [DisplayRemoveEntryIvb; 0],
}

#[repr(C)]
pub struct ToggleListEntryHsw {
    pub display_select: u16,
}

#[repr(C, packed)]
pub struct ToggleListTableHsw {
    pub num_entries: u16,
    pub entry_size: u8,
    pub list: [ToggleListEntryHsw; 0],
}

#[repr(C)]
pub struct BdbDisplaySelectHsw {
    pub tables: [ToggleListTableHsw; 4],
}

#[repr(C)]
pub struct DisplayRemoveEntryHsw {
    pub display_select: u16,
}

#[repr(C)]
pub struct BdbDisplayRemoveHsw {
    pub num_entries: u8,
    pub entry_size: u8,
    pub table: [DisplayRemoveEntryHsw; 0],
}

#[repr(C, packed)]
pub struct BdbLfpOptions {
    pub panel_type: u8,
    pub panel_type2: u8,
    /// bit-fields pfit_mode:2, pfit_text_mode_enhanced:1, pfit_gfx_mode_enhanced:1, pfit_ratio_auto:1, pixel_dither:1, lvds_edid:1, rsvd2:1
    pub _bitfields_2: [u8; 1],
    pub rsvd4: u8,
    pub lvds_panel_channel_bits: u32,
    pub ssc_bits: u16,
    pub ssc_freq: u16,
    pub ssc_ddt: u16,
    pub panel_color_depth: u16,
    pub dps_panel_type_bits: u32,
    pub blt_control_type_bits: u32,
    pub lcdvcc_s0_enable: u16,
    pub rotation: u32,
    pub position: u32,
}

#[repr(C, packed)]
pub struct LfpDataPtrTable {
    pub offset: u16,
    pub table_size: u8,
}

#[repr(C, packed)]
pub struct LfpDataPtr {
    pub fp_timing: LfpDataPtrTable,
    pub dvo_timing: LfpDataPtrTable,
    pub panel_pnp_id: LfpDataPtrTable,
}

#[repr(C, packed)]
pub struct BdbLfpDataPtrs {
    pub num_entries: u8,
    pub ptr: [LfpDataPtr; 16],
    pub panel_name: LfpDataPtrTable,
}

#[repr(C, packed)]
pub struct FpTiming {
    pub x_res: u16,
    pub y_res: u16,
    pub lvds_reg: u32,
    pub lvds_reg_val: u32,
    pub pp_on_reg: u32,
    pub pp_on_reg_val: u32,
    pub pp_off_reg: u32,
    pub pp_off_reg_val: u32,
    pub pp_cycle_reg: u32,
    pub pp_cycle_reg_val: u32,
    pub pfit_reg: u32,
    pub pfit_reg_val: u32,
    pub terminator: u16,
}

#[repr(C, packed)]
pub struct LfpDataEntry {
    pub fp_timing: FpTiming,
    pub dvo_timing: BdbEdidDtd,
    pub pnp_id: BdbEdidPnpId,
}

#[repr(C)]
pub struct BdbLfpData {
    pub data: [LfpDataEntry; 16],
}

#[repr(C)]
pub struct LfpBlackBorder {
    pub top: u8,
    pub bottom: u8,
    pub left: u8,
    pub right: u8,
}

#[repr(C)]
pub struct BdbLfpDataTail {
    pub panel_name: [BdbEdidProductName; 16],
    pub scaling_enable: u16,
    pub seamless_drrs_min_refresh_rate: [u8; 16],
    pub pixel_overlap_count: [u8; 16],
    pub black_border: [LfpBlackBorder; 16],
    pub dual_lfp_port_sync_enable: u16,
    pub gpu_dithering_for_banding_artifacts: u16,
}

#[repr(C, packed)]
pub struct LfpBacklightDataEntry {
    /// bit-fields type:2, active_low_pwm:1, i2c_pin:3, i2c_speed:2
    pub _bitfields_0: [u8; 1],
    pub pwm_freq_hz: u16,
    pub min_brightness: u8,
    pub i2c_address: u8,
    pub i2c_command: u8,
}

#[repr(C)]
pub struct LfpBacklightControlMethod {
    /// bit-fields type:4, controller:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct LfpBrightnessLevel {
    pub level: u16,
    pub reserved: u16,
}

#[repr(C, packed)]
pub struct BdbLfpBacklight {
    pub entry_size: u8,
    pub data: [LfpBacklightDataEntry; 16],
    pub level: [u8; 16],
    pub backlight_control: [LfpBacklightControlMethod; 16],
    pub brightness_level: [LfpBrightnessLevel; 16],
    pub brightness_min_level: [LfpBrightnessLevel; 16],
    pub brightness_precision_bits: [u8; 16],
    pub hdr_dpcd_refresh_timeout: [u16; 16],
}

#[repr(C)]
pub struct LfpPowerFeatures {
    /// bit-fields dpst_support:1, power_conservation_pref:3, reserved2:1, lace_enabled_status:1, lace_support:1, als_enable:1
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct AlsDataEntry {
    pub backlight_adjust: u16,
    pub lux: u16,
}

#[repr(C)]
pub struct AggressivenessProfileEntry {
    /// bit-fields dpst_aggressiveness:4, lace_aggressiveness:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct AggressivenessProfile2Entry {
    /// bit-fields opst_aggressiveness:4, elp_aggressiveness:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct AggressivenessProfile3Entry {
    /// bit-fields apd_aggressiveness:4, pixoptix_aggressiveness:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct AggressivenessProfile4Entry {
    /// bit-fields xpst_aggressiveness:4, tcon_aggressiveness:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct PanelIdentification {
    /// bit-fields panel_technology:4, reserved:4
    pub _bitfields_0: [u8; 1],
}

#[repr(C, packed)]
pub struct BdbLfpPower {
    pub features: LfpPowerFeatures,
    pub als: [AlsDataEntry; 5],
    /// bit-fields lace_aggressiveness_profile:3, reserved1:5
    pub _bitfields_21: [u8; 1],
    pub dpst: u16,
    pub psr: u16,
    pub drrs: u16,
    pub lace_support: u16,
    pub adt: u16,
    pub dmrrs: u16,
    pub adb: u16,
    pub lace_enabled_status: u16,
    pub aggressiveness: [AggressivenessProfileEntry; 16],
    pub hobl: u16,
    pub vrr_feature_enabled: u16,
    pub elp: u16,
    pub opst: u16,
    pub aggressiveness2: [AggressivenessProfile2Entry; 16],
    pub apd: u16,
    pub pixoptix: u16,
    pub aggressiveness3: [AggressivenessProfile3Entry; 16],
    pub panel_identification: [PanelIdentification; 16],
    pub xpst_support: u16,
    pub tcon_based_backlight_optimization: u16,
    pub aggressiveness4: [AggressivenessProfile4Entry; 16],
    pub tcon_backlight_xpst_coexistence: u16,
}

#[repr(C)]
pub struct EdpBfi {
    /// bit-fields enable_bfi_in_driver:1, enable_brightness_control_in_cui:1, reserved:6
    pub _bitfields_0: [u8; 1],
    pub brightness_percentage_when_bfi_disabled: u8,
}

#[repr(C)]
pub struct BdbEdpBfi {
    pub bfi_structure_size: u8,
    pub bfi: [EdpBfi; 16],
}

#[repr(C)]
pub struct Chromaticity {
    /// bit-fields chromaticity_enable:1, chromaticity_from_edid_base_block:1, rsvd:6, green_y_lo:2, green_x_lo:2, red_y_lo:2, red_x_lo:2, white_y_lo:2, white_x_lo:2, blue_y_lo:2, blue_x_lo:2
    pub _bitfields_0: [u8; 3],
    pub red_x_hi: u8,
    pub red_y_hi: u8,
    pub green_x_hi: u8,
    pub green_y_hi: u8,
    pub blue_x_hi: u8,
    pub blue_y_hi: u8,
    pub white_x_hi: u8,
    pub white_y_hi: u8,
}

#[repr(C, packed)]
pub struct LuminanceAndGamma {
    /// bit-fields luminance_enable:1, gamma_enable:1, rsvd:6
    pub _bitfields_0: [u8; 1],
    pub min_luminance: u16,
    pub max_luminance: u16,
    pub one_percent_max_luminance: u16,
    pub gamma: u8,
}

#[repr(C)]
pub struct BdbChromaticity {
    pub chromaticity: [Chromaticity; 16],
    pub luminance_and_gamma: [LuminanceAndGamma; 16],
}

#[repr(C, packed)]
pub struct MipiData {
    pub panel_identifier: u16,
    pub bridge_revision: u16,
    /// bit-fields dithering:1, pixel_format_18bpp:1, reserved1:1, dphy_params_valid:1, reserved2:28
    pub _bitfields_4: [u8; 4],
    pub port_info: u16,
    /// bit-fields reserved3:2, num_lanes:2, reserved4:12, virtual_channel_num:2, video_transfer_mode:2, reserved5:12
    pub _bitfields_10: [u8; 4],
    pub dsi_ddr_clock: u32,
    pub renesas_bridge_ref_clock: u32,
    pub power_conservation: u16,
    /// bit-fields prepare_count:5, reserved6:3, clk_zero_count:8, trail_count:5, reserved7:3, exit_zero_count:6, reserved8:2
    pub _bitfields_24: [u8; 4],
    pub high_low_switch_count: u32,
    pub lp_byte_clock: u32,
    pub clock_lane_switch_time_counter: u32,
    pub panel_color_depth: u32,
}

#[repr(C)]
pub struct BdbMipi {
    pub mipi: [MipiData; 16],
}

#[repr(C, packed)]
pub struct BdbFixedSetMode {
    pub enable: u8,
    pub x_res: u32,
    pub y_res: u32,
}

#[repr(C, packed)]
pub struct BdbMipiConfig {
    pub config: [MipiConfig; 6],
    pub pps: [MipiPpsData; 6],
    pub pwm_delays: [EdpPwmDelays; 6],
    pub pmic_i2c_bus_number: [u8; 6],
}

#[repr(C)]
pub struct BdbMipiSequence {
    pub version: u8,
    pub data: [u8; 0],
}

#[repr(C)]
pub struct BdbRgbPalette {
    pub is_enabled: u8,
    pub red: [u8; 256],
    pub blue: [u8; 256],
    pub green: [u8; 256],
}

#[repr(C, packed)]
pub struct DscCompressionParametersEntry {
    /// bit-fields version_major:4, version_minor:4, rc_buffer_block_size:2, reserved1:6
    pub _bitfields_0: [u8; 2],
    pub rc_buffer_size: u8,
    pub slices_per_line: u32,
    /// bit-fields line_buffer_depth:4, reserved2:4, block_prediction_enable:1, reserved3:7
    pub _bitfields_7: [u8; 2],
    pub max_bpp: u8,
    /// bit-fields reserved4:1, support_8bpc:1, support_10bpc:1, support_12bpc:1, reserved5:4
    pub _bitfields_10: [u8; 1],
    pub slice_height: u16,
}

#[repr(C, packed)]
pub struct BdbCompressionParameters {
    pub entry_size: u16,
    pub data: [DscCompressionParametersEntry; 16],
}

#[repr(C, packed)]
pub struct BdbVswingPreemph {
    pub num_tables: u8,
    pub num_columns: u8,
    pub tables: [u32; 0],
}

#[repr(C)]
pub struct GenericDtdEntry {
    pub pixel_clock: u32,
    pub hactive: u16,
    pub hblank: u16,
    pub hfront_porch: u16,
    pub hsync: u16,
    pub vactive: u16,
    pub vblank: u16,
    pub vfront_porch: u16,
    pub vsync: u16,
    pub width_mm: u16,
    pub height_mm: u16,
    /// bit-fields rsvd_flags:6, vsync_positive_polarity:1, hsync_positive_polarity:1
    pub _bitfields_24: [u8; 1],
    pub rsvd: [u8; 3],
}

#[repr(C, packed)]
pub struct BdbGenericDtd {
    pub gdtd_size: u16,
    pub dtd: [GenericDtdEntry; 0],
}

#[repr(C)]
pub struct PrdEntryOld {
    pub displays_attached: u8,
    pub display_in_pipe_a: u8,
    pub display_in_pipe_b: u8,
}

#[repr(C)]
pub struct BdbPrdTableOld {
    pub list: [PrdEntryOld; 0],
    pub num_entries: u16,
}

#[repr(C)]
pub struct PrdEntryNew {
    pub primary_display: u16,
    pub secondary_display: u16,
}

#[repr(C)]
pub struct BdbPrdTableNew {
    pub num_entries: u16,
    pub list: [PrdEntryNew; 0],
}

const _: () = assert!(size_of::<BdbEdidPnpId>() == 10);
const _: () = assert!(core::mem::offset_of!(BdbEdidPnpId, mfg_name) == 0);
const _: () = assert!(core::mem::offset_of!(BdbEdidPnpId, product_code) == 2);
const _: () = assert!(core::mem::offset_of!(BdbEdidPnpId, serial) == 4);
const _: () = assert!(core::mem::offset_of!(BdbEdidPnpId, mfg_week) == 8);
const _: () = assert!(core::mem::offset_of!(BdbEdidPnpId, mfg_year) == 9);
const _: () = assert!(size_of::<BdbEdidProductName>() == 13);
const _: () = assert!(core::mem::offset_of!(BdbEdidProductName, name) == 0);
const _: () = assert!(size_of::<[c_char; 13]>() == 13);
const _: () = assert!(size_of::<BdbEdidDtd>() == 18);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, clock) == 0);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, hactive_lo) == 2);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, hblank_lo) == 3);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, _bitfields_4) == 4);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, vactive_lo) == 5);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, vblank_lo) == 6);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, _bitfields_7) == 7);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, hsync_off_lo) == 8);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, hsync_pulse_width_lo) == 9);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, _bitfields_10) == 10);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, himage_lo) == 12);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, vimage_lo) == 13);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, _bitfields_14) == 14);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, h_border) == 15);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, v_border) == 16);
const _: () = assert!(core::mem::offset_of!(BdbEdidDtd, _bitfields_17) == 17);
const _: () = assert!(size_of::<VbtHeader>() == 48);
const _: () = assert!(core::mem::offset_of!(VbtHeader, signature) == 0);
const _: () = assert!(size_of::<[u8; 20]>() == 20);
const _: () = assert!(core::mem::offset_of!(VbtHeader, version) == 20);
const _: () = assert!(core::mem::offset_of!(VbtHeader, header_size) == 22);
const _: () = assert!(core::mem::offset_of!(VbtHeader, vbt_size) == 24);
const _: () = assert!(core::mem::offset_of!(VbtHeader, vbt_checksum) == 26);
const _: () = assert!(core::mem::offset_of!(VbtHeader, reserved0) == 27);
const _: () = assert!(core::mem::offset_of!(VbtHeader, bdb_offset) == 28);
const _: () = assert!(core::mem::offset_of!(VbtHeader, aim_offset) == 32);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
const _: () = assert!(size_of::<BdbHeader>() == 22);
const _: () = assert!(core::mem::offset_of!(BdbHeader, signature) == 0);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbHeader, version) == 16);
const _: () = assert!(core::mem::offset_of!(BdbHeader, header_size) == 18);
const _: () = assert!(core::mem::offset_of!(BdbHeader, bdb_size) == 20);
const _: () = assert!(size_of::<BdbGeneralFeatures>() == 7);
const _: () = assert!(core::mem::offset_of!(BdbGeneralFeatures, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(BdbGeneralFeatures, legacy_monitor_detect) == 3);
const _: () = assert!(core::mem::offset_of!(BdbGeneralFeatures, _bitfields_4) == 4);
const _: () = assert!(size_of::<ChildDeviceConfigAnon4x2Anon4x1>() == 10);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon4x2Anon4x1, i2c_speed) == 0);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon4x2Anon4x1, _bitfields_5) == 1);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon4x2Anon4x1, dtd_buf_ptr) == 4);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon4x2Anon4x1, _bitfields_10) == 6);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon4x2Anon4x1, reserved5) == 9);
const _: () = assert!(size_of::<ChildDeviceConfigAnon4x2>() == 10);
const _: () = assert!(size_of::<ChildDeviceConfigAnon23x10Anon23x0>() == 4);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x0, dvo2_port) == 0);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x0, i2c2_pin) == 1);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x0, target2_addr) == 2);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x0, ddc2_pin) == 3);
const _: () = assert!(size_of::<ChildDeviceConfigAnon23x10Anon23x1>() == 4);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x1, _bitfields_23) == 0);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x1, aux_channel) == 2);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfigAnon23x10Anon23x1, dongle_detect) == 3);
const _: () = assert!(size_of::<ChildDeviceConfigAnon23x10>() == 4);
const _: () = assert!(size_of::<ChildDeviceConfigAnon29x17>() == 1);
const _: () = assert!(size_of::<ChildDeviceConfig>() == 44);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, handle) == 0);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, device_type) == 2);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _anon_4_2) == 4);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, addin_offset) == 14);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dvo_port) == 16);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, i2c_pin) == 17);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, target_addr) == 18);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, ddc_pin) == 19);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, edid_ptr) == 20);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dvo_cfg) == 22);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _anon_23_10) == 23);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _bitfields_27) == 27);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dvo_wiring) == 28);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _anon_29_17) == 29);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, extended_type) == 30);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dvo_function) == 32);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _bitfields_33) == 33);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dp_gpio_index) == 34);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, dp_gpio_pin_num) == 35);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _bitfields_37) == 37);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, efp_index) == 39);
const _: () = assert!(core::mem::offset_of!(ChildDeviceConfig, _bitfields_40) == 40);
const _: () = assert!(size_of::<BdbGeneralDefinitions>() == 5);
const _: () = assert!(core::mem::offset_of!(BdbGeneralDefinitions, crt_ddc_gmbus_pin) == 0);
const _: () = assert!(core::mem::offset_of!(BdbGeneralDefinitions, _bitfields_1) == 1);
const _: () = assert!(core::mem::offset_of!(BdbGeneralDefinitions, boot_display) == 2);
const _: () = assert!(size_of::<[u8; 2]>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbGeneralDefinitions, child_dev_size) == 4);
const _: () = assert!(core::mem::offset_of!(BdbGeneralDefinitions, devices) == 5);
const _: () = assert!(size_of::<[u8; 0]>() == 0);
const _: () = assert!(size_of::<BdbDisplayToggle>() == 3);
const _: () = assert!(core::mem::offset_of!(BdbDisplayToggle, feature_bits) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDisplayToggle, num_entries) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDisplayToggle, list) == 3);
const _: () = assert!(size_of::<[u16; 0]>() == 0);
const _: () = assert!(size_of::<BdbModeSupportList>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbModeSupportList, intel_mode_number) == 0);
const _: () = assert!(size_of::<[u8; 0]>() == 0);
const _: () = assert!(core::mem::offset_of!(BdbModeSupportList, mode_list_length) == 0);
const _: () = assert!(size_of::<GenericModeTable>() == 15);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, x_res) == 0);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, y_res) == 2);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, color_depths) == 4);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, refresh_rate) == 5);
const _: () = assert!(size_of::<[u8; 3]>() == 3);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, reserved) == 8);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, text_cols) == 9);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, text_rows) == 10);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, font_height) == 11);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, page_size) == 12);
const _: () = assert!(core::mem::offset_of!(GenericModeTable, misc) == 14);
const _: () = assert!(size_of::<GenericModeTimings>() == 28);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, dotclock_khz) == 0);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, hdisplay) == 4);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, htotal) == 6);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, hblank_start) == 8);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, hblank_end) == 10);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, hsync_start) == 12);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, hsync_end) == 14);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vdisplay) == 16);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vtotal) == 18);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vblank_start) == 20);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vblank_end) == 22);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vsync_start) == 24);
const _: () = assert!(core::mem::offset_of!(GenericModeTimings, vsync_end) == 26);
const _: () = assert!(size_of::<GenericModeTimingsAlm>() == 34);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, timings) == 0);
const _: () = assert!(size_of::<GenericModeTimings>() == 28);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, wm_8bpp) == 28);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, burst_8bpp) == 29);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, wm_16bpp) == 30);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, burst_16bpp) == 31);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, wm_32bpp) == 32);
const _: () = assert!(core::mem::offset_of!(GenericModeTimingsAlm, burst_32bpp) == 33);
const _: () = assert!(size_of::<BdbGenericModeTableAlm>() == 117);
const _: () = assert!(core::mem::offset_of!(BdbGenericModeTableAlm, table) == 0);
const _: () = assert!(size_of::<GenericModeTable>() == 15);
const _: () = assert!(core::mem::offset_of!(BdbGenericModeTableAlm, timings) == 15);
const _: () = assert!(size_of::<[GenericModeTimingsAlm; 3]>() == 102);
const _: () = assert!(size_of::<BdbGenericModeTableMgm>() == 101);
const _: () = assert!(core::mem::offset_of!(BdbGenericModeTableMgm, mode_flag) == 0);
const _: () = assert!(core::mem::offset_of!(BdbGenericModeTableMgm, table) == 2);
const _: () = assert!(size_of::<GenericModeTable>() == 15);
const _: () = assert!(core::mem::offset_of!(BdbGenericModeTableMgm, timings) == 17);
const _: () = assert!(size_of::<[GenericModeTimings; 3]>() == 84);
const _: () = assert!(size_of::<BdbRegTable>() == 3);
const _: () = assert!(core::mem::offset_of!(BdbRegTable, table_id) == 0);
const _: () = assert!(core::mem::offset_of!(BdbRegTable, data_access_size) == 2);
const _: () = assert!(size_of::<DotClockOverrideEntryGen2>() == 8);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen2, dotclock) == 0);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen2, n) == 4);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen2, m1) == 5);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen2, m2) == 6);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen2, _bitfields_7) == 7);
const _: () = assert!(size_of::<BdbDotClockOverrideAlm>() == 0);
const _: () = assert!(core::mem::offset_of!(BdbDotClockOverrideAlm, t) == 0);
const _: () = assert!(size_of::<[DotClockOverrideEntryGen2; 0]>() == 0);
const _: () = assert!(size_of::<PsrTable>() == 6);
const _: () = assert!(core::mem::offset_of!(PsrTable, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(PsrTable, tp1_wakeup_time) == 2);
const _: () = assert!(core::mem::offset_of!(PsrTable, tp2_tp3_wakeup_time) == 4);
const _: () = assert!(size_of::<BdbPsr>() == 100);
const _: () = assert!(core::mem::offset_of!(BdbPsr, psr_table) == 0);
const _: () = assert!(size_of::<[PsrTable; 16]>() == 96);
const _: () = assert!(core::mem::offset_of!(BdbPsr, psr2_tp2_tp3_wakeup_time) == 96);
const _: () = assert!(size_of::<ModeRemovalTable>() == 10);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, x_res) == 0);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, y_res) == 2);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, bpp) == 4);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, refresh_rate) == 5);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, removal_flags) == 7);
const _: () = assert!(core::mem::offset_of!(ModeRemovalTable, panel_flags) == 8);
const _: () = assert!(size_of::<BdbModeRemoval>() == 1);
const _: () = assert!(core::mem::offset_of!(BdbModeRemoval, row_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbModeRemoval, modes) == 1);
const _: () = assert!(size_of::<[ModeRemovalTable; 0]>() == 0);
const _: () = assert!(size_of::<BdbDriverFeatures>() == 19);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, boot_mode_x) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, boot_mode_y) == 3);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, boot_mode_bpp) == 5);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, boot_mode_refresh) == 6);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, _bitfields_7) == 7);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, legacy_crt_max_x) == 10);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, legacy_crt_max_y) == 12);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, legacy_crt_max_refresh) == 14);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, _bitfields_15) == 15);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, custom_vbt_version) == 16);
const _: () = assert!(core::mem::offset_of!(BdbDriverFeatures, _bitfields_17) == 17);
const _: () = assert!(size_of::<BdbDriverPersistence>() == 3);
const _: () = assert!(core::mem::offset_of!(BdbDriverPersistence, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDriverPersistence, persistent_max_config) == 2);
const _: () = assert!(size_of::<DotClockOverrideEntryGen3>() == 9);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, dotclock) == 0);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, n) == 4);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, m1) == 5);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, m2) == 6);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, p1) == 7);
const _: () = assert!(core::mem::offset_of!(DotClockOverrideEntryGen3, p2) == 8);
const _: () = assert!(size_of::<BdbDotClockOverride>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbDotClockOverride, row_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDotClockOverride, num_rows) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDotClockOverride, table) == 2);
const _: () = assert!(size_of::<[DotClockOverrideEntryGen3; 0]>() == 0);
const _: () = assert!(size_of::<ToggleListEntryOld>() == 3);
const _: () = assert!(core::mem::offset_of!(ToggleListEntryOld, display_select_pipe_a) == 0);
const _: () = assert!(core::mem::offset_of!(ToggleListEntryOld, display_select_pipe_b) == 1);
const _: () = assert!(core::mem::offset_of!(ToggleListEntryOld, caps) == 2);
const _: () = assert!(size_of::<ToggleListTableOld>() == 3);
const _: () = assert!(core::mem::offset_of!(ToggleListTableOld, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(ToggleListTableOld, entry_size) == 2);
const _: () = assert!(core::mem::offset_of!(ToggleListTableOld, list) == 3);
const _: () = assert!(size_of::<[ToggleListEntryOld; 0]>() == 0);
const _: () = assert!(size_of::<BdbDisplaySelectOld>() == 12);
const _: () = assert!(core::mem::offset_of!(BdbDisplaySelectOld, tables) == 0);
const _: () = assert!(size_of::<[ToggleListTableOld; 4]>() == 12);
const _: () = assert!(size_of::<BdbSvTestFunctions>() == 8);
const _: () = assert!(core::mem::offset_of!(BdbSvTestFunctions, sv_bits) == 0);
const _: () = assert!(size_of::<[u8; 8]>() == 8);
const _: () = assert!(size_of::<BdbDriverRotation>() == 12);
const _: () = assert!(core::mem::offset_of!(BdbDriverRotation, rotation_enable) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDriverRotation, rotation_flags_1) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDriverRotation, rotation_flags_2) == 2);
const _: () = assert!(core::mem::offset_of!(BdbDriverRotation, rotation_flags_3) == 4);
const _: () = assert!(core::mem::offset_of!(BdbDriverRotation, rotation_flags_4) == 8);
const _: () = assert!(size_of::<DisplayRemoveEntryOld>() == 2);
const _: () = assert!(core::mem::offset_of!(DisplayRemoveEntryOld, display_select_pipe_a) == 0);
const _: () = assert!(core::mem::offset_of!(DisplayRemoveEntryOld, display_select_pipe_b) == 1);
const _: () = assert!(size_of::<BdbDisplayRemoveOld>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveOld, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveOld, entry_size) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveOld, table) == 2);
const _: () = assert!(size_of::<[DisplayRemoveEntryOld; 0]>() == 0);
const _: () = assert!(size_of::<OemMode>() == 28);
const _: () = assert!(core::mem::offset_of!(OemMode, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(OemMode, display_flags) == 1);
const _: () = assert!(core::mem::offset_of!(OemMode, x_res) == 2);
const _: () = assert!(core::mem::offset_of!(OemMode, y_res) == 4);
const _: () = assert!(core::mem::offset_of!(OemMode, color_depth) == 6);
const _: () = assert!(core::mem::offset_of!(OemMode, refresh_rate) == 7);
const _: () = assert!(core::mem::offset_of!(OemMode, dtd) == 8);
const _: () = assert!(size_of::<BdbEdidDtd>() == 18);
const _: () = assert!(core::mem::offset_of!(OemMode, display_flags_2) == 26);
const _: () = assert!(size_of::<BdbOemCustom>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbOemCustom, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbOemCustom, entry_size) == 1);
const _: () = assert!(core::mem::offset_of!(BdbOemCustom, modes) == 2);
const _: () = assert!(size_of::<[OemMode; 0]>() == 0);
const _: () = assert!(size_of::<EfpEntry>() == 4);
const _: () = assert!(core::mem::offset_of!(EfpEntry, mfg_name) == 0);
const _: () = assert!(core::mem::offset_of!(EfpEntry, product_code) == 2);
const _: () = assert!(size_of::<BdbEfpList>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbEfpList, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbEfpList, entry_size) == 1);
const _: () = assert!(core::mem::offset_of!(BdbEfpList, efp) == 2);
const _: () = assert!(size_of::<[EfpEntry; 0]>() == 0);
const _: () = assert!(size_of::<BdbSdvoLvdsOptions>() == 22);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_backlight) == 0);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, h40_set_panel_type) == 1);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_type) == 2);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, ssc_clk_freq) == 3);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, als_low_trip) == 4);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, als_high_trip) == 6);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, sclalarcoeff_tab_row_num) == 8);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, sclalarcoeff_tab_row_size) == 9);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, coefficient) == 10);
const _: () = assert!(size_of::<[u8; 8]>() == 8);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_misc_bits_1) == 18);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_misc_bits_2) == 19);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_misc_bits_3) == 20);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsOptions, panel_misc_bits_4) == 21);
const _: () = assert!(size_of::<BdbSdvoLvdsDtd>() == 72);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsDtd, dtd) == 0);
const _: () = assert!(size_of::<[BdbEdidDtd; 4]>() == 72);
const _: () = assert!(size_of::<BdbSdvoLvdsPnpId>() == 40);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsPnpId, pnp_id) == 0);
const _: () = assert!(size_of::<[BdbEdidPnpId; 4]>() == 40);
const _: () = assert!(size_of::<SdvoLvdsPps>() == 10);
const _: () = assert!(core::mem::offset_of!(SdvoLvdsPps, t0) == 0);
const _: () = assert!(core::mem::offset_of!(SdvoLvdsPps, t1) == 2);
const _: () = assert!(core::mem::offset_of!(SdvoLvdsPps, t2) == 4);
const _: () = assert!(core::mem::offset_of!(SdvoLvdsPps, t3) == 6);
const _: () = assert!(core::mem::offset_of!(SdvoLvdsPps, t4) == 8);
const _: () = assert!(size_of::<BdbSdvoLvdsPps>() == 40);
const _: () = assert!(core::mem::offset_of!(BdbSdvoLvdsPps, pps) == 0);
const _: () = assert!(size_of::<[SdvoLvdsPps; 4]>() == 40);
const _: () = assert!(size_of::<BdbTvOptions>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbTvOptions, _bitfields_0) == 0);
const _: () = assert!(size_of::<EdpPowerSeq>() == 10);
const _: () = assert!(core::mem::offset_of!(EdpPowerSeq, t1_t3) == 0);
const _: () = assert!(core::mem::offset_of!(EdpPowerSeq, t8) == 2);
const _: () = assert!(core::mem::offset_of!(EdpPowerSeq, t9) == 4);
const _: () = assert!(core::mem::offset_of!(EdpPowerSeq, t10) == 6);
const _: () = assert!(core::mem::offset_of!(EdpPowerSeq, t11_t12) == 8);
const _: () = assert!(size_of::<EdpFastLinkParams>() == 2);
const _: () = assert!(core::mem::offset_of!(EdpFastLinkParams, _bitfields_0) == 0);
const _: () = assert!(size_of::<EdpPwmDelays>() == 4);
const _: () = assert!(core::mem::offset_of!(EdpPwmDelays, pwm_on_to_backlight_enable) == 0);
const _: () = assert!(core::mem::offset_of!(EdpPwmDelays, backlight_disable_to_pwm_off) == 2);
const _: () = assert!(size_of::<EdpFullLinkParams>() == 1);
const _: () = assert!(core::mem::offset_of!(EdpFullLinkParams, _bitfields_0) == 0);
const _: () = assert!(size_of::<EdpApicalParams>() == 28);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, panel_oui) == 0);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, dpcd_base_address) == 4);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, dpcd_idridix_control_0) == 8);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, dpcd_option_select) == 12);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, dpcd_backlight) == 16);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, ambient_light) == 20);
const _: () = assert!(core::mem::offset_of!(EdpApicalParams, backlight_scale) == 24);
const _: () = assert!(size_of::<BdbEdp>() == 850);
const _: () = assert!(core::mem::offset_of!(BdbEdp, power_seqs) == 0);
const _: () = assert!(size_of::<[EdpPowerSeq; 16]>() == 160);
const _: () = assert!(core::mem::offset_of!(BdbEdp, color_depth) == 160);
const _: () = assert!(core::mem::offset_of!(BdbEdp, fast_link_params) == 164);
const _: () = assert!(size_of::<[EdpFastLinkParams; 16]>() == 32);
const _: () = assert!(core::mem::offset_of!(BdbEdp, sdrrs_msa_timing_delay) == 196);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_s3d_feature) == 200);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_t3_optimization) == 202);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_vswing_preemph) == 204);
const _: () = assert!(core::mem::offset_of!(BdbEdp, fast_link_training) == 212);
const _: () = assert!(core::mem::offset_of!(BdbEdp, dpcd_600h_write_required) == 214);
const _: () = assert!(core::mem::offset_of!(BdbEdp, pwm_delays) == 216);
const _: () = assert!(size_of::<[EdpPwmDelays; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(BdbEdp, full_link_params_provided) == 280);
const _: () = assert!(core::mem::offset_of!(BdbEdp, full_link_params) == 282);
const _: () = assert!(size_of::<[EdpFullLinkParams; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbEdp, apical_enable) == 298);
const _: () = assert!(core::mem::offset_of!(BdbEdp, apical_params) == 300);
const _: () = assert!(size_of::<[EdpApicalParams; 16]>() == 448);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_fast_link_training_rate) == 748);
const _: () = assert!(size_of::<[u16; 16]>() == 32);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_max_port_link_rate) == 780);
const _: () = assert!(size_of::<[u16; 16]>() == 32);
const _: () = assert!(core::mem::offset_of!(BdbEdp, edp_dsc_disable) == 812);
const _: () = assert!(core::mem::offset_of!(BdbEdp, t6_delay_support) == 814);
const _: () = assert!(core::mem::offset_of!(BdbEdp, link_idle_time) == 816);
const _: () = assert!(size_of::<[u16; 16]>() == 32);
const _: () = assert!(core::mem::offset_of!(BdbEdp, pipe_joiner_enable) == 848);
const _: () = assert!(size_of::<BdbEfpDtd>() == 54);
const _: () = assert!(core::mem::offset_of!(BdbEfpDtd, dtd) == 0);
const _: () = assert!(size_of::<[BdbEdidDtd; 3]>() == 54);
const _: () = assert!(size_of::<ToggleListEntryIvb>() == 1);
const _: () = assert!(core::mem::offset_of!(ToggleListEntryIvb, display_select) == 0);
const _: () = assert!(size_of::<ToggleListTableIvb>() == 3);
const _: () = assert!(core::mem::offset_of!(ToggleListTableIvb, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(ToggleListTableIvb, entry_size) == 2);
const _: () = assert!(core::mem::offset_of!(ToggleListTableIvb, list) == 3);
const _: () = assert!(size_of::<[ToggleListEntryIvb; 0]>() == 0);
const _: () = assert!(size_of::<BdbDisplaySelectIvb>() == 12);
const _: () = assert!(core::mem::offset_of!(BdbDisplaySelectIvb, tables) == 0);
const _: () = assert!(size_of::<[ToggleListTableIvb; 4]>() == 12);
const _: () = assert!(size_of::<DisplayRemoveEntryIvb>() == 1);
const _: () = assert!(core::mem::offset_of!(DisplayRemoveEntryIvb, display_select) == 0);
const _: () = assert!(size_of::<BdbDisplayRemoveIvb>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveIvb, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveIvb, entry_size) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveIvb, table) == 2);
const _: () = assert!(size_of::<[DisplayRemoveEntryIvb; 0]>() == 0);
const _: () = assert!(size_of::<ToggleListEntryHsw>() == 2);
const _: () = assert!(core::mem::offset_of!(ToggleListEntryHsw, display_select) == 0);
const _: () = assert!(size_of::<ToggleListTableHsw>() == 3);
const _: () = assert!(core::mem::offset_of!(ToggleListTableHsw, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(ToggleListTableHsw, entry_size) == 2);
const _: () = assert!(core::mem::offset_of!(ToggleListTableHsw, list) == 3);
const _: () = assert!(size_of::<[ToggleListEntryHsw; 0]>() == 0);
const _: () = assert!(size_of::<BdbDisplaySelectHsw>() == 12);
const _: () = assert!(core::mem::offset_of!(BdbDisplaySelectHsw, tables) == 0);
const _: () = assert!(size_of::<[ToggleListTableHsw; 4]>() == 12);
const _: () = assert!(size_of::<DisplayRemoveEntryHsw>() == 2);
const _: () = assert!(core::mem::offset_of!(DisplayRemoveEntryHsw, display_select) == 0);
const _: () = assert!(size_of::<BdbDisplayRemoveHsw>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveHsw, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveHsw, entry_size) == 1);
const _: () = assert!(core::mem::offset_of!(BdbDisplayRemoveHsw, table) == 2);
const _: () = assert!(size_of::<[DisplayRemoveEntryHsw; 0]>() == 0);
const _: () = assert!(size_of::<BdbLfpOptions>() == 34);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, panel_type) == 0);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, panel_type2) == 1);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, _bitfields_2) == 2);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, rsvd4) == 3);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, lvds_panel_channel_bits) == 4);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, ssc_bits) == 8);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, ssc_freq) == 10);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, ssc_ddt) == 12);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, panel_color_depth) == 14);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, dps_panel_type_bits) == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, blt_control_type_bits) == 20);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, lcdvcc_s0_enable) == 24);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, rotation) == 26);
const _: () = assert!(core::mem::offset_of!(BdbLfpOptions, position) == 30);
const _: () = assert!(size_of::<LfpDataPtrTable>() == 3);
const _: () = assert!(core::mem::offset_of!(LfpDataPtrTable, offset) == 0);
const _: () = assert!(core::mem::offset_of!(LfpDataPtrTable, table_size) == 2);
const _: () = assert!(size_of::<LfpDataPtr>() == 9);
const _: () = assert!(core::mem::offset_of!(LfpDataPtr, fp_timing) == 0);
const _: () = assert!(size_of::<LfpDataPtrTable>() == 3);
const _: () = assert!(core::mem::offset_of!(LfpDataPtr, dvo_timing) == 3);
const _: () = assert!(size_of::<LfpDataPtrTable>() == 3);
const _: () = assert!(core::mem::offset_of!(LfpDataPtr, panel_pnp_id) == 6);
const _: () = assert!(size_of::<LfpDataPtrTable>() == 3);
const _: () = assert!(size_of::<BdbLfpDataPtrs>() == 148);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataPtrs, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataPtrs, ptr) == 1);
const _: () = assert!(size_of::<[LfpDataPtr; 16]>() == 144);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataPtrs, panel_name) == 145);
const _: () = assert!(size_of::<LfpDataPtrTable>() == 3);
const _: () = assert!(size_of::<FpTiming>() == 46);
const _: () = assert!(core::mem::offset_of!(FpTiming, x_res) == 0);
const _: () = assert!(core::mem::offset_of!(FpTiming, y_res) == 2);
const _: () = assert!(core::mem::offset_of!(FpTiming, lvds_reg) == 4);
const _: () = assert!(core::mem::offset_of!(FpTiming, lvds_reg_val) == 8);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_on_reg) == 12);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_on_reg_val) == 16);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_off_reg) == 20);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_off_reg_val) == 24);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_cycle_reg) == 28);
const _: () = assert!(core::mem::offset_of!(FpTiming, pp_cycle_reg_val) == 32);
const _: () = assert!(core::mem::offset_of!(FpTiming, pfit_reg) == 36);
const _: () = assert!(core::mem::offset_of!(FpTiming, pfit_reg_val) == 40);
const _: () = assert!(core::mem::offset_of!(FpTiming, terminator) == 44);
const _: () = assert!(size_of::<LfpDataEntry>() == 74);
const _: () = assert!(core::mem::offset_of!(LfpDataEntry, fp_timing) == 0);
const _: () = assert!(size_of::<FpTiming>() == 46);
const _: () = assert!(core::mem::offset_of!(LfpDataEntry, dvo_timing) == 46);
const _: () = assert!(size_of::<BdbEdidDtd>() == 18);
const _: () = assert!(core::mem::offset_of!(LfpDataEntry, pnp_id) == 64);
const _: () = assert!(size_of::<BdbEdidPnpId>() == 10);
const _: () = assert!(size_of::<BdbLfpData>() == 1184);
const _: () = assert!(core::mem::offset_of!(BdbLfpData, data) == 0);
const _: () = assert!(size_of::<[LfpDataEntry; 16]>() == 1184);
const _: () = assert!(size_of::<LfpBlackBorder>() == 4);
const _: () = assert!(core::mem::offset_of!(LfpBlackBorder, top) == 0);
const _: () = assert!(core::mem::offset_of!(LfpBlackBorder, bottom) == 1);
const _: () = assert!(core::mem::offset_of!(LfpBlackBorder, left) == 2);
const _: () = assert!(core::mem::offset_of!(LfpBlackBorder, right) == 3);
const _: () = assert!(size_of::<BdbLfpDataTail>() == 310);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, panel_name) == 0);
const _: () = assert!(size_of::<[BdbEdidProductName; 16]>() == 208);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, scaling_enable) == 208);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, seamless_drrs_min_refresh_rate) == 210);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, pixel_overlap_count) == 226);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, black_border) == 242);
const _: () = assert!(size_of::<[LfpBlackBorder; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, dual_lfp_port_sync_enable) == 306);
const _: () = assert!(core::mem::offset_of!(BdbLfpDataTail, gpu_dithering_for_banding_artifacts) == 308);
const _: () = assert!(size_of::<LfpBacklightDataEntry>() == 6);
const _: () = assert!(core::mem::offset_of!(LfpBacklightDataEntry, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(LfpBacklightDataEntry, pwm_freq_hz) == 1);
const _: () = assert!(core::mem::offset_of!(LfpBacklightDataEntry, min_brightness) == 3);
const _: () = assert!(core::mem::offset_of!(LfpBacklightDataEntry, i2c_address) == 4);
const _: () = assert!(core::mem::offset_of!(LfpBacklightDataEntry, i2c_command) == 5);
const _: () = assert!(size_of::<LfpBacklightControlMethod>() == 1);
const _: () = assert!(core::mem::offset_of!(LfpBacklightControlMethod, _bitfields_0) == 0);
const _: () = assert!(size_of::<LfpBrightnessLevel>() == 4);
const _: () = assert!(core::mem::offset_of!(LfpBrightnessLevel, level) == 0);
const _: () = assert!(core::mem::offset_of!(LfpBrightnessLevel, reserved) == 2);
const _: () = assert!(size_of::<BdbLfpBacklight>() == 305);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, entry_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, data) == 1);
const _: () = assert!(size_of::<[LfpBacklightDataEntry; 16]>() == 96);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, level) == 97);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, backlight_control) == 113);
const _: () = assert!(size_of::<[LfpBacklightControlMethod; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, brightness_level) == 129);
const _: () = assert!(size_of::<[LfpBrightnessLevel; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, brightness_min_level) == 193);
const _: () = assert!(size_of::<[LfpBrightnessLevel; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, brightness_precision_bits) == 257);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpBacklight, hdr_dpcd_refresh_timeout) == 273);
const _: () = assert!(size_of::<[u16; 16]>() == 32);
const _: () = assert!(size_of::<LfpPowerFeatures>() == 1);
const _: () = assert!(core::mem::offset_of!(LfpPowerFeatures, _bitfields_0) == 0);
const _: () = assert!(size_of::<AlsDataEntry>() == 4);
const _: () = assert!(core::mem::offset_of!(AlsDataEntry, backlight_adjust) == 0);
const _: () = assert!(core::mem::offset_of!(AlsDataEntry, lux) == 2);
const _: () = assert!(size_of::<AggressivenessProfileEntry>() == 1);
const _: () = assert!(core::mem::offset_of!(AggressivenessProfileEntry, _bitfields_0) == 0);
const _: () = assert!(size_of::<AggressivenessProfile2Entry>() == 1);
const _: () = assert!(core::mem::offset_of!(AggressivenessProfile2Entry, _bitfields_0) == 0);
const _: () = assert!(size_of::<AggressivenessProfile3Entry>() == 1);
const _: () = assert!(core::mem::offset_of!(AggressivenessProfile3Entry, _bitfields_0) == 0);
const _: () = assert!(size_of::<AggressivenessProfile4Entry>() == 1);
const _: () = assert!(core::mem::offset_of!(AggressivenessProfile4Entry, _bitfields_0) == 0);
const _: () = assert!(size_of::<PanelIdentification>() == 1);
const _: () = assert!(core::mem::offset_of!(PanelIdentification, _bitfields_0) == 0);
const _: () = assert!(size_of::<BdbLfpPower>() == 136);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, features) == 0);
const _: () = assert!(size_of::<LfpPowerFeatures>() == 1);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, als) == 1);
const _: () = assert!(size_of::<[AlsDataEntry; 5]>() == 20);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, _bitfields_21) == 21);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, dpst) == 22);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, psr) == 24);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, drrs) == 26);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, lace_support) == 28);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, adt) == 30);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, dmrrs) == 32);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, adb) == 34);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, lace_enabled_status) == 36);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, aggressiveness) == 38);
const _: () = assert!(size_of::<[AggressivenessProfileEntry; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, hobl) == 54);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, vrr_feature_enabled) == 56);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, elp) == 58);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, opst) == 60);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, aggressiveness2) == 62);
const _: () = assert!(size_of::<[AggressivenessProfile2Entry; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, apd) == 78);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, pixoptix) == 80);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, aggressiveness3) == 82);
const _: () = assert!(size_of::<[AggressivenessProfile3Entry; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, panel_identification) == 98);
const _: () = assert!(size_of::<[PanelIdentification; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, xpst_support) == 114);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, tcon_based_backlight_optimization) == 116);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, aggressiveness4) == 118);
const _: () = assert!(size_of::<[AggressivenessProfile4Entry; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(BdbLfpPower, tcon_backlight_xpst_coexistence) == 134);
const _: () = assert!(size_of::<EdpBfi>() == 2);
const _: () = assert!(core::mem::offset_of!(EdpBfi, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(EdpBfi, brightness_percentage_when_bfi_disabled) == 1);
const _: () = assert!(size_of::<BdbEdpBfi>() == 33);
const _: () = assert!(core::mem::offset_of!(BdbEdpBfi, bfi_structure_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbEdpBfi, bfi) == 1);
const _: () = assert!(size_of::<[EdpBfi; 16]>() == 32);
const _: () = assert!(size_of::<Chromaticity>() == 11);
const _: () = assert!(core::mem::offset_of!(Chromaticity, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(Chromaticity, red_x_hi) == 3);
const _: () = assert!(core::mem::offset_of!(Chromaticity, red_y_hi) == 4);
const _: () = assert!(core::mem::offset_of!(Chromaticity, green_x_hi) == 5);
const _: () = assert!(core::mem::offset_of!(Chromaticity, green_y_hi) == 6);
const _: () = assert!(core::mem::offset_of!(Chromaticity, blue_x_hi) == 7);
const _: () = assert!(core::mem::offset_of!(Chromaticity, blue_y_hi) == 8);
const _: () = assert!(core::mem::offset_of!(Chromaticity, white_x_hi) == 9);
const _: () = assert!(core::mem::offset_of!(Chromaticity, white_y_hi) == 10);
const _: () = assert!(size_of::<LuminanceAndGamma>() == 8);
const _: () = assert!(core::mem::offset_of!(LuminanceAndGamma, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(LuminanceAndGamma, min_luminance) == 1);
const _: () = assert!(core::mem::offset_of!(LuminanceAndGamma, max_luminance) == 3);
const _: () = assert!(core::mem::offset_of!(LuminanceAndGamma, one_percent_max_luminance) == 5);
const _: () = assert!(core::mem::offset_of!(LuminanceAndGamma, gamma) == 7);
const _: () = assert!(size_of::<BdbChromaticity>() == 304);
const _: () = assert!(core::mem::offset_of!(BdbChromaticity, chromaticity) == 0);
const _: () = assert!(size_of::<[Chromaticity; 16]>() == 176);
const _: () = assert!(core::mem::offset_of!(BdbChromaticity, luminance_and_gamma) == 176);
const _: () = assert!(size_of::<[LuminanceAndGamma; 16]>() == 128);
const _: () = assert!(size_of::<MipiData>() == 44);
const _: () = assert!(core::mem::offset_of!(MipiData, panel_identifier) == 0);
const _: () = assert!(core::mem::offset_of!(MipiData, bridge_revision) == 2);
const _: () = assert!(core::mem::offset_of!(MipiData, _bitfields_4) == 4);
const _: () = assert!(core::mem::offset_of!(MipiData, port_info) == 8);
const _: () = assert!(core::mem::offset_of!(MipiData, _bitfields_10) == 10);
const _: () = assert!(core::mem::offset_of!(MipiData, dsi_ddr_clock) == 14);
const _: () = assert!(core::mem::offset_of!(MipiData, renesas_bridge_ref_clock) == 18);
const _: () = assert!(core::mem::offset_of!(MipiData, power_conservation) == 22);
const _: () = assert!(core::mem::offset_of!(MipiData, _bitfields_24) == 24);
const _: () = assert!(core::mem::offset_of!(MipiData, high_low_switch_count) == 28);
const _: () = assert!(core::mem::offset_of!(MipiData, lp_byte_clock) == 32);
const _: () = assert!(core::mem::offset_of!(MipiData, clock_lane_switch_time_counter) == 36);
const _: () = assert!(core::mem::offset_of!(MipiData, panel_color_depth) == 40);
const _: () = assert!(size_of::<BdbMipi>() == 704);
const _: () = assert!(core::mem::offset_of!(BdbMipi, mipi) == 0);
const _: () = assert!(size_of::<[MipiData; 16]>() == 704);
const _: () = assert!(size_of::<BdbFixedSetMode>() == 9);
const _: () = assert!(core::mem::offset_of!(BdbFixedSetMode, enable) == 0);
const _: () = assert!(core::mem::offset_of!(BdbFixedSetMode, x_res) == 1);
const _: () = assert!(core::mem::offset_of!(BdbFixedSetMode, y_res) == 5);
const _: () = assert!(size_of::<BdbMipiConfig>() == 822);
const _: () = assert!(core::mem::offset_of!(BdbMipiConfig, config) == 0);
const _: () = assert!(size_of::<[MipiConfig; 6]>() == 732);
const _: () = assert!(core::mem::offset_of!(BdbMipiConfig, pps) == 732);
const _: () = assert!(size_of::<[MipiPpsData; 6]>() == 60);
const _: () = assert!(core::mem::offset_of!(BdbMipiConfig, pwm_delays) == 792);
const _: () = assert!(size_of::<[EdpPwmDelays; 6]>() == 24);
const _: () = assert!(core::mem::offset_of!(BdbMipiConfig, pmic_i2c_bus_number) == 816);
const _: () = assert!(size_of::<[u8; 6]>() == 6);
const _: () = assert!(size_of::<BdbMipiSequence>() == 1);
const _: () = assert!(core::mem::offset_of!(BdbMipiSequence, version) == 0);
const _: () = assert!(core::mem::offset_of!(BdbMipiSequence, data) == 1);
const _: () = assert!(size_of::<[u8; 0]>() == 0);
const _: () = assert!(size_of::<BdbRgbPalette>() == 769);
const _: () = assert!(core::mem::offset_of!(BdbRgbPalette, is_enabled) == 0);
const _: () = assert!(core::mem::offset_of!(BdbRgbPalette, red) == 1);
const _: () = assert!(size_of::<[u8; 256]>() == 256);
const _: () = assert!(core::mem::offset_of!(BdbRgbPalette, blue) == 257);
const _: () = assert!(size_of::<[u8; 256]>() == 256);
const _: () = assert!(core::mem::offset_of!(BdbRgbPalette, green) == 513);
const _: () = assert!(size_of::<[u8; 256]>() == 256);
const _: () = assert!(size_of::<DscCompressionParametersEntry>() == 13);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, _bitfields_0) == 0);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, rc_buffer_size) == 2);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, slices_per_line) == 3);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, _bitfields_7) == 7);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, max_bpp) == 9);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, _bitfields_10) == 10);
const _: () = assert!(core::mem::offset_of!(DscCompressionParametersEntry, slice_height) == 11);
const _: () = assert!(size_of::<BdbCompressionParameters>() == 210);
const _: () = assert!(core::mem::offset_of!(BdbCompressionParameters, entry_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbCompressionParameters, data) == 2);
const _: () = assert!(size_of::<[DscCompressionParametersEntry; 16]>() == 208);
const _: () = assert!(size_of::<BdbVswingPreemph>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbVswingPreemph, num_tables) == 0);
const _: () = assert!(core::mem::offset_of!(BdbVswingPreemph, num_columns) == 1);
const _: () = assert!(core::mem::offset_of!(BdbVswingPreemph, tables) == 2);
const _: () = assert!(size_of::<[u32; 0]>() == 0);
const _: () = assert!(size_of::<GenericDtdEntry>() == 28);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, pixel_clock) == 0);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, hactive) == 4);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, hblank) == 6);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, hfront_porch) == 8);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, hsync) == 10);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, vactive) == 12);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, vblank) == 14);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, vfront_porch) == 16);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, vsync) == 18);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, width_mm) == 20);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, height_mm) == 22);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, _bitfields_24) == 24);
const _: () = assert!(core::mem::offset_of!(GenericDtdEntry, rsvd) == 25);
const _: () = assert!(size_of::<[u8; 3]>() == 3);
const _: () = assert!(size_of::<BdbGenericDtd>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbGenericDtd, gdtd_size) == 0);
const _: () = assert!(core::mem::offset_of!(BdbGenericDtd, dtd) == 2);
const _: () = assert!(size_of::<[GenericDtdEntry; 0]>() == 0);
const _: () = assert!(size_of::<PrdEntryOld>() == 3);
const _: () = assert!(core::mem::offset_of!(PrdEntryOld, displays_attached) == 0);
const _: () = assert!(core::mem::offset_of!(PrdEntryOld, display_in_pipe_a) == 1);
const _: () = assert!(core::mem::offset_of!(PrdEntryOld, display_in_pipe_b) == 2);
const _: () = assert!(size_of::<BdbPrdTableOld>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbPrdTableOld, list) == 0);
const _: () = assert!(size_of::<[PrdEntryOld; 0]>() == 0);
const _: () = assert!(core::mem::offset_of!(BdbPrdTableOld, num_entries) == 0);
const _: () = assert!(size_of::<PrdEntryNew>() == 4);
const _: () = assert!(core::mem::offset_of!(PrdEntryNew, primary_display) == 0);
const _: () = assert!(core::mem::offset_of!(PrdEntryNew, secondary_display) == 2);
const _: () = assert!(size_of::<BdbPrdTableNew>() == 2);
const _: () = assert!(core::mem::offset_of!(BdbPrdTableNew, num_entries) == 0);
const _: () = assert!(core::mem::offset_of!(BdbPrdTableNew, list) == 2);
const _: () = assert!(size_of::<[PrdEntryNew; 0]>() == 0);
