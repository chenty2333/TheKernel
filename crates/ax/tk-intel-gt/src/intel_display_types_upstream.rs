// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_types.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use crate::intel_workarounds_types_upstream::I915RegT;
use tk_linux_kapi::types::AtomicT;
use tk_linux_kapi::types::DelayedWork;
use tk_linux_kapi::types::ListHead;
use tk_linux_kapi::types::Mutex;
use tk_linux_kapi::types::Spinlock;
use tk_linux_kapi::types::WaitQueueHead;
use tk_linux_kapi::types::WorkStruct;
use tk_drm_upstream::drm_atomic_types::{DrmAtomicCommit};
use tk_drm_upstream::drm_color_mgmt_types::{DrmColorEncoding, DrmColorRange};
use tk_drm_upstream::drm_connector_types::{DrmConnector, DrmConnectorState, DrmPanelOrientation};
use tk_drm_upstream::drm_crtc_types::{DrmCrtc, DrmCrtcState};
use tk_drm_upstream::drm_dp_helper_types::{DrmDpAsSdp, DrmDpAux, DrmDpDesc, DrmDpPhyTestParams, DrmDpVscSdp, DrmEdpBacklightInfo};
use tk_drm_upstream::drm_encoder_types::{DrmEncoder};
use tk_drm_upstream::drm_fourcc_types::{DrmFormatInfo};
use tk_drm_upstream::drm_framebuffer_types::{DrmFramebuffer};
use tk_drm_upstream::drm_modes_types::{DrmDisplayMode};
use tk_drm_upstream::drm_plane_types::{DrmPlane, DrmPlaneState, DrmScalingFilter};
use tk_drm_upstream::drm_property_types::{DrmPropertyBlob};
use tk_drm_upstream::drm_rect_types::{DrmRect};
use tk_drm_upstream::drm_vblank_types::{DrmPendingVblankEvent};
use tk_drm_upstream::drm_vblank_work_types::{DrmVblankWork};
use tk_drm_upstream::hdmi_types::{HdmiInfoframe};
use crate::i915_external_placeholder_types_upstream::{DrmColorop, DrmDpDualModeType, DrmDpMstMode, DrmDpMstPort, DrmDpMstTopologyMgr, DrmDpTunnelRef, DrmDscConfig, DrmIntelSpriteColorkey, DrmLspconMode, HdcpPortData, Irqreturn, PmQosRequest, PwmDevice, PwmState};
use crate::i915_gtt_view_types_upstream::{I915GttView};
use crate::intel_bios_types_upstream::{IntelBacklightType};
use crate::intel_ddi_buf_trans_types_upstream::{IntelDdiBufTrans};
use crate::intel_display_limits_types_upstream::{AuxCh, HpdPin, I9xxPlaneId, IntelColorBlock, Pipe, PlaneId, Port, Transcoder};
use crate::intel_display_power_types_upstream::{IntelDisplayPowerDomain, IntelDisplayPowerDomainSet};
use crate::intel_dpll_mgr_types_upstream::{IclPortDpllId, IntelDpll, IntelDpllHwState, IntelDpllState};
use crate::intel_dsi_vbt_defs_types_upstream::{MipiConfig, MipiPpsData};
use crate::intel_frontbuffer_types_upstream::{IntelFrontbuffer};
use crate::intel_hdcp_shim_types_upstream::{IntelHdcpShim};
use crate::intel_wm_types_upstream::{G4xPipeWm, G4xSrWm, SklDdbEntry};

#[repr(C)]
pub struct IntelFbView {
    pub gtt: I915GttView,
    pub color_plane: [I915ColorPlaneView; 4],
}

#[repr(C)]
pub struct I915ColorPlaneView {
    pub offset: u32,
    pub x: u32,
    pub y: u32,
    pub mapping_stride: u32,
    pub scanout_stride: u32,
}

#[repr(C)]
pub union IntelFramebufferAnon336x3 {
    pub rotated_view: ManuallyDrop<IntelFbView>,
    pub remapped_view: ManuallyDrop<IntelFbView>,
}

#[repr(C, align(8))]
pub struct IntelFramebuffer {
    pub base: DrmFramebuffer,
    pub frontbuffer: *mut IntelFrontbuffer,
    pub normal_view: IntelFbView,
    pub _anon_336_3: IntelFramebufferAnon336x3,
    pub dpt: *mut c_void,
    pub min_alignment: u32,
    pub vtd_guard: u32,
    pub panic_tiling: Option<unsafe extern "C" fn(u32, u32, u32) -> u32>,
    pub panic: *mut c_void,
}

#[repr(C, align(8))]
pub struct IntelEncoder {
    pub base: DrmEncoder,
    pub r#type: IntelOutputType,
    pub port: Port,
    pub cloneable: u16,
    pub pipe_mask: u8,
    pub _pad_163: [u8; 5],
    pub link_check_work: DelayedWork,
    pub link_check: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub hotplug: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelConnector) -> IntelHotplugState>,
    pub compute_output_type: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState) -> IntelOutputType>,
    pub compute_config: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState) -> i32>,
    pub compute_config_late: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState) -> i32>,
    pub pre_pll_enable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub pre_enable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub enable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub disable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub post_disable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub post_pll_disable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub update_pipe: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub audio_enable: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub audio_disable: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub get_hw_state: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut Pipe) -> bool>,
    pub get_config: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState)>,
    pub sync_state: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState)>,
    pub initial_fastset_check: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState) -> bool>,
    pub get_power_domains: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState)>,
    pub suspend: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub suspend_complete: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub shutdown: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub shutdown_complete: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub enable_clock: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState)>,
    pub disable_clock: Option<unsafe extern "C" fn(*mut IntelEncoder)>,
    pub is_clock_enabled: Option<unsafe extern "C" fn(*mut IntelEncoder) -> bool>,
    pub port_pll_type: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState) -> IclPortDpllId>,
    pub get_buf_trans: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, *mut i32) -> *mut IntelDdiBufTrans>,
    pub set_signal_levels: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState)>,
    pub hpd_pin: HpdPin,
    pub power_domain: IntelDisplayPowerDomain,
    pub devdata: *mut c_void,
}

#[repr(C)]
pub struct IntelPanelBlFuncs {
    pub setup: Option<unsafe extern "C" fn(*mut IntelConnector, Pipe) -> i32>,
    pub get: Option<unsafe extern "C" fn(*mut IntelConnector, Pipe) -> u32>,
    pub set: Option<unsafe extern "C" fn(*mut DrmConnectorState, u32)>,
    pub disable: Option<unsafe extern "C" fn(*mut DrmConnectorState, u32)>,
    pub enable: Option<unsafe extern "C" fn(*mut IntelCrtcState, *mut DrmConnectorState, u32)>,
    pub hz_to_pwm: Option<unsafe extern "C" fn(*mut IntelConnector, u32) -> u32>,
}

#[repr(C)]
pub struct IntelPpsDelays {
    pub power_up: u16,
    pub backlight_on: u16,
    pub backlight_off: u16,
    pub power_down: u16,
    pub power_cycle: u16,
}

#[repr(C)]
pub struct IntelVbtPanelDataEdp {
    pub max_link_rate: i32,
    pub rate: i32,
    pub lanes: i32,
    pub preemphasis: i32,
    pub vswing: i32,
    pub bpp: i32,
    pub pps: IntelPpsDelays,
    pub drrs_msa_timing_delay: u8,
    pub low_vswing: bool,
    pub hobl: bool,
    pub dsc_disable: bool,
    pub pipe_joiner_enable: bool,
    pub _pad_39: [u8; 1],
}

#[repr(C)]
pub struct IntelVbtPanelDataPsr {
    pub enable: bool,
    pub full_link: bool,
    pub require_aux_wakeup: bool,
    pub _pad_3: [u8; 1],
    pub idle_frames: i32,
    pub tp1_wakeup_time_us: i32,
    pub tp2_tp3_wakeup_time_us: i32,
    pub psr2_tp2_tp3_wakeup_time_us: i32,
}

#[repr(C)]
pub struct IntelVbtPanelDataBacklight {
    pub pwm_freq_hz: u16,
    pub brightness_precision_bits: u16,
    pub hdr_dpcd_refresh_timeout: u16,
    pub present: bool,
    pub active_low_pwm: bool,
    pub min_brightness: u8,
    pub controller: i8,
    pub _pad_10: [u8; 2],
    pub r#type: IntelBacklightType,
}

#[repr(C)]
pub struct IntelVbtPanelDataDsi {
    pub panel_id: u16,
    pub _pad_2: [u8; 6],
    pub config: *mut MipiConfig,
    pub pps: *mut MipiPpsData,
    pub bl_ports: u16,
    pub cabc_ports: u16,
    pub seq_version: u8,
    pub _pad_29: [u8; 3],
    pub size: u32,
    pub _pad_36: [u8; 4],
    pub data: *mut u8,
    pub sequence: [*mut u8; 12],
    pub deassert_seq: *mut u8,
    pub orientation: DrmPanelOrientation,
    pub _pad_156: [u8; 4],
}

#[repr(C)]
pub struct IntelVbtPanelData {
    pub lfp_vbt_mode: *mut DrmDisplayMode,
    pub sdvo_lvds_vbt_mode: *mut DrmDisplayMode,
    pub panel_type: i32,
    /// bit-fields lvds_dither:1
    pub _bitfields_20: [u8; 1],
    pub _pad_21: [u8; 3],
    pub bios_lvds_val: u32,
    pub vrr: bool,
    pub seamless_drrs_min_refresh_rate: u8,
    pub _pad_30: [u8; 2],
    pub drrs_type: DrrsType,
    pub edp: IntelVbtPanelDataEdp,
    pub psr: IntelVbtPanelDataPsr,
    pub backlight: IntelVbtPanelDataBacklight,
    pub dsi: IntelVbtPanelDataDsi,
}

#[repr(C)]
pub struct IntelPanelBacklightEdpVesa {
    pub info: DrmEdpBacklightInfo,
    pub luminance_control_support: bool,
    pub _pad_13: [u8; 3],
}

#[repr(C)]
pub struct IntelPanelBacklightEdpIntelCap {
    pub sdr_uses_aux: bool,
    pub supports_2084_decode: bool,
    pub supports_2020_gamut: bool,
    pub supports_segmented_backlight: bool,
    pub supports_sdp_colorimetry: bool,
    pub supports_tone_mapping: bool,
}

#[repr(C)]
pub union IntelPanelBacklightEdp {
    pub vesa: ManuallyDrop<IntelPanelBacklightEdpVesa>,
    pub intel_cap: ManuallyDrop<IntelPanelBacklightEdpIntelCap>,
}

#[repr(C)]
pub struct IntelPanelBacklight {
    pub present: bool,
    pub _pad_1: [u8; 3],
    pub level: u32,
    pub min: u32,
    pub max: u32,
    pub enabled: bool,
    pub combination_mode: bool,
    pub active_low_pwm: bool,
    pub alternate_pwm_increment: bool,
    pub pwm_level_min: u32,
    pub pwm_level_max: u32,
    pub pwm_enabled: bool,
    pub util_pin_active_low: bool,
    pub controller: u8,
    pub _pad_31: [u8; 1],
    pub pwm: *mut PwmDevice,
    pub pwm_state: PwmState,
    pub edp: IntelPanelBacklightEdp,
    pub device: *mut c_void,
    pub funcs: *mut IntelPanelBlFuncs,
    pub pwm_funcs: *mut IntelPanelBlFuncs,
    pub power: Option<unsafe extern "C" fn(*mut IntelConnector, bool)>,
}

#[repr(C)]
pub struct IntelPanel {
    pub base: *mut c_void,
    pub fixed_edid: *mut c_void,
    pub fixed_modes: ListHead,
    pub backlight: IntelPanelBacklight,
    pub vbt: IntelVbtPanelData,
}

#[repr(C, align(8))]
pub struct IntelHdcp {
    pub shim: *mut IntelHdcpShim,
    pub mutex: Mutex,
    pub value: u64,
    pub check_work: DelayedWork,
    pub prop_work: WorkStruct,
    pub hdcp_encrypted: bool,
    pub hdcp2_supported: bool,
    pub hdcp2_encrypted: bool,
    pub content_type: u8,
    pub is_paired: bool,
    pub is_repeater: bool,
    pub _pad_166: [u8; 2],
    pub seq_num_v: u32,
    pub seq_num_m: u32,
    pub cp_irq_queue: WaitQueueHead,
    pub cp_irq_count: AtomicT,
    pub cp_irq_count_cached: i32,
    pub cpu_transcoder: Transcoder,
    pub stream_transcoder: Transcoder,
    pub force_hdcp14: bool,
    pub _pad_217: [u8; 7],
}

#[repr(C)]
pub struct IntelConnectorDpDscBranchCapsOverallThroughput {
    pub rgb_yuv444: i32,
    pub yuv422_420: i32,
}

#[repr(C)]
pub struct IntelConnectorDpDscBranchCaps {
    pub overall_throughput: IntelConnectorDpDscBranchCapsOverallThroughput,
    pub max_line_width: i32,
}

#[repr(C)]
pub struct IntelConnectorDpPanelReplayCaps {
    pub dpcd: [u8; 7],
    pub support: bool,
    pub su_support: bool,
    pub _pad_9: [u8; 3],
    pub dsc_support: IntelPanelReplayDscSupport,
    pub su_w_granularity: u16,
    pub su_y_granularity: u16,
}

#[repr(C)]
pub struct IntelConnectorDpPsrCaps {
    pub dpcd: [u8; 2],
    pub intel_wa_dpcd: u8,
    pub support: bool,
    pub su_support: bool,
    pub _pad_5: [u8; 1],
    pub su_w_granularity: u16,
    pub su_y_granularity: u16,
    pub sync_latency: u8,
    pub _pad_11: [u8; 1],
}

#[repr(C)]
pub struct IntelConnectorDp {
    pub dsc_decompression_aux: *mut DrmDpAux,
    pub dsc_dpcd: [u8; 16],
    pub fec_capability: u8,
    /// bit-fields dsc_hblank_expansion_quirk:1, dsc_throughput_quirk:1, dsc_decompression_enabled:1
    pub _bitfields_2745: [u8; 1],
    pub _pad_26: [u8; 2],
    pub dsc_branch_caps: IntelConnectorDpDscBranchCaps,
    pub panel_replay_caps: IntelConnectorDpPanelReplayCaps,
    pub psr_caps: IntelConnectorDpPsrCaps,
}

#[repr(C)]
pub struct IntelConnectorMst {
    pub port: *mut DrmDpMstPort,
    pub dp: *mut IntelDp,
}

#[repr(C)]
pub struct IntelConnectorLink {
    pub force_bpp_x16: i32,
}

