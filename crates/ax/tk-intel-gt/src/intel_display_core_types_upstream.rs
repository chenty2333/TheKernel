// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_core.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::DelayedWork;
use tk_linux_kapi::types::ListHead;
use tk_linux_kapi::types::Mutex;
use tk_linux_kapi::types::Spinlock;
use tk_linux_kapi::types::WaitQueueHead;
use tk_linux_kapi::types::WorkStruct;
use tk_drm_upstream::drm_atomic_types::{DrmAtomicCommit};
use tk_drm_upstream::drm_connector_types::{DrmPanelOrientation};
use tk_drm_upstream::drm_device_types::{DrmDevice};
use tk_drm_upstream::drm_modeset_lock_types::{DrmModesetAcquireCtx};
use tk_drm_upstream::drm_property_types::{DrmProperty, DrmPropertyBlob};
use crate::i915_external_placeholder_types_upstream::{I915HdcpArbiter};
use crate::intel_cdclk_types_upstream::{IntelCdclkConfig};
use crate::intel_display_device_types_upstream::{IntelDisplayDeviceInfo, IntelDisplayPlatforms, IntelDisplayRuntimeInfo};
use crate::intel_display_params_types_upstream::{IntelDisplayParams};
use crate::intel_display_power_types_upstream::{I915PowerDomains};
use crate::intel_display_types_upstream::{IntelAtomicState, IntelCrtc, IntelCrtcState, IntelEncoder, IntelInitialPlaneConfig};
use crate::intel_dmc_wl_types_upstream::{IntelDmcWl};
use crate::intel_dpll_mgr_types_upstream::{IntelDpll};
use crate::intel_dram_types_upstream::{DramInfo};
use crate::intel_fbc_types_upstream::{IntelFbcId};
use crate::intel_global_state_types_upstream::{IntelGlobalObj};
use crate::intel_pch_types_upstream::{IntelPch};
use crate::intel_wm_types_upstream::{G4xWmValues, IlkWmValues, VlvWmValues};

#[repr(C)]
pub struct IntelModesetFuncs {
    pub get_pipe_config: Option<unsafe extern "C" fn(*mut IntelCrtc, *mut IntelCrtcState) -> bool>,
    pub get_initial_plane_config: Option<unsafe extern "C" fn(*mut IntelCrtc, *mut IntelInitialPlaneConfig)>,
    pub fixup_initial_plane_config: Option<unsafe extern "C" fn(*mut IntelCrtc, *mut IntelInitialPlaneConfig) -> bool>,
    pub crtc_enable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc)>,
    pub crtc_disable: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc)>,
    pub commit_modeset_enables: Option<unsafe extern "C" fn(*mut IntelAtomicState)>,
}

#[repr(C)]
pub struct IntelWmFuncs {
    pub update_wm: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
    pub compute_watermarks: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc) -> i32>,
    pub initial_watermarks: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc)>,
    pub atomic_update_watermarks: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc)>,
    pub optimize_watermarks: Option<unsafe extern "C" fn(*mut IntelAtomicState, *mut IntelCrtc)>,
    pub compute_global_watermarks: Option<unsafe extern "C" fn(*mut IntelAtomicState) -> i32>,
    pub get_hw_state: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
    pub sanitize: Option<unsafe extern "C" fn(*mut IntelDisplay)>,
}

#[repr(C)]
pub struct IntelAudioState {
    pub encoder: *mut IntelEncoder,
    pub eld: [u8; 128],
}

#[repr(C)]
pub struct IntelAudioLpe {
    pub platdev: *mut c_void,
    pub irq: i32,
    pub _pad_12: [u8; 4],
}

#[repr(C, align(8))]
pub struct IntelAudio {
    pub funcs: *mut c_void,
    pub component: *mut c_void,
    pub component_registered: bool,
    pub _pad_17: [u8; 7],
    pub mutex: Mutex,
    pub power_refcount: i32,
    pub freq_cntrl: u32,
    pub state: [IntelAudioState; 7],
    pub lpe: IntelAudioLpe,
}

#[repr(C)]
pub struct IntelDpllGlobalRefClks {
    pub nssc: i32,
    pub ssc: i32,
}

