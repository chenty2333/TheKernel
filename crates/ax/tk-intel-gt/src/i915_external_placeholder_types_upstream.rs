// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drm/display/drm_dp_dual_mode_helper.h`
//! `include/drm/display/drm_dp_mst_helper.h`
//! `include/drm/display/drm_dp_tunnel.h`
//! `include/drm/display/drm_dsc.h`
//! `include/drm/drm_colorop.h`
//! `include/drm/drm_mipi_dsi.h`
//! `include/drm/intel/i915_hdcp_interface.h`
//! `include/linux/irqreturn.h`
//! `include/linux/plist_types.h`
//! `include/linux/pm_qos.h`
//! `include/linux/pwm.h`
//! `include/uapi/drm/i915_drm.h`
//! `uapi/drm/drm_mode.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::ListHead;
use tk_linux_kapi::types::Mutex;
use tk_linux_kapi::types::WaitQueueHead;
use tk_linux_kapi::types::WorkStruct;
use tk_drm_upstream::drm_atomic_types::{DrmPrivateObj, DrmPrivateStateFuncs};
use tk_drm_upstream::drm_device_types::{DrmDevice};
use tk_drm_upstream::drm_dp_helper_types::{DrmDpAux};
use tk_drm_upstream::drm_mode_object_types::{DrmModeObject, DrmObjectProperties};
use tk_drm_upstream::drm_plane_types::{DrmPlane};
use tk_drm_upstream::drm_property_types::{DrmProperty};

#[repr(C)]
pub struct PwmState {
    pub period: u64,
    pub duty_cycle: u64,
    pub polarity: PwmPolarity,
    pub enabled: bool,
    pub usage_power: bool,
    pub _pad_22: [u8; 2],
}

#[repr(C)]
pub struct DrmIntelSpriteColorkey {
    pub plane_id: u32,
    pub min_value: u32,
    pub channel_mask: u32,
    pub max_value: u32,
    pub flags: u32,
}

#[repr(C)]
pub struct DrmDscConfig {
    pub line_buf_depth: u8,
    pub bits_per_component: u8,
    pub convert_rgb: bool,
    pub slice_count: u8,
    pub slice_width: u16,
    pub slice_height: u16,
    pub simple_422: bool,
    pub _pad_9: [u8; 1],
    pub pic_width: u16,
    pub pic_height: u16,
    pub rc_tgt_offset_high: u8,
    pub rc_tgt_offset_low: u8,
    pub bits_per_pixel: u16,
    pub rc_edge_factor: u8,
    pub rc_quant_incr_limit1: u8,
    pub rc_quant_incr_limit0: u8,
    pub _pad_21: [u8; 1],
    pub initial_xmit_delay: u16,
    pub initial_dec_delay: u16,
    pub block_pred_enable: bool,
    pub first_line_bpg_offset: u8,
    pub initial_offset: u16,
    pub rc_buf_thresh: [u16; 14],
    pub rc_range_params: [DrmDscRcRangeParameters; 15],
    pub _pad_103: [u8; 1],
    pub rc_model_size: u16,
    pub flatness_min_qp: u8,
    pub flatness_max_qp: u8,
    pub initial_scale_value: u8,
    pub _pad_109: [u8; 1],
    pub scale_decrement_interval: u16,
    pub scale_increment_interval: u16,
    pub nfl_bpg_offset: u16,
    pub slice_bpg_offset: u16,
    pub final_offset: u16,
    pub vbr_enable: bool,
    pub mux_word_size: u8,
    pub slice_chunk_size: u16,
    pub rc_bits: u16,
    pub dsc_version_minor: u8,
    pub dsc_version_major: u8,
    pub native_422: bool,
    pub native_420: bool,
    pub second_line_bpg_offset: u8,
    pub _pad_131: [u8; 1],
    pub nsl_bpg_offset: u16,
    pub second_line_offset_adj: u16,
}