#[repr(C, align(8))]
pub struct IntelConnector {
    pub base: DrmConnector,
    pub encoder: *mut IntelEncoder,
    pub acpi_device_id: u32,
    pub _pad_2260: [u8; 4],
    pub get_hw_state: Option<unsafe extern "C" fn(*mut IntelConnector) -> bool>,
    pub sync_state: Option<unsafe extern "C" fn(*mut IntelConnector, *mut IntelCrtcState)>,
    pub panel: IntelPanel,
    pub detect_edid: *mut c_void,
    pub hotplug_retries: i32,
    pub polled: u8,
    pub _pad_2709: [u8; 3],
    pub force_joined_pipes: i32,
    pub _pad_2716: [u8; 4],
    pub dp: IntelConnectorDp,
    pub mst: IntelConnectorMst,
    pub link: IntelConnectorLink,
    pub _pad_2812: [u8; 4],
    pub modeset_retry_work: WorkStruct,
    pub hdcp: IntelHdcp,
}

#[repr(C)]
pub struct IntelDigitalConnectorState {
    pub base: DrmConnectorState,
    pub force_audio: HdmiForceAudio,
    pub broadcast_rgb: i32,
}

#[repr(C)]
pub struct Dpll {
    pub n: i32,
    pub m1: i32,
    pub m2: i32,
    pub p1: i32,
    pub p2: i32,
    pub dot: i32,
    pub vco: i32,
    pub m: i32,
    pub p: i32,
}

#[repr(C, align(8))]
pub struct IntelAtomicState {
    pub base: DrmAtomicCommit,
    pub wakeref: *mut c_void,
    pub global_objs: *mut c_void,
    pub num_global_objs: i32,
    pub internal: bool,
    pub dpll_set: bool,
    pub modeset: bool,
    pub _pad_151: [u8; 1],
    pub dpll_state: [IntelDpllState; 9],
    pub _pad_980: [u8; 4],
    pub inherited_dp_tunnels: *mut c_void,
    pub skip_intermediate_wm: bool,
    pub rps_interactive: bool,
    pub _pad_994: [u8; 6],
    pub cleanup_work: WorkStruct,
}

#[repr(C)]
pub struct IntelPlaneStateHw {
    pub crtc: *mut DrmCrtc,
    pub fb: *mut DrmFramebuffer,
    pub alpha: u16,
    pub pixel_blend_mode: u16,
    pub rotation: u32,
    pub color_encoding: DrmColorEncoding,
    pub color_range: DrmColorRange,
    pub scaling_filter: DrmScalingFilter,
    pub _pad_36: [u8; 4],
    pub ctm: *mut DrmPropertyBlob,
    pub degamma_lut: *mut DrmPropertyBlob,
    pub gamma_lut: *mut DrmPropertyBlob,
    pub lut_3d: *mut DrmPropertyBlob,
}

#[repr(C)]
pub struct IntelPlaneState {
    pub uapi: DrmPlaneState,
    pub hw: IntelPlaneStateHw,
    pub ggtt_vma: *mut c_void,
    pub dpt_vma: *mut c_void,
    pub view: IntelFbView,
    pub unpin_work: DrmVblankWork,
    pub fence_id: i8,
    pub decrypt: bool,
    pub force_black: bool,
    pub is_y_plane: bool,
    pub ctl: u32,
    pub color_ctl: u32,
    pub cus_ctl: u32,
    pub surf: u32,
    pub scaler_id: i32,
    pub planar_linked_plane: *mut IntelPlane,
    pub ckey: DrmIntelSpriteColorkey,
    pub psr2_sel_fetch_area: DrmRect,
    pub _pad_556: [u8; 4],
    pub ccval: u64,
    pub no_fbc_reason: *mut c_char,
    pub damage: DrmRect,
}

#[repr(C)]
pub struct IntelInitialPlaneConfig {
    pub fb: *mut DrmFramebuffer,
    pub vma: *mut c_void,
    pub size: i32,
    pub base: u32,
    pub rotation: u8,
    pub _pad_25: [u8; 7],
}

#[repr(C)]
pub struct IntelScaler {
    pub mode: u32,
    pub in_use: bool,
    pub _pad_5: [u8; 3],
    pub hscale: i32,
    pub vscale: i32,
}

#[repr(C)]
pub struct IntelCrtcScalerState {
    pub scalers: [IntelScaler; 2],
    pub scaler_users: u32,
    pub scaler_id: i32,
}

#[repr(C)]
pub struct IntelWmLevel {
    pub enable: bool,
    pub _pad_1: [u8; 3],
    pub pri_val: u32,
    pub spr_val: u32,
    pub cur_val: u32,
    pub fbc_val: u32,
}

#[repr(C)]
pub struct IntelPipeWm {
    pub wm: [IntelWmLevel; 5],
    pub fbc_wm_enabled: bool,
    pub pipe_enabled: bool,
    pub sprites_enabled: bool,
    pub sprites_scaled: bool,
}

#[repr(C)]
pub struct SklWmLevel {
    pub min_ddb_alloc: u16,
    pub min_ddb_alloc_uv: u16,
    pub blocks: u16,
    pub lines: u8,
    pub enable: bool,
    pub ignore_lines: bool,
    pub auto_min_alloc_wm_enable: bool,
    pub can_sagv: bool,
    pub _pad_11: [u8; 1],
}

#[repr(C)]
pub struct SklPlaneWmSagv {
    pub wm0: SklWmLevel,
    pub trans_wm: SklWmLevel,
}

#[repr(C)]
pub struct SklPlaneWm {
    pub wm: [SklWmLevel; 8],
    pub trans_wm: SklWmLevel,
    pub sagv: SklPlaneWmSagv,
}

#[repr(C)]
pub struct SklPipeWm {
    pub planes: [SklPlaneWm; 8],
    pub use_sagv_wm: bool,
    pub _pad_1057: [u8; 1],
}

#[repr(C)]
pub struct VlvWmState {
    pub wm: [G4xPipeWm; 3],
    pub sr: [G4xSrWm; 3],
    pub num_levels: u8,
    pub cxsr: bool,
}

#[repr(C)]
pub struct VlvFifoState {
    pub plane: [u16; 8],
}

#[repr(C)]
pub struct G4xWmState {
    pub wm: G4xPipeWm,
    pub sr: G4xSrWm,
    pub hpll: G4xSrWm,
    pub cxsr: bool,
    pub hpll_en: bool,
    pub fbc_en: bool,
    pub _pad_33: [u8; 1],
}

#[repr(C)]
pub struct IntelCrtcWmStateAnon0x0Ilk {
    pub intermediate: IntelPipeWm,
    pub optimal: IntelPipeWm,
}

#[repr(C)]
pub struct IntelCrtcWmStateAnon0x0Skl {
    pub raw: SklPipeWm,
    pub optimal: SklPipeWm,
    pub ddb: SklDdbEntry,
    pub plane_ddb: [SklDdbEntry; 8],
    pub plane_ddb_y: [SklDdbEntry; 8],
    pub plane_min_ddb: [u16; 8],
    pub plane_interim_ddb: [u16; 8],
}

#[repr(C)]
pub struct IntelCrtcWmStateAnon0x0Vlv {
    pub raw: [G4xPipeWm; 3],
    pub intermediate: VlvWmState,
    pub optimal: VlvWmState,
    pub fifo_state: VlvFifoState,
}

#[repr(C)]
pub struct IntelCrtcWmStateAnon0x0G4x {
    pub raw: [G4xPipeWm; 3],
    pub intermediate: G4xWmState,
    pub optimal: G4xWmState,
}

#[repr(C)]
pub union IntelCrtcWmStateAnon0x0 {
    pub ilk: ManuallyDrop<IntelCrtcWmStateAnon0x0Ilk>,
    pub skl: ManuallyDrop<IntelCrtcWmStateAnon0x0Skl>,
    pub vlv: ManuallyDrop<IntelCrtcWmStateAnon0x0Vlv>,
    pub g4x: ManuallyDrop<IntelCrtcWmStateAnon0x0G4x>,
}

#[repr(C)]
pub struct IntelCrtcWmState {
    pub _anon_0_0: IntelCrtcWmStateAnon0x0,
    pub need_postvbl_update: bool,
    pub _pad_2217: [u8; 3],
}

#[repr(C)]
pub struct IntelLinkMN {
    pub tu: u32,
    pub data_m: u32,
    pub data_n: u32,
    pub link_m: u32,
    pub link_n: u32,
}

#[repr(C)]
pub struct IntelCscMatrix {
    pub coeff: [u16; 9],
    pub preoff: [u16; 3],
    pub postoff: [u16; 3],
}

#[repr(C)]
pub struct ScalerFilterCoeff {
    pub sign: u16,
    pub exp: u16,
    pub mantissa: u16,
}

#[repr(C)]
pub struct IntelCasf {
    pub coeff: [ScalerFilterCoeff; 7],
    pub strength: u8,
    pub win_size: u8,
    pub enable: bool,
    pub _pad_45: [u8; 1],
}

#[repr(C)]
pub struct IntelCrtcStateHw {
    pub active: bool,
    pub enable: bool,
    pub _pad_2: [u8; 6],
    pub degamma_lut: *mut DrmPropertyBlob,
    pub gamma_lut: *mut DrmPropertyBlob,
    pub ctm: *mut DrmPropertyBlob,
    pub mode: DrmDisplayMode,
    pub pipe_mode: DrmDisplayMode,
    pub adjusted_mode: DrmDisplayMode,
    pub background_color: u32,
    pub scaling_filter: DrmScalingFilter,
    pub sharpness_strength: u8,
    pub _pad_401: [u8; 7],
}

#[repr(C)]
pub struct IntelCrtcStateDsiPll {
    pub ctrl: u32,
    pub div: u32,
}

#[repr(C)]
pub struct IntelCrtcStateGmchPfit {
    pub control: u32,
    pub pgm_ratios: u32,
    pub lvds_border_bits: u32,
}

#[repr(C)]
pub struct IntelCrtcStatePchPfit {
    pub casf: IntelCasf,
    pub _pad_46: [u8; 2],
    pub dst: DrmRect,
    pub enabled: bool,
    pub force_thru: bool,
    pub _pad_66: [u8; 2],
}

#[repr(C)]
pub union IntelCrtcStateAnon3940x83 {
    pub csc_mode: u32,
    pub cgm_mode: u32,
}

#[repr(C)]
pub struct IntelCrtcStateInfoframes {
    pub enable: u32,
    pub gcp: u32,
    pub avi: HdmiInfoframe,
    pub spd: HdmiInfoframe,
    pub hdmi: HdmiInfoframe,
    pub drm: HdmiInfoframe,
    pub vsc: DrmDpVscSdp,
    pub as_sdp: DrmDpAsSdp,
}

#[repr(C)]
pub struct IntelCrtcStateDsc {
    pub compression_enabled_on_link: bool,
    pub compression_enable: bool,
    pub _pad_2: [u8; 2],
    pub slice_config: IntelDscSliceConfig,
    pub compressed_bpp_x16: u16,
    pub config: DrmDscConfig,
    pub _pad_154: [u8; 2],
}

#[repr(C)]
pub struct IntelCrtcStateVrrDcBalance {
    pub enable: bool,
    pub _pad_1: [u8; 1],
    pub vmin: u16,
    pub vmax: u16,
    pub guardband: u16,
    pub slope: u16,
    pub max_increase: u16,
    pub max_decrease: u16,
    pub vblank_target: u16,
}

#[repr(C)]
pub struct IntelCrtcStateVrr {
    pub enable: bool,
    pub in_range: bool,
    pub pipeline_full: u8,
    pub _pad_3: [u8; 1],
    pub flipline: u16,
    pub vmin: u16,
    pub vmax: u16,
    pub guardband: u16,
    pub vsync_end: u32,
    pub vsync_start: u32,
    pub dc_balance: IntelCrtcStateVrrDcBalance,
}

#[repr(C)]
pub struct IntelCrtcStateCmrr {
    pub enable: bool,
    pub _pad_1: [u8; 7],
    pub cmrr_n: u64,
    pub cmrr_m: u64,
}

#[repr(C)]
pub struct IntelCrtcStateSplitter {
    pub enable: bool,
    pub link_count: u8,
    pub pixel_overlap: u8,
}

#[repr(C)]
pub struct IntelCrtcStateAlpmState {
    pub io_wake_lines: u8,
    pub fast_wake_lines: u8,
    pub check_entry_lines: u8,
    pub aux_less_wake_lines: u8,
    pub silence_period_sym_clocks: u8,
    pub lfps_half_cycle_num_of_syms: u8,
}