#[repr(C, align(8))]
pub struct IntelDpllGlobal {
    pub funcs: *mut c_void,
    pub lock: Mutex,
    pub num_dpll: i32,
    pub _pad_36: [u8; 4],
    pub dplls: [IntelDpll; 9],
    pub mgr: *mut c_void,
    pub ref_clks: IntelDpllGlobalRefClks,
    pub pch_ssc_use: u8,
    pub _pad_1065: [u8; 7],
}

#[repr(C, align(4))]
pub struct IntelFrontbufferTracking {
    pub lock: Spinlock,
    pub busy_bits: u32,
}

#[repr(C)]
pub struct IntelHotplugStats {
    pub last_jiffies: u64,
    pub count: i32,
    pub blocked_count: i32,
    pub state: u32,
    pub _pad_20: [u8; 4],
}

#[repr(C)]
pub struct IntelHotplug {
    pub funcs: *mut c_void,
    pub hotplug_work: DelayedWork,
    pub hpd: *mut u32,
    pub pch_hpd: *mut u32,
    pub stats: [IntelHotplugStats; 15],
    pub event_bits: u32,
    pub retry_bits: u32,
    pub reenable_work: DelayedWork,
    pub long_hpd_pin_mask: u32,
    pub short_hpd_pin_mask: u32,
    pub dig_port_work: WorkStruct,
    pub poll_init_work: WorkStruct,
    pub poll_enabled: bool,
    pub detection_work_enabled: bool,
    pub _pad_642: [u8; 2],
    pub hpd_storm_threshold: u32,
    pub hpd_short_storm_enabled: u8,
    pub _pad_649: [u8; 7],
    pub oob_hotplug_last_state: u64,
    pub dp_wq: *mut c_void,
    pub ignore_long_hpd: bool,
    pub _pad_673: [u8; 7],
}

#[repr(C)]
pub struct IntelVbtData {
    pub version: u16,
    /// bit-fields int_tv_support:1, int_crt_support:1, lvds_use_ssc:1, int_lvds_support:1, display_clock_mode:1, fdi_rx_polarity_inverted:1
    pub _bitfields_2: [u8; 1],
    pub _pad_3: [u8; 1],
    pub lvds_ssc_freq: i32,
    pub orientation: DrmPanelOrientation,
    pub override_afc_startup: bool,
    pub override_afc_startup_val: u8,
    pub _pad_14: [u8; 2],
    pub crt_ddc_pin: i32,
    pub _pad_20: [u8; 4],
    pub display_devices: ListHead,
    pub bdb_blocks: ListHead,
    pub sdvo_mappings: [SdvoDeviceMapping; 2],
    pub _pad_68: [u8; 4],
}

#[repr(C)]
pub struct SdvoDeviceMapping {
    pub initialized: u8,
    pub dvo_port: u8,
    pub target_addr: u8,
    pub dvo_wiring: u8,
    pub i2c_pin: u8,
    pub ddc_pin: u8,
}

#[repr(C)]
pub union IntelWmAnon56x5 {
    pub hw: ManuallyDrop<IlkWmValues>,
    pub vlv: ManuallyDrop<VlvWmValues>,
    pub g4x: ManuallyDrop<G4xWmValues>,
}

#[repr(C, align(8))]
pub struct IntelWm {
    pub funcs: *mut IntelWmFuncs,
    pub pri_latency: [u16; 5],
    pub spr_latency: [u16; 5],
    pub cur_latency: [u16; 5],
    pub skl_latency: [u16; 8],
    pub _pad_54: [u8; 2],
    pub _anon_56_5: IntelWmAnon56x5,
    pub num_levels: u8,
    pub _pad_145: [u8; 7],
    pub wm_mutex: Mutex,
    pub ipc_enabled: bool,
    pub _pad_177: [u8; 7],
}

#[repr(C)]
pub struct IntelDisplayAccess {
    pub any_task_allowed: bool,
    pub _pad_1: [u8; 7],
    pub allowed_task: *mut c_void,
}

#[repr(C, align(8))]
pub struct IntelDisplayBacklight {
    pub lock: Mutex,
}

#[repr(C)]
pub struct IntelDisplayBw {
    pub obj: IntelGlobalObj,
    pub max: [IntelBwInfo; 6],
}

#[repr(C)]
pub struct IntelDisplayCdclk {
    pub funcs: *mut c_void,
    pub hw: IntelCdclkConfig,
    pub _pad_28: [u8; 4],
    pub table: *mut c_void,
    pub obj: IntelGlobalObj,
    pub max_cdclk_freq: u32,
    pub max_dotclk_freq: u32,
    pub skl_preferred_vco_freq: u32,
    pub _pad_84: [u8; 4],
}