#[repr(C)]
pub struct DrmDpTunnelRef {
    pub tunnel: *mut c_void,
    pub tracker: *mut c_void,
}

#[repr(C)]
pub struct PmQosRequest {
    pub node: PlistNode,
    pub qos: *mut PmQosConstraints,
}

#[repr(C, align(8))]
pub struct DrmDpMstTopologyMgr {
    pub base: DrmPrivateObj,
    pub dev: *mut DrmDevice,
    pub cbs: *mut DrmDpMstTopologyCbs,
    pub max_dpcd_transaction_bytes: i32,
    pub _pad_108: [u8; 4],
    pub aux: *mut DrmDpAux,
    pub max_payloads: i32,
    pub conn_base_id: i32,
    pub up_req_recv: DrmDpSidebandMsgRx,
    pub down_rep_recv: DrmDpSidebandMsgRx,
    pub _pad_780: [u8; 4],
    pub lock: Mutex,
    pub probe_lock: Mutex,
    /// bit-fields mst_state:1, payload_id_table_cleared:1, reset_rx_state:1
    pub _bitfields_832: [u8; 1],
    pub payload_count: u8,
    pub next_start_slot: u8,
    pub _pad_835: [u8; 5],
    pub mst_primary: *mut DrmDpMstBranch,
    pub dpcd: [u8; 15],
    pub sink_count: u8,
    pub funcs: *mut DrmPrivateStateFuncs,
    pub qlock: Mutex,
    pub tx_msg_downq: ListHead,
    pub tx_waitq: WaitQueueHead,
    pub work: WorkStruct,
    pub tx_work: WorkStruct,
    pub destroy_port_list: ListHead,
    pub destroy_branch_device_list: ListHead,
    pub delayed_destroy_lock: Mutex,
    pub delayed_destroy_wq: *mut c_void,
    pub delayed_destroy_work: WorkStruct,
    pub up_req_list: ListHead,
    pub up_req_lock: Mutex,
    pub up_req_work: WorkStruct,
}

#[repr(C)]
pub struct HdcpPortData {
    pub hdcp_ddi: HdcpDdi,
    pub hdcp_transcoder: HdcpTranscoder,
    pub port_type: u8,
    pub protocol: u8,
    pub k: u16,
    pub seq_num_m: u32,
    pub streams: *mut c_void,
}

#[repr(C, align(8))]
pub struct DrmColorop {
    pub dev: *mut DrmDevice,
    pub head: ListHead,
    pub index: u32,
    pub _pad_28: [u8; 4],
    pub base: DrmModeObject,
    pub plane: *mut DrmPlane,
    pub state: *mut DrmColoropState,
    pub properties: DrmObjectProperties,
    pub r#type: DrmColoropType,
    pub _pad_1116: [u8; 4],
    pub next: *mut DrmColorop,
    pub type_property: *mut DrmProperty,
    pub bypass_property: *mut DrmProperty,
    pub size: u32,
    pub _pad_1148: [u8; 4],
    pub lut1d_interpolation_property: *mut DrmProperty,
    pub curve_1d_type_property: *mut DrmProperty,
    pub multiplier_property: *mut DrmProperty,
    pub size_property: *mut DrmProperty,
    pub lut3d_interpolation_property: *mut DrmProperty,
    pub data_property: *mut DrmProperty,
    pub next_property: *mut DrmProperty,
    pub funcs: *mut DrmColoropFuncs,
}

#[repr(C)]
pub struct MipiDsiHost {
    pub dev: *mut c_void,
    pub ops: *mut MipiDsiHostOps,
    pub list: ListHead,
}

#[repr(C)]
pub struct DrmDscRcRangeParameters {
    pub range_min_qp: u8,
    pub range_max_qp: u8,
    pub range_bpg_offset: u8,
}

#[repr(C)]
pub struct PlistNode {
    pub prio: i32,
    pub _pad_4: [u8; 4],
    pub prio_list: ListHead,
    pub node_list: ListHead,
}