#[repr(C)]
pub struct IntelCrtcState {
    pub uapi: DrmCrtcState,
    pub hw: IntelCrtcStateHw,
    pub pre_csc_lut: *mut DrmPropertyBlob,
    pub post_csc_lut: *mut DrmPropertyBlob,
    pub csc: IntelCscMatrix,
    pub output_csc: IntelCscMatrix,
    pub _pad_828: [u8; 4],
    pub quirks: u64,
    pub fb_bits: u32,
    pub update_pipe: bool,
    pub update_m_n: bool,
    pub update_lrr: bool,
    pub disable_cxsr: bool,
    pub update_wm_pre: bool,
    pub update_wm_post: bool,
    pub fifo_changed: bool,
    pub preload_luts: bool,
    pub inherited: bool,
    pub do_async_flip: bool,
    pub _pad_854: [u8; 2],
    pub pipe_src: DrmRect,
    pub pixel_rate: u32,
    pub has_pch_encoder: bool,
    pub has_infoframe: bool,
    pub _pad_878: [u8; 2],
    pub cpu_transcoder: Transcoder,
    pub limited_color_range: bool,
    pub _pad_885: [u8; 3],
    pub output_types: u32,
    pub has_hdmi_sink: bool,
    pub has_audio: bool,
    pub dither: bool,
    pub dither_force_disable: bool,
    pub clock_set: bool,
    pub sdvo_tv_clock: bool,
    pub bw_constrained: bool,
    pub _pad_899: [u8; 1],
    pub dpll: Dpll,
    pub intel_dpll: *mut IntelDpll,
    pub dpll_hw_state: IntelDpllHwState,
    pub icl_port_dplls: [IclPortDpll; 2],
    pub dsi_pll: IntelCrtcStateDsiPll,
    pub max_link_bpp_x16: i32,
    pub max_pipe_bpp: i32,
    pub pipe_bpp: i32,
    pub min_hblank: i32,
    pub dp_m_n: IntelLinkMN,
    pub dp_m2_n2: IntelLinkMN,
    pub has_drrs: bool,
    pub has_psr: bool,
    pub has_sel_update: bool,
    pub enable_psr2_sel_fetch: bool,
    pub enable_psr2_su_region_et: bool,
    pub req_psr2_sdp_prior_scanline: bool,
    pub has_panel_replay: bool,
    pub link_off_after_as_sdp_when_pr_active: bool,
    pub disable_as_sdp_when_pr_active: bool,
    pub wm_level_disabled: bool,
    pub pkg_c_latency_used: bool,
    pub _pad_1299: [u8; 1],
    pub panel_replay_dsc_support: IntelPanelReplayDscSupport,
    pub dc3co_exitline: u32,
    pub su_y_granularity: u16,
    pub active_non_psr_pipes: u8,
    pub entry_setup_frames: u8,
    pub no_psr_reason: *mut c_char,
    pub port_clock: i32,
    pub pixel_multiplier: u32,
    pub mode_flags: u8,
    pub lane_count: u8,
    pub lane_lat_optim_mask: u8,
    pub min_voltage_level: u8,
    pub gmch_pfit: IntelCrtcStateGmchPfit,
    pub pch_pfit: IntelCrtcStatePchPfit,
    pub fdi_lanes: i32,
    pub fdi_m_n: IntelLinkMN,
    pub ips_enabled: bool,
    pub crc_enabled: bool,
    pub double_wide: bool,
    pub _pad_1439: [u8; 1],
    pub scaler_state: IntelCrtcScalerState,
    pub hsw_workaround_pipe: Pipe,
    pub wm: IntelCrtcWmState,
    pub min_cdclk: i32,
    pub plane_min_cdclk: [i32; 8],
    pub data_rate: [u32; 8],
    pub data_rate_y: [u32; 8],
    pub _pad_3804: [u8; 4],
    pub rel_data_rate: [u64; 8],
    pub rel_data_rate_y: [u64; 8],
    pub gamma_mode: u32,
    pub _anon_3940_83: IntelCrtcStateAnon3940x83,
    pub enabled_planes: u8,
    pub active_planes: u8,
    pub scaled_planes: u8,
    pub nv12_planes: u8,
    pub c8_planes: u8,
    pub update_planes: u8,
    pub async_flip_planes: u8,
    pub framestart_delay: u8,
    pub msa_timing_delay: u8,
    pub _pad_3953: [u8; 3],
    pub infoframes: IntelCrtcStateInfoframes,
    pub eld: [u8; 128],
    pub hdmi_scrambling: bool,
    pub hdmi_high_tmds_clock_ratio: bool,
    pub _pad_4390: [u8; 2],
    pub output_format: IntelOutputFormat,
    pub sink_format: IntelOutputFormat,
    pub gamma_enable: bool,
    pub csc_enable: bool,
    pub wgc_enable: bool,
    pub joiner_pipes: u8,
    pub dsc: IntelCrtcStateDsc,
    pub dp_tunnel_ref: DrmDpTunnelRef,
    pub linetime: u16,
    pub ips_linetime: u16,
    pub enhanced_framing: bool,
    pub fec_enable: bool,
    pub sdp_split_enable: bool,
    pub _pad_4583: [u8; 1],
    pub master_transcoder: Transcoder,
    pub sync_mode_slaves_mask: u8,
    pub _pad_4589: [u8; 3],
    pub mst_master_transcoder: Transcoder,
    pub _pad_4596: [u8; 4],
    pub dsb_color: *mut c_void,
    pub dsb_commit: *mut c_void,
    pub use_dsb: bool,
    pub use_flipq: bool,
    pub _pad_4618: [u8; 2],
    pub psr2_man_track_ctl: u32,
    pub pipe_srcsz_early_tpt: u32,
    pub psr2_su_area: DrmRect,
    pub vrr: IntelCrtcStateVrr,
    pub cmrr: IntelCrtcStateCmrr,
    pub splitter: IntelCrtcStateSplitter,
    pub _pad_4707: [u8; 5],
    pub vblank_work: DrmVblankWork,
    pub has_lobf: bool,
    pub _pad_4793: [u8; 1],
    pub set_context_latency: u16,
    pub alpm_state: IntelCrtcStateAlpmState,
    pub plane_color_changed: bool,
    pub _pad_4803: [u8; 5],
}

#[repr(C)]
pub struct IclPortDpll {
    pub pll: *mut IntelDpll,
    pub hw_state: IntelDpllHwState,
}

#[repr(C)]
pub struct IntelDscSliceConfig {
    pub pipes_per_line: i32,
    pub streams_per_pipe: i32,
    pub slices_per_stream: i32,
}

#[repr(C, align(4))]
pub struct IntelPipeCrc {
    pub lock: Spinlock,
    pub skipped: i32,
    pub source: IntelPipeCrcSource,
}

#[repr(C)]
pub struct IntelFlipq {
    pub start_mmioaddr: u32,
    pub flipq_id: IntelFlipqId,
    pub tail: u8,
    pub _pad_9: [u8; 3],
}

#[repr(C)]
pub union IntelCrtcWmActive {
    pub ilk: ManuallyDrop<IntelPipeWm>,
    pub vlv: ManuallyDrop<VlvWmState>,
    pub g4x: ManuallyDrop<G4xWmState>,
}

#[repr(C)]
pub struct IntelCrtcWm {
    pub active: IntelCrtcWmActive,
}

#[repr(C, align(8))]
pub struct IntelCrtcDrrs {
    pub mutex: Mutex,
    pub work: DelayedWork,
    pub refresh_rate: DrrsRefreshRate,
    pub frontbuffer_bits: u32,
    pub busy_frontbuffer_bits: u32,
    pub cpu_transcoder: Transcoder,
    pub m_n: IntelLinkMN,
    pub m2_n2: IntelLinkMN,
}

#[repr(C)]
pub struct IntelCrtcDcBalance {
    pub flip_count: u64,
}

#[repr(C)]
pub struct IntelCrtcDebug {
    pub start_vbl_count: u32,
    pub _pad_4: [u8; 4],
    pub start_vbl_time: i64,
    pub min_vbl: i32,
    pub max_vbl: i32,
    pub scanline_start: i32,
    pub _pad_28: [u8; 4],
}

#[repr(C, align(8))]
pub struct IntelCrtc {
    pub base: DrmCrtc,
    pub pipe_head: ListHead,
    pub pipe: Pipe,
    pub active: bool,
    pub plane_ids_mask: u8,
    pub mode_flags: u8,
    pub _pad_1671: [u8; 1],
    pub vmax_vblank_start: u16,
    pub _pad_1674: [u8; 6],
    pub enabled_power_domains: IntelDisplayPowerDomainSet,
    pub hw_readout_power_domains: IntelDisplayPowerDomainSet,
    pub overlay: *mut c_void,
    pub config: *mut IntelCrtcState,
    pub flip_done_event: *mut DrmPendingVblankEvent,
    pub dsb_event: *mut DrmPendingVblankEvent,
    pub flipq_event: *mut DrmPendingVblankEvent,
    pub cpu_fifo_underrun_disabled: bool,
    pub pch_fifo_underrun_disabled: bool,
    pub _pad_1754: [u8; 2],
    pub flipq: [IntelFlipq; 5],
    pub wm: IntelCrtcWm,
    pub drrs: IntelCrtcDrrs,
    pub dc_balance: IntelCrtcDcBalance,
    pub scanline_offset: i32,
    pub _pad_2100: [u8; 4],
    pub debug: IntelCrtcDebug,
    pub num_scalers: i32,
    pub _pad_2140: [u8; 4],
    pub vblank_pm_qos: PmQosRequest,
    pub pipe_crc: IntelPipeCrc,
    pub vblank_psr_notify: bool,
    pub _pad_2205: [u8; 3],
}

#[repr(C)]
pub struct IntelPlaneError {
    pub ctl: u32,
    pub surf: u32,
    pub surflive: u32,
}

#[repr(C)]
pub struct IntelPlaneCursor {
    pub base: u32,
    pub cntl: u32,
    pub size: u32,
}

#[repr(C, align(8))]
pub struct IntelPlane {
    pub base: DrmPlane,
    pub i9xx_plane: I9xxPlaneId,
    pub id: PlaneId,
    pub pipe: Pipe,
    pub need_async_flip_toggle_wa: bool,
    pub vtd_guard: u8,
    pub _pad_1366: [u8; 2],
    pub frontbuffer_bit: u32,
    pub cursor: IntelPlaneCursor,
    pub fbc: *mut c_void,
    pub min_width: Option<unsafe extern "C" fn(*mut DrmFramebuffer, i32, u32) -> i32>,
    pub max_width: Option<unsafe extern "C" fn(*mut DrmFramebuffer, i32, u32) -> i32>,
    pub max_height: Option<unsafe extern "C" fn(*mut DrmFramebuffer, i32, u32) -> i32>,
    pub min_alignment: Option<unsafe extern "C" fn(*mut IntelPlane, *mut DrmFramebuffer, i32) -> u32>,
    pub max_stride: Option<unsafe extern "C" fn(*mut IntelPlane, *mut DrmFormatInfo, u64, u32) -> u32>,
    pub can_async_flip: Option<unsafe extern "C" fn(u64) -> bool>,
    pub update_noarm: Option<unsafe extern "C" fn(*mut c_void, *mut IntelPlane, *mut IntelCrtcState, *mut IntelPlaneState)>,
    pub update_arm: Option<unsafe extern "C" fn(*mut c_void, *mut IntelPlane, *mut IntelCrtcState, *mut IntelPlaneState)>,
    pub disable_arm: Option<unsafe extern "C" fn(*mut c_void, *mut IntelPlane, *mut IntelCrtcState)>,
    pub capture_error: Option<unsafe extern "C" fn(*mut IntelCrtc, *mut IntelPlane, *mut IntelPlaneError)>,
    pub get_hw_state: Option<unsafe extern "C" fn(*mut IntelPlane, *mut Pipe) -> bool>,
    pub check_plane: Option<unsafe extern "C" fn(*mut IntelCrtcState, *mut IntelPlaneState) -> i32>,
    pub surf_offset: Option<unsafe extern "C" fn(*mut IntelPlaneState) -> u32>,
    pub min_cdclk: Option<unsafe extern "C" fn(*mut IntelCrtcState, *mut IntelPlaneState) -> i32>,
    pub async_flip: Option<unsafe extern "C" fn(*mut c_void, *mut IntelPlane, *mut IntelCrtcState, *mut IntelPlaneState, bool)>,
    pub enable_flip_done: Option<unsafe extern "C" fn(*mut IntelPlane)>,
    pub disable_flip_done: Option<unsafe extern "C" fn(*mut IntelPlane)>,
    pub disable_tiling: Option<unsafe extern "C" fn(*mut IntelPlane)>,
}

#[repr(C)]
pub struct IntelHdmiDpDualMode {
    pub r#type: DrmDpDualModeType,
    pub max_tmds_clock: i32,
}

#[repr(C)]
pub struct IntelHdmi {
    pub hdmi_reg: I915RegT,
    pub dp_dual_mode: IntelHdmiDpDualMode,
    pub _pad_12: [u8; 4],
    pub attached_connector: *mut IntelConnector,
    pub cec_notifier: *mut c_void,
}

#[repr(C)]
pub struct IntelDpComplianceData {
    pub edid: u64,
    pub video_pattern: u8,
    pub _pad_9: [u8; 1],
    pub hdisplay: u16,
    pub vdisplay: u16,
    pub bpc: u8,
    pub _pad_15: [u8; 1],
    pub phytest: DrmDpPhyTestParams,
    pub _pad_36: [u8; 4],
}

#[repr(C)]
pub struct IntelDpCompliance {
    pub test_type: u64,
    pub test_data: IntelDpComplianceData,
    pub test_active: bool,
    pub _pad_49: [u8; 3],
    pub test_link_rate: i32,
    pub test_lane_count: u8,
    pub _pad_57: [u8; 7],
}

#[repr(C)]
pub struct IntelDpPconFrl {
    pub is_trained: bool,
    pub _pad_1: [u8; 3],
    pub trained_rate_gbps: i32,
}

#[repr(C)]
pub union IntelPpsAnon152x12 {
    pub vlv_pps_pipe: ManuallyDrop<Pipe>,
    pub pps_idx: i32,
}

#[repr(C)]
pub struct IntelPps {
    pub panel_power_up_delay: i32,
    pub panel_power_down_delay: i32,
    pub panel_power_cycle_delay: i32,
    pub backlight_on_delay: i32,
    pub backlight_off_delay: i32,
    pub _pad_20: [u8; 4],
    pub panel_vdd_work: DelayedWork,
    pub want_panel_vdd: bool,
    pub initializing: bool,
    pub _pad_114: [u8; 6],
    pub last_power_on: u64,
    pub last_backlight_off: u64,
    pub panel_power_off_time: i64,
    pub vdd_wakeref: *mut c_void,
    pub _anon_152_12: IntelPpsAnon152x12,
    pub vlv_active_pipe: Pipe,
    pub bxt_pps_reset: bool,
    pub _pad_161: [u8; 1],
    pub pps_delays: IntelPpsDelays,
    pub bios_pps_delays: IntelPpsDelays,
    pub _pad_182: [u8; 2],
}

#[repr(C, align(8))]
pub struct IntelPsr {
    pub lock: Mutex,
    pub debug: u32,
    pub sink_support: bool,
    pub source_support: bool,
    pub enabled: bool,
    pub _pad_31: [u8; 1],
    pub pause_counter: i32,
    pub pipe: Pipe,
    pub transcoder: Transcoder,
    pub active: bool,
    pub _pad_45: [u8; 3],
    pub work: WorkStruct,
    pub busy_frontbuffer_bits: u32,
    pub link_standby: bool,
    pub sel_update_enabled: bool,
    pub psr2_sel_fetch_enabled: bool,
    pub psr2_sel_fetch_cff_enabled: bool,
    pub su_region_et_enabled: bool,
    pub req_psr2_sdp_prior_scanline: bool,
    pub _pad_90: [u8; 6],
    pub last_entry_attempt: i64,
    pub last_exit: i64,
    pub sink_not_reliable: bool,
    pub irq_aux_error: bool,
    pub su_w_granularity: u16,
    pub su_y_granularity: u16,
    pub source_panel_replay_support: bool,
    pub sink_panel_replay_support: bool,
    pub panel_replay_enabled: bool,
    pub _pad_121: [u8; 3],
    pub dc3co_exitline: u32,
    pub dc3co_exit_delay: u32,
    pub _pad_132: [u8; 4],
    pub dc3co_work: DelayedWork,
    pub entry_setup_frames: u8,
    pub io_wake_lines: u8,
    pub fast_wake_lines: u8,
    pub link_ok: bool,
    pub pkg_c_latency_used: bool,
    pub active_non_psr_pipes: u8,
    pub _pad_230: [u8; 2],
    pub no_psr_reason: *mut c_char,
    pub vblank_wakeref: *mut c_void,
}