#[repr(C)]
pub struct IntelDisplayColor {
    pub funcs: *mut c_void,
    pub glk_linear_degamma_lut: *mut DrmPropertyBlob,
}

#[repr(C)]
pub struct IntelDisplayDbuf {
    pub enabled_slices: u8,
    pub _pad_1: [u8; 7],
    pub obj: IntelGlobalObj,
}

#[repr(C)]
pub struct IntelDisplayDbufBw {
    pub obj: IntelGlobalObj,
}

#[repr(C, align(4))]
pub struct IntelDisplayDkl {
    pub phy_lock: Spinlock,
}

#[repr(C)]
pub struct IntelDisplayDmc {
    pub dmc: *mut c_void,
    pub wakeref: *mut c_void,
}

#[repr(C)]
pub struct IntelDisplayDsi {
    pub mmio_base: u32,
}

#[repr(C)]
pub struct IntelDisplayDram {
    pub info: *mut DramInfo,
}

#[repr(C, align(8))]
pub struct IntelDisplayFbc {
    pub instances: [*mut c_void; 4],
    pub sys_cache: SysCacheCfg,
}

#[repr(C)]
pub struct IntelDisplayFbdev {
    pub fbdev: *mut c_void,
}

#[repr(C)]
pub struct IntelDisplayFdi {
    pub funcs: *mut c_void,
    pub pll_freq: u32,
    pub rx_config: u32,
}

#[repr(C)]
pub struct IntelDisplayGlobal {
    pub obj_list: ListHead,
}

#[repr(C, align(8))]
pub struct IntelDisplayGmbus {
    pub mmio_base: u32,
    pub _pad_4: [u8; 4],
    pub mutex: Mutex,
    pub bus: [*mut c_void; 15],
    pub wait_queue: WaitQueueHead,
}

#[repr(C, align(8))]
pub struct IntelDisplayHdcp {
    pub arbiter: *mut I915HdcpArbiter,
    pub comp_added: bool,
    pub _pad_9: [u8; 7],
    pub gsc_context: *mut c_void,
    pub hdcp_mutex: Mutex,
}

#[repr(C)]
pub struct IntelDisplayHti {
    pub state: u32,
}

#[repr(C)]
pub struct IntelDisplayInfo {
    pub __device_info: *mut IntelDisplayDeviceInfo,
    pub __runtime_info: IntelDisplayRuntimeInfo,
}

#[repr(C)]
pub struct IntelDisplayIps {
    pub false_color: bool,
}

#[repr(C, align(8))]
pub struct IntelDisplayIrq {
    pub funcs: *mut c_void,
    pub lock: Spinlock,
    pub vlv_display_irqs_enabled: bool,
    pub vblank_enabled: u8,
    pub _pad_14: [u8; 2],
    pub vblank_enable_count: i32,
    pub vblank_status_last_notified: bool,
    pub _pad_21: [u8; 3],
    pub vblank_notify_work: WorkStruct,
    pub vlv_imr_mask: u32,
    pub ilk_de_imr_mask: u32,
    pub de_pipe_imr_mask: [u32; 4],
    pub pipestat_irq_mask: [u32; 4],
}

#[repr(C)]
pub struct IntelDisplayModeset {
    pub funcs: *mut IntelModesetFuncs,
}

#[repr(C)]
pub struct IntelDisplayPkgc {
    pub linetime: [u16; 4],
    pub disable: [bool; 4],
}

#[repr(C, align(8))]
pub struct IntelDisplayPmdemand {
    pub waitqueue: WaitQueueHead,
    pub lock: Mutex,
    pub obj: IntelGlobalObj,
}

#[repr(C, align(8))]
pub struct IntelDisplayPower {
    pub domains: I915PowerDomains,
    pub chv_phy_control: u32,
    pub chv_phy_assert: [bool; 2],
    pub _pad_518: [u8; 2],
}

#[repr(C, align(8))]
pub struct IntelDisplayPps {
    pub mmio_base: u32,
    pub _pad_4: [u8; 4],
    pub mutex: Mutex,
}

#[repr(C)]
pub struct IntelDisplayProperties {
    pub broadcast_rgb: *mut DrmProperty,
    pub force_audio: *mut DrmProperty,
}