#[repr(C)]
pub struct DrmDpSidebandMsgRx {
    pub chunk: [u8; 48],
    pub msg: [u8; 256],
    pub curchunk_len: u8,
    pub curchunk_idx: u8,
    pub curchunk_hdrlen: u8,
    pub curlen: u8,
    pub have_somt: bool,
    pub have_eomt: bool,
    pub initial_hdr: DrmDpSidebandMsgHdr,
}

#[repr(C)]
pub struct DrmDpSidebandMsgHdr {
    pub lct: u8,
    pub lcr: u8,
    pub rad: [u8; 8],
    pub broadcast: bool,
    pub path_msg: bool,
    pub msg_len: u8,
    pub somt: bool,
    pub eomt: bool,
    pub seqno: bool,
}

pub type DrmDpDualModeType = u32;
pub const DRM_DP_DUAL_MODE_NONE: DrmDpDualModeType = 0;
pub const DRM_DP_DUAL_MODE_UNKNOWN: DrmDpDualModeType = 1;
pub const DRM_DP_DUAL_MODE_TYPE1_DVI: DrmDpDualModeType = 2;
pub const DRM_DP_DUAL_MODE_TYPE1_HDMI: DrmDpDualModeType = 3;
pub const DRM_DP_DUAL_MODE_TYPE2_DVI: DrmDpDualModeType = 4;
pub const DRM_DP_DUAL_MODE_TYPE2_HDMI: DrmDpDualModeType = 5;
pub const DRM_DP_DUAL_MODE_LSPCON: DrmDpDualModeType = 6;

pub type DrmDpMstMode = u32;
pub const DRM_DP_SST: DrmDpMstMode = 0;
pub const DRM_DP_MST: DrmDpMstMode = 1;
pub const DRM_DP_SST_SIDEBAND_MSG: DrmDpMstMode = 2;

pub type DrmLspconMode = u32;
pub const DRM_LSPCON_MODE_INVALID: DrmLspconMode = 0;
pub const DRM_LSPCON_MODE_LS: DrmLspconMode = 1;
pub const DRM_LSPCON_MODE_PCON: DrmLspconMode = 2;

pub type Irqreturn = u32;
pub const IRQ_NONE: Irqreturn = 0;
pub const IRQ_HANDLED: Irqreturn = 1;
pub const IRQ_WAKE_THREAD: Irqreturn = 2;

pub type MipiDsiPixelFormat = u32;
pub const MIPI_DSI_FMT_RGB888: MipiDsiPixelFormat = 0;
pub const MIPI_DSI_FMT_RGB666: MipiDsiPixelFormat = 1;
pub const MIPI_DSI_FMT_RGB666_PACKED: MipiDsiPixelFormat = 2;
pub const MIPI_DSI_FMT_RGB565: MipiDsiPixelFormat = 3;
pub const MIPI_DSI_FMT_RGB101010: MipiDsiPixelFormat = 4;

pub type HdcpWiredProtocol = u32;
pub const HDCP_PROTOCOL_INVALID: HdcpWiredProtocol = 0;
pub const HDCP_PROTOCOL_HDMI: HdcpWiredProtocol = 1;
pub const HDCP_PROTOCOL_DP: HdcpWiredProtocol = 2;

pub type PwmPolarity = u32;
pub const PWM_POLARITY_NORMAL: PwmPolarity = 0;
pub const PWM_POLARITY_INVERSED: PwmPolarity = 1;

pub type HdcpDdi = u32;
pub const HDCP_DDI_INVALID_PORT: HdcpDdi = 0;
pub const HDCP_DDI_B: HdcpDdi = 1;
pub const HDCP_DDI_C: HdcpDdi = 2;
pub const HDCP_DDI_D: HdcpDdi = 3;
pub const HDCP_DDI_E: HdcpDdi = 4;
pub const HDCP_DDI_F: HdcpDdi = 5;
pub const HDCP_DDI_A: HdcpDdi = 7;
pub const HDCP_DDI_RANGE_END: HdcpDdi = 7;