#[repr(C)]
pub struct IntelDpLink {
    pub active: bool,
    pub _pad_1: [u8; 3],
    pub num_configs: i32,
    pub configs: [IntelDpLinkConfig; 24],
    pub max_lane_count: i32,
    pub max_rate: i32,
    pub mst_probed_lane_count: i32,
    pub mst_probed_rate: i32,
    pub force_lane_count: i32,
    pub force_rate: i32,
    pub retrain_disabled: bool,
    pub _pad_57: [u8; 3],
    pub seq_train_failures: i32,
    pub force_train_failure: i32,
    pub force_retrain: bool,
    pub _pad_69: [u8; 3],
}

#[repr(C, align(8))]
pub struct IntelDpMst {
    pub stream_encoders: [*mut IntelDpMstEncoder; 4],
    pub mgr: DrmDpMstTopologyMgr,
    pub active_streams: i32,
    pub _pad_1204: [u8; 4],
}

#[repr(C)]
pub struct IntelDpDfp {
    pub min_tmds_clock: i32,
    pub max_tmds_clock: i32,
    pub max_dotclock: i32,
    pub pcon_max_frl_bw: i32,
    pub max_bpc: u8,
    pub ycbcr_444_to_420: bool,
    pub ycbcr420_passthrough: bool,
    pub rgb_to_ycbcr: bool,
}

#[repr(C, align(8))]
pub struct IntelDpAlpm {
    pub transcoder: Transcoder,
    pub _pad_4: [u8; 4],
    pub lock: Mutex,
    pub lobf_disable_debug: bool,
    pub sink_alpm_error: bool,
    pub _pad_34: [u8; 6],
}

#[repr(C)]
pub struct IntelDpQuirks {
    pub mask: u64,
}

#[repr(C, align(8))]
pub struct IntelDp {
    pub output_reg: I915RegT,
    pub DP: u32,
    pub link_rate: i32,
    pub lane_count: u8,
    pub sink_count: u8,
    pub downstream_port_changed: bool,
    pub needs_modeset_retry: bool,
    pub use_max_params: bool,
    pub dpcd: [u8; 15],
    pub downstream_ports: [u8; 16],
    pub edp_dpcd: [u8; 5],
    pub lttpr_common_caps: [u8; 8],
    pub lttpr_phy_caps: [[u8; 3]; 8],
    pub pcon_dsc_dpcd: [u8; 13],
    pub _pad_98: [u8; 2],
    pub num_source_rates: i32,
    pub source_rates: *mut i32,
    pub num_sink_rates: i32,
    pub sink_rates: [i32; 8],
    pub use_rate_select: bool,
    pub _pad_149: [u8; 3],
    pub max_sink_lane_count: i32,
    pub num_common_rates: i32,
    pub common_rates: [i32; 8],
    pub max_common_lane_count: i32,
    pub link: IntelDpLink,
    pub reset_link_params: bool,
    pub _pad_269: [u8; 3],
    pub mso_link_count: i32,
    pub mso_pixel_overlap: i32,
    pub desc: DrmDpDesc,
    pub aux: DrmDpAux,
    pub aux_busy_last_status: u32,
    pub train_set: [u8; 4],
    pub pps: IntelPps,
    pub is_mst: bool,
    pub _pad_1809: [u8; 3],
    pub mst_detect: DrmDpMstMode,
    pub attached_connector: *mut IntelConnector,
    pub as_sdp_supported: bool,
    pub as_sdp_v2_supported: bool,
    pub _pad_1826: [u8; 6],
    pub tunnel: *mut c_void,
    /// bit-fields tunnel_suspended:1
    pub _bitfields_1840: [u8; 1],
    pub _pad_1841: [u8; 7],
    pub mst: IntelDpMst,
    pub get_aux_clock_divider: Option<unsafe extern "C" fn(*mut IntelDp, i32) -> u32>,
    pub get_aux_send_ctl: Option<unsafe extern "C" fn(*mut IntelDp, i32, u32) -> u32>,
    pub aux_ch_ctl_reg: Option<unsafe extern "C" fn(*mut IntelDp) -> I915RegT>,
    pub aux_ch_data_reg: Option<unsafe extern "C" fn(*mut IntelDp, i32) -> I915RegT>,
    pub prepare_link_retrain: Option<unsafe extern "C" fn(*mut IntelDp, *mut IntelCrtcState)>,
    pub set_link_train: Option<unsafe extern "C" fn(*mut IntelDp, *mut IntelCrtcState, u8)>,
    pub set_idle_link_train: Option<unsafe extern "C" fn(*mut IntelDp, *mut IntelCrtcState)>,
    pub preemph_max: Option<unsafe extern "C" fn(*mut IntelDp) -> u8>,
    pub voltage_max: Option<unsafe extern "C" fn(*mut IntelDp, *mut IntelCrtcState) -> u8>,
    pub compliance: IntelDpCompliance,
    pub dfp: IntelDpDfp,
    pub _pad_3212: [u8; 4],
    pub pm_qos: PmQosRequest,
    pub force_dsc_en: bool,
    pub _pad_3265: [u8; 3],
    pub force_dsc_output_format: i32,
    pub force_dsc_fractional_bpp_en: bool,
    pub _pad_3273: [u8; 3],
    pub force_dsc_bpc: i32,
    pub hobl_failed: bool,
    pub hobl_active: bool,
    pub _pad_3282: [u8; 2],
    pub frl: IntelDpPconFrl,
    pub _pad_3292: [u8; 4],
    pub psr: IntelPsr,
    pub last_oui_write: u64,
    pub oui_valid: bool,
    pub colorimetry_support: bool,
    pub _pad_3554: [u8; 6],
    pub alpm: IntelDpAlpm,
    pub alpm_dpcd: u8,
    pub _pad_3601: [u8; 7],
    pub quirks: IntelDpQuirks,
}

#[repr(C)]
pub struct IntelDpLinkConfig {
    /// bit-fields link_rate_idx:6, lane_count_exp:2
    pub _bitfields_0: [u8; 1],
}

#[repr(C)]
pub struct IntelLspcon {
    pub active: bool,
    pub hdr_supported: bool,
    pub _pad_2: [u8; 2],
    pub mode: DrmLspconMode,
    pub vendor: LspconVendor,
}

#[repr(C, align(8))]
pub struct IntelDigitalPortHdcp {
    pub mutex: Mutex,
    pub num_streams: u32,
    pub auth_status: bool,
    pub _pad_29: [u8; 3],
    pub port_data: HdcpPortData,
    pub mst_type1_capable: bool,
    pub _pad_57: [u8; 7],
}

#[repr(C, align(8))]
pub struct IntelDigitalPort {
    pub base: IntelEncoder,
    pub dp: IntelDp,
    pub hdmi: IntelHdmi,
    pub lspcon: IntelLspcon,
    pub _pad_4164: [u8; 4],
    pub hpd_pulse: Option<unsafe extern "C" fn(*mut IntelDigitalPort, bool) -> Irqreturn>,
    pub lane_reversal: bool,
    pub ddi_a_4_lanes: bool,
    pub release_cl2_override: bool,
    pub dedicated_external: bool,
    pub max_lanes: u8,
    pub _pad_4181: [u8; 3],
    pub aux_ch: AuxCh,
    pub ddi_io_power_domain: IntelDisplayPowerDomain,
    pub ddi_io_wakeref: *mut c_void,
    pub aux_wakeref: *mut c_void,
    pub tc: *mut c_void,
    pub hdcp: IntelDigitalPortHdcp,
    pub write_infoframe: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, u32, *mut c_void, isize)>,
    pub read_infoframe: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState, u32, *mut c_void, isize)>,
    pub set_infoframes: Option<unsafe extern "C" fn(*mut IntelEncoder, bool, *mut IntelCrtcState, *mut DrmConnectorState)>,
    pub infoframes_enabled: Option<unsafe extern "C" fn(*mut IntelEncoder, *mut IntelCrtcState) -> u32>,
    pub connected: Option<unsafe extern "C" fn(*mut IntelEncoder) -> bool>,
    pub lock: Option<unsafe extern "C" fn(*mut IntelDigitalPort)>,
    pub unlock: Option<unsafe extern "C" fn(*mut IntelDigitalPort)>,
}

#[repr(C, align(8))]
pub struct IntelDpMstEncoder {
    pub base: IntelEncoder,
    pub pipe: Pipe,
    pub _pad_508: [u8; 4],
    pub primary: *mut IntelDigitalPort,
    pub connector: *mut IntelConnector,
}

#[repr(C, align(8))]
pub struct IntelColorop {
    pub base: DrmColorop,
    pub id: IntelColorBlock,
    pub _pad_1220: [u8; 4],
}

pub type IntelOutputType = u32;
pub const INTEL_OUTPUT_UNUSED: IntelOutputType = 0;
pub const INTEL_OUTPUT_ANALOG: IntelOutputType = 1;
pub const INTEL_OUTPUT_DVO: IntelOutputType = 2;
pub const INTEL_OUTPUT_SDVO: IntelOutputType = 3;
pub const INTEL_OUTPUT_LVDS: IntelOutputType = 4;
pub const INTEL_OUTPUT_TVOUT: IntelOutputType = 5;
pub const INTEL_OUTPUT_HDMI: IntelOutputType = 6;
pub const INTEL_OUTPUT_DP: IntelOutputType = 7;
pub const INTEL_OUTPUT_EDP: IntelOutputType = 8;
pub const INTEL_OUTPUT_DSI: IntelOutputType = 9;
pub const INTEL_OUTPUT_DDI: IntelOutputType = 10;
pub const INTEL_OUTPUT_DP_MST: IntelOutputType = 11;

pub type IntelHotplugState = u32;
pub const INTEL_HOTPLUG_UNCHANGED: IntelHotplugState = 0;
pub const INTEL_HOTPLUG_CHANGED: IntelHotplugState = 1;
pub const INTEL_HOTPLUG_RETRY: IntelHotplugState = 2;

pub type DrrsType = u32;
pub const DRRS_TYPE_NONE: DrrsType = 0;
pub const DRRS_TYPE_STATIC: DrrsType = 1;
pub const DRRS_TYPE_SEAMLESS: DrrsType = 2;

pub type IntelPanelReplayDscSupport = u32;
pub const INTEL_DP_PANEL_REPLAY_DSC_NOT_SUPPORTED: IntelPanelReplayDscSupport = 0;
pub const INTEL_DP_PANEL_REPLAY_DSC_FULL_FRAME_ONLY: IntelPanelReplayDscSupport = 1;
pub const INTEL_DP_PANEL_REPLAY_DSC_SELECTIVE_UPDATE: IntelPanelReplayDscSupport = 2;

pub type HdmiForceAudio = i32;
pub const HDMI_AUDIO_OFF_DVI: HdmiForceAudio = -2;
pub const HDMI_AUDIO_OFF: HdmiForceAudio = -1;
pub const HDMI_AUDIO_AUTO: HdmiForceAudio = 0;
pub const HDMI_AUDIO_ON: HdmiForceAudio = 1;

pub type IntelOutputFormat = u32;
pub const INTEL_OUTPUT_FORMAT_RGB: IntelOutputFormat = 0;
pub const INTEL_OUTPUT_FORMAT_YCBCR420: IntelOutputFormat = 1;
pub const INTEL_OUTPUT_FORMAT_YCBCR444: IntelOutputFormat = 2;

pub type IntelPipeCrcSource = u32;
pub const INTEL_PIPE_CRC_SOURCE_NONE: IntelPipeCrcSource = 0;
pub const INTEL_PIPE_CRC_SOURCE_PLANE1: IntelPipeCrcSource = 1;
pub const INTEL_PIPE_CRC_SOURCE_PLANE2: IntelPipeCrcSource = 2;
pub const INTEL_PIPE_CRC_SOURCE_PLANE3: IntelPipeCrcSource = 3;
pub const INTEL_PIPE_CRC_SOURCE_PLANE4: IntelPipeCrcSource = 4;
pub const INTEL_PIPE_CRC_SOURCE_PLANE5: IntelPipeCrcSource = 5;
pub const INTEL_PIPE_CRC_SOURCE_PLANE6: IntelPipeCrcSource = 6;
pub const INTEL_PIPE_CRC_SOURCE_PLANE7: IntelPipeCrcSource = 7;
pub const INTEL_PIPE_CRC_SOURCE_PIPE: IntelPipeCrcSource = 8;
pub const INTEL_PIPE_CRC_SOURCE_TV: IntelPipeCrcSource = 9;
pub const INTEL_PIPE_CRC_SOURCE_DP_B: IntelPipeCrcSource = 10;
pub const INTEL_PIPE_CRC_SOURCE_DP_C: IntelPipeCrcSource = 11;
pub const INTEL_PIPE_CRC_SOURCE_DP_D: IntelPipeCrcSource = 12;
pub const INTEL_PIPE_CRC_SOURCE_AUTO: IntelPipeCrcSource = 13;
pub const INTEL_PIPE_CRC_SOURCE_MAX: IntelPipeCrcSource = 14;

pub type IntelFlipqId = u32;
pub const INTEL_FLIPQ_PLANE_1: IntelFlipqId = 0;
pub const INTEL_FLIPQ_PLANE_2: IntelFlipqId = 1;
pub const INTEL_FLIPQ_PLANE_3: IntelFlipqId = 2;
pub const INTEL_FLIPQ_GENERAL: IntelFlipqId = 3;
pub const INTEL_FLIPQ_FAST: IntelFlipqId = 4;
pub const MAX_INTEL_FLIPQ: IntelFlipqId = 5;

pub type DrrsRefreshRate = u32;
pub const DRRS_REFRESH_RATE_HIGH: DrrsRefreshRate = 0;
pub const DRRS_REFRESH_RATE_LOW: DrrsRefreshRate = 1;

pub type LspconVendor = u32;
pub const LSPCON_VENDOR_MCA: LspconVendor = 0;
pub const LSPCON_VENDOR_PARADE: LspconVendor = 1;