#[repr(C)]
pub struct IntelDisplayQuirks {
    pub mask: u64,
}

#[repr(C)]
pub struct IntelDisplayReset {
    pub count: u32,
}

#[repr(C)]
pub struct IntelDisplayRestore {
    pub modeset_state: *mut DrmAtomicCommit,
    pub reset_ctx: DrmModesetAcquireCtx,
    pub saveDSPARB: u32,
    pub saveSWF0: [u32; 16],
    pub saveSWF1: [u32; 16],
    pub saveSWF3: [u32; 3],
    pub saveGCDGMBUS: u16,
    pub _pad_218: [u8; 6],
}

#[repr(C)]
pub struct IntelDisplaySagv {
    pub status: u32,
    pub block_time_us: u32,
}

#[repr(C, align(8))]
pub struct IntelDisplaySbi {
    pub lock: Mutex,
}

#[repr(C)]
pub struct IntelDisplaySnps {
    pub phy_failed_calibration: u8,
}

#[repr(C)]
pub struct IntelDisplayState {
    pub chv_dpll_md: [u32; 4],
    pub bxt_phy_grc: u32,
}

#[repr(C)]
pub struct IntelDisplayVlvClock {
    pub hpll_freq: u32,
    pub czclk_freq: u32,
}

#[repr(C)]
pub struct IntelDisplayWq {
    pub modeset: *mut c_void,
    pub flip: *mut c_void,
    pub cleanup: *mut c_void,
    pub unordered: *mut c_void,
}

#[repr(C, align(8))]
pub struct IntelDisplay {
    pub drm: *mut DrmDevice,
    pub platform: IntelDisplayPlatforms,
    pub pch_type: IntelPch,
    pub _pad_28: [u8; 4],
    pub parent: *mut c_void,
    pub pipe_list: ListHead,
    pub access: IntelDisplayAccess,
    pub backlight: IntelDisplayBacklight,
    pub bw: IntelDisplayBw,
    pub cdclk: IntelDisplayCdclk,
    pub color: IntelDisplayColor,
    pub dbuf: IntelDisplayDbuf,
    pub dbuf_bw: IntelDisplayDbufBw,
    pub dkl: IntelDisplayDkl,
    pub _pad_788: [u8; 4],
    pub dmc: IntelDisplayDmc,
    pub dsi: IntelDisplayDsi,
    pub _pad_812: [u8; 4],
    pub dram: IntelDisplayDram,
    pub fbc: IntelDisplayFbc,
    pub fbdev: IntelDisplayFbdev,
    pub fdi: IntelDisplayFdi,
    pub global: IntelDisplayGlobal,
    pub gmbus: IntelDisplayGmbus,
    pub hdcp: IntelDisplayHdcp,
    pub hti: IntelDisplayHti,
    pub _pad_1156: [u8; 4],
    pub info: IntelDisplayInfo,
    pub ips: IntelDisplayIps,
    pub _pad_1209: [u8; 7],
    pub irq: IntelDisplayIrq,
    pub modeset: IntelDisplayModeset,
    pub pkgc: IntelDisplayPkgc,
    pub _pad_1332: [u8; 4],
    pub pmdemand: IntelDisplayPmdemand,
    pub power: IntelDisplayPower,
    pub pps: IntelDisplayPps,
    pub properties: IntelDisplayProperties,
    pub quirks: IntelDisplayQuirks,
    pub reset: IntelDisplayReset,
    pub _pad_1996: [u8; 4],
    pub restore: IntelDisplayRestore,
    pub sagv: IntelDisplaySagv,
    pub sbi: IntelDisplaySbi,
    pub snps: IntelDisplaySnps,
    pub _pad_2257: [u8; 3],
    pub state: IntelDisplayState,
    pub vlv_clock: IntelDisplayVlvClock,
    pub wq: IntelDisplayWq,
    pub dp_tunnel_mgr: *mut c_void,
    pub audio: IntelAudio,
    pub dpll: IntelDpllGlobal,
    pub fb_tracking: IntelFrontbufferTracking,
    pub hotplug: IntelHotplug,
    pub opregion: *mut c_void,
    pub overlay: *mut c_void,
    pub params: IntelDisplayParams,
    pub vbt: IntelVbtData,
    pub wl: IntelDmcWl,
    pub wm: IntelWm,
    pub psr_dc5_dc6_wa_work: WorkStruct,
}