pub type HdcpTranscoder = u32;
pub const HDCP_INVALID_TRANSCODER: HdcpTranscoder = 0;
pub const HDCP_TRANSCODER_EDP: HdcpTranscoder = 1;
pub const HDCP_TRANSCODER_DSI0: HdcpTranscoder = 2;
pub const HDCP_TRANSCODER_DSI1: HdcpTranscoder = 3;
pub const HDCP_TRANSCODER_A: HdcpTranscoder = 16;
pub const HDCP_TRANSCODER_B: HdcpTranscoder = 17;
pub const HDCP_TRANSCODER_C: HdcpTranscoder = 18;
pub const HDCP_TRANSCODER_D: HdcpTranscoder = 19;

pub type DrmColoropType = u32;
pub const DRM_COLOROP_1D_CURVE: DrmColoropType = 0;
pub const DRM_COLOROP_1D_LUT: DrmColoropType = 1;
pub const DRM_COLOROP_CTM_3X4: DrmColoropType = 2;
pub const DRM_COLOROP_MULTIPLIER: DrmColoropType = 3;
pub const DRM_COLOROP_3D_LUT: DrmColoropType = 4;

#[repr(C)]
pub struct DrmColoropFuncs {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmColoropState {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmDpMstBranch {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmDpMstPort {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct DrmDpMstTopologyCbs {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct I915HdcpArbiter {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct MipiDsiDevice {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct MipiDsiHostOps {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct PmQosConstraints {
    _opaque: [u8; 0],
}

#[repr(C)]
pub struct PwmDevice {
    _opaque: [u8; 0],
}

const _: () = assert!(size_of::<PwmState>() == 24);
const _: () = assert!(core::mem::offset_of!(PwmState, period) == 0);
const _: () = assert!(core::mem::offset_of!(PwmState, duty_cycle) == 8);
const _: () = assert!(core::mem::offset_of!(PwmState, polarity) == 16);
const _: () = assert!(size_of::<PwmPolarity>() == 4);
const _: () = assert!(core::mem::offset_of!(PwmState, enabled) == 20);
const _: () = assert!(core::mem::offset_of!(PwmState, usage_power) == 21);
const _: () = assert!(size_of::<DrmIntelSpriteColorkey>() == 20);
const _: () = assert!(core::mem::offset_of!(DrmIntelSpriteColorkey, plane_id) == 0);
const _: () = assert!(core::mem::offset_of!(DrmIntelSpriteColorkey, min_value) == 4);
const _: () = assert!(core::mem::offset_of!(DrmIntelSpriteColorkey, channel_mask) == 8);
const _: () = assert!(core::mem::offset_of!(DrmIntelSpriteColorkey, max_value) == 12);
const _: () = assert!(core::mem::offset_of!(DrmIntelSpriteColorkey, flags) == 16);
const _: () = assert!(size_of::<DrmDscConfig>() == 136);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, line_buf_depth) == 0);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, bits_per_component) == 1);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, convert_rgb) == 2);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, slice_count) == 3);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, slice_width) == 4);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, slice_height) == 6);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, simple_422) == 8);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, pic_width) == 10);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, pic_height) == 12);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_tgt_offset_high) == 14);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_tgt_offset_low) == 15);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, bits_per_pixel) == 16);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_edge_factor) == 18);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_quant_incr_limit1) == 19);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_quant_incr_limit0) == 20);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, initial_xmit_delay) == 22);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, initial_dec_delay) == 24);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, block_pred_enable) == 26);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, first_line_bpg_offset) == 27);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, initial_offset) == 28);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_buf_thresh) == 30);
const _: () = assert!(size_of::<[u16; 14]>() == 28);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_range_params) == 58);
const _: () = assert!(size_of::<[DrmDscRcRangeParameters; 15]>() == 45);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_model_size) == 104);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, flatness_min_qp) == 106);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, flatness_max_qp) == 107);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, initial_scale_value) == 108);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, scale_decrement_interval) == 110);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, scale_increment_interval) == 112);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, nfl_bpg_offset) == 114);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, slice_bpg_offset) == 116);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, final_offset) == 118);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, vbr_enable) == 120);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, mux_word_size) == 121);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, slice_chunk_size) == 122);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, rc_bits) == 124);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, dsc_version_minor) == 126);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, dsc_version_major) == 127);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, native_422) == 128);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, native_420) == 129);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, second_line_bpg_offset) == 130);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, nsl_bpg_offset) == 132);
const _: () = assert!(core::mem::offset_of!(DrmDscConfig, second_line_offset_adj) == 134);
const _: () = assert!(size_of::<DrmDpTunnelRef>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpTunnelRef, tunnel) == 0);
const _: () = assert!(core::mem::offset_of!(DrmDpTunnelRef, tracker) == 8);
const _: () = assert!(size_of::<PmQosRequest>() == 48);
const _: () = assert!(core::mem::offset_of!(PmQosRequest, node) == 0);
const _: () = assert!(size_of::<PlistNode>() == 40);
const _: () = assert!(core::mem::offset_of!(PmQosRequest, qos) == 40);
const _: () = assert!(size_of::<DrmDpMstTopologyMgr>() == 1168);
const _: () = assert!(align_of::<DrmDpMstTopologyMgr>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, base) == 0);
const _: () = assert!(size_of::<DrmPrivateObj>() == 88);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, dev) == 88);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, cbs) == 96);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, max_dpcd_transaction_bytes) == 104);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, aux) == 112);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, max_payloads) == 120);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, conn_base_id) == 124);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, up_req_recv) == 128);
const _: () = assert!(size_of::<DrmDpSidebandMsgRx>() == 326);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, down_rep_recv) == 454);
const _: () = assert!(size_of::<DrmDpSidebandMsgRx>() == 326);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, lock) == 784);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, probe_lock) == 808);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, _bitfields_832) == 832);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, payload_count) == 833);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, next_start_slot) == 834);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, mst_primary) == 840);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, dpcd) == 848);
const _: () = assert!(size_of::<[u8; 15]>() == 15);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, sink_count) == 863);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, funcs) == 864);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, qlock) == 872);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, tx_msg_downq) == 896);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, tx_waitq) == 912);
const _: () = assert!(size_of::<WaitQueueHead>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, work) == 936);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, tx_work) == 968);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, destroy_port_list) == 1000);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, destroy_branch_device_list) == 1016);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, delayed_destroy_lock) == 1032);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, delayed_destroy_wq) == 1056);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, delayed_destroy_work) == 1064);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, up_req_list) == 1096);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, up_req_lock) == 1112);
const _: () = assert!(size_of::<Mutex>() == 24);
const _: () = assert!(core::mem::offset_of!(DrmDpMstTopologyMgr, up_req_work) == 1136);
const _: () = assert!(size_of::<WorkStruct>() == 32);
const _: () = assert!(size_of::<HdcpPortData>() == 24);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, hdcp_ddi) == 0);
const _: () = assert!(size_of::<HdcpDdi>() == 4);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, hdcp_transcoder) == 4);
const _: () = assert!(size_of::<HdcpTranscoder>() == 4);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, port_type) == 8);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, protocol) == 9);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, k) == 10);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, seq_num_m) == 12);
const _: () = assert!(core::mem::offset_of!(HdcpPortData, streams) == 16);
const _: () = assert!(size_of::<DrmColorop>() == 1216);
const _: () = assert!(align_of::<DrmColorop>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmColorop, dev) == 0);
const _: () = assert!(core::mem::offset_of!(DrmColorop, head) == 8);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmColorop, index) == 24);
const _: () = assert!(core::mem::offset_of!(DrmColorop, base) == 32);
const _: () = assert!(size_of::<DrmModeObject>() == 32);
const _: () = assert!(core::mem::offset_of!(DrmColorop, plane) == 64);
const _: () = assert!(core::mem::offset_of!(DrmColorop, state) == 72);
const _: () = assert!(core::mem::offset_of!(DrmColorop, properties) == 80);
const _: () = assert!(size_of::<DrmObjectProperties>() == 1032);
const _: () = assert!(core::mem::offset_of!(DrmColorop, r#type) == 1112);
const _: () = assert!(size_of::<DrmColoropType>() == 4);
const _: () = assert!(core::mem::offset_of!(DrmColorop, next) == 1120);
const _: () = assert!(core::mem::offset_of!(DrmColorop, type_property) == 1128);
const _: () = assert!(core::mem::offset_of!(DrmColorop, bypass_property) == 1136);
const _: () = assert!(core::mem::offset_of!(DrmColorop, size) == 1144);
const _: () = assert!(core::mem::offset_of!(DrmColorop, lut1d_interpolation_property) == 1152);
const _: () = assert!(core::mem::offset_of!(DrmColorop, curve_1d_type_property) == 1160);
const _: () = assert!(core::mem::offset_of!(DrmColorop, multiplier_property) == 1168);
const _: () = assert!(core::mem::offset_of!(DrmColorop, size_property) == 1176);
const _: () = assert!(core::mem::offset_of!(DrmColorop, lut3d_interpolation_property) == 1184);
const _: () = assert!(core::mem::offset_of!(DrmColorop, data_property) == 1192);
const _: () = assert!(core::mem::offset_of!(DrmColorop, next_property) == 1200);
const _: () = assert!(core::mem::offset_of!(DrmColorop, funcs) == 1208);
const _: () = assert!(size_of::<MipiDsiHost>() == 32);
const _: () = assert!(core::mem::offset_of!(MipiDsiHost, dev) == 0);
const _: () = assert!(core::mem::offset_of!(MipiDsiHost, ops) == 8);
const _: () = assert!(core::mem::offset_of!(MipiDsiHost, list) == 16);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(size_of::<DrmDscRcRangeParameters>() == 3);
const _: () = assert!(core::mem::offset_of!(DrmDscRcRangeParameters, range_min_qp) == 0);
const _: () = assert!(core::mem::offset_of!(DrmDscRcRangeParameters, range_max_qp) == 1);
const _: () = assert!(core::mem::offset_of!(DrmDscRcRangeParameters, range_bpg_offset) == 2);
const _: () = assert!(size_of::<PlistNode>() == 40);
const _: () = assert!(core::mem::offset_of!(PlistNode, prio) == 0);
const _: () = assert!(core::mem::offset_of!(PlistNode, prio_list) == 8);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(PlistNode, node_list) == 24);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(size_of::<DrmDpSidebandMsgRx>() == 326);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, chunk) == 0);
const _: () = assert!(size_of::<[u8; 48]>() == 48);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, msg) == 48);
const _: () = assert!(size_of::<[u8; 256]>() == 256);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, curchunk_len) == 304);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, curchunk_idx) == 305);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, curchunk_hdrlen) == 306);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, curlen) == 307);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, have_somt) == 308);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, have_eomt) == 309);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgRx, initial_hdr) == 310);
const _: () = assert!(size_of::<DrmDpSidebandMsgHdr>() == 16);
const _: () = assert!(size_of::<DrmDpSidebandMsgHdr>() == 16);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, lct) == 0);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, lcr) == 1);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, rad) == 2);
const _: () = assert!(size_of::<[u8; 8]>() == 8);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, broadcast) == 10);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, path_msg) == 11);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, msg_len) == 12);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, somt) == 13);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, eomt) == 14);
const _: () = assert!(core::mem::offset_of!(DrmDpSidebandMsgHdr, seqno) == 15);