const _: () = assert!(size_of::<IntelFbView>() == 136);
const _: () = assert!(core::mem::offset_of!(IntelFbView, gtt) == 0);
const _: () = assert!(size_of::<I915GttView>() == 56);
const _: () = assert!(core::mem::offset_of!(IntelFbView, color_plane) == 56);
const _: () = assert!(size_of::<[I915ColorPlaneView; 4]>() == 80);
const _: () = assert!(size_of::<I915ColorPlaneView>() == 20);
const _: () = assert!(core::mem::offset_of!(I915ColorPlaneView, offset) == 0);
const _: () = assert!(core::mem::offset_of!(I915ColorPlaneView, x) == 4);
const _: () = assert!(core::mem::offset_of!(I915ColorPlaneView, y) == 8);
const _: () = assert!(core::mem::offset_of!(I915ColorPlaneView, mapping_stride) == 12);
const _: () = assert!(core::mem::offset_of!(I915ColorPlaneView, scanout_stride) == 16);
const _: () = assert!(size_of::<IntelFramebufferAnon336x3>() == 136);
const _: () = assert!(size_of::<IntelFramebuffer>() == 504);
const _: () = assert!(align_of::<IntelFramebuffer>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, base) == 0);
const _: () = assert!(size_of::<DrmFramebuffer>() == 192);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, frontbuffer) == 192);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, normal_view) == 200);
const _: () = assert!(size_of::<IntelFbView>() == 136);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, _anon_336_3) == 336);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, dpt) == 472);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, min_alignment) == 480);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, vtd_guard) == 484);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, panic_tiling) == 488);
const _: () = assert!(core::mem::offset_of!(IntelFramebuffer, panic) == 496);
const _: () = assert!(size_of::<IntelEncoder>() == 504);
const _: () = assert!(align_of::<IntelEncoder>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, base) == 0);
const _: () = assert!(size_of::<DrmEncoder>() == 152);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, r#type) == 152);
const _: () = assert!(size_of::<IntelOutputType>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, port) == 156);
const _: () = assert!(size_of::<Port>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, cloneable) == 160);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, pipe_mask) == 162);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, link_check_work) == 168);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, link_check) == 256);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, hotplug) == 264);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, compute_output_type) == 272);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, compute_config) == 280);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, compute_config_late) == 288);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, pre_pll_enable) == 296);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, pre_enable) == 304);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, enable) == 312);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, disable) == 320);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, post_disable) == 328);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, post_pll_disable) == 336);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, update_pipe) == 344);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, audio_enable) == 352);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, audio_disable) == 360);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, get_hw_state) == 368);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, get_config) == 376);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, sync_state) == 384);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, initial_fastset_check) == 392);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, get_power_domains) == 400);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, suspend) == 408);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, suspend_complete) == 416);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, shutdown) == 424);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, shutdown_complete) == 432);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, enable_clock) == 440);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, disable_clock) == 448);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, is_clock_enabled) == 456);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, port_pll_type) == 464);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, get_buf_trans) == 472);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, set_signal_levels) == 480);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, hpd_pin) == 488);
const _: () = assert!(size_of::<HpdPin>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, power_domain) == 492);
const _: () = assert!(size_of::<IntelDisplayPowerDomain>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelEncoder, devdata) == 496);
const _: () = assert!(size_of::<IntelPanelBlFuncs>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, setup) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, get) == 8);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, set) == 16);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, disable) == 24);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, enable) == 32);
const _: () = assert!(core::mem::offset_of!(IntelPanelBlFuncs, hz_to_pwm) == 40);
const _: () = assert!(size_of::<IntelPpsDelays>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelPpsDelays, power_up) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPpsDelays, backlight_on) == 2);
const _: () = assert!(core::mem::offset_of!(IntelPpsDelays, backlight_off) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPpsDelays, power_down) == 6);
const _: () = assert!(core::mem::offset_of!(IntelPpsDelays, power_cycle) == 8);
const _: () = assert!(size_of::<IntelVbtPanelDataEdp>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, max_link_rate) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, rate) == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, lanes) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, preemphasis) == 12);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, vswing) == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, bpp) == 20);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, pps) == 24);
const _: () = assert!(size_of::<IntelPpsDelays>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, drrs_msa_timing_delay) == 34);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, low_vswing) == 35);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, hobl) == 36);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, dsc_disable) == 37);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataEdp, pipe_joiner_enable) == 38);
const _: () = assert!(size_of::<IntelVbtPanelDataPsr>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, full_link) == 1);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, require_aux_wakeup) == 2);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, idle_frames) == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, tp1_wakeup_time_us) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, tp2_tp3_wakeup_time_us) == 12);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataPsr, psr2_tp2_tp3_wakeup_time_us) == 16);
const _: () = assert!(size_of::<IntelVbtPanelDataBacklight>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, pwm_freq_hz) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, brightness_precision_bits) == 2);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, hdr_dpcd_refresh_timeout) == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, present) == 6);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, active_low_pwm) == 7);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, min_brightness) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, controller) == 9);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataBacklight, r#type) == 12);
const _: () = assert!(size_of::<IntelBacklightType>() == 4);
const _: () = assert!(size_of::<IntelVbtPanelDataDsi>() == 160);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, panel_id) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, config) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, pps) == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, bl_ports) == 24);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, cabc_ports) == 26);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, seq_version) == 28);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, size) == 32);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, data) == 40);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, sequence) == 48);
const _: () = assert!(size_of::<[*mut u8; 12]>() == 96);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, deassert_seq) == 144);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelDataDsi, orientation) == 152);
const _: () = assert!(size_of::<DrmPanelOrientation>() == 4);
const _: () = assert!(size_of::<IntelVbtPanelData>() == 272);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, lfp_vbt_mode) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, sdvo_lvds_vbt_mode) == 8);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, panel_type) == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, _bitfields_20) == 20);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, bios_lvds_val) == 24);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, vrr) == 28);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, seamless_drrs_min_refresh_rate) == 29);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, drrs_type) == 32);
const _: () = assert!(size_of::<DrrsType>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, edp) == 36);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, psr) == 76);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, backlight) == 96);
const _: () = assert!(core::mem::offset_of!(IntelVbtPanelData, dsi) == 112);
const _: () = assert!(size_of::<IntelPanelBacklightEdpVesa>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpVesa, info) == 0);
const _: () = assert!(size_of::<DrmEdpBacklightInfo>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpVesa, luminance_control_support) == 12);
const _: () = assert!(size_of::<IntelPanelBacklightEdpIntelCap>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, sdr_uses_aux) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, supports_2084_decode) == 1);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, supports_2020_gamut) == 2);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, supports_segmented_backlight) == 3);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, supports_sdp_colorimetry) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklightEdpIntelCap, supports_tone_mapping) == 5);
const _: () = assert!(size_of::<IntelPanelBacklightEdp>() == 16);
const _: () = assert!(size_of::<IntelPanelBacklight>() == 112);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, present) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, level) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, min) == 8);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, max) == 12);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, enabled) == 16);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, combination_mode) == 17);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, active_low_pwm) == 18);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, alternate_pwm_increment) == 19);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm_level_min) == 20);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm_level_max) == 24);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm_enabled) == 28);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, util_pin_active_low) == 29);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, controller) == 30);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm) == 32);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm_state) == 40);
const _: () = assert!(size_of::<PwmState>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, edp) == 64);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, device) == 80);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, funcs) == 88);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, pwm_funcs) == 96);
const _: () = assert!(core::mem::offset_of!(IntelPanelBacklight, power) == 104);
const _: () = assert!(size_of::<IntelPanel>() == 416);
const _: () = assert!(core::mem::offset_of!(IntelPanel, base) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPanel, fixed_edid) == 8);
const _: () = assert!(core::mem::offset_of!(IntelPanel, fixed_modes) == 16);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelPanel, backlight) == 32);
const _: () = assert!(core::mem::offset_of!(IntelPanel, vbt) == 144);
const _: () = assert!(size_of::<IntelVbtPanelData>() == 272);
const _: () = assert!(size_of::<IntelHdcp>() == 224);
const _: () = assert!(align_of::<IntelHdcp>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, shim) == 0);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, mutex) == 8);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, value) == 32);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, check_work) == 40);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, prop_work) == 128);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, hdcp_encrypted) == 160);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, hdcp2_supported) == 161);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, hdcp2_encrypted) == 162);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, content_type) == 163);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, is_paired) == 164);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, is_repeater) == 165);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, seq_num_v) == 168);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, seq_num_m) == 172);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, cp_irq_queue) == 176);
const _: () = assert!(size_of::<WaitQueueHead>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, cp_irq_count) == 200);
const _: () = assert!(size_of::<AtomicT>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, cp_irq_count_cached) == 204);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, cpu_transcoder) == 208);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, stream_transcoder) == 212);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdcp, force_hdcp14) == 216);
const _: () = assert!(size_of::<IntelConnectorDpDscBranchCapsOverallThroughput>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpDscBranchCapsOverallThroughput, rgb_yuv444) == 0);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpDscBranchCapsOverallThroughput, yuv422_420) == 4);
const _: () = assert!(size_of::<IntelConnectorDpDscBranchCaps>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpDscBranchCaps, overall_throughput) == 0);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpDscBranchCaps, max_line_width) == 8);
const _: () = assert!(size_of::<IntelConnectorDpPanelReplayCaps>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, dpcd) == 0);
const _: () = assert!(size_of::<[u8; 7]>() == 7);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, support) == 7);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, su_support) == 8);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, dsc_support) == 12);
const _: () = assert!(size_of::<IntelPanelReplayDscSupport>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, su_w_granularity) == 16);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPanelReplayCaps, su_y_granularity) == 18);
const _: () = assert!(size_of::<IntelConnectorDpPsrCaps>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, dpcd) == 0);
const _: () = assert!(size_of::<[u8; 2]>() == 2);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, intel_wa_dpcd) == 2);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, support) == 3);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, su_support) == 4);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, su_w_granularity) == 6);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, su_y_granularity) == 8);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDpPsrCaps, sync_latency) == 10);
const _: () = assert!(size_of::<IntelConnectorDp>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, dsc_decompression_aux) == 0);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, dsc_dpcd) == 8);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, fec_capability) == 24);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, _bitfields_2745) == 25);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, dsc_branch_caps) == 28);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, panel_replay_caps) == 40);
const _: () = assert!(core::mem::offset_of!(IntelConnectorDp, psr_caps) == 60);
const _: () = assert!(size_of::<IntelConnectorMst>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelConnectorMst, port) == 0);
const _: () = assert!(core::mem::offset_of!(IntelConnectorMst, dp) == 8);
const _: () = assert!(size_of::<IntelConnectorLink>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelConnectorLink, force_bpp_x16) == 0);
const _: () = assert!(size_of::<IntelConnector>() == 3072);
const _: () = assert!(align_of::<IntelConnector>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelConnector, base) == 0);
const _: () = assert!(size_of::<DrmConnector>() == 2248);
const _: () = assert!(core::mem::offset_of!(IntelConnector, encoder) == 2248);
const _: () = assert!(core::mem::offset_of!(IntelConnector, acpi_device_id) == 2256);
const _: () = assert!(core::mem::offset_of!(IntelConnector, get_hw_state) == 2264);
const _: () = assert!(core::mem::offset_of!(IntelConnector, sync_state) == 2272);
const _: () = assert!(core::mem::offset_of!(IntelConnector, panel) == 2280);
const _: () = assert!(size_of::<IntelPanel>() == 416);
const _: () = assert!(core::mem::offset_of!(IntelConnector, detect_edid) == 2696);
const _: () = assert!(core::mem::offset_of!(IntelConnector, hotplug_retries) == 2704);
const _: () = assert!(core::mem::offset_of!(IntelConnector, polled) == 2708);
const _: () = assert!(core::mem::offset_of!(IntelConnector, force_joined_pipes) == 2712);
const _: () = assert!(core::mem::offset_of!(IntelConnector, dp) == 2720);
const _: () = assert!(core::mem::offset_of!(IntelConnector, mst) == 2792);
const _: () = assert!(core::mem::offset_of!(IntelConnector, link) == 2808);
const _: () = assert!(core::mem::offset_of!(IntelConnector, modeset_retry_work) == 2816);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelConnector, hdcp) == 2848);
const _: () = assert!(size_of::<IntelHdcp>() == 224);
const _: () = assert!(size_of::<IntelDigitalConnectorState>() == 448);
const _: () = assert!(core::mem::offset_of!(IntelDigitalConnectorState, base) == 0);
const _: () = assert!(size_of::<DrmConnectorState>() == 440);
const _: () = assert!(core::mem::offset_of!(IntelDigitalConnectorState, force_audio) == 440);
const _: () = assert!(size_of::<HdmiForceAudio>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDigitalConnectorState, broadcast_rgb) == 444);
const _: () = assert!(size_of::<Dpll>() == 36);
const _: () = assert!(core::mem::offset_of!(Dpll, n) == 0);
const _: () = assert!(core::mem::offset_of!(Dpll, m1) == 4);
const _: () = assert!(core::mem::offset_of!(Dpll, m2) == 8);
const _: () = assert!(core::mem::offset_of!(Dpll, p1) == 12);
const _: () = assert!(core::mem::offset_of!(Dpll, p2) == 16);
const _: () = assert!(core::mem::offset_of!(Dpll, dot) == 20);
const _: () = assert!(core::mem::offset_of!(Dpll, vco) == 24);
const _: () = assert!(core::mem::offset_of!(Dpll, m) == 28);
const _: () = assert!(core::mem::offset_of!(Dpll, p) == 32);
const _: () = assert!(size_of::<IntelAtomicState>() == 1032);
const _: () = assert!(align_of::<IntelAtomicState>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, base) == 0);
const _: () = assert!(size_of::<DrmAtomicCommit>() == 128);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, wakeref) == 128);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, global_objs) == 136);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, num_global_objs) == 144);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, internal) == 148);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, dpll_set) == 149);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, modeset) == 150);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, dpll_state) == 152);
const _: () = assert!(size_of::<[IntelDpllState; 9]>() == 828);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, inherited_dp_tunnels) == 984);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, skip_intermediate_wm) == 992);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, rps_interactive) == 993);
const _: () = assert!(core::mem::offset_of!(IntelAtomicState, cleanup_work) == 1000);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(size_of::<IntelPlaneStateHw>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, crtc) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, fb) == 8);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, alpha) == 16);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, pixel_blend_mode) == 18);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, rotation) == 20);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, color_encoding) == 24);
const _: () = assert!(size_of::<DrmColorEncoding>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, color_range) == 28);
const _: () = assert!(size_of::<DrmColorRange>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, scaling_filter) == 32);
const _: () = assert!(size_of::<DrmScalingFilter>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, ctm) == 40);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, degamma_lut) == 48);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, gamma_lut) == 56);
const _: () = assert!(core::mem::offset_of!(IntelPlaneStateHw, lut_3d) == 64);
const _: () = assert!(size_of::<IntelPlaneState>() == 592);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, uapi) == 0);
const _: () = assert!(size_of::<DrmPlaneState>() == 184);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, hw) == 184);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, ggtt_vma) == 256);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, dpt_vma) == 264);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, view) == 272);
const _: () = assert!(size_of::<IntelFbView>() == 136);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, unpin_work) == 408);
const _: () = assert!(size_of::<DrmVblankWork>() == 80);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, fence_id) == 488);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, decrypt) == 489);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, force_black) == 490);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, is_y_plane) == 491);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, ctl) == 492);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, color_ctl) == 496);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, cus_ctl) == 500);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, surf) == 504);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, scaler_id) == 508);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, planar_linked_plane) == 512);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, ckey) == 520);
const _: () = assert!(size_of::<DrmIntelSpriteColorkey>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, psr2_sel_fetch_area) == 540);
const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, ccval) == 560);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, no_fbc_reason) == 568);
const _: () = assert!(core::mem::offset_of!(IntelPlaneState, damage) == 576);
const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(size_of::<IntelInitialPlaneConfig>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelInitialPlaneConfig, fb) == 0);
const _: () = assert!(core::mem::offset_of!(IntelInitialPlaneConfig, vma) == 8);
const _: () = assert!(core::mem::offset_of!(IntelInitialPlaneConfig, size) == 16);
const _: () = assert!(core::mem::offset_of!(IntelInitialPlaneConfig, base) == 20);
const _: () = assert!(core::mem::offset_of!(IntelInitialPlaneConfig, rotation) == 24);
const _: () = assert!(size_of::<IntelScaler>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelScaler, mode) == 0);
const _: () = assert!(core::mem::offset_of!(IntelScaler, in_use) == 4);
const _: () = assert!(core::mem::offset_of!(IntelScaler, hscale) == 8);
const _: () = assert!(core::mem::offset_of!(IntelScaler, vscale) == 12);
const _: () = assert!(size_of::<IntelCrtcScalerState>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelCrtcScalerState, scalers) == 0);
const _: () = assert!(size_of::<[IntelScaler; 2]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcScalerState, scaler_users) == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcScalerState, scaler_id) == 36);
const _: () = assert!(size_of::<IntelWmLevel>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelWmLevel, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelWmLevel, pri_val) == 4);
const _: () = assert!(core::mem::offset_of!(IntelWmLevel, spr_val) == 8);
const _: () = assert!(core::mem::offset_of!(IntelWmLevel, cur_val) == 12);
const _: () = assert!(core::mem::offset_of!(IntelWmLevel, fbc_val) == 16);
const _: () = assert!(size_of::<IntelPipeWm>() == 104);
const _: () = assert!(core::mem::offset_of!(IntelPipeWm, wm) == 0);
const _: () = assert!(size_of::<[IntelWmLevel; 5]>() == 100);
const _: () = assert!(core::mem::offset_of!(IntelPipeWm, fbc_wm_enabled) == 100);
const _: () = assert!(core::mem::offset_of!(IntelPipeWm, pipe_enabled) == 101);
const _: () = assert!(core::mem::offset_of!(IntelPipeWm, sprites_enabled) == 102);
const _: () = assert!(core::mem::offset_of!(IntelPipeWm, sprites_scaled) == 103);
const _: () = assert!(size_of::<SklWmLevel>() == 12);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, min_ddb_alloc) == 0);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, min_ddb_alloc_uv) == 2);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, blocks) == 4);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, lines) == 6);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, enable) == 7);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, ignore_lines) == 8);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, auto_min_alloc_wm_enable) == 9);
const _: () = assert!(core::mem::offset_of!(SklWmLevel, can_sagv) == 10);
const _: () = assert!(size_of::<SklPlaneWmSagv>() == 24);
const _: () = assert!(core::mem::offset_of!(SklPlaneWmSagv, wm0) == 0);
const _: () = assert!(size_of::<SklWmLevel>() == 12);
const _: () = assert!(core::mem::offset_of!(SklPlaneWmSagv, trans_wm) == 12);
const _: () = assert!(size_of::<SklWmLevel>() == 12);
const _: () = assert!(size_of::<SklPlaneWm>() == 132);
const _: () = assert!(core::mem::offset_of!(SklPlaneWm, wm) == 0);
const _: () = assert!(size_of::<[SklWmLevel; 8]>() == 96);
const _: () = assert!(core::mem::offset_of!(SklPlaneWm, trans_wm) == 96);
const _: () = assert!(size_of::<SklWmLevel>() == 12);
const _: () = assert!(core::mem::offset_of!(SklPlaneWm, sagv) == 108);
const _: () = assert!(size_of::<SklPipeWm>() == 1058);
const _: () = assert!(core::mem::offset_of!(SklPipeWm, planes) == 0);
const _: () = assert!(size_of::<[SklPlaneWm; 8]>() == 1056);
const _: () = assert!(core::mem::offset_of!(SklPipeWm, use_sagv_wm) == 1056);
const _: () = assert!(size_of::<VlvWmState>() == 74);
const _: () = assert!(core::mem::offset_of!(VlvWmState, wm) == 0);
const _: () = assert!(size_of::<[G4xPipeWm; 3]>() == 54);
const _: () = assert!(core::mem::offset_of!(VlvWmState, sr) == 54);
const _: () = assert!(size_of::<[G4xSrWm; 3]>() == 18);
const _: () = assert!(core::mem::offset_of!(VlvWmState, num_levels) == 72);
const _: () = assert!(core::mem::offset_of!(VlvWmState, cxsr) == 73);
const _: () = assert!(size_of::<VlvFifoState>() == 16);
const _: () = assert!(core::mem::offset_of!(VlvFifoState, plane) == 0);
const _: () = assert!(size_of::<[u16; 8]>() == 16);
const _: () = assert!(size_of::<G4xWmState>() == 34);
const _: () = assert!(core::mem::offset_of!(G4xWmState, wm) == 0);
const _: () = assert!(size_of::<G4xPipeWm>() == 18);
const _: () = assert!(core::mem::offset_of!(G4xWmState, sr) == 18);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(G4xWmState, hpll) == 24);
const _: () = assert!(size_of::<G4xSrWm>() == 6);
const _: () = assert!(core::mem::offset_of!(G4xWmState, cxsr) == 30);
const _: () = assert!(core::mem::offset_of!(G4xWmState, hpll_en) == 31);
const _: () = assert!(core::mem::offset_of!(G4xWmState, fbc_en) == 32);
const _: () = assert!(size_of::<IntelCrtcWmStateAnon0x0Ilk>() == 208);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Ilk, intermediate) == 0);
const _: () = assert!(size_of::<IntelPipeWm>() == 104);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Ilk, optimal) == 104);
const _: () = assert!(size_of::<IntelPipeWm>() == 104);
const _: () = assert!(size_of::<IntelCrtcWmStateAnon0x0Skl>() == 2216);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, raw) == 0);
const _: () = assert!(size_of::<SklPipeWm>() == 1058);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, optimal) == 1058);
const _: () = assert!(size_of::<SklPipeWm>() == 1058);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, ddb) == 2116);
const _: () = assert!(size_of::<SklDdbEntry>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, plane_ddb) == 2120);
const _: () = assert!(size_of::<[SklDdbEntry; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, plane_ddb_y) == 2152);
const _: () = assert!(size_of::<[SklDdbEntry; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, plane_min_ddb) == 2184);
const _: () = assert!(size_of::<[u16; 8]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Skl, plane_interim_ddb) == 2200);
const _: () = assert!(size_of::<[u16; 8]>() == 16);
const _: () = assert!(size_of::<IntelCrtcWmStateAnon0x0Vlv>() == 218);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Vlv, raw) == 0);
const _: () = assert!(size_of::<[G4xPipeWm; 3]>() == 54);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Vlv, intermediate) == 54);
const _: () = assert!(size_of::<VlvWmState>() == 74);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Vlv, optimal) == 128);
const _: () = assert!(size_of::<VlvWmState>() == 74);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0Vlv, fifo_state) == 202);
const _: () = assert!(size_of::<VlvFifoState>() == 16);
const _: () = assert!(size_of::<IntelCrtcWmStateAnon0x0G4x>() == 122);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0G4x, raw) == 0);
const _: () = assert!(size_of::<[G4xPipeWm; 3]>() == 54);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0G4x, intermediate) == 54);
const _: () = assert!(size_of::<G4xWmState>() == 34);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmStateAnon0x0G4x, optimal) == 88);
const _: () = assert!(size_of::<G4xWmState>() == 34);
const _: () = assert!(size_of::<IntelCrtcWmStateAnon0x0>() == 2216);
const _: () = assert!(size_of::<IntelCrtcWmState>() == 2220);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmState, _anon_0_0) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWmState, need_postvbl_update) == 2216);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelLinkMN, tu) == 0);
const _: () = assert!(core::mem::offset_of!(IntelLinkMN, data_m) == 4);
const _: () = assert!(core::mem::offset_of!(IntelLinkMN, data_n) == 8);
const _: () = assert!(core::mem::offset_of!(IntelLinkMN, link_m) == 12);
const _: () = assert!(core::mem::offset_of!(IntelLinkMN, link_n) == 16);
const _: () = assert!(size_of::<IntelCscMatrix>() == 30);
const _: () = assert!(core::mem::offset_of!(IntelCscMatrix, coeff) == 0);
const _: () = assert!(size_of::<[u16; 9]>() == 18);
const _: () = assert!(core::mem::offset_of!(IntelCscMatrix, preoff) == 18);
const _: () = assert!(size_of::<[u16; 3]>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelCscMatrix, postoff) == 24);
const _: () = assert!(size_of::<[u16; 3]>() == 6);
const _: () = assert!(size_of::<ScalerFilterCoeff>() == 6);
const _: () = assert!(core::mem::offset_of!(ScalerFilterCoeff, sign) == 0);
const _: () = assert!(core::mem::offset_of!(ScalerFilterCoeff, exp) == 2);
const _: () = assert!(core::mem::offset_of!(ScalerFilterCoeff, mantissa) == 4);
const _: () = assert!(size_of::<IntelCasf>() == 46);
const _: () = assert!(core::mem::offset_of!(IntelCasf, coeff) == 0);
const _: () = assert!(size_of::<[ScalerFilterCoeff; 7]>() == 42);
const _: () = assert!(core::mem::offset_of!(IntelCasf, strength) == 42);
const _: () = assert!(core::mem::offset_of!(IntelCasf, win_size) == 43);
const _: () = assert!(core::mem::offset_of!(IntelCasf, enable) == 44);
const _: () = assert!(size_of::<IntelCrtcStateHw>() == 408);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, active) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, enable) == 1);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, degamma_lut) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, gamma_lut) == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, ctm) == 24);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, mode) == 32);
const _: () = assert!(size_of::<DrmDisplayMode>() == 120);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, pipe_mode) == 152);
const _: () = assert!(size_of::<DrmDisplayMode>() == 120);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, adjusted_mode) == 272);
const _: () = assert!(size_of::<DrmDisplayMode>() == 120);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, background_color) == 392);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, scaling_filter) == 396);
const _: () = assert!(size_of::<DrmScalingFilter>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateHw, sharpness_strength) == 400);
const _: () = assert!(size_of::<IntelCrtcStateDsiPll>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsiPll, ctrl) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsiPll, div) == 4);
const _: () = assert!(size_of::<IntelCrtcStateGmchPfit>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateGmchPfit, control) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateGmchPfit, pgm_ratios) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateGmchPfit, lvds_border_bits) == 8);
const _: () = assert!(size_of::<IntelCrtcStatePchPfit>() == 68);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStatePchPfit, casf) == 0);
const _: () = assert!(size_of::<IntelCasf>() == 46);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStatePchPfit, dst) == 48);
const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStatePchPfit, enabled) == 64);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStatePchPfit, force_thru) == 65);
const _: () = assert!(size_of::<IntelCrtcStateAnon3940x83>() == 4);
const _: () = assert!(size_of::<IntelCrtcStateInfoframes>() == 304);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, gcp) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, avi) == 8);
const _: () = assert!(size_of::<HdmiInfoframe>() == 60);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, spd) == 68);
const _: () = assert!(size_of::<HdmiInfoframe>() == 60);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, hdmi) == 128);
const _: () = assert!(size_of::<HdmiInfoframe>() == 60);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, drm) == 188);
const _: () = assert!(size_of::<HdmiInfoframe>() == 60);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, vsc) == 248);
const _: () = assert!(size_of::<DrmDpVscSdp>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateInfoframes, as_sdp) == 272);
const _: () = assert!(size_of::<DrmDpAsSdp>() == 32);
const _: () = assert!(size_of::<IntelCrtcStateDsc>() == 156);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsc, compression_enabled_on_link) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsc, compression_enable) == 1);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsc, slice_config) == 4);
const _: () = assert!(size_of::<IntelDscSliceConfig>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsc, compressed_bpp_x16) == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateDsc, config) == 18);
const _: () = assert!(size_of::<DrmDscConfig>() == 136);
const _: () = assert!(size_of::<IntelCrtcStateVrrDcBalance>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, vmin) == 2);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, vmax) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, guardband) == 6);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, slope) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, max_increase) == 10);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, max_decrease) == 12);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrrDcBalance, vblank_target) == 14);
const _: () = assert!(size_of::<IntelCrtcStateVrr>() == 36);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, in_range) == 1);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, pipeline_full) == 2);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, flipline) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, vmin) == 6);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, vmax) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, guardband) == 10);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, vsync_end) == 12);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, vsync_start) == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateVrr, dc_balance) == 20);
const _: () = assert!(size_of::<IntelCrtcStateCmrr>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateCmrr, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateCmrr, cmrr_n) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateCmrr, cmrr_m) == 16);
const _: () = assert!(size_of::<IntelCrtcStateSplitter>() == 3);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateSplitter, enable) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateSplitter, link_count) == 1);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateSplitter, pixel_overlap) == 2);
const _: () = assert!(size_of::<IntelCrtcStateAlpmState>() == 6);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, io_wake_lines) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, fast_wake_lines) == 1);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, check_entry_lines) == 2);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, aux_less_wake_lines) == 3);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, silence_period_sym_clocks) == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcStateAlpmState, lfps_half_cycle_num_of_syms) == 5);
const _: () = assert!(size_of::<IntelCrtcState>() == 4808);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, uapi) == 0);
const _: () = assert!(size_of::<DrmCrtcState>() == 344);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, hw) == 344);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pre_csc_lut) == 752);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, post_csc_lut) == 760);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, csc) == 768);
const _: () = assert!(size_of::<IntelCscMatrix>() == 30);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, output_csc) == 798);
const _: () = assert!(size_of::<IntelCscMatrix>() == 30);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, quirks) == 832);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, fb_bits) == 840);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_pipe) == 844);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_m_n) == 845);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_lrr) == 846);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, disable_cxsr) == 847);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_wm_pre) == 848);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_wm_post) == 849);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, fifo_changed) == 850);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, preload_luts) == 851);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, inherited) == 852);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, do_async_flip) == 853);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pipe_src) == 856);
const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pixel_rate) == 872);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_pch_encoder) == 876);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_infoframe) == 877);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, cpu_transcoder) == 880);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, limited_color_range) == 884);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, output_types) == 888);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_hdmi_sink) == 892);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_audio) == 893);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dither) == 894);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dither_force_disable) == 895);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, clock_set) == 896);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, sdvo_tv_clock) == 897);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, bw_constrained) == 898);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dpll) == 900);
const _: () = assert!(size_of::<Dpll>() == 36);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, intel_dpll) == 936);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dpll_hw_state) == 944);
const _: () = assert!(size_of::<IntelDpllHwState>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, icl_port_dplls) == 1032);
const _: () = assert!(size_of::<[IclPortDpll; 2]>() == 192);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dsi_pll) == 1224);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, max_link_bpp_x16) == 1232);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, max_pipe_bpp) == 1236);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pipe_bpp) == 1240);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, min_hblank) == 1244);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dp_m_n) == 1248);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dp_m2_n2) == 1268);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_drrs) == 1288);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_psr) == 1289);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_sel_update) == 1290);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, enable_psr2_sel_fetch) == 1291);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, enable_psr2_su_region_et) == 1292);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, req_psr2_sdp_prior_scanline) == 1293);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_panel_replay) == 1294);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, link_off_after_as_sdp_when_pr_active) == 1295);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, disable_as_sdp_when_pr_active) == 1296);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, wm_level_disabled) == 1297);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pkg_c_latency_used) == 1298);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, panel_replay_dsc_support) == 1300);
const _: () = assert!(size_of::<IntelPanelReplayDscSupport>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dc3co_exitline) == 1304);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, su_y_granularity) == 1308);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, active_non_psr_pipes) == 1310);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, entry_setup_frames) == 1311);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, no_psr_reason) == 1312);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, port_clock) == 1320);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pixel_multiplier) == 1324);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, mode_flags) == 1328);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, lane_count) == 1329);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, lane_lat_optim_mask) == 1330);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, min_voltage_level) == 1331);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, gmch_pfit) == 1332);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pch_pfit) == 1344);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, fdi_lanes) == 1412);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, fdi_m_n) == 1416);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, ips_enabled) == 1436);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, crc_enabled) == 1437);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, double_wide) == 1438);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, scaler_state) == 1440);
const _: () = assert!(size_of::<IntelCrtcScalerState>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, hsw_workaround_pipe) == 1480);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, wm) == 1484);
const _: () = assert!(size_of::<IntelCrtcWmState>() == 2220);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, min_cdclk) == 3704);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, plane_min_cdclk) == 3708);
const _: () = assert!(size_of::<[i32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, data_rate) == 3740);
const _: () = assert!(size_of::<[u32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, data_rate_y) == 3772);
const _: () = assert!(size_of::<[u32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, rel_data_rate) == 3808);
const _: () = assert!(size_of::<[u64; 8]>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, rel_data_rate_y) == 3872);
const _: () = assert!(size_of::<[u64; 8]>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, gamma_mode) == 3936);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, _anon_3940_83) == 3940);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, enabled_planes) == 3944);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, active_planes) == 3945);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, scaled_planes) == 3946);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, nv12_planes) == 3947);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, c8_planes) == 3948);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, update_planes) == 3949);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, async_flip_planes) == 3950);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, framestart_delay) == 3951);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, msa_timing_delay) == 3952);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, infoframes) == 3956);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, eld) == 4260);
const _: () = assert!(size_of::<[u8; 128]>() == 128);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, hdmi_scrambling) == 4388);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, hdmi_high_tmds_clock_ratio) == 4389);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, output_format) == 4392);
const _: () = assert!(size_of::<IntelOutputFormat>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, sink_format) == 4396);
const _: () = assert!(size_of::<IntelOutputFormat>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, gamma_enable) == 4400);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, csc_enable) == 4401);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, wgc_enable) == 4402);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, joiner_pipes) == 4403);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dsc) == 4404);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dp_tunnel_ref) == 4560);
const _: () = assert!(size_of::<DrmDpTunnelRef>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, linetime) == 4576);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, ips_linetime) == 4578);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, enhanced_framing) == 4580);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, fec_enable) == 4581);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, sdp_split_enable) == 4582);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, master_transcoder) == 4584);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, sync_mode_slaves_mask) == 4588);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, mst_master_transcoder) == 4592);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dsb_color) == 4600);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, dsb_commit) == 4608);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, use_dsb) == 4616);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, use_flipq) == 4617);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, psr2_man_track_ctl) == 4620);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, pipe_srcsz_early_tpt) == 4624);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, psr2_su_area) == 4628);
const _: () = assert!(size_of::<DrmRect>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, vrr) == 4644);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, cmrr) == 4680);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, splitter) == 4704);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, vblank_work) == 4712);
const _: () = assert!(size_of::<DrmVblankWork>() == 80);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, has_lobf) == 4792);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, set_context_latency) == 4794);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, alpm_state) == 4796);
const _: () = assert!(core::mem::offset_of!(IntelCrtcState, plane_color_changed) == 4802);
const _: () = assert!(size_of::<IclPortDpll>() == 96);
const _: () = assert!(core::mem::offset_of!(IclPortDpll, pll) == 0);
const _: () = assert!(core::mem::offset_of!(IclPortDpll, hw_state) == 8);
const _: () = assert!(size_of::<IntelDpllHwState>() == 88);
const _: () = assert!(size_of::<IntelDscSliceConfig>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelDscSliceConfig, pipes_per_line) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDscSliceConfig, streams_per_pipe) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDscSliceConfig, slices_per_stream) == 8);
const _: () = assert!(size_of::<IntelPipeCrc>() == 12);
const _: () = assert!(align_of::<IntelPipeCrc>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPipeCrc, lock) == 0);
const _: () = assert!(size_of::<Spinlock>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPipeCrc, skipped) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPipeCrc, source) == 8);
const _: () = assert!(size_of::<IntelPipeCrcSource>() == 4);
const _: () = assert!(size_of::<IntelFlipq>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelFlipq, start_mmioaddr) == 0);
const _: () = assert!(core::mem::offset_of!(IntelFlipq, flipq_id) == 4);
const _: () = assert!(size_of::<IntelFlipqId>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelFlipq, tail) == 8);
const _: () = assert!(size_of::<IntelCrtcWmActive>() == 104);
const _: () = assert!(size_of::<IntelCrtcWm>() == 104);
const _: () = assert!(core::mem::offset_of!(IntelCrtcWm, active) == 0);
const _: () = assert!(size_of::<IntelCrtcDrrs>() == 168);
const _: () = assert!(align_of::<IntelCrtcDrrs>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, mutex) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, work) == 24);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, refresh_rate) == 112);
const _: () = assert!(size_of::<DrrsRefreshRate>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, frontbuffer_bits) == 116);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, busy_frontbuffer_bits) == 120);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, cpu_transcoder) == 124);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, m_n) == 128);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDrrs, m2_n2) == 148);
const _: () = assert!(size_of::<IntelLinkMN>() == 20);
const _: () = assert!(size_of::<IntelCrtcDcBalance>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDcBalance, flip_count) == 0);
const _: () = assert!(size_of::<IntelCrtcDebug>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDebug, start_vbl_count) == 0);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDebug, start_vbl_time) == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDebug, min_vbl) == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDebug, max_vbl) == 20);
const _: () = assert!(core::mem::offset_of!(IntelCrtcDebug, scanline_start) == 24);
const _: () = assert!(size_of::<IntelCrtc>() == 2208);
const _: () = assert!(align_of::<IntelCrtc>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, base) == 0);
const _: () = assert!(size_of::<DrmCrtc>() == 1648);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, pipe_head) == 1648);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, pipe) == 1664);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, active) == 1668);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, plane_ids_mask) == 1669);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, mode_flags) == 1670);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, vmax_vblank_start) == 1672);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, enabled_power_domains) == 1680);
const _: () = assert!(size_of::<IntelDisplayPowerDomainSet>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, hw_readout_power_domains) == 1696);
const _: () = assert!(size_of::<IntelDisplayPowerDomainSet>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, overlay) == 1712);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, config) == 1720);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, flip_done_event) == 1728);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, dsb_event) == 1736);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, flipq_event) == 1744);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, cpu_fifo_underrun_disabled) == 1752);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, pch_fifo_underrun_disabled) == 1753);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, flipq) == 1756);
const _: () = assert!(size_of::<[IntelFlipq; 5]>() == 60);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, wm) == 1816);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, drrs) == 1920);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, dc_balance) == 2088);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, scanline_offset) == 2096);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, debug) == 2104);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, num_scalers) == 2136);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, vblank_pm_qos) == 2144);
const _: () = assert!(size_of::<PmQosRequest>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, pipe_crc) == 2192);
const _: () = assert!(size_of::<IntelPipeCrc>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelCrtc, vblank_psr_notify) == 2204);
const _: () = assert!(size_of::<IntelPlaneError>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelPlaneError, ctl) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPlaneError, surf) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlaneError, surflive) == 8);
const _: () = assert!(size_of::<IntelPlaneCursor>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelPlaneCursor, base) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPlaneCursor, cntl) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlaneCursor, size) == 8);
const _: () = assert!(size_of::<IntelPlane>() == 1536);
const _: () = assert!(align_of::<IntelPlane>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelPlane, base) == 0);
const _: () = assert!(size_of::<DrmPlane>() == 1352);
const _: () = assert!(core::mem::offset_of!(IntelPlane, i9xx_plane) == 1352);
const _: () = assert!(size_of::<I9xxPlaneId>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlane, id) == 1356);
const _: () = assert!(size_of::<PlaneId>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlane, pipe) == 1360);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPlane, need_async_flip_toggle_wa) == 1364);
const _: () = assert!(core::mem::offset_of!(IntelPlane, vtd_guard) == 1365);
const _: () = assert!(core::mem::offset_of!(IntelPlane, frontbuffer_bit) == 1368);
const _: () = assert!(core::mem::offset_of!(IntelPlane, cursor) == 1372);
const _: () = assert!(core::mem::offset_of!(IntelPlane, fbc) == 1384);
const _: () = assert!(core::mem::offset_of!(IntelPlane, min_width) == 1392);
const _: () = assert!(core::mem::offset_of!(IntelPlane, max_width) == 1400);
const _: () = assert!(core::mem::offset_of!(IntelPlane, max_height) == 1408);
const _: () = assert!(core::mem::offset_of!(IntelPlane, min_alignment) == 1416);
const _: () = assert!(core::mem::offset_of!(IntelPlane, max_stride) == 1424);
const _: () = assert!(core::mem::offset_of!(IntelPlane, can_async_flip) == 1432);
const _: () = assert!(core::mem::offset_of!(IntelPlane, update_noarm) == 1440);
const _: () = assert!(core::mem::offset_of!(IntelPlane, update_arm) == 1448);
const _: () = assert!(core::mem::offset_of!(IntelPlane, disable_arm) == 1456);
const _: () = assert!(core::mem::offset_of!(IntelPlane, capture_error) == 1464);
const _: () = assert!(core::mem::offset_of!(IntelPlane, get_hw_state) == 1472);
const _: () = assert!(core::mem::offset_of!(IntelPlane, check_plane) == 1480);
const _: () = assert!(core::mem::offset_of!(IntelPlane, surf_offset) == 1488);
const _: () = assert!(core::mem::offset_of!(IntelPlane, min_cdclk) == 1496);
const _: () = assert!(core::mem::offset_of!(IntelPlane, async_flip) == 1504);
const _: () = assert!(core::mem::offset_of!(IntelPlane, enable_flip_done) == 1512);
const _: () = assert!(core::mem::offset_of!(IntelPlane, disable_flip_done) == 1520);
const _: () = assert!(core::mem::offset_of!(IntelPlane, disable_tiling) == 1528);
const _: () = assert!(size_of::<IntelHdmiDpDualMode>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelHdmiDpDualMode, r#type) == 0);
const _: () = assert!(size_of::<DrmDpDualModeType>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdmiDpDualMode, max_tmds_clock) == 4);
const _: () = assert!(size_of::<IntelHdmi>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelHdmi, hdmi_reg) == 0);
const _: () = assert!(size_of::<I915RegT>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdmi, dp_dual_mode) == 4);
const _: () = assert!(core::mem::offset_of!(IntelHdmi, attached_connector) == 16);
const _: () = assert!(core::mem::offset_of!(IntelHdmi, cec_notifier) == 24);
const _: () = assert!(size_of::<IntelDpComplianceData>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, edid) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, video_pattern) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, hdisplay) == 10);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, vdisplay) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, bpc) == 14);
const _: () = assert!(core::mem::offset_of!(IntelDpComplianceData, phytest) == 16);
const _: () = assert!(size_of::<DrmDpPhyTestParams>() == 20);
const _: () = assert!(size_of::<IntelDpCompliance>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelDpCompliance, test_type) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpCompliance, test_data) == 8);
const _: () = assert!(size_of::<IntelDpComplianceData>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelDpCompliance, test_active) == 48);
const _: () = assert!(core::mem::offset_of!(IntelDpCompliance, test_link_rate) == 52);
const _: () = assert!(core::mem::offset_of!(IntelDpCompliance, test_lane_count) == 56);
const _: () = assert!(size_of::<IntelDpPconFrl>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpPconFrl, is_trained) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpPconFrl, trained_rate_gbps) == 4);
const _: () = assert!(size_of::<IntelPpsAnon152x12>() == 4);
const _: () = assert!(size_of::<IntelPps>() == 184);
const _: () = assert!(core::mem::offset_of!(IntelPps, panel_power_up_delay) == 0);
const _: () = assert!(core::mem::offset_of!(IntelPps, panel_power_down_delay) == 4);
const _: () = assert!(core::mem::offset_of!(IntelPps, panel_power_cycle_delay) == 8);
const _: () = assert!(core::mem::offset_of!(IntelPps, backlight_on_delay) == 12);
const _: () = assert!(core::mem::offset_of!(IntelPps, backlight_off_delay) == 16);
const _: () = assert!(core::mem::offset_of!(IntelPps, panel_vdd_work) == 24);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelPps, want_panel_vdd) == 112);
const _: () = assert!(core::mem::offset_of!(IntelPps, initializing) == 113);
const _: () = assert!(core::mem::offset_of!(IntelPps, last_power_on) == 120);
const _: () = assert!(core::mem::offset_of!(IntelPps, last_backlight_off) == 128);
const _: () = assert!(core::mem::offset_of!(IntelPps, panel_power_off_time) == 136);
const _: () = assert!(core::mem::offset_of!(IntelPps, vdd_wakeref) == 144);
const _: () = assert!(core::mem::offset_of!(IntelPps, _anon_152_12) == 152);
const _: () = assert!(core::mem::offset_of!(IntelPps, vlv_active_pipe) == 156);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPps, bxt_pps_reset) == 160);
const _: () = assert!(core::mem::offset_of!(IntelPps, pps_delays) == 162);
const _: () = assert!(size_of::<IntelPpsDelays>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelPps, bios_pps_delays) == 172);
const _: () = assert!(size_of::<IntelPpsDelays>() == 10);
const _: () = assert!(size_of::<IntelPsr>() == 248);
const _: () = assert!(align_of::<IntelPsr>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelPsr, lock) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelPsr, debug) == 24);
const _: () = assert!(core::mem::offset_of!(IntelPsr, sink_support) == 28);
const _: () = assert!(core::mem::offset_of!(IntelPsr, source_support) == 29);
const _: () = assert!(core::mem::offset_of!(IntelPsr, enabled) == 30);
const _: () = assert!(core::mem::offset_of!(IntelPsr, pause_counter) == 32);
const _: () = assert!(core::mem::offset_of!(IntelPsr, pipe) == 36);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPsr, transcoder) == 40);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelPsr, active) == 44);
const _: () = assert!(core::mem::offset_of!(IntelPsr, work) == 48);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelPsr, busy_frontbuffer_bits) == 80);
const _: () = assert!(core::mem::offset_of!(IntelPsr, link_standby) == 84);
const _: () = assert!(core::mem::offset_of!(IntelPsr, sel_update_enabled) == 85);
const _: () = assert!(core::mem::offset_of!(IntelPsr, psr2_sel_fetch_enabled) == 86);
const _: () = assert!(core::mem::offset_of!(IntelPsr, psr2_sel_fetch_cff_enabled) == 87);
const _: () = assert!(core::mem::offset_of!(IntelPsr, su_region_et_enabled) == 88);
const _: () = assert!(core::mem::offset_of!(IntelPsr, req_psr2_sdp_prior_scanline) == 89);
const _: () = assert!(core::mem::offset_of!(IntelPsr, last_entry_attempt) == 96);
const _: () = assert!(core::mem::offset_of!(IntelPsr, last_exit) == 104);
const _: () = assert!(core::mem::offset_of!(IntelPsr, sink_not_reliable) == 112);
const _: () = assert!(core::mem::offset_of!(IntelPsr, irq_aux_error) == 113);
const _: () = assert!(core::mem::offset_of!(IntelPsr, su_w_granularity) == 114);
const _: () = assert!(core::mem::offset_of!(IntelPsr, su_y_granularity) == 116);
const _: () = assert!(core::mem::offset_of!(IntelPsr, source_panel_replay_support) == 118);
const _: () = assert!(core::mem::offset_of!(IntelPsr, sink_panel_replay_support) == 119);
const _: () = assert!(core::mem::offset_of!(IntelPsr, panel_replay_enabled) == 120);
const _: () = assert!(core::mem::offset_of!(IntelPsr, dc3co_exitline) == 124);
const _: () = assert!(core::mem::offset_of!(IntelPsr, dc3co_exit_delay) == 128);
const _: () = assert!(core::mem::offset_of!(IntelPsr, dc3co_work) == 136);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelPsr, entry_setup_frames) == 224);
const _: () = assert!(core::mem::offset_of!(IntelPsr, io_wake_lines) == 225);
const _: () = assert!(core::mem::offset_of!(IntelPsr, fast_wake_lines) == 226);
const _: () = assert!(core::mem::offset_of!(IntelPsr, link_ok) == 227);
const _: () = assert!(core::mem::offset_of!(IntelPsr, pkg_c_latency_used) == 228);
const _: () = assert!(core::mem::offset_of!(IntelPsr, active_non_psr_pipes) == 229);
const _: () = assert!(core::mem::offset_of!(IntelPsr, no_psr_reason) == 232);
const _: () = assert!(core::mem::offset_of!(IntelPsr, vblank_wakeref) == 240);
const _: () = assert!(size_of::<IntelDpLink>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, active) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, num_configs) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, configs) == 8);
const _: () = assert!(size_of::<[IntelDpLinkConfig; 24]>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, max_lane_count) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, max_rate) == 36);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, mst_probed_lane_count) == 40);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, mst_probed_rate) == 44);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, force_lane_count) == 48);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, force_rate) == 52);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, retrain_disabled) == 56);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, seq_train_failures) == 60);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, force_train_failure) == 64);
const _: () = assert!(core::mem::offset_of!(IntelDpLink, force_retrain) == 68);
const _: () = assert!(size_of::<IntelDpMst>() == 1208);
const _: () = assert!(align_of::<IntelDpMst>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpMst, stream_encoders) == 0);
const _: () = assert!(size_of::<[*mut IntelDpMstEncoder; 4]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDpMst, mgr) == 32);
const _: () = assert!(size_of::<DrmDpMstTopologyMgr>() == 1168);
const _: () = assert!(core::mem::offset_of!(IntelDpMst, active_streams) == 1200);
const _: () = assert!(size_of::<IntelDpDfp>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, min_tmds_clock) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, max_tmds_clock) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, max_dotclock) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, pcon_max_frl_bw) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, max_bpc) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, ycbcr_444_to_420) == 17);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, ycbcr420_passthrough) == 18);
const _: () = assert!(core::mem::offset_of!(IntelDpDfp, rgb_to_ycbcr) == 19);
const _: () = assert!(size_of::<IntelDpAlpm>() == 40);
const _: () = assert!(align_of::<IntelDpAlpm>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpAlpm, transcoder) == 0);
const _: () = assert!(size_of::<Transcoder>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDpAlpm, lock) == 8);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDpAlpm, lobf_disable_debug) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDpAlpm, sink_alpm_error) == 33);
const _: () = assert!(size_of::<IntelDpQuirks>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpQuirks, mask) == 0);
const _: () = assert!(size_of::<IntelDp>() == 3616);
const _: () = assert!(align_of::<IntelDp>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDp, output_reg) == 0);
const _: () = assert!(size_of::<I915RegT>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDp, DP) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDp, link_rate) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDp, lane_count) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDp, sink_count) == 13);
const _: () = assert!(core::mem::offset_of!(IntelDp, downstream_port_changed) == 14);
const _: () = assert!(core::mem::offset_of!(IntelDp, needs_modeset_retry) == 15);
const _: () = assert!(core::mem::offset_of!(IntelDp, use_max_params) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDp, dpcd) == 17);
const _: () = assert!(size_of::<[u8; 15]>() == 15);
const _: () = assert!(core::mem::offset_of!(IntelDp, downstream_ports) == 32);
const _: () = assert!(size_of::<[u8; 16]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDp, edp_dpcd) == 48);
const _: () = assert!(size_of::<[u8; 5]>() == 5);
const _: () = assert!(core::mem::offset_of!(IntelDp, lttpr_common_caps) == 53);
const _: () = assert!(size_of::<[u8; 8]>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDp, lttpr_phy_caps) == 61);
const _: () = assert!(size_of::<[[u8; 3]; 8]>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDp, pcon_dsc_dpcd) == 85);
const _: () = assert!(size_of::<[u8; 13]>() == 13);
const _: () = assert!(core::mem::offset_of!(IntelDp, num_source_rates) == 100);
const _: () = assert!(core::mem::offset_of!(IntelDp, source_rates) == 104);
const _: () = assert!(core::mem::offset_of!(IntelDp, num_sink_rates) == 112);
const _: () = assert!(core::mem::offset_of!(IntelDp, sink_rates) == 116);
const _: () = assert!(size_of::<[i32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDp, use_rate_select) == 148);
const _: () = assert!(core::mem::offset_of!(IntelDp, max_sink_lane_count) == 152);
const _: () = assert!(core::mem::offset_of!(IntelDp, num_common_rates) == 156);
const _: () = assert!(core::mem::offset_of!(IntelDp, common_rates) == 160);
const _: () = assert!(size_of::<[i32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDp, max_common_lane_count) == 192);
const _: () = assert!(core::mem::offset_of!(IntelDp, link) == 196);
const _: () = assert!(core::mem::offset_of!(IntelDp, reset_link_params) == 268);
const _: () = assert!(core::mem::offset_of!(IntelDp, mso_link_count) == 272);
const _: () = assert!(core::mem::offset_of!(IntelDp, mso_pixel_overlap) == 276);
const _: () = assert!(core::mem::offset_of!(IntelDp, desc) == 280);
const _: () = assert!(size_of::<DrmDpDesc>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDp, aux) == 296);
const _: () = assert!(size_of::<DrmDpAux>() == 1320);
const _: () = assert!(core::mem::offset_of!(IntelDp, aux_busy_last_status) == 1616);
const _: () = assert!(core::mem::offset_of!(IntelDp, train_set) == 1620);
const _: () = assert!(size_of::<[u8; 4]>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDp, pps) == 1624);
const _: () = assert!(size_of::<IntelPps>() == 184);
const _: () = assert!(core::mem::offset_of!(IntelDp, is_mst) == 1808);
const _: () = assert!(core::mem::offset_of!(IntelDp, mst_detect) == 1812);
const _: () = assert!(size_of::<DrmDpMstMode>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDp, attached_connector) == 1816);
const _: () = assert!(core::mem::offset_of!(IntelDp, as_sdp_supported) == 1824);
const _: () = assert!(core::mem::offset_of!(IntelDp, as_sdp_v2_supported) == 1825);
const _: () = assert!(core::mem::offset_of!(IntelDp, tunnel) == 1832);
const _: () = assert!(core::mem::offset_of!(IntelDp, _bitfields_1840) == 1840);
const _: () = assert!(core::mem::offset_of!(IntelDp, mst) == 1848);
const _: () = assert!(core::mem::offset_of!(IntelDp, get_aux_clock_divider) == 3056);
const _: () = assert!(core::mem::offset_of!(IntelDp, get_aux_send_ctl) == 3064);
const _: () = assert!(core::mem::offset_of!(IntelDp, aux_ch_ctl_reg) == 3072);
const _: () = assert!(core::mem::offset_of!(IntelDp, aux_ch_data_reg) == 3080);
const _: () = assert!(core::mem::offset_of!(IntelDp, prepare_link_retrain) == 3088);
const _: () = assert!(core::mem::offset_of!(IntelDp, set_link_train) == 3096);
const _: () = assert!(core::mem::offset_of!(IntelDp, set_idle_link_train) == 3104);
const _: () = assert!(core::mem::offset_of!(IntelDp, preemph_max) == 3112);
const _: () = assert!(core::mem::offset_of!(IntelDp, voltage_max) == 3120);
const _: () = assert!(core::mem::offset_of!(IntelDp, compliance) == 3128);
const _: () = assert!(size_of::<IntelDpCompliance>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelDp, dfp) == 3192);
const _: () = assert!(core::mem::offset_of!(IntelDp, pm_qos) == 3216);
const _: () = assert!(size_of::<PmQosRequest>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelDp, force_dsc_en) == 3264);
const _: () = assert!(core::mem::offset_of!(IntelDp, force_dsc_output_format) == 3268);
const _: () = assert!(core::mem::offset_of!(IntelDp, force_dsc_fractional_bpp_en) == 3272);
const _: () = assert!(core::mem::offset_of!(IntelDp, force_dsc_bpc) == 3276);
const _: () = assert!(core::mem::offset_of!(IntelDp, hobl_failed) == 3280);
const _: () = assert!(core::mem::offset_of!(IntelDp, hobl_active) == 3281);
const _: () = assert!(core::mem::offset_of!(IntelDp, frl) == 3284);
const _: () = assert!(size_of::<IntelDpPconFrl>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDp, psr) == 3296);
const _: () = assert!(size_of::<IntelPsr>() == 248);
const _: () = assert!(core::mem::offset_of!(IntelDp, last_oui_write) == 3544);
const _: () = assert!(core::mem::offset_of!(IntelDp, oui_valid) == 3552);
const _: () = assert!(core::mem::offset_of!(IntelDp, colorimetry_support) == 3553);
const _: () = assert!(core::mem::offset_of!(IntelDp, alpm) == 3560);
const _: () = assert!(core::mem::offset_of!(IntelDp, alpm_dpcd) == 3600);
const _: () = assert!(core::mem::offset_of!(IntelDp, quirks) == 3608);
const _: () = assert!(size_of::<IntelDpLinkConfig>() == 1);
const _: () = assert!(core::mem::offset_of!(IntelDpLinkConfig, _bitfields_0) == 0);
const _: () = assert!(size_of::<IntelLspcon>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelLspcon, active) == 0);
const _: () = assert!(core::mem::offset_of!(IntelLspcon, hdr_supported) == 1);
const _: () = assert!(core::mem::offset_of!(IntelLspcon, mode) == 4);
const _: () = assert!(size_of::<DrmLspconMode>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelLspcon, vendor) == 8);
const _: () = assert!(size_of::<LspconVendor>() == 4);
const _: () = assert!(size_of::<IntelDigitalPortHdcp>() == 64);
const _: () = assert!(align_of::<IntelDigitalPortHdcp>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPortHdcp, mutex) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPortHdcp, num_streams) == 24);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPortHdcp, auth_status) == 28);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPortHdcp, port_data) == 32);
const _: () = assert!(size_of::<HdcpPortData>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPortHdcp, mst_type1_capable) == 56);
const _: () = assert!(size_of::<IntelDigitalPort>() == 4336);
const _: () = assert!(align_of::<IntelDigitalPort>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, base) == 0);
const _: () = assert!(size_of::<IntelEncoder>() == 504);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, dp) == 504);
const _: () = assert!(size_of::<IntelDp>() == 3616);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, hdmi) == 4120);
const _: () = assert!(size_of::<IntelHdmi>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, lspcon) == 4152);
const _: () = assert!(size_of::<IntelLspcon>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, hpd_pulse) == 4168);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, lane_reversal) == 4176);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, ddi_a_4_lanes) == 4177);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, release_cl2_override) == 4178);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, dedicated_external) == 4179);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, max_lanes) == 4180);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, aux_ch) == 4184);
const _: () = assert!(size_of::<AuxCh>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, ddi_io_power_domain) == 4188);
const _: () = assert!(size_of::<IntelDisplayPowerDomain>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, ddi_io_wakeref) == 4192);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, aux_wakeref) == 4200);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, tc) == 4208);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, hdcp) == 4216);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, write_infoframe) == 4280);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, read_infoframe) == 4288);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, set_infoframes) == 4296);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, infoframes_enabled) == 4304);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, connected) == 4312);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, lock) == 4320);
const _: () = assert!(core::mem::offset_of!(IntelDigitalPort, unlock) == 4328);
const _: () = assert!(size_of::<IntelDpMstEncoder>() == 528);
const _: () = assert!(align_of::<IntelDpMstEncoder>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpMstEncoder, base) == 0);
const _: () = assert!(size_of::<IntelEncoder>() == 504);
const _: () = assert!(core::mem::offset_of!(IntelDpMstEncoder, pipe) == 504);
const _: () = assert!(size_of::<Pipe>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDpMstEncoder, primary) == 512);
const _: () = assert!(core::mem::offset_of!(IntelDpMstEncoder, connector) == 520);
const _: () = assert!(size_of::<IntelColorop>() == 1224);
const _: () = assert!(align_of::<IntelColorop>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelColorop, base) == 0);
const _: () = assert!(size_of::<DrmColorop>() == 1216);
const _: () = assert!(core::mem::offset_of!(IntelColorop, id) == 1216);
const _: () = assert!(size_of::<IntelColorBlock>() == 4);