#[repr(C)]
pub struct IntelBwInfo {
    pub deratedbw: [u32; 8],
    pub psf_bw: [u32; 3],
    pub peakbw: [u32; 8],
    pub num_qgv_points: u8,
    pub num_psf_gv_points: u8,
    pub num_planes: u8,
    pub _pad_79: [u8; 1],
}

#[repr(C, align(8))]
pub struct SysCacheCfg {
    pub lock: Mutex,
    pub id: IntelFbcId,
    pub _pad_28: [u8; 4],
}

const _: () = assert!(size_of::<IntelModesetFuncs>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, get_pipe_config) == 0);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, get_initial_plane_config) == 8);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, fixup_initial_plane_config) == 16);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, crtc_enable) == 24);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, crtc_disable) == 32);
const _: () = assert!(core::mem::offset_of!(IntelModesetFuncs, commit_modeset_enables) == 40);
const _: () = assert!(size_of::<IntelWmFuncs>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, update_wm) == 0);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, compute_watermarks) == 8);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, initial_watermarks) == 16);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, atomic_update_watermarks) == 24);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, optimize_watermarks) == 32);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, compute_global_watermarks) == 40);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, get_hw_state) == 48);
const _: () = assert!(core::mem::offset_of!(IntelWmFuncs, sanitize) == 56);
const _: () = assert!(size_of::<IntelAudioState>() == 136);
const _: () = assert!(core::mem::offset_of!(IntelAudioState, encoder) == 0);
const _: () = assert!(core::mem::offset_of!(IntelAudioState, eld) == 8);
const _: () = assert!(size_of::<[u8; 128]>() == 128);
const _: () = assert!(size_of::<IntelAudioLpe>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelAudioLpe, platdev) == 0);
const _: () = assert!(core::mem::offset_of!(IntelAudioLpe, irq) == 8);
const _: () = assert!(size_of::<IntelAudio>() == 1024);
const _: () = assert!(align_of::<IntelAudio>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelAudio, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelAudio, component) == 8);
const _: () = assert!(core::mem::offset_of!(IntelAudio, component_registered) == 16);
const _: () = assert!(core::mem::offset_of!(IntelAudio, mutex) == 24);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelAudio, power_refcount) == 48);
const _: () = assert!(core::mem::offset_of!(IntelAudio, freq_cntrl) == 52);
const _: () = assert!(core::mem::offset_of!(IntelAudio, state) == 56);
const _: () = assert!(size_of::<[IntelAudioState; 7]>() == 952);
const _: () = assert!(core::mem::offset_of!(IntelAudio, lpe) == 1008);
const _: () = assert!(size_of::<IntelDpllGlobalRefClks>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobalRefClks, nssc) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobalRefClks, ssc) == 4);
const _: () = assert!(size_of::<IntelDpllGlobal>() == 1072);
const _: () = assert!(align_of::<IntelDpllGlobal>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, lock) == 8);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, num_dpll) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, dplls) == 40);
const _: () = assert!(size_of::<[IntelDpll; 9]>() == 1008);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, mgr) == 1048);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, ref_clks) == 1056);
const _: () = assert!(core::mem::offset_of!(IntelDpllGlobal, pch_ssc_use) == 1064);
const _: () = assert!(size_of::<IntelFrontbufferTracking>() == 8);
const _: () = assert!(align_of::<IntelFrontbufferTracking>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelFrontbufferTracking, lock) == 0);
const _: () = assert!(size_of::<Spinlock>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelFrontbufferTracking, busy_bits) == 4);
const _: () = assert!(size_of::<IntelHotplugStats>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelHotplugStats, last_jiffies) == 0);
const _: () = assert!(core::mem::offset_of!(IntelHotplugStats, count) == 8);
const _: () = assert!(core::mem::offset_of!(IntelHotplugStats, blocked_count) == 12);
const _: () = assert!(core::mem::offset_of!(IntelHotplugStats, state) == 16);
const _: () = assert!(size_of::<IntelHotplug>() == 680);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, hotplug_work) == 8);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, hpd) == 96);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, pch_hpd) == 104);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, stats) == 112);
const _: () = assert!(size_of::<[IntelHotplugStats; 15]>() == 360);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, event_bits) == 472);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, retry_bits) == 476);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, reenable_work) == 480);
const _: () = assert!(size_of::<DelayedWork>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, long_hpd_pin_mask) == 568);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, short_hpd_pin_mask) == 572);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, dig_port_work) == 576);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, poll_init_work) == 608);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, poll_enabled) == 640);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, detection_work_enabled) == 641);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, hpd_storm_threshold) == 644);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, hpd_short_storm_enabled) == 648);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, oob_hotplug_last_state) == 656);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, dp_wq) == 664);
const _: () = assert!(core::mem::offset_of!(IntelHotplug, ignore_long_hpd) == 672);
const _: () = assert!(size_of::<IntelVbtData>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, version) == 0);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, _bitfields_2) == 2);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, lvds_ssc_freq) == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, orientation) == 8);
const _: () = assert!(size_of::<DrmPanelOrientation>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, override_afc_startup) == 12);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, override_afc_startup_val) == 13);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, crt_ddc_pin) == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, display_devices) == 24);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, bdb_blocks) == 40);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelVbtData, sdvo_mappings) == 56);
const _: () = assert!(size_of::<[SdvoDeviceMapping; 2]>() == 12);
const _: () = assert!(size_of::<SdvoDeviceMapping>() == 6);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, initialized) == 0);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, dvo_port) == 1);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, target_addr) == 2);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, dvo_wiring) == 3);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, i2c_pin) == 4);
const _: () = assert!(core::mem::offset_of!(SdvoDeviceMapping, ddc_pin) == 5);
const _: () = assert!(size_of::<IntelWmAnon56x5>() == 88);
const _: () = assert!(size_of::<IntelWm>() == 184);
const _: () = assert!(align_of::<IntelWm>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelWm, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelWm, pri_latency) == 8);
const _: () = assert!(size_of::<[u16; 5]>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelWm, spr_latency) == 18);
const _: () = assert!(size_of::<[u16; 5]>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelWm, cur_latency) == 28);
const _: () = assert!(size_of::<[u16; 5]>() == 10);
const _: () = assert!(core::mem::offset_of!(IntelWm, skl_latency) == 38);
const _: () = assert!(size_of::<[u16; 8]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelWm, _anon_56_5) == 56);
const _: () = assert!(core::mem::offset_of!(IntelWm, num_levels) == 144);
const _: () = assert!(core::mem::offset_of!(IntelWm, wm_mutex) == 152);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelWm, ipc_enabled) == 176);
const _: () = assert!(size_of::<IntelDisplayAccess>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayAccess, any_task_allowed) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayAccess, allowed_task) == 8);
const _: () = assert!(size_of::<IntelDisplayBacklight>() == 24);
const _: () = assert!(align_of::<IntelDisplayBacklight>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayBacklight, lock) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(size_of::<IntelDisplayBw>() == 512);
const _: () = assert!(core::mem::offset_of!(IntelDisplayBw, obj) == 0);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayBw, max) == 32);
const _: () = assert!(size_of::<[IntelBwInfo; 6]>() == 480);
const _: () = assert!(size_of::<IntelDisplayCdclk>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, hw) == 8);
const _: () = assert!(size_of::<IntelCdclkConfig>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, table) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, obj) == 40);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, max_cdclk_freq) == 72);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, max_dotclk_freq) == 76);
const _: () = assert!(core::mem::offset_of!(IntelDisplayCdclk, skl_preferred_vco_freq) == 80);
const _: () = assert!(size_of::<IntelDisplayColor>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayColor, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayColor, glk_linear_degamma_lut) == 8);
const _: () = assert!(size_of::<IntelDisplayDbuf>() == 40);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDbuf, enabled_slices) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDbuf, obj) == 8);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(size_of::<IntelDisplayDbufBw>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDbufBw, obj) == 0);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(size_of::<IntelDisplayDkl>() == 4);
const _: () = assert!(align_of::<IntelDisplayDkl>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDkl, phy_lock) == 0);
const _: () = assert!(size_of::<Spinlock>() == 4);
const _: () = assert!(size_of::<IntelDisplayDmc>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDmc, dmc) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDmc, wakeref) == 8);
const _: () = assert!(size_of::<IntelDisplayDsi>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDsi, mmio_base) == 0);
const _: () = assert!(size_of::<IntelDisplayDram>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayDram, info) == 0);
const _: () = assert!(size_of::<IntelDisplayFbc>() == 64);
const _: () = assert!(align_of::<IntelDisplayFbc>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFbc, instances) == 0);
const _: () = assert!(size_of::<[*mut c_void; 4]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFbc, sys_cache) == 32);
const _: () = assert!(size_of::<SysCacheCfg>() == 32);
const _: () = assert!(size_of::<IntelDisplayFbdev>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFbdev, fbdev) == 0);
const _: () = assert!(size_of::<IntelDisplayFdi>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFdi, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFdi, pll_freq) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayFdi, rx_config) == 12);
const _: () = assert!(size_of::<IntelDisplayGlobal>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayGlobal, obj_list) == 0);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(size_of::<IntelDisplayGmbus>() == 176);
const _: () = assert!(align_of::<IntelDisplayGmbus>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayGmbus, mmio_base) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayGmbus, mutex) == 8);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDisplayGmbus, bus) == 32);
const _: () = assert!(size_of::<[*mut c_void; 15]>() == 120);
const _: () = assert!(core::mem::offset_of!(IntelDisplayGmbus, wait_queue) == 152);
const _: () = assert!(size_of::<WaitQueueHead>() == 24);
const _: () = assert!(size_of::<IntelDisplayHdcp>() == 48);
const _: () = assert!(align_of::<IntelDisplayHdcp>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayHdcp, arbiter) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayHdcp, comp_added) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayHdcp, gsc_context) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayHdcp, hdcp_mutex) == 24);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(size_of::<IntelDisplayHti>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayHti, state) == 0);
const _: () = assert!(size_of::<IntelDisplayInfo>() == 48);
const _: () = assert!(core::mem::offset_of!(IntelDisplayInfo, __device_info) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayInfo, __runtime_info) == 8);
const _: () = assert!(size_of::<IntelDisplayRuntimeInfo>() == 40);
const _: () = assert!(size_of::<IntelDisplayIps>() == 1);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIps, false_color) == 0);
const _: () = assert!(size_of::<IntelDisplayIrq>() == 96);
const _: () = assert!(align_of::<IntelDisplayIrq>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, funcs) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, lock) == 8);
const _: () = assert!(size_of::<Spinlock>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vlv_display_irqs_enabled) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vblank_enabled) == 13);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vblank_enable_count) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vblank_status_last_notified) == 20);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vblank_notify_work) == 24);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, vlv_imr_mask) == 56);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, ilk_de_imr_mask) == 60);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, de_pipe_imr_mask) == 64);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrq, pipestat_irq_mask) == 80);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
const _: () = assert!(size_of::<IntelDisplayModeset>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayModeset, funcs) == 0);
const _: () = assert!(size_of::<IntelDisplayPkgc>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPkgc, linetime) == 0);
const _: () = assert!(size_of::<[u16; 4]>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPkgc, disable) == 8);
const _: () = assert!(size_of::<[bool; 4]>() == 4);
const _: () = assert!(size_of::<IntelDisplayPmdemand>() == 80);
const _: () = assert!(align_of::<IntelDisplayPmdemand>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPmdemand, waitqueue) == 0);
const _: () = assert!(size_of::<WaitQueueHead>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPmdemand, lock) == 24);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPmdemand, obj) == 48);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(size_of::<IntelDisplayPower>() == 520);
const _: () = assert!(align_of::<IntelDisplayPower>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPower, domains) == 0);
const _: () = assert!(size_of::<I915PowerDomains>() == 512);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPower, chv_phy_control) == 512);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPower, chv_phy_assert) == 516);
const _: () = assert!(size_of::<[bool; 2]>() == 2);
const _: () = assert!(size_of::<IntelDisplayPps>() == 32);
const _: () = assert!(align_of::<IntelDisplayPps>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPps, mmio_base) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayPps, mutex) == 8);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(size_of::<IntelDisplayProperties>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayProperties, broadcast_rgb) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayProperties, force_audio) == 8);
const _: () = assert!(size_of::<IntelDisplayQuirks>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayQuirks, mask) == 0);
const _: () = assert!(size_of::<IntelDisplayReset>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayReset, count) == 0);
const _: () = assert!(size_of::<IntelDisplayRestore>() == 224);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, modeset_state) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, reset_ctx) == 8);
const _: () = assert!(size_of::<DrmModesetAcquireCtx>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, saveDSPARB) == 72);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, saveSWF0) == 76);
const _: () = assert!(size_of::<[u32; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, saveSWF1) == 140);
const _: () = assert!(size_of::<[u32; 16]>() == 64);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, saveSWF3) == 204);
const _: () = assert!(size_of::<[u32; 3]>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelDisplayRestore, saveGCDGMBUS) == 216);
const _: () = assert!(size_of::<IntelDisplaySagv>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplaySagv, status) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplaySagv, block_time_us) == 4);
const _: () = assert!(size_of::<IntelDisplaySbi>() == 24);
const _: () = assert!(align_of::<IntelDisplaySbi>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplaySbi, lock) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(size_of::<IntelDisplaySnps>() == 1);
const _: () = assert!(core::mem::offset_of!(IntelDisplaySnps, phy_failed_calibration) == 0);
const _: () = assert!(size_of::<IntelDisplayState>() == 20);
const _: () = assert!(core::mem::offset_of!(IntelDisplayState, chv_dpll_md) == 0);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayState, bxt_phy_grc) == 16);
const _: () = assert!(size_of::<IntelDisplayVlvClock>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayVlvClock, hpll_freq) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayVlvClock, czclk_freq) == 4);
const _: () = assert!(size_of::<IntelDisplayWq>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplayWq, modeset) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayWq, flip) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayWq, cleanup) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayWq, unordered) == 24);
const _: () = assert!(size_of::<IntelDisplay>() == 5608);
const _: () = assert!(align_of::<IntelDisplay>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, drm) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, platform) == 8);
const _: () = assert!(size_of::<IntelDisplayPlatforms>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, pch_type) == 24);
const _: () = assert!(size_of::<IntelPch>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, parent) == 32);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, pipe_list) == 40);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, access) == 56);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, backlight) == 72);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, bw) == 96);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, cdclk) == 608);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, color) == 696);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dbuf) == 712);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dbuf_bw) == 752);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dkl) == 784);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dmc) == 792);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dsi) == 808);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dram) == 816);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, fbc) == 824);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, fbdev) == 888);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, fdi) == 896);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, global) == 912);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, gmbus) == 928);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, hdcp) == 1104);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, hti) == 1152);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, info) == 1160);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, ips) == 1208);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, irq) == 1216);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, modeset) == 1312);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, pkgc) == 1320);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, pmdemand) == 1336);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, power) == 1416);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, pps) == 1936);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, properties) == 1968);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, quirks) == 1984);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, reset) == 1992);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, restore) == 2000);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, sagv) == 2224);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, sbi) == 2232);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, snps) == 2256);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, state) == 2260);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, vlv_clock) == 2280);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, wq) == 2288);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dp_tunnel_mgr) == 2320);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, audio) == 2328);
const _: () = assert!(size_of::<IntelAudio>() == 1024);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, dpll) == 3352);
const _: () = assert!(size_of::<IntelDpllGlobal>() == 1072);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, fb_tracking) == 4424);
const _: () = assert!(size_of::<IntelFrontbufferTracking>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, hotplug) == 4432);
const _: () = assert!(size_of::<IntelHotplug>() == 680);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, opregion) == 5112);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, overlay) == 5120);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, params) == 5128);
const _: () = assert!(size_of::<IntelDisplayParams>() == 88);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, vbt) == 5216);
const _: () = assert!(size_of::<IntelVbtData>() == 72);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, wl) == 5288);
const _: () = assert!(size_of::<IntelDmcWl>() == 104);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, wm) == 5392);
const _: () = assert!(size_of::<IntelWm>() == 184);
const _: () = assert!(core::mem::offset_of!(IntelDisplay, psr_dc5_dc6_wa_work) == 5576);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(size_of::<IntelBwInfo>() == 80);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, deratedbw) == 0);
const _: () = assert!(size_of::<[u32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, psf_bw) == 32);
const _: () = assert!(size_of::<[u32; 3]>() == 12);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, peakbw) == 44);
const _: () = assert!(size_of::<[u32; 8]>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, num_qgv_points) == 76);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, num_psf_gv_points) == 77);
const _: () = assert!(core::mem::offset_of!(IntelBwInfo, num_planes) == 78);
const _: () = assert!(size_of::<SysCacheCfg>() == 32);
const _: () = assert!(align_of::<SysCacheCfg>() == 8);
const _: () = assert!(core::mem::offset_of!(SysCacheCfg, lock) == 0);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(SysCacheCfg, id) == 24);
const _: () = assert!(size_of::<IntelFbcId>() == 4);
