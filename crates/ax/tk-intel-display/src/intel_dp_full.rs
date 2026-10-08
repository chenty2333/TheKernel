// Copyright © 2008 Intel Corporation.
// License: MIT (the upstream source carries the full MIT grant; see LICENSE-MIT).
// Faithful display-12/13-oriented translation of Linux 7.2.3
// drivers/gpu/drm/i915/display/intel_dp.c. Framework work is represented by
// narrow traits and explicit input/output state; behavior not selected by the
// caller is not silently synthesized here.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::cmp::{max, min};

pub const DP_MAX_SUPPORTED_RATES: usize = 16;
pub const DP_DSC_MAX_LINE_BUF_DEPTH: u8 = 13;
pub const DP_DSC_FEC_OVERHEAD_FACTOR: u32 = 1_028_530;
pub const DP_DSC_MIN_SLICE_WIDTH: u16 = 256;
pub const DP_DSC_RC_MODEL_SIZE_CONST: u16 = 8192;
pub const DP_DPCD_REV_12: u8 = 0x12;
pub const DP_DPCD_REV_14: u8 = 0x14;

const VALID_DSC_BPP: [u8; 5] = [6, 8, 10, 12, 15];
const ICL_SOURCE_RATES: [u32; 10] = [
    162_000, 216_000, 270_000, 324_000, 432_000, 540_000, 648_000, 810_000, 1_000_000, 1_350_000,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputFormat {
    #[default]
    Rgb,
    Ycbcr444,
    Ycbcr420,
    Ycbcr422,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PortKind {
    #[default]
    Combo,
    Dkl,
    C10,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayInfo {
    pub display_ver: u8,
    pub platform_dg2: bool,
    pub platform_icl: bool,
    pub platform_ehl_jsl: bool,
    pub platform_alderlake: bool,
    pub port_kind: PortKind,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpLinkConfig {
    pub link_rate_idx: u8,
    pub lane_count_exp: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DpLinkState {
    pub source_rates: Vec<u32>,
    pub sink_rates: Vec<u32>,
    pub common_rates: Vec<u32>,
    pub configs: Vec<DpLinkConfig>,
    pub max_common_lane_count: u8,
    pub max_sink_lane_count: u8,
    pub max_source_lane_count: u8,
    pub tc_lane_count: u8,
    pub max_rate: u32,
    pub lttpr_max_rate: u32,
    pub lttpr_max_lane_count: u8,
    pub max_lane_count: u8,
    pub force_rate: u32,
    pub force_lane_count: u8,
    pub mst_probed_lane_count: u8,
    pub mst_probed_rate: u32,
    pub active: bool,
    pub retrain_disabled: bool,
    pub force_retrain: bool,
    pub seq_train_failures: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DpSinkCaps {
    /// Base receiver capability bytes beginning at DPCD address zero.
    pub dpcd: Vec<u8>,
    pub lttpr_caps: Vec<u8>,
    pub uhbr_rate_mask: u8,
    pub max_lanes: u8,
    pub max_rate: u32,
    pub branch: bool,
    pub quirk_max_rate_3_24: bool,
    pub lttpr_count: u8,
    pub lttpr_supports_uhbr: bool,
    pub lttpr_uhbr_rate_mask: u8,
    pub tunneling_max_rate: Option<u32>,
    pub tunneling_max_lanes: Option<u8>,
    pub tunneling_available_bw: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkConfigLimits {
    pub min_rate: u32,
    pub max_rate: u32,
    pub min_lane_count: u8,
    pub max_lane_count: u8,
    pub min_pipe_bpp: u8,
    pub max_pipe_bpp: u8,
    pub min_link_bpp_x16: i32,
    pub max_link_bpp_x16: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpConfigPolicy {
    pub display_ver: u8,
    pub mode: DisplayMode,
    pub output: OutputFormat,
    pub min_rate: u32,
    pub max_rate: u32,
    pub min_lane_count: u8,
    pub max_lane_count: u8,
    pub requested_max_pipe_bpp: u8,
    pub connector_max_pipe_bpp: u8,
    pub max_link_bpp_x16: u16,
    pub joined_pipes: u8,
    pub max_cdclk: u32,
    pub max_dotclk: u32,
    pub force_dsc: bool,
    pub force_dsc_bpc: u8,
    pub force_dsc_fractional_bpp: bool,
    pub mst: bool,
    pub hdr: bool,
    pub use_max_params: bool,
    pub respect_downstream_limits: bool,
    pub dsc: bool,
    pub dsc_bpp_step_x16: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpPipeConfig {
    pub mode: DisplayMode,
    pub output_format: OutputFormat,
    pub sink_format: OutputFormat,
    pub pipe_bpp: u8,
    pub max_pipe_bpp: u8,
    pub port_clock: u32,
    pub lane_count: u8,
    pub dsc: bool,
    pub compressed_bpp_x16: u16,
    pub fec_enable: bool,
    pub limited_color_range: bool,
    pub enhanced_framing: bool,
    pub mst_master_transcoder: bool,
    pub has_audio: bool,
    pub sdp_split_enable: bool,
    pub has_psr: bool,
    pub has_vrr: bool,
    pub panel_replay: bool,
    pub has_drrs: bool,
    pub splitter_links: u8,
    pub splitter_overlap: u16,
    pub min_hblank: u16,
    pub mode_dblscan: bool,
    pub mode_interlaced: bool,
    pub mode_dblclk: bool,
    pub m_n_valid: bool,
    pub m2_n2_valid: bool,
    pub infoframe_enable: u32,
    pub dp_m_n: DpMn,
    pub dp_m2_n2: DpMn,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpMn {
    pub data_m: u32,
    pub data_n: u32,
    pub link_m: u32,
    pub link_n: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrrsPolicy {
    pub vrr_enabled: bool,
    pub psr_enabled: bool,
    pub has_pch_encoder: bool,
    pub transcoder_has_drrs: bool,
    pub downclock_mode_clock: u32,
    pub seamless_m_n: bool,
    pub joined_pipes: bool,
    pub ironlake_or_sandybridge_or_ivybridge: bool,
    pub vbt_msa_timing_delay: u8,
    pub splitter_links: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrrsResult {
    pub update_m_n: bool,
    pub has_drrs: bool,
    pub msa_timing_delay: u8,
    pub m2_n2: DpMn,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpAtomicConnector {
    pub has_tile: bool,
    pub tile_group_id: u32,
    pub old_crtc: Option<usize>,
    pub new_crtc: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpAtomicCrtc {
    pub transcoder: u8,
    pub pipe: u8,
    pub hw_active: bool,
    pub hw_enable: bool,
    pub sync_mode_slaves_mask: u8,
    pub master_transcoder: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpModeRequest {
    pub display_ver: u8,
    pub target_clock: u32,
    pub mode: DisplayMode,
    pub dbldclk: bool,
    pub dblscan: bool,
    pub interlaced: bool,
    pub interlace_allowed: bool,
    pub has_ddi: bool,
    pub is_edp: bool,
    pub sink_420_only: bool,
    pub sink_420_also: bool,
    pub is_hdmi_sink: bool,
    pub min_tmds_clock: u32,
    pub ycbcr_420_allowed: bool,
    pub dsc_supported: bool,
    pub fec_supported: bool,
    pub max_link_rate: u32,
    pub max_lane_count: u8,
    pub max_cdclk: u32,
    pub max_dotclk: u32,
    pub max_uncompressed_dotclk: u32,
    pub display_hdisplay_limit: u32,
    pub dsc_bpp_step_x16: u16,
    pub force_dsc: bool,
    pub has_gmch: bool,
    pub ironlake: bool,
    pub is_branch: bool,
    pub rgb_to_ycbcr: bool,
    pub ycbcr444_to_420: bool,
    pub forced_output_format: Option<OutputFormat>,
    pub has_uncompressed_joiner: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayMode {
    pub clock_khz: u32,
    pub hdisplay: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub htotal: u32,
    pub vdisplay: u32,
    pub interlaced: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SelectedLink {
    pub rate: u32,
    pub lane_count: u8,
    pub pipe_bpp: u8,
    pub compressed_bpp_x16: u16,
    pub dsc: bool,
    pub fec: bool,
    pub joiner_pipes: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeStatus {
    Ok,
    ClockLow,
    ClockHigh,
    No420,
    Bad,
    HorizontalIllegal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioForce {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MstMode {
    #[default]
    Sst,
    SstSideband,
    Mst,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectorStatus {
    #[default]
    Disconnected,
    Connected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DpConnectorType {
    #[default]
    DisplayPort,
    EmbeddedDisplayPort,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DscSliceConfig {
    pub slices_per_pipe: u8,
    pub pipes: u8,
    pub slices_per_line: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DscSinkCaps {
    pub dpcd: [u8; 16],
    pub max_slice_width: u16,
    pub max_line_width: u16,
    pub max_slices: u8,
    /// Bit n means exactly n slices per line is accepted (1..=24).
    pub slice_count_mask: u32,
    pub throughput_rgb_yuv444: u32,
    pub throughput_yuv422_420: u32,
    pub input_bpc_mask: u8,
    pub sink_bpp_incr: u8,
    pub sink_max_bpp_x16: u16,
    pub sink_min_bpp: u8,
    pub supports: bool,
    pub supports_ycbcr420: bool,
    pub branch: bool,
    pub throughput_quirk: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DscConfig {
    pub dsc_major: u8,
    pub dsc_minor: u8,
    pub line_buf_depth: u8,
    pub input_bpc: u8,
    pub compressed_bpp_x16: u16,
    pub slice_count: u8,
    pub slice_height: u16,
    pub slice_width: u16,
    pub pic_width: u16,
    pub pic_height: u16,
    pub rc_model_size: u16,
    pub convert_rgb: bool,
    pub block_pred_enable: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SdpHeader {
    pub hb0: u8,
    pub hb1: u8,
    pub hb2: u8,
    pub hb3: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpSdp {
    pub header: SdpHeader,
    pub db: [u8; 32],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdaptiveSyncSdp {
    pub sdp_type: u8,
    pub revision: u8,
    pub length: u8,
    pub mode: u8,
    pub vtotal: u16,
    pub target_rr: u16,
    pub target_rr_divider: bool,
    pub coasting_vtotal: u16,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DpRuntimeState {
    pub dpcd: Vec<u8>,
    pub downstream_ports: Vec<u8>,
    pub link: DpLinkState,
    pub dsc_enabled: bool,
    pub source_oui_valid: bool,
    pub last_oui_write_ms: u64,
    pub sink_count: u8,
    pub reset_link_params: bool,
    pub downstream_port_changed: bool,
    pub needs_modeset_retry: bool,
    pub sink_alpm_error: bool,
    pub colorimetry_support: bool,
    pub is_mst: bool,
    pub mst_detect: MstMode,
    pub as_sdp_supported: bool,
    pub as_sdp_v2_supported: bool,
    pub panel_replay_supported: bool,
    pub pcon_dsc_dpcd: Vec<u8>,
    pub hardware_control: u32,
    pub edp_dpcd: Vec<u8>,
    pub use_max_params: bool,
    pub alpm_dpcd: u8,
    pub frl_is_trained: bool,
    pub frl_trained_rate_gbps: u8,
    pub train_set: [u8; 4],
    pub link_rate: u32,
    pub lane_count: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FastsetState {
    pub port_clock: u32,
    pub dsc_enabled: bool,
    pub connectors_changed: bool,
    pub mode_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IrqReturn {
    #[default]
    Handled,
    None,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpDisplayInfo {
    pub is_hdmi: bool,
    pub has_audio: bool,
    pub max_bpc: u8,
    pub max_tmds_clock: u32,
    pub ycbcr_420_allowed: bool,
    pub mso_pixel_overlap: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpDownstreamCaps {
    pub max_bpc: u8,
    pub max_dotclock: u32,
    pub min_tmds_clock: u32,
    pub max_tmds_clock: u32,
    pub pcon_max_frl_bw: u8,
    pub ycbcr420_passthrough: bool,
    pub ycbcr444_to_420: bool,
    pub rgb_to_ycbcr: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DpEdidState {
    pub info: DpDisplayInfo,
    pub downstream: DpDownstreamCaps,
    pub has_edid: bool,
    pub vrr_capable: bool,
    pub cec_physical_address: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DscAuxMember {
    pub topology_id: u64,
    pub aux_id: u64,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ColorState {
    pub output_format: OutputFormat,
    pub pipe_bpp: u8,
    pub limited_color_range: bool,
    pub colorspace: DpColorimetry,
    pub panel_replay: bool,
    pub psr: bool,
    pub selective_update: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DpColorimetry {
    #[default]
    Default,
    Bt709Ycc,
    Xvycc601,
    Xvycc709,
    DciP3RgbD65,
    DciP3RgbTheater,
    Sycc601,
    Opycc601,
    Bt2020Rgb,
    Bt2020Cycc,
    Bt2020Ycc,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VscSdp {
    pub sdp_type: u8,
    pub revision: u8,
    pub length: u8,
    pub pixelformat: u8,
    pub colorimetry: u8,
    pub dynamic_range: u8,
    pub bpc: u8,
    pub content_type: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HdrMetadataInfoframe {
    pub kind: u8,
    pub version: u8,
    pub length: u8,
    pub payload: [u8; 26],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SdpGuardbandState {
    pub enable: u32,
    pub dsc_enabled: bool,
    pub vrr_enabled: bool,
    pub vrr_vsync_start: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DpError {
    Invalid,
    NoSpace,
    Io,
    Unsupported,
    NoLinkConfig,
    Deadlock,
}

/// The narrow adapter boundary used for DPCD transactions and required waits.
/// Implementations must preserve AUX transaction ordering and the kernel
/// convention that a successful DPCD byte operation transfers exactly one byte.
pub trait DpAuxIo {
    fn read(&mut self, address: u32, bytes: &mut [u8]) -> Result<usize, DpError>;
    fn write(&mut self, address: u32, bytes: &[u8]) -> Result<usize, DpError>;
    fn delay_ms(&mut self, milliseconds: u32);
}

/// Optional callbacks for framework-owned DSC/joiner and DP 2.x helpers.
/// Pure policy remains here; the adapter is responsible only for protocol
/// helpers whose canonical implementation is in DRM core or i915 subsystems.
pub trait DpFramework {
    fn max_slice_count(&self, caps: &DscSinkCaps, edp: bool) -> u8;
    fn max_slice_throughput(&self, caps: &DscSinkCaps, clock_khz: u32, rgb: bool) -> u32;
    fn get_slice_config(&self, joined_pipes: u8, slices_per_pipe: u8) -> Option<DscSliceConfig>;
    fn compute_dsc_rc(&self, config: &mut DscConfig) -> Result<(), DpError>;
    fn dsc_line_buf_depth(&self, caps: &DscSinkCaps) -> u8;
    fn dsc_branch_overall_throughput(&self, caps: &[u8], rgb_yuv444: bool) -> u32;
    fn dsc_branch_max_line_width(&self, caps: &[u8]) -> u16;
    /// Return a bitset whose bit n represents support for n slices per line.
    fn dsc_supported_slice_counts(&self, caps: &DscSinkCaps) -> u32;
    fn link_status_ok(&self, link_rate: u32, lanes: u8, status: &[u8]) -> bool;
    fn dsc_pixel_rate_with_bubbles(&self, mode_clock: u32, htotal: u32, slices: u8) -> u32;
    fn max_uncompressed_dotclock(&self) -> u32;
    fn mtp_tu_fits(&self, rate: u32, lanes: u8, mode: DisplayMode, bpp_x16: u16) -> bool;
    fn dpcd_rate(&self, code: u8) -> u32;
    fn max_dprx_data_rate(&self, rate: u32, lanes: u8) -> u64;
    fn dp_bw_overhead(&self, lanes: u8, hdisplay: u32, slices: u8, bpp_x16: u16, flags: u32)
    -> u32;
}

pub const BW_OVERHEAD_MST: u32 = 1 << 0;
pub const BW_OVERHEAD_SSC_REF_CLK: u32 = 1 << 1;
pub const BW_OVERHEAD_FEC: u32 = 1 << 2;
pub const BW_OVERHEAD_UHBR: u32 = 1 << 3;
pub const BW_OVERHEAD_DSC: u32 = 1 << 4;

/// Is the link rate UHBR and therefore 128b/132b coded?
// upstream: intel_dp.c intel_dp_is_uhbr()
pub const fn intel_dp_is_uhbr(rate: u32) -> bool {
    rate >= 1_000_000
}

/// Return link symbol bits, selected from the channel coding at `rate`.
// upstream: intel_dp.c intel_dp_link_symbol_size()
pub const fn intel_dp_link_symbol_size(rate: u32) -> u32 {
    if intel_dp_is_uhbr(rate) { 32 } else { 10 }
}

/// Convert the advertised per-lane rate to the actual symbol clock in kHz.
// upstream: intel_dp.c intel_dp_link_symbol_clock()
pub fn intel_dp_link_symbol_clock(rate: u32) -> u32 {
    (rate * 10 + intel_dp_link_symbol_size(rate) / 2) / intel_dp_link_symbol_size(rate)
}

/// DG2 source limit is HBR3 externally and HBR2 for eDP.
// upstream: intel_dp.c dg2_max_source_rate()
pub const fn dg2_max_source_rate(edp: bool) -> u32 {
    if edp { 810_000 } else { 1_350_000 }
}

/// ICL-family combo PHY is HBR2 externally; non-combo/eDP supports HBR3.
// upstream: intel_dp.c icl_max_source_rate()
pub const fn icl_max_source_rate(port_kind: PortKind, edp: bool) -> u32 {
    if matches!(port_kind, PortKind::Combo) && !edp {
        540_000
    } else {
        810_000
    }
}

/// EHL/JSL source limit is HBR2 for eDP and HBR3 externally.
// upstream: intel_dp.c ehl_max_source_rate()
pub const fn ehl_max_source_rate(edp: bool) -> u32 {
    if edp { 540_000 } else { 810_000 }
}

/// Select the source maximum link rate used by display 12/13 platforms.
pub fn intel_dp_source_max_rate(info: DisplayInfo, edp: bool) -> u32 {
    if info.platform_dg2 {
        dg2_max_source_rate(edp)
    } else if info.platform_alderlake {
        810_000
    } else if info.platform_ehl_jsl {
        ehl_max_source_rate(edp)
    } else {
        icl_max_source_rate(info.port_kind, edp)
    }
}

/// Limit BIOS max link-rate by eDP panel VBT max where both are known.
// upstream: intel_dp.c vbt_max_link_rate()
pub fn vbt_max_link_rate(bios_max_rate: u32, edp_panel_max_rate: u32, edp: bool) -> u32 {
    let mut max_rate = bios_max_rate;
    if edp {
        if max_rate != 0 && edp_panel_max_rate != 0 {
            max_rate = min(max_rate, edp_panel_max_rate);
        } else if edp_panel_max_rate != 0 {
            max_rate = edp_panel_max_rate;
        }
    }
    max_rate
}

/// Set the generation-11/12/13 source-rate table, clamped by VBT when present.
// upstream: intel_dp.c intel_dp_set_source_rates()
pub fn intel_dp_set_source_rates(
    link: &mut DpLinkState,
    info: DisplayInfo,
    edp: bool,
    vbt_max_rate: u32,
) {
    let mut max_rate = intel_dp_source_max_rate(info, edp);
    if vbt_max_rate != 0 {
        max_rate = min(max_rate, vbt_max_rate);
    }
    let count = intel_dp_rate_limit_len(&ICL_SOURCE_RATES, ICL_SOURCE_RATES.len(), max_rate);
    link.source_rates.clear();
    link.source_rates
        .extend_from_slice(&ICL_SOURCE_RATES[..count]);
}

/// Format integer link-rate arrays using the DP debug output's comma separator.
// upstream: intel_dp.c seq_buf_print_array()
pub fn seq_buf_print_array(array: &[u32]) -> String {
    array
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Report source, sink and common rates only when KMS debug logging is enabled.
// upstream: intel_dp.c intel_dp_print_rates()
pub fn intel_dp_print_rates(link: &DpLinkState, kms_debug_enabled: bool) -> Option<[String; 3]> {
    if !kms_debug_enabled {
        return None;
    }
    Some([
        seq_buf_print_array(&link.source_rates),
        seq_buf_print_array(&link.sink_rates),
        seq_buf_print_array(&link.common_rates),
    ])
}

/// Return receiver's maximum advertised rate, accounting for a DP tunnel.
// upstream: intel_dp.c max_dprx_rate()
pub fn max_dprx_rate(caps: &DpSinkCaps, framework: &impl DpFramework, edp_hbr2_quirk: bool) -> u32 {
    let mut rate = caps.tunneling_max_rate.unwrap_or_else(|| {
        if caps.max_rate != 0 {
            caps.max_rate
        } else {
            framework.dpcd_rate(caps.dpcd.get(1).copied().unwrap_or(0))
        }
    });
    if edp_hbr2_quirk {
        rate = min(rate, 540_000);
    }
    rate
}

/// Return receiver's maximum lane count, accounting for a DP tunnel.
// upstream: intel_dp.c max_dprx_lane_count()
pub fn max_dprx_lane_count(caps: &DpSinkCaps) -> u8 {
    caps.tunneling_max_lanes.unwrap_or_else(|| {
        if caps.max_lanes != 0 {
            caps.max_lanes
        } else {
            caps.dpcd.get(2).copied().unwrap_or(0) & 0x1f
        }
    })
}

/// Initialize the conservative fallback sink-rate list.
// upstream: intel_dp.c intel_dp_set_default_sink_rates()
pub fn intel_dp_set_default_sink_rates(link: &mut DpLinkState) {
    link.sink_rates.clear();
    link.sink_rates.push(162_000);
}

/// Update sink rates from DPCD and LTTPR capabilities, preserving source order.
// upstream: intel_dp.c intel_dp_set_dpcd_sink_rates()
pub fn intel_dp_set_dpcd_sink_rates(
    link: &mut DpLinkState,
    caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    const DP_RATES: [u32; 4] = [162_000, 270_000, 540_000, 810_000];
    link.sink_rates.clear();
    if caps.quirk_max_rate_3_24 {
        link.sink_rates
            .extend_from_slice(&[162_000, 270_000, 324_000]);
        return;
    }
    let mut max_rate = max_dprx_rate(caps, framework, false);
    let lttpr_rate = caps
        .lttpr_caps
        .get(1)
        .copied()
        .map(|rate| framework.dpcd_rate(rate))
        .unwrap_or(0);
    if lttpr_rate != 0 {
        max_rate = min(max_rate, lttpr_rate);
    }
    link.sink_rates
        .extend(DP_RATES.into_iter().take_while(|rate| *rate <= max_rate));
    let coding_128_132 = caps.dpcd.get(6).copied().unwrap_or(0) & 0x01 != 0;
    if coding_128_132 {
        let mut rates = [0u8; 1];
        if io.read(0x2215, &mut rates).is_ok() {
            let mut uhbr = rates[0];
            if caps.lttpr_count != 0 {
                if caps.lttpr_supports_uhbr {
                    uhbr &= caps.lttpr_uhbr_rate_mask;
                } else {
                    uhbr = 0;
                }
            }
            if uhbr & 0x01 != 0 {
                link.sink_rates.push(1_000_000);
            }
            if uhbr & 0x02 != 0 {
                link.sink_rates.push(1_350_000);
            }
            if uhbr & 0x04 != 0 {
                link.sink_rates.push(2_000_000);
            }
        }
    }
}

/// Clamp a sorted rate-table prefix to the greatest rate not above `max_rate`.
// upstream: intel_dp.c intel_dp_rate_limit_len()
pub fn intel_dp_rate_limit_len(rates: &[u32], len: usize, max_rate: u32) -> usize {
    let len = min(len, rates.len());
    for i in 0..len {
        if rates[len - i - 1] <= max_rate {
            return len - i;
        }
    }
    0
}

/// Return the length of common rates after the configured upper-rate limit.
// upstream: intel_dp.c intel_dp_common_len_rate_limit()
pub fn intel_dp_common_len_rate_limit(link: &DpLinkState, max_rate: u32) -> usize {
    intel_dp_rate_limit_len(&link.common_rates, link.common_rates.len(), max_rate)
}

/// Return common rate at index; i915 falls back to RBR on invalid indices.
// upstream: intel_dp.c intel_dp_common_rate()
pub fn intel_dp_common_rate(link: &DpLinkState, index: usize) -> u32 {
    link.common_rates.get(index).copied().unwrap_or(162_000)
}

/// Return the theoretical maximum between source and receiver.
// upstream: intel_dp.c intel_dp_max_common_rate()
pub fn intel_dp_max_common_rate(link: &DpLinkState) -> u32 {
    intel_dp_common_rate(link, link.common_rates.len().saturating_sub(1))
}

/// Restrict VBT/source lane count by source hardware's maximum.
// upstream: intel_dp.c intel_dp_max_source_lane_count()
pub fn intel_dp_max_source_lane_count(source_max: u8, vbt_max: u8) -> u8 {
    if vbt_max != 0 {
        min(source_max, vbt_max)
    } else {
        source_max
    }
}

/// Update the theoretical maximum common lane count, reporting a change.
// upstream: intel_dp.c intel_dp_set_max_common_lane_count()
pub fn intel_dp_set_max_common_lane_count(link: &mut DpLinkState, lttpr_max: u8) -> bool {
    let old = link.max_common_lane_count;
    let mut sink_max = link.max_sink_lane_count;
    if lttpr_max != 0 {
        sink_max = min(sink_max, lttpr_max);
    }
    link.max_common_lane_count = min(
        link.max_source_lane_count,
        min(sink_max, link.tc_lane_count),
    );
    old != link.max_common_lane_count
}

/// Return the negotiated maximum lane count.
// upstream: intel_dp.c intel_dp_max_common_lane_count()
pub fn intel_dp_max_common_lane_count(link: &DpLinkState) -> u8 {
    link.max_common_lane_count
}

// upstream: intel_dp.c forced_lane_count()
fn forced_lane_count(link: &DpLinkState) -> u8 {
    link.force_lane_count
        .clamp(1, intel_dp_max_common_lane_count(link))
}

/// Return the active maximum lane count, respecting a forced link setting.
// upstream: intel_dp.c intel_dp_max_lane_count()
pub fn intel_dp_max_lane_count(link: &DpLinkState) -> u8 {
    let count = if link.force_lane_count != 0 {
        forced_lane_count(link)
    } else {
        link.max_lane_count
    };
    match count {
        1 | 2 | 4 => count,
        _ => 1,
    }
}

// upstream: intel_dp.c intel_dp_min_lane_count()
fn intel_dp_min_lane_count(link: &DpLinkState) -> u8 {
    if link.force_lane_count != 0 {
        forced_lane_count(link)
    } else {
        1
    }
}

/// Intersect two sorted link-rate arrays, stopping before capacity overflow.
// upstream: intel_dp.c intersect_rates()
pub fn intersect_rates(source: &[u32], sink: &[u32]) -> Vec<u32> {
    let (mut i, mut j) = (0, 0);
    let mut common = Vec::new();
    while i < source.len() && j < sink.len() && common.len() < DP_MAX_SUPPORTED_RATES {
        match source[i].cmp(&sink[j]) {
            core::cmp::Ordering::Equal => {
                common.push(source[i]);
                i += 1;
                j += 1;
            }
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
        }
    }
    common
}

/// Find a link rate in an array, returning -1 if absent.
// upstream: intel_dp.c intel_dp_rate_index()
pub fn intel_dp_rate_index(rates: &[u32], rate: u32) -> i32 {
    rates
        .iter()
        .position(|candidate| *candidate == rate)
        .map_or(-1, |index| index as i32)
}

/// Resolve link rate from the indexed link-configuration structure.
// upstream: intel_dp.c intel_dp_link_config_rate()
pub fn intel_dp_link_config_rate(link: &DpLinkState, config: DpLinkConfig) -> u32 {
    intel_dp_common_rate(link, config.link_rate_idx as usize)
}

/// Decode lane count from its power-of-two exponent.
// upstream: intel_dp.c intel_dp_link_config_lane_count()
pub fn intel_dp_link_config_lane_count(config: DpLinkConfig) -> u8 {
    1 << config.lane_count_exp
}

/// Calculate the payload bandwidth for an exact rate/lane combination.
// upstream: intel_dp.c intel_dp_link_config_bw()
pub fn intel_dp_link_config_bw(
    link: &DpLinkState,
    config: DpLinkConfig,
    framework: &impl DpFramework,
) -> u64 {
    framework.max_dprx_data_rate(
        intel_dp_link_config_rate(link, config),
        intel_dp_link_config_lane_count(config),
    )
}

/// Sort by data bandwidth, then by link rate for equal-bandwidth configurations.
// upstream: intel_dp.c link_config_cmp_by_bw()
pub fn link_config_cmp_by_bw(
    link: &DpLinkState,
    a: DpLinkConfig,
    b: DpLinkConfig,
    framework: &impl DpFramework,
) -> core::cmp::Ordering {
    intel_dp_link_config_bw(link, a, framework)
        .cmp(&intel_dp_link_config_bw(link, b, framework))
        .then_with(|| intel_dp_link_config_rate(link, a).cmp(&intel_dp_link_config_rate(link, b)))
}

/// Construct every common rate/lane-count combination and sort by bandwidth.
// upstream: intel_dp.c intel_dp_link_config_init()
pub fn intel_dp_link_config_init(link: &mut DpLinkState, framework: &impl DpFramework) {
    if link.max_common_lane_count == 0 || !link.max_common_lane_count.is_power_of_two() {
        link.configs.clear();
        return;
    }
    let lane_levels = link.max_common_lane_count.ilog2() as usize + 1;
    let mut configs = Vec::new();
    for rate_idx in 0..link.common_rates.len() {
        for lane_exp in 0..lane_levels {
            configs.push(DpLinkConfig {
                link_rate_idx: rate_idx as u8,
                lane_count_exp: lane_exp as u8,
            });
        }
    }
    configs.sort_by(|a, b| link_config_cmp_by_bw(link, *a, *b, framework));
    link.configs = configs;
}

/// Decode one indexed link-configuration entry to rate and lane count.
// upstream: intel_dp.c intel_dp_link_config_get()
pub fn intel_dp_link_config_get(link: &DpLinkState, index: usize) -> (u32, u8) {
    let config = link.configs.get(index).copied().unwrap_or_default();
    (
        intel_dp_link_config_rate(link, config),
        intel_dp_link_config_lane_count(config),
    )
}

/// Find the index of an exact common-rate/lane-count configuration.
// upstream: intel_dp.c intel_dp_link_config_index()
pub fn intel_dp_link_config_index(link: &DpLinkState, rate: u32, lanes: u8) -> Option<usize> {
    let rate_idx = intel_dp_rate_index(&link.common_rates, rate);
    let lane_exp = lanes.trailing_zeros() as u8;
    link.configs.iter().position(|config| {
        config.link_rate_idx as i32 == rate_idx && config.lane_count_exp == lane_exp
    })
}

/// Refresh common rates, applying the source's RBR paranoia fallback.
// upstream: intel_dp.c intel_dp_set_common_rates()
pub fn intel_dp_set_common_rates(link: &mut DpLinkState) -> bool {
    let old = link.common_rates.clone();
    link.common_rates = intersect_rates(&link.source_rates, &link.sink_rates);
    if link.common_rates.is_empty() {
        link.common_rates.push(162_000);
    }
    old != link.common_rates
}

/// Validate a proposed link rate and lane count against negotiated maxima.
// upstream: intel_dp.c intel_dp_link_params_valid()
pub fn intel_dp_link_params_valid(link: &DpLinkState, rate: u32, lanes: u8) -> bool {
    rate != 0 && rate <= link.max_rate && lanes != 0 && lanes <= intel_dp_max_lane_count(link)
}

/// Convert a mode clock to the hard-coded DSC/FEC overhead clock.
// upstream: intel_dp.c intel_dp_mode_to_fec_clock()
pub fn intel_dp_mode_to_fec_clock(mode_clock: u32) -> u32 {
    ((mode_clock as u64 * DP_DSC_FEC_OVERHEAD_FACTOR as u64) / 1_000_000) as u32
}

/// Return the source's fixed FEC/DSC overhead factor.
// upstream: intel_dp.c intel_dp_bw_fec_overhead()
pub const fn intel_dp_bw_fec_overhead(fec_enabled: bool) -> u32 {
    if fec_enabled {
        DP_DSC_FEC_OVERHEAD_FACTOR
    } else {
        1_000_000
    }
}

/// Calculate DP link-bandwidth overhead using the DRM-core calculation and
/// i915's fixed FEC minimum, preserving the source flag augmentation order.
// upstream: intel_dp.c intel_dp_link_bw_overhead()
pub fn intel_dp_link_bw_overhead(
    link_clock: u32,
    lane_count: u8,
    hdisplay: u32,
    dsc_slice_count: u8,
    bpp_x16: u16,
    mut flags: u32,
    framework: &impl DpFramework,
) -> u32 {
    if intel_dp_is_uhbr(link_clock) {
        flags |= BW_OVERHEAD_UHBR;
    }
    if dsc_slice_count != 0 {
        flags |= BW_OVERHEAD_DSC;
    }
    max(
        framework.dp_bw_overhead(lane_count, hdisplay, dsc_slice_count, bpp_x16, flags),
        intel_dp_bw_fec_overhead(flags & BW_OVERHEAD_FEC != 0),
    )
}

/// Return required net data rate in kB/s after the specified BW overhead.
// upstream: intel_dp.c intel_dp_effective_data_rate()
pub fn intel_dp_effective_data_rate(pixel_clock: u32, bpp_x16: u16, bw_overhead: u32) -> u64 {
    let product = pixel_clock as u64 * bpp_x16 as u64 * bw_overhead as u64;
    product.div_ceil(1_000_000 * 16 * 8)
}

/// Return required DP link data rate for the given pixel-mode and link.
// upstream: intel_dp.c intel_dp_link_required()
pub fn intel_dp_link_required(
    link_clock: u32,
    lane_count: u8,
    mode_clock: u32,
    mode_hdisplay: u32,
    link_bpp_x16: u16,
    bw_overhead_flags: u32,
    framework: &impl DpFramework,
) -> u64 {
    let overhead = intel_dp_link_bw_overhead(
        link_clock,
        lane_count,
        mode_hdisplay,
        0,
        link_bpp_x16,
        bw_overhead_flags,
        framework,
    );
    intel_dp_effective_data_rate(mode_clock, link_bpp_x16, overhead)
}

/// Maximum DPRX payload bandwidth, reduced by any available tunnel allocation.
// upstream: intel_dp.c intel_dp_max_link_data_rate()
pub fn intel_dp_max_link_data_rate(
    caps: &DpSinkCaps,
    rate: u32,
    lanes: u8,
    framework: &impl DpFramework,
) -> u64 {
    let max_rate = framework.max_dprx_data_rate(rate, lanes);
    caps.tunneling_available_bw
        .map_or(max_rate, |tunnel| min(max_rate, tunnel))
}

/// Whether this DP connector may use pipe joiner on the selected display.
// upstream: intel_dp.c intel_dp_has_joiner()
pub fn intel_dp_has_joiner(
    display_ver: u8,
    is_edp: bool,
    edp_mso_links: u8,
    edp_joiner_enabled: bool,
    port_a: bool,
) -> bool {
    if edp_mso_links != 0 || (is_edp && !edp_joiner_enabled) {
        return false;
    }
    display_ver >= 12 || (display_ver == 11 && !port_a)
}

/// Compressed line-buffer capacity for the display generation.
// upstream: intel_dp.c small_joiner_ram_size_bits()
pub fn small_joiner_ram_size_bits(display_ver: u8) -> u32 {
    if display_ver >= 13 {
        17_280 * 8
    } else {
        7_680 * 8
    }
}

// upstream: intel_dp.c bigjoiner_interface_bits()
pub fn bigjoiner_interface_bits(display_ver: u8) -> u32 {
    if display_ver >= 14 { 36 } else { 24 }
}

// upstream: intel_dp.c bigjoiner_bw_max_bpp()
pub fn bigjoiner_bw_max_bpp(
    display_ver: u8,
    mode_clock: u32,
    num_joined_pipes: u8,
    max_cdclk: u32,
) -> u32 {
    let fec_clock = intel_dp_mode_to_fec_clock(mode_clock);
    if fec_clock == 0 {
        return 0;
    }
    let big_joiners = num_joined_pipes / 2;
    max_cdclk
        .saturating_mul(2)
        .saturating_mul(bigjoiner_interface_bits(display_ver))
        .checked_div(fec_clock)
        .unwrap_or(0)
        .saturating_mul(big_joiners as u32)
}

// upstream: intel_dp.c small_joiner_ram_max_bpp()
pub fn small_joiner_ram_max_bpp(display_ver: u8, mode_hdisplay: u32, num_joined_pipes: u8) -> u32 {
    if mode_hdisplay == 0 {
        return 0;
    }
    small_joiner_ram_size_bits(display_ver) / mode_hdisplay * num_joined_pipes as u32
}

// upstream: intel_dp.c ultrajoiner_ram_bits()
pub const fn ultrajoiner_ram_bits() -> u32 {
    4 * 72 * 512
}

// upstream: intel_dp.c ultrajoiner_ram_max_bpp()
pub fn ultrajoiner_ram_max_bpp(mode_hdisplay: u32) -> u32 {
    if mode_hdisplay == 0 {
        0
    } else {
        ultrajoiner_ram_bits() / mode_hdisplay
    }
}

// upstream: intel_dp.c get_max_compressed_bpp_with_joiner()
pub fn get_max_compressed_bpp_with_joiner(
    display_ver: u8,
    mode_clock: u32,
    mode_hdisplay: u32,
    num_joined_pipes: u8,
    max_cdclk: u32,
) -> u32 {
    if mode_hdisplay == 0 {
        return 0;
    }
    let mut max_bpp = small_joiner_ram_max_bpp(display_ver, mode_hdisplay, num_joined_pipes);
    if num_joined_pipes > 1 && mode_clock != 0 {
        max_bpp = min(
            max_bpp,
            bigjoiner_bw_max_bpp(display_ver, mode_clock, num_joined_pipes, max_cdclk),
        );
    }
    if num_joined_pipes == 4 {
        max_bpp = min(max_bpp, ultrajoiner_ram_max_bpp(mode_hdisplay));
    }
    max_bpp
}

/// Align an uncompressed/DSC source minimum to a VESA compressed bpp point.
// upstream: intel_dp.c align_min_vesa_compressed_bpp_x16()
pub fn align_min_vesa_compressed_bpp_x16(min_link_bpp_x16: i32) -> i32 {
    VALID_DSC_BPP
        .into_iter()
        .map(|bpp| i32::from(bpp) << 4)
        .find(|bpp| *bpp >= min_link_bpp_x16)
        .unwrap_or(0)
}

/// Align an uncompressed/DSC source maximum to a VESA compressed bpp point.
// upstream: intel_dp.c align_max_vesa_compressed_bpp_x16()
pub fn align_max_vesa_compressed_bpp_x16(max_link_bpp_x16: i32) -> i32 {
    VALID_DSC_BPP
        .into_iter()
        .rev()
        .map(|bpp| i32::from(bpp) << 4)
        .find(|bpp| *bpp <= max_link_bpp_x16)
        .unwrap_or(0)
}

/// Smallest sink/source slice count that can process this mode.
// upstream: intel_dp.c intel_dp_dsc_min_slice_count()
pub fn intel_dp_dsc_min_slice_count(
    caps: &DscSinkCaps,
    mode_clock: u32,
    mode_hdisplay: u32,
    is_edp: bool,
    max_cdclk: u32,
    framework: &impl DpFramework,
) -> u8 {
    if is_edp {
        return framework.max_slice_count(caps, true);
    }
    if mode_clock > max(caps.throughput_rgb_yuv444, caps.throughput_yuv422_420)
        || mode_hdisplay > u32::from(caps.max_line_width)
    {
        return 0;
    }
    let throughput = min(
        framework.max_slice_throughput(caps, mode_clock, true),
        framework.max_slice_throughput(caps, mode_clock, false),
    );
    if throughput == 0 {
        return 0;
    }
    let mut min_slices = mode_clock.div_ceil(throughput) as u8;
    if mode_clock as u64 >= max_cdclk as u64 * 85 / 100 {
        min_slices = max(min_slices, 2);
    }
    if caps.max_slice_width < DP_DSC_MIN_SLICE_WIDTH {
        return 0;
    }
    min_slices = max(
        min_slices,
        mode_hdisplay.div_ceil(caps.max_slice_width as u32) as u8,
    );
    min_slices
}

/// Pick the first legal slices-per-pipe configuration satisfying sink limits.
// upstream: intel_dp.c intel_dp_dsc_get_slice_config()
pub fn intel_dp_dsc_get_slice_config(
    caps: &DscSinkCaps,
    mode_clock: u32,
    mode_hdisplay: u32,
    joined_pipes: u8,
    is_edp: bool,
    max_cdclk: u32,
    framework: &impl DpFramework,
) -> Option<DscSliceConfig> {
    let min_count = intel_dp_dsc_min_slice_count(
        caps,
        mode_clock,
        mode_hdisplay,
        is_edp,
        max_cdclk,
        framework,
    );
    for slices_per_pipe in 1..=4 {
        let config = match framework.get_slice_config(joined_pipes, slices_per_pipe) {
            Some(config) => config,
            None => continue,
        };
        let count = if config.slices_per_line != 0 {
            config.slices_per_line
        } else {
            config.slices_per_pipe * config.pipes
        };
        if count == 0 || count > 24 || caps.slice_count_mask & (1u32 << count) == 0 {
            continue;
        }
        if mode_hdisplay % u32::from(count) != 0 || min_count > count {
            continue;
        }
        return Some(DscSliceConfig {
            slices_per_line: count,
            ..config
        });
    }
    None
}

/// Return DP DSC slices per scan line, or zero when no legal layout exists.
// upstream: intel_dp.c intel_dp_dsc_get_slice_count()
pub fn intel_dp_dsc_get_slice_count(
    caps: &DscSinkCaps,
    mode_clock: u32,
    mode_hdisplay: u32,
    joined_pipes: u8,
    is_edp: bool,
    max_cdclk: u32,
    framework: &impl DpFramework,
) -> u8 {
    intel_dp_dsc_get_slice_config(
        caps,
        mode_clock,
        mode_hdisplay,
        joined_pipes,
        is_edp,
        max_cdclk,
        framework,
    )
    .map_or(0, |config| config.slices_per_line)
}

/// Validate source output format reachability on this display generation.
// upstream: intel_dp.c source_can_output()
pub fn source_can_output(
    display_ver: u8,
    format: OutputFormat,
    has_gmch: bool,
    ironlake: bool,
) -> bool {
    match format {
        OutputFormat::Rgb => true,
        OutputFormat::Ycbcr444 => !has_gmch && !ironlake,
        OutputFormat::Ycbcr420 => display_ver >= 11,
        OutputFormat::Ycbcr422 => false,
    }
}

// upstream: intel_dp.c dfp_can_convert_from_rgb()
pub fn dfp_can_convert_from_rgb(
    is_branch: bool,
    sink: OutputFormat,
    rgb_to_ycbcr: bool,
    ycbcr444_to_420: bool,
) -> bool {
    is_branch
        && match sink {
            OutputFormat::Ycbcr444 => rgb_to_ycbcr,
            OutputFormat::Ycbcr420 => rgb_to_ycbcr && ycbcr444_to_420,
            _ => false,
        }
}

// upstream: intel_dp.c dfp_can_convert_from_ycbcr444()
pub fn dfp_can_convert_from_ycbcr444(
    is_branch: bool,
    sink: OutputFormat,
    ycbcr444_to_420: bool,
) -> bool {
    is_branch && sink == OutputFormat::Ycbcr420 && ycbcr444_to_420
}

// upstream: intel_dp.c dfp_can_convert()
pub fn dfp_can_convert(
    is_branch: bool,
    output: OutputFormat,
    sink: OutputFormat,
    rgb_to_ycbcr: bool,
    ycbcr444_to_420: bool,
) -> bool {
    match output {
        OutputFormat::Rgb => {
            dfp_can_convert_from_rgb(is_branch, sink, rgb_to_ycbcr, ycbcr444_to_420)
        }
        OutputFormat::Ycbcr444 => dfp_can_convert_from_ycbcr444(is_branch, sink, ycbcr444_to_420),
        _ => false,
    }
}

/// Select source output format according to sink encoding and supported branch conversion.
// upstream: intel_dp.c intel_dp_output_format()
pub fn intel_dp_output_format(
    sink: OutputFormat,
    forced: Option<OutputFormat>,
    display_ver: u8,
    has_gmch: bool,
    ironlake: bool,
    is_branch: bool,
    rgb_to_ycbcr: bool,
    ycbcr444_to_420: bool,
) -> OutputFormat {
    if let Some(forced) = forced {
        if source_can_output(display_ver, forced, has_gmch, ironlake)
            && (!is_branch
                || sink != forced
                || dfp_can_convert(is_branch, forced, sink, rgb_to_ycbcr, ycbcr444_to_420))
        {
            return forced;
        }
    }
    if sink == OutputFormat::Rgb
        || dfp_can_convert(
            is_branch,
            OutputFormat::Rgb,
            sink,
            rgb_to_ycbcr,
            ycbcr444_to_420,
        )
    {
        OutputFormat::Rgb
    } else if sink == OutputFormat::Ycbcr444
        || dfp_can_convert(
            is_branch,
            OutputFormat::Ycbcr444,
            sink,
            rgb_to_ycbcr,
            ycbcr444_to_420,
        )
    {
        OutputFormat::Ycbcr444
    } else {
        OutputFormat::Ycbcr420
    }
}

/// Return platform's minimum pipe bpp for the selected DP pixel format.
// upstream: intel_dp.c intel_dp_min_bpp()
pub const fn intel_dp_min_bpp(format: OutputFormat) -> u8 {
    if matches!(format, OutputFormat::Rgb) {
        18
    } else {
        24
    }
}

/// Convert pipe bpp to Q4 link bpp, halving 4:2:0 payload pixel rate.
// upstream: intel_dp.c intel_dp_output_format_link_bpp_x16()
pub const fn intel_dp_output_format_link_bpp_x16(format: OutputFormat, pipe_bpp: u8) -> u16 {
    let bpp = if matches!(format, OutputFormat::Ycbcr420) {
        pipe_bpp / 2
    } else {
        pipe_bpp
    };
    bpp as u16 * 16
}

/// Source's minimum DSC input BPC is 8 for ICL and later.
// upstream: intel_dp.c intel_dp_dsc_min_src_input_bpc()
pub const fn intel_dp_dsc_min_src_input_bpc() -> u8 {
    8
}

/// Compute DP DSC minimum compressed bpp in Q4 after sink/source alignment.
// upstream: intel_dp.c intel_dp_compute_min_compressed_bpp_x16()
pub fn intel_dp_compute_min_compressed_bpp_x16(
    display_ver: u8,
    output: OutputFormat,
    bpp_step_x16: u16,
) -> u16 {
    let min_bpp = max(
        intel_dp_dsc_min_src_compressed_bpp(),
        intel_dp_dsc_sink_min_compressed_bpp(output),
    );
    align_min_compressed_bpp_x16(display_ver, u16::from(min_bpp) << 4, bpp_step_x16)
}

/// DSC branch throughput workaround ceiling, in Q4 bpp.
// upstream: intel_dp.c dsc_throughput_quirk_max_bpp_x16()
pub fn dsc_throughput_quirk_max_bpp_x16(caps: &DscSinkCaps, mode_clock: u32) -> u16 {
    if !caps.throughput_quirk {
        return u16::MAX;
    }
    let max_throughput = min(caps.throughput_rgb_yuv444, caps.throughput_yuv422_420);
    if mode_clock < max_throughput / 2 {
        u16::MAX
    } else {
        12 * 16
    }
}

/// Maximum compressed bpp after source, sink, joiner and branch-throughput limits.
// upstream: intel_dp.c compute_max_compressed_bpp_x16()
pub fn compute_max_compressed_bpp_x16(
    display_ver: u8,
    caps: &DscSinkCaps,
    mode_clock: u32,
    mode_hdisplay: u32,
    joined_pipes: u8,
    output: OutputFormat,
    pipe_max_bpp: u8,
    max_link_bpp_x16: u16,
    max_cdclk: u32,
    force_dsc: bool,
    bpp_step_x16: u16,
) -> u16 {
    let source_max = dsc_src_max_compressed_bpp(display_ver, force_dsc);
    let joiner_max = get_max_compressed_bpp_with_joiner(
        display_ver,
        mode_clock,
        mode_hdisplay,
        joined_pipes,
        max_cdclk,
    );
    let sink_max = intel_dp_dsc_sink_max_compressed_bpp(caps, output, pipe_max_bpp / 3);
    let maximum = min(min(u32::from(sink_max), u32::from(source_max)), joiner_max) as u8;
    let mut max_link_bpp_x16 = min(max_link_bpp_x16, u16::from(maximum) * 16);
    max_link_bpp_x16 = min(
        max_link_bpp_x16,
        dsc_throughput_quirk_max_bpp_x16(caps, mode_clock),
    );
    align_max_compressed_bpp_x16(
        display_ver,
        pipe_max_bpp,
        output,
        max_link_bpp_x16,
        bpp_step_x16,
    )
}

/// Decide whether at least one valid DSC bpp and slice layout fits this link.
// upstream: intel_dp.c intel_dp_mode_valid_with_dsc()
pub fn intel_dp_mode_valid_with_dsc(
    display_ver: u8,
    caps: &DscSinkCaps,
    link_caps: &DpSinkCaps,
    link_clock: u32,
    lane_count: u8,
    mode: DisplayMode,
    joined_pipes: u8,
    output: OutputFormat,
    pipe_bpp: u8,
    overhead_flags: u32,
    max_cdclk: u32,
    bpp_step_x16: u16,
    force_dsc: bool,
    framework: &impl DpFramework,
) -> bool {
    let min_bpp_x16 = intel_dp_compute_min_compressed_bpp_x16(display_ver, output, bpp_step_x16);
    let max_bpp_x16 = compute_max_compressed_bpp_x16(
        display_ver,
        caps,
        mode.clock_khz,
        mode.hdisplay,
        joined_pipes,
        output,
        pipe_bpp,
        u16::MAX,
        max_cdclk,
        force_dsc,
        bpp_step_x16,
    );
    let slices = intel_dp_dsc_get_slice_count(
        caps,
        mode.clock_khz,
        mode.hdisplay,
        joined_pipes,
        false,
        max_cdclk,
        framework,
    );
    min_bpp_x16 != 0
        && min_bpp_x16 <= max_bpp_x16
        && slices != 0
        && is_bw_sufficient_for_dsc_config(
            link_clock,
            lane_count,
            mode,
            slices,
            min_bpp_x16,
            overhead_flags,
            link_caps,
            framework,
        )
}

/// Align DSC input bpp range to source and sink limits.
// upstream: intel_dp.c intel_dp_dsc_compute_pipe_bpp_limits()
pub fn intel_dp_dsc_compute_pipe_bpp_limits(
    caps: &DscSinkCaps,
    display_ver: u8,
    limits: &mut LinkConfigLimits,
) -> bool {
    limits.min_pipe_bpp = max(limits.min_pipe_bpp, intel_dp_dsc_min_src_input_bpc() * 3);
    limits.min_pipe_bpp = align_min_sink_dsc_input_bpp(caps, limits.min_pipe_bpp);
    limits.max_pipe_bpp = min(
        limits.max_pipe_bpp,
        intel_dp_dsc_max_src_input_bpc(display_ver) * 3,
    );
    limits.max_pipe_bpp = align_max_sink_dsc_input_bpp(caps, limits.max_pipe_bpp);
    limits.min_pipe_bpp != 0 && limits.min_pipe_bpp <= limits.max_pipe_bpp
}

/// Is pipe bpp inside the computed DSC range?
// upstream: intel_dp.c is_dsc_pipe_bpp_sufficient()
pub const fn is_dsc_pipe_bpp_sufficient(limits: LinkConfigLimits, pipe_bpp: u8) -> bool {
    pipe_bpp >= limits.min_pipe_bpp && pipe_bpp <= limits.max_pipe_bpp
}

/// Choose user-forced DSC input BPC only if it remains inside valid limits.
// upstream: intel_dp.c intel_dp_force_dsc_pipe_bpp()
pub fn intel_dp_force_dsc_pipe_bpp(force_bpc: u8, limits: LinkConfigLimits) -> u8 {
    let forced_bpp = force_bpc.saturating_mul(3);
    if force_bpc != 0 && is_dsc_pipe_bpp_sufficient(limits, forced_bpp) {
        forced_bpp
    } else {
        0
    }
}

/// Clear every active DSC/FEC state field before a fresh config attempt.
// upstream: intel_dp.c intel_dp_dsc_reset_config()
pub fn intel_dp_dsc_reset_config(
    link: &mut SelectedLink,
    config: &mut DscConfig,
    slice: &mut DscSliceConfig,
) {
    link.fec = false;
    link.dsc = false;
    link.compressed_bpp_x16 = 0;
    *slice = DscSliceConfig::default();
    *config = DscConfig::default();
}

/// Display-12/13 FEC source capability, with the Gen11 non-A SST exception.
// upstream: intel_dp.c intel_dp_source_supports_fec()
pub const fn intel_dp_source_supports_fec(display_ver: u8, port_a: bool, mst: bool) -> bool {
    display_ver >= 12 || (display_ver == 11 && !port_a && !mst)
}

/// Whether a DPCD receiver advertises FEC.
// upstream: intel_dp.c intel_dp_supports_fec()
pub const fn intel_dp_supports_fec(
    display_ver: u8,
    port_a: bool,
    mst: bool,
    sink_fec: bool,
) -> bool {
    intel_dp_source_supports_fec(display_ver, port_a, mst) && sink_fec
}

/// Source TPS3 support predicate.
// upstream: intel_dp.c intel_dp_source_supports_tps3()
pub const fn intel_dp_source_supports_tps3(display_ver: u8) -> bool {
    display_ver >= 9
}

/// Source TPS4 support predicate.
// upstream: intel_dp.c intel_dp_source_supports_tps4()
pub const fn intel_dp_source_supports_tps4(display_ver: u8) -> bool {
    display_ver >= 10
}

/// Whether a link needs explicit 8b/10b FEC (UHBR FEC is implicit; eDP is optional).
// upstream: intel_dp.c intel_dp_needs_8b10b_fec()
pub const fn intel_dp_needs_8b10b_fec(rate: u32, edp: bool, dsc_enabled: bool) -> bool {
    !intel_dp_is_uhbr(rate) && !edp && dsc_enabled
}

/// Choose the first wide/slow link configuration that carries uncompressed video.
// upstream: intel_dp.c intel_dp_compute_link_config_wide()
pub fn intel_dp_compute_link_config_wide(
    link: &DpLinkState,
    limits: LinkConfigLimits,
    mode: DisplayMode,
    output: OutputFormat,
    max_data_rate: impl Fn(u32, u8) -> u64,
    fec_overhead_flags: u32,
    framework: &impl DpFramework,
) -> Result<SelectedLink, DpError> {
    let mut bpp = if limits.max_link_bpp_x16 > 0 {
        (limits.max_link_bpp_x16 / 16).min(i32::from(u8::MAX)) as u8
    } else {
        limits.max_pipe_bpp
    };
    let min_bpp = if limits.min_link_bpp_x16 > 0 {
        (limits.min_link_bpp_x16 / 16).max(0) as u8
    } else {
        limits.min_pipe_bpp
    };
    while bpp >= min_bpp {
        let link_bpp_x16 = intel_dp_output_format_link_bpp_x16(output, bpp);
        for &rate in &link.common_rates {
            if rate < limits.min_rate || rate > limits.max_rate {
                continue;
            }
            let mut lanes = limits.min_lane_count;
            while lanes <= limits.max_lane_count {
                let needed = intel_dp_link_required(
                    rate,
                    lanes,
                    mode.clock_khz,
                    mode.hdisplay,
                    link_bpp_x16,
                    fec_overhead_flags,
                    framework,
                );
                if needed <= max_data_rate(rate, lanes) {
                    return Ok(SelectedLink {
                        rate,
                        lane_count: lanes,
                        pipe_bpp: bpp,
                        compressed_bpp_x16: 0,
                        dsc: false,
                        fec: fec_overhead_flags & BW_OVERHEAD_FEC != 0,
                        joiner_pipes: 1,
                    });
                }
                lanes = lanes.saturating_mul(2);
            }
        }
        if bpp < 6 {
            break;
        }
        bpp -= 6;
    }
    Err(DpError::NoLinkConfig)
}

/// Display-12 DSC input permits 12 bpc; Gen11 is the 10-bpc exception.
// upstream: intel_dp.c intel_dp_dsc_max_src_input_bpc()
pub const fn intel_dp_dsc_max_src_input_bpc(display_ver: u8) -> u8 {
    if display_ver >= 12 {
        12
    } else if display_ver == 11 {
        10
    } else {
        8
    }
}

fn supported_dsc_bpcs(caps: &DscSinkCaps) -> impl Iterator<Item = u8> + '_ {
    [8, 10, 12]
        .into_iter()
        .filter(|bpc| caps.input_bpc_mask & (1 << (bpc / 2 - 4)) != 0)
}

/// Align minimum uncompressed pipe bpp to a receiver-supported DSC input bpc.
// upstream: intel_dp.c align_min_sink_dsc_input_bpp()
pub fn align_min_sink_dsc_input_bpp(caps: &DscSinkCaps, min_pipe_bpp: u8) -> u8 {
    supported_dsc_bpcs(caps)
        .filter(|bpc| bpc * 3 >= min_pipe_bpp)
        .last()
        .map_or(0, |bpc| bpc * 3)
}

/// Align maximum uncompressed pipe bpp to a receiver-supported DSC input bpc.
// upstream: intel_dp.c align_max_sink_dsc_input_bpp()
pub fn align_max_sink_dsc_input_bpp(caps: &DscSinkCaps, max_pipe_bpp: u8) -> u8 {
    supported_dsc_bpcs(caps)
        .filter(|bpc| bpc * 3 <= max_pipe_bpp)
        .last()
        .map_or(0, |bpc| bpc * 3)
}

/// Compute the greatest DSC input bpp common to source request and sink.
// upstream: intel_dp.c intel_dp_dsc_compute_max_bpp()
pub fn intel_dp_dsc_compute_max_bpp(display_ver: u8, caps: &DscSinkCaps, max_req_bpc: u8) -> u8 {
    let max_bpc = min(intel_dp_dsc_max_src_input_bpc(display_ver), max_req_bpc);
    if max_bpc == 0 {
        0
    } else {
        align_max_sink_dsc_input_bpp(caps, max_bpc * 3)
    }
}

/// Source DSC minor version supported by the selected display generation.
// upstream: intel_dp.c intel_dp_source_dsc_version_minor()
pub const fn intel_dp_source_dsc_version_minor(display_ver: u8) -> u8 {
    if display_ver >= 14 { 2 } else { 1 }
}

/// Extract receiver DSC minor version from receiver-capability bytes.
// upstream: intel_dp.c intel_dp_sink_dsc_version_minor()
pub fn intel_dp_sink_dsc_version_minor(caps: &DscSinkCaps) -> u8 {
    caps.dpcd.first().copied().unwrap_or(0) & 0x0f
}

/// Pick the greatest even slice height starting at 108 that divides vactive.
// upstream: intel_dp.c intel_dp_get_slice_height()
pub fn intel_dp_get_slice_height(vactive: u16) -> u16 {
    let mut height = 108;
    while height <= vactive {
        if vactive % height == 0 {
            return height;
        }
        height += 2;
    }
    2
}

/// Calculate DSC config fields and let DRM core compute RC parameters.
// upstream: intel_dp.c intel_dp_dsc_compute_params()
pub fn intel_dp_dsc_compute_params(
    display_ver: u8,
    caps: &DscSinkCaps,
    mode: DisplayMode,
    output_format: OutputFormat,
    input_bpc: u8,
    compressed_bpp_x16: u16,
    framework: &impl DpFramework,
) -> Result<DscConfig, DpError> {
    let sink_major = (caps.dpcd.first().copied().unwrap_or(0) >> 4) & 0x0f;
    let sink_minor = intel_dp_sink_dsc_version_minor(caps);
    let line_buf_depth = min(
        DP_DSC_MAX_LINE_BUF_DEPTH,
        framework.dsc_line_buf_depth(caps),
    );
    if line_buf_depth == 0 {
        return Err(DpError::Invalid);
    }
    let mut config = DscConfig {
        dsc_major: sink_major,
        dsc_minor: min(intel_dp_source_dsc_version_minor(display_ver), sink_minor),
        line_buf_depth,
        input_bpc,
        compressed_bpp_x16,
        pic_width: mode.hdisplay.min(u16::MAX as u32) as u16,
        pic_height: mode.vdisplay.min(u16::MAX as u32) as u16,
        slice_height: intel_dp_get_slice_height(mode.vdisplay.min(u16::MAX as u32) as u16),
        rc_model_size: DP_DSC_RC_MODEL_SIZE_CONST,
        convert_rgb: output_format == OutputFormat::Rgb
            && caps.dpcd.get(3).copied().unwrap_or(0) & 1 != 0,
        block_pred_enable: caps.dpcd.get(5).copied().unwrap_or(0) & 1 != 0,
        ..DscConfig::default()
    };
    framework.compute_dsc_rc(&mut config)?;
    Ok(config)
}

/// True if receiver DSC capabilities include the chosen output format.
// upstream: intel_dp.c intel_dp_dsc_supports_format()
pub fn intel_dp_dsc_supports_format(
    display_ver: u8,
    caps: &DscSinkCaps,
    format: OutputFormat,
) -> bool {
    match format {
        OutputFormat::Rgb => caps.dpcd.get(3).copied().unwrap_or(0) & 1 != 0,
        OutputFormat::Ycbcr444 => caps.dpcd.get(3).copied().unwrap_or(0) & 2 != 0,
        OutputFormat::Ycbcr420 => {
            min(
                intel_dp_source_dsc_version_minor(display_ver),
                intel_dp_sink_dsc_version_minor(caps),
            ) >= 2
                && caps.supports_ycbcr420
        }
        OutputFormat::Ycbcr422 => caps.dpcd.get(3).copied().unwrap_or(0) & 4 != 0,
    }
}

/// Verify that an encoded DSC mode fits the available payload bandwidth.
// upstream: intel_dp.c is_bw_sufficient_for_dsc_config()
pub fn is_bw_sufficient_for_dsc_config(
    link_clock: u32,
    lane_count: u8,
    mode: DisplayMode,
    dsc_slice_count: u8,
    link_bpp_x16: u16,
    overhead_flags: u32,
    caps: &DpSinkCaps,
    framework: &impl DpFramework,
) -> bool {
    let available = intel_dp_max_link_data_rate(caps, link_clock, lane_count, framework);
    let overhead = intel_dp_link_bw_overhead(
        link_clock,
        lane_count,
        mode.hdisplay,
        dsc_slice_count,
        link_bpp_x16,
        overhead_flags,
        framework,
    );
    let required = intel_dp_effective_data_rate(mode.clock_khz, link_bpp_x16, overhead);
    available >= required
}

/// Search common rates then ascending lane widths for a valid DSC link.
// upstream: intel_dp.c dsc_compute_link_config()
pub fn dsc_compute_link_config(
    link: &DpLinkState,
    limits: LinkConfigLimits,
    mode: DisplayMode,
    slice_count: u8,
    dsc_bpp_x16: u16,
    fec: bool,
    caps: &DpSinkCaps,
    framework: &impl DpFramework,
) -> Result<(u32, u8), DpError> {
    for &rate in &link.common_rates {
        if rate < limits.min_rate || rate > limits.max_rate {
            continue;
        }
        let mut lanes = limits.min_lane_count;
        while lanes <= limits.max_lane_count {
            let fits = if intel_dp_is_uhbr(rate) {
                framework.mtp_tu_fits(rate, lanes, mode, dsc_bpp_x16)
            } else {
                is_bw_sufficient_for_dsc_config(
                    rate,
                    lanes,
                    mode,
                    slice_count,
                    dsc_bpp_x16,
                    if fec { BW_OVERHEAD_FEC } else { 0 },
                    caps,
                    framework,
                )
            };
            if fits {
                return Ok((rate, lanes));
            }
            lanes = lanes.saturating_mul(2);
        }
    }
    Err(DpError::NoLinkConfig)
}

/// Maximum sink DSC output bpp in Q4, using DPCD 67h/68h or DP 2.0 defaults.
// upstream: intel_dp.c intel_dp_dsc_max_sink_compressed_bppx16()
pub fn intel_dp_dsc_max_sink_compressed_bppx16(
    caps: &DscSinkCaps,
    format: OutputFormat,
    bpc: u8,
) -> u16 {
    if caps.sink_max_bpp_x16 != 0 {
        return caps.sink_max_bpp_x16;
    }
    match format {
        OutputFormat::Rgb | OutputFormat::Ycbcr444 => u16::from(3 * bpc) << 4,
        OutputFormat::Ycbcr420 => u16::from(3 * (bpc / 2)) << 4,
        OutputFormat::Ycbcr422 => u16::from(2 * bpc) << 4,
    }
}

/// Maximum sink output bpp in integer form.
// upstream: intel_dp.c intel_dp_dsc_sink_max_compressed_bpp()
pub fn intel_dp_dsc_sink_max_compressed_bpp(
    caps: &DscSinkCaps,
    format: OutputFormat,
    bpc: u8,
) -> u8 {
    (intel_dp_dsc_max_sink_compressed_bppx16(caps, format, bpc) >> 4) as u8
}

/// Minimum DP DSC output bpp required by the format.
// upstream: intel_dp.c intel_dp_dsc_sink_min_compressed_bpp()
pub const fn intel_dp_dsc_sink_min_compressed_bpp(format: OutputFormat) -> u8 {
    if matches!(format, OutputFormat::Ycbcr420) {
        6
    } else {
        8
    }
}

/// Source's minimum compressed bpp is 8.
// upstream: intel_dp.c intel_dp_dsc_min_src_compressed_bpp()
pub const fn intel_dp_dsc_min_src_compressed_bpp() -> u8 {
    8
}

/// Return source compressed-bpp ceiling: 23 before display 13, 27 at display 13+.
// upstream: intel_dp.c dsc_src_max_compressed_bpp()
pub fn dsc_src_max_compressed_bpp(display_ver: u8, force_dsc: bool) -> u8 {
    if force_dsc {
        18
    } else if display_ver < 13 {
        23
    } else {
        27
    }
}

/// DSC Q4 bpp quantization step selected by display generation and receiver.
// upstream: intel_dp.c intel_dp_dsc_bpp_step_x16()
pub fn intel_dp_dsc_bpp_step_x16(
    display_ver: u8,
    sink_incr: u8,
    mst_force_fractional: bool,
    force_bpp: bool,
) -> u16 {
    if display_ver < 14 || sink_incr == 0 || (mst_force_fractional && !force_bpp) {
        16
    } else {
        16 / u16::from(sink_incr)
    }
}

/// Enforce fractional-bpp policy and pre-display13 VESA points.
// upstream: intel_dp.c intel_dp_dsc_valid_compressed_bpp()
pub fn intel_dp_dsc_valid_compressed_bpp(
    display_ver: u8,
    bpp_x16: u16,
    force_fractional: bool,
) -> bool {
    if display_ver >= 13 {
        force_fractional || bpp_x16 & 0x0f == 0
    } else {
        bpp_x16 & 0x0f == 0
            && align_max_vesa_compressed_bpp_x16(i32::from(bpp_x16)) == i32::from(bpp_x16)
    }
}

/// Round a DSC minimum to the display/sink's supported compressed-bpp step.
// upstream: intel_dp.c align_min_compressed_bpp_x16()
pub fn align_min_compressed_bpp_x16(display_ver: u8, min_bpp_x16: u16, bpp_step_x16: u16) -> u16 {
    if display_ver >= 13 && bpp_step_x16 != 0 {
        min_bpp_x16.div_ceil(bpp_step_x16) * bpp_step_x16
    } else {
        align_min_vesa_compressed_bpp_x16(i32::from(min_bpp_x16)).max(0) as u16
    }
}

/// Round a compressed-bpp maximum down after keeping it below uncompressed bpp.
// upstream: intel_dp.c align_max_compressed_bpp_x16()
pub fn align_max_compressed_bpp_x16(
    display_ver: u8,
    pipe_bpp: u8,
    output: OutputFormat,
    max_bpp_x16: u16,
    bpp_step_x16: u16,
) -> u16 {
    let link_bpp_x16 = intel_dp_output_format_link_bpp_x16(output, pipe_bpp);
    let ceiling = min(max_bpp_x16, link_bpp_x16.saturating_sub(bpp_step_x16));
    if display_ver >= 13 && bpp_step_x16 != 0 {
        ceiling / bpp_step_x16 * bpp_step_x16
    } else {
        align_max_vesa_compressed_bpp_x16(i32::from(ceiling)).max(0) as u16
    }
}

/// Find the greatest compressed bpp with a valid link configuration.
// upstream: intel_dp.c dsc_compute_compressed_bpp()
pub fn dsc_compute_compressed_bpp(
    display_ver: u8,
    link: &DpLinkState,
    limits: LinkConfigLimits,
    mode: DisplayMode,
    output: OutputFormat,
    pipe_bpp: u8,
    max_bpp_x16: u16,
    min_bpp_x16: u16,
    step_x16: u16,
    joined_pipes: u8,
    is_edp: bool,
    fec: bool,
    force_fractional: bool,
    dsc_slice_count: u8,
    caps: &DpSinkCaps,
    framework: &impl DpFramework,
) -> Result<SelectedLink, DpError> {
    let maximum =
        align_max_compressed_bpp_x16(display_ver, pipe_bpp, output, max_bpp_x16, step_x16);
    if is_edp {
        return Ok(SelectedLink {
            rate: limits.max_rate,
            lane_count: limits.max_lane_count,
            pipe_bpp,
            compressed_bpp_x16: maximum,
            dsc: true,
            fec,
            joiner_pipes: joined_pipes,
        });
    }
    if step_x16 == 0 {
        return Err(DpError::Invalid);
    }
    let mut bpp_x16 = maximum;
    while bpp_x16 >= min_bpp_x16 {
        if intel_dp_dsc_valid_compressed_bpp(display_ver, bpp_x16, force_fractional) {
            if let Ok((rate, lane_count)) = dsc_compute_link_config(
                link,
                limits,
                mode,
                dsc_slice_count,
                bpp_x16,
                fec,
                caps,
                framework,
            ) {
                return Ok(SelectedLink {
                    rate,
                    lane_count,
                    pipe_bpp,
                    compressed_bpp_x16: bpp_x16,
                    dsc: true,
                    fec,
                    joiner_pipes: joined_pipes,
                });
            }
        }
        if bpp_x16 < step_x16 {
            break;
        }
        bpp_x16 -= step_x16;
    }
    Err(DpError::NoLinkConfig)
}

/// Choose forced or maximum DSC input pipe-bpp and find its compressed link bpp.
// upstream: intel_dp.c intel_dp_dsc_compute_pipe_bpp()
pub fn intel_dp_dsc_compute_pipe_bpp(
    display_ver: u8,
    force_dsc_bpc: u8,
    link: &DpLinkState,
    limits: LinkConfigLimits,
    mode: DisplayMode,
    output: OutputFormat,
    max_bpp_x16: u16,
    min_bpp_x16: u16,
    bpp_step_x16: u16,
    joined_pipes: u8,
    is_edp: bool,
    fec: bool,
    force_fractional: bool,
    dsc_slice_count: u8,
    caps: &DpSinkCaps,
    framework: &impl DpFramework,
) -> Result<SelectedLink, DpError> {
    let forced_bpp = intel_dp_force_dsc_pipe_bpp(force_dsc_bpc, limits);
    let pipe_bpp = if forced_bpp != 0 {
        forced_bpp
    } else {
        limits.max_pipe_bpp
    };
    let mut selected = dsc_compute_compressed_bpp(
        display_ver,
        link,
        limits,
        mode,
        output,
        pipe_bpp,
        max_bpp_x16,
        min_bpp_x16,
        bpp_step_x16,
        joined_pipes,
        is_edp,
        fec,
        force_fractional,
        dsc_slice_count,
        caps,
        framework,
    )?;
    selected.pipe_bpp = pipe_bpp;
    Ok(selected)
}

/// Complete DP DSC setup: FEC, format, pipe bpp, slice layout and PPS parameters.
// upstream: intel_dp.c intel_dp_dsc_compute_config()
pub fn intel_dp_dsc_compute_config(
    display_ver: u8,
    dsc_caps: &DscSinkCaps,
    link_caps: &DpSinkCaps,
    link: &DpLinkState,
    limits: LinkConfigLimits,
    mode: DisplayMode,
    output: OutputFormat,
    mut selected: SelectedLink,
    joined_pipes: u8,
    is_edp: bool,
    is_mst: bool,
    max_cdclk: u32,
    force_dsc: bool,
    force_dsc_bpc: u8,
    force_fractional_bpp: bool,
    framework: &impl DpFramework,
) -> Result<(SelectedLink, DscSliceConfig, DscConfig), DpError> {
    if !intel_dp_dsc_supports_format(display_ver, dsc_caps, output) {
        return Err(DpError::Unsupported);
    }
    selected.fec = intel_dp_needs_8b10b_fec(selected.rate, is_edp, true);
    if !is_mst {
        let bpp_step = intel_dp_dsc_bpp_step_x16(display_ver, dsc_caps.sink_bpp_incr, false, false);
        let min_bpp_x16 = if limits.min_link_bpp_x16 > 0 {
            limits.min_link_bpp_x16 as u16
        } else {
            intel_dp_compute_min_compressed_bpp_x16(display_ver, output, bpp_step)
        };
        let max_link_bpp_x16 = if limits.max_link_bpp_x16 > 0 {
            limits.max_link_bpp_x16 as u16
        } else {
            u16::from(limits.max_pipe_bpp) * 16
        };
        let max_bpp_x16 = compute_max_compressed_bpp_x16(
            display_ver,
            dsc_caps,
            mode.clock_khz,
            mode.hdisplay,
            joined_pipes,
            output,
            limits.max_pipe_bpp,
            max_link_bpp_x16,
            max_cdclk,
            force_dsc,
            bpp_step,
        );
        let slice_count = intel_dp_dsc_get_slice_count(
            dsc_caps,
            mode.clock_khz,
            mode.hdisplay,
            joined_pipes,
            is_edp,
            max_cdclk,
            framework,
        );
        selected = intel_dp_dsc_compute_pipe_bpp(
            display_ver,
            force_dsc_bpc,
            link,
            limits,
            mode,
            output,
            max_bpp_x16,
            min_bpp_x16,
            bpp_step,
            joined_pipes,
            is_edp,
            selected.fec,
            force_fractional_bpp,
            slice_count,
            link_caps,
            framework,
        )?;
    }
    let slice_config = intel_dp_dsc_get_slice_config(
        dsc_caps,
        mode.clock_khz,
        mode.hdisplay,
        joined_pipes,
        is_edp,
        max_cdclk,
        framework,
    )
    .ok_or(DpError::Unsupported)?;
    let mut dsc = intel_dp_dsc_compute_params(
        display_ver,
        dsc_caps,
        mode,
        output,
        selected.pipe_bpp / 3,
        selected.compressed_bpp_x16,
        framework,
    )?;
    dsc.slice_count = slice_config.slices_per_line;
    dsc.slice_width = (mode.hdisplay / u32::from(slice_config.slices_per_line)) as u16;
    selected.dsc = true;
    Ok((selected, slice_config, dsc))
}

/// Preserve protocol-converter power/resume hooks around the DPCD power write.
pub trait DpPowerHooks {
    fn resume_converter(&mut self);
    fn converter_active(&mut self) -> bool;
    fn wait_converter_mode(&mut self);
}

/// Whether an HPD-aware DPCD 1.1 branch requires the receiver kept in D0.
// upstream: intel_dp.c downstream_hpd_needs_d0()
pub fn downstream_hpd_needs_d0(state: &DpRuntimeState) -> bool {
    state.dpcd.first().copied() == Some(0x11)
        && state.dpcd.get(5).copied().unwrap_or(0) & 0x1 != 0
        && state.downstream_ports.first().copied().unwrap_or(0) & 0x80 != 0
}

/// Mark the source OUI initialized once; avoid rewriting a matching OUI.
// upstream: intel_dp.c intel_dp_init_source_oui()
pub fn intel_dp_init_source_oui(state: &mut DpRuntimeState, io: &mut impl DpAuxIo, now_ms: u64) {
    if state.source_oui_valid {
        return;
    }
    state.source_oui_valid = true;
    const SOURCE_OUI: [u8; 3] = [0x00, 0xaa, 0x01];
    let mut oui = [0; 3];
    if io.read(0x0300, &mut oui).is_ok() && oui == SOURCE_OUI {
        state.last_oui_write_ms = now_ms;
        return;
    }
    if io.write(0x0300, &SOURCE_OUI) != Ok(SOURCE_OUI.len()) {
        state.source_oui_valid = false;
    }
    state.last_oui_write_ms = now_ms;
}

/// Invalidate source OUI state so the next D0 path refreshes the value.
// upstream: intel_dp.c intel_dp_invalidate_source_oui()
pub fn intel_dp_invalidate_source_oui(state: &mut DpRuntimeState) {
    state.source_oui_valid = false;
}

/// Wait only the remaining part of the source-OUI refresh interval.
// upstream: intel_dp.c intel_dp_wait_source_oui()
pub fn intel_dp_wait_source_oui(
    state: &DpRuntimeState,
    now_ms: u64,
    refresh_timeout_ms: u32,
    io: &mut impl DpAuxIo,
) {
    let elapsed = now_ms.saturating_sub(state.last_oui_write_ms);
    let remaining = u64::from(refresh_timeout_ms).saturating_sub(elapsed);
    if remaining != 0 {
        io.delay_ms(remaining.min(u64::from(u32::MAX)) as u32);
    }
}

/// Set receiver power. D0 performs OUI setup first and retries three 1-ms writes.
// upstream: intel_dp.c intel_dp_set_power()
pub fn intel_dp_set_power(
    state: &mut DpRuntimeState,
    mode: u8,
    io: &mut impl DpAuxIo,
    hooks: &mut impl DpPowerHooks,
    now_ms: u64,
) -> Result<(), DpError> {
    const DP_SET_POWER: u32 = 0x0600;
    const DP_SET_POWER_D0: u8 = 1;
    if state.dpcd.first().copied().unwrap_or(0) < 0x11 {
        return Ok(());
    }
    if mode != DP_SET_POWER_D0 {
        if downstream_hpd_needs_d0(state) {
            return Ok(());
        }
        return (io.write(DP_SET_POWER, &[mode])? == 1)
            .then_some(())
            .ok_or(DpError::Io);
    }
    hooks.resume_converter();
    intel_dp_init_source_oui(state, io, now_ms);
    let mut result = Err(DpError::Io);
    for _ in 0..3 {
        match io.write(DP_SET_POWER, &[mode]) {
            Ok(1) => {
                result = Ok(());
                break;
            }
            Ok(_) | Err(_) => io.delay_ms(1),
        }
    }
    if result.is_ok() && hooks.converter_active() {
        hooks.wait_converter_mode();
    }
    result
}

/// Update active link params and clear training/retry state in source order.
// upstream: intel_dp.c intel_dp_set_link_params()
pub fn intel_dp_set_link_params(state: &mut DpRuntimeState, link_rate: u32, lane_count: u8) {
    state.train_set = [0; 4];
    state.link.active = false;
    state.needs_modeset_retry = false;
    state.link_rate = link_rate;
    state.lane_count = lane_count;
}

/// Reset link maxima, MST probing values and retraining gates.
// upstream: intel_dp.c intel_dp_reset_link_params()
pub fn intel_dp_reset_link_params(state: &mut DpRuntimeState) {
    state.link.max_lane_count = state.link.max_common_lane_count;
    state.link.max_rate = intel_dp_max_common_rate(&state.link);
    state.link.mst_probed_lane_count = 0;
    state.link.mst_probed_rate = 0;
    state.link.retrain_disabled = false;
    state.link.seq_train_failures = 0;
}

/// Return the maximum display-mode horizontal active size for this generation.
// upstream: intel_dp.c intel_dp_max_hdisplay_per_pipe()
pub const fn intel_dp_max_hdisplay_per_pipe(display_ver: u8) -> u32 {
    if display_ver >= 30 { 6144 } else { 5120 }
}

/// Mode clock and bpp feasibility test for a joiner/DSC candidate.
// upstream: intel_dp.c intel_dp_dotclk_valid()
pub fn intel_dp_dotclk_valid(
    max_dotclk: u32,
    max_uncompressed_dotclk: u32,
    mode_clock: u32,
    htotal: u32,
    dsc_slice_count: u8,
    joined_pipes: u8,
    dsc_bubble_clock: impl FnOnce(u32, u32, u8) -> u32,
) -> bool {
    let mut limit = max_dotclk.saturating_mul(u32::from(joined_pipes));
    let clock = if dsc_slice_count != 0 {
        dsc_bubble_clock(mode_clock, htotal, dsc_slice_count)
    } else {
        limit = max_uncompressed_dotclk.saturating_mul(u32::from(joined_pipes));
        mode_clock
    };
    clock <= limit
}

/// Return whether pipe joiner requires DSC on this source (and always for 4 pipes).
// upstream: intel_dp.c intel_dp_joiner_needs_dsc()
pub const fn intel_dp_joiner_needs_dsc(
    has_uncompressed_joiner: bool,
    num_joined_pipes: u8,
) -> bool {
    (!has_uncompressed_joiner && num_joined_pipes == 2) || num_joined_pipes == 4
}

/// Require connector-side DSC capabilities and FEC for native DP compressed output.
// upstream: intel_dp.c intel_dp_supports_dsc()
pub const fn intel_dp_supports_dsc(
    has_dsc: bool,
    is_dp: bool,
    fec_supported: bool,
    source_dsc_supported: bool,
) -> bool {
    has_dsc && (!is_dp || fec_supported) && source_dsc_supported
}

/// Select DP rate code or eDP rate-select index, preserving G4x's clock fixup.
// upstream: intel_dp.c intel_dp_compute_rate()
pub fn intel_dp_compute_rate(
    g4x: bool,
    use_rate_select: bool,
    sink_rates: &[u32],
    port_clock: u32,
) -> (u8, u8) {
    let port_clock = if g4x && port_clock == 268_800 {
        270_000
    } else {
        port_clock
    };
    if use_rate_select {
        let index = intel_dp_rate_index(sink_rates, port_clock).max(0) as u8;
        (0, index)
    } else {
        let code = match port_clock {
            162_000 => 0x06,
            270_000 => 0x0a,
            324_000 => 0x0c,
            540_000 => 0x14,
            810_000 => 0x1e,
            _ => 0,
        };
        (code, 0)
    }
}

/// Return whether a sink/branch requests VSC SDP for pixel encoding/colorimetry.
// upstream: intel_dp.c intel_dp_needs_vsc_sdp()
pub fn intel_dp_needs_vsc_sdp(output_format: OutputFormat, colorspace: DpColorimetry) -> bool {
    output_format == OutputFormat::Ycbcr420
        || matches!(
            colorspace,
            DpColorimetry::Sycc601
                | DpColorimetry::Opycc601
                | DpColorimetry::Bt2020Ycc
                | DpColorimetry::Bt2020Rgb
                | DpColorimetry::Bt2020Cycc
        )
}

/// Convert DRM colorimetry to the VSC SDP values while preserving limited-range rules.
// upstream: intel_dp.c intel_dp_compute_vsc_colorimetry()
pub fn intel_dp_compute_vsc_colorimetry(color: ColorState) -> VscSdp {
    let mut vsc = VscSdp {
        sdp_type: 0x07,
        revision: if color.panel_replay { 0x07 } else { 0x05 },
        length: 0x13,
        pixelformat: match color.output_format {
            OutputFormat::Ycbcr444 => 0x02,
            OutputFormat::Ycbcr420 => 0x03,
            _ => 0x00,
        },
        bpc: color.pipe_bpp / 3,
        dynamic_range: 1,
        content_type: 0,
        ..VscSdp::default()
    };
    vsc.colorimetry = match color.colorspace {
        DpColorimetry::Bt709Ycc => 0x01,
        DpColorimetry::Xvycc601 => 0x02,
        DpColorimetry::Xvycc709 => 0x03,
        DpColorimetry::DciP3RgbD65 | DpColorimetry::DciP3RgbTheater | DpColorimetry::Sycc601 => {
            0x04
        }
        DpColorimetry::Opycc601 => 0x05,
        DpColorimetry::Bt2020Rgb | DpColorimetry::Bt2020Cycc => 0x06,
        DpColorimetry::Bt2020Ycc => 0x07,
        DpColorimetry::Default if color.output_format == OutputFormat::Ycbcr420 => 0x01,
        DpColorimetry::Default => 0x00,
    };
    if vsc.pixelformat == 0 {
        vsc.dynamic_range = if color.limited_color_range { 1 } else { 0 };
    }
    vsc
}

/// Apply source adaptive-sync SDP eligibility, including branch and ALPM gates.
// upstream: intel_dp.c intel_dp_needs_as_sdp()
pub fn intel_dp_needs_as_sdp(
    supported_v2: bool,
    is_branch: bool,
    alpm_aux_less: bool,
    async_timing_supported: bool,
    vrr_possible: bool,
) -> bool {
    supported_v2 && !is_branch && ((alpm_aux_less && !async_timing_supported) || vrr_possible)
}

/// Build AS SDP only when display supports v2 and PSR/VRR policy requires it.
// upstream: intel_dp.c intel_dp_compute_as_sdp()
pub fn intel_dp_compute_as_sdp(
    supported_v2: bool,
    is_branch: bool,
    alpm_aux_less: bool,
    async_timing_supported: bool,
    vrr_enabled: bool,
    cmrr_enabled: bool,
    mode_refresh: u16,
    vmin_vtotal: u16,
    vmax_vtotal: u16,
    infoframe_enable: &mut u32,
) -> Option<AdaptiveSyncSdp> {
    if !intel_dp_needs_as_sdp(
        supported_v2,
        is_branch,
        alpm_aux_less,
        async_timing_supported,
        vrr_enabled,
    ) {
        return None;
    }
    *infoframe_enable |= intel_dp_sdp_enable_mask(DP_SDP_ADAPTIVE_SYNC);
    Some(AdaptiveSyncSdp {
        sdp_type: 0x22,
        length: 0x09,
        revision: 0x02,
        mode: if cmrr_enabled {
            0x02
        } else if vrr_enabled {
            0x01
        } else {
            0x00
        },
        vtotal: vmin_vtotal,
        target_rr: if cmrr_enabled { mode_refresh } else { 0 },
        target_rr_divider: cmrr_enabled,
        coasting_vtotal: if async_timing_supported {
            vmax_vtotal
        } else {
            0
        },
    })
}

/// Read and cache all standard receiver DSC capabilities.
// upstream: intel_dp.c intel_dp_read_dsc_dpcd()
pub fn intel_dp_read_dsc_dpcd(
    io: &mut impl DpAuxIo,
    caps: &mut DscSinkCaps,
) -> Result<(), DpError> {
    const DP_DSC_SUPPORT: u32 = 0x0060;
    let mut raw = [0u8; 16];
    if io.read(DP_DSC_SUPPORT, &mut raw)? != raw.len() {
        return Err(DpError::Io);
    }
    caps.dpcd = raw;
    Ok(())
}

/// Initialize DSC branch throughput and line-width values to unlimited first.
// upstream: intel_dp.c init_dsc_overall_throughput_limits()
pub fn init_dsc_overall_throughput_limits(
    caps: &mut DscSinkCaps,
    is_branch: bool,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    caps.throughput_rgb_yuv444 = u32::MAX;
    caps.throughput_yuv422_420 = u32::MAX;
    caps.max_line_width = u16::MAX;
    if !is_branch {
        return;
    }
    const DP_DSC_BRANCH_OVERALL_THROUGHPUT_0: u32 = 0x060f;
    let mut branch_caps = [0u8; 4];
    if io.read(DP_DSC_BRANCH_OVERALL_THROUGHPUT_0, &mut branch_caps) != Ok(branch_caps.len()) {
        return;
    }
    let rgb = framework.dsc_branch_overall_throughput(&branch_caps, true);
    let yuv = framework.dsc_branch_overall_throughput(&branch_caps, false);
    let line_width = framework.dsc_branch_max_line_width(&branch_caps);
    caps.throughput_rgb_yuv444 = if rgb != 0 { rgb } else { u32::MAX };
    caps.throughput_yuv422_420 = if yuv != 0 { yuv } else { u32::MAX };
    caps.max_line_width = if line_width != 0 {
        line_width
    } else {
        u16::MAX
    };
}

/// Read sink DSC and FEC caps in source order; clear stale fields before probing.
// upstream: intel_dp.c intel_dp_get_dsc_sink_cap()
pub fn intel_dp_get_dsc_sink_cap(
    dpcd_rev: u8,
    is_branch: bool,
    throughput_quirk: bool,
    caps: &mut DscSinkCaps,
    fec_capability: &mut u8,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    caps.dpcd = [0; 16];
    *fec_capability = 0;
    *caps = DscSinkCaps::default();
    *fec_capability = 0;
    if dpcd_rev < DP_DPCD_REV_14 {
        return;
    }
    if intel_dp_read_dsc_dpcd(io, caps).is_err() {
        return;
    }
    const DP_FEC_CAPABILITY: u32 = 0x0090;
    let mut fec = [0u8; 1];
    if io.read(DP_FEC_CAPABILITY, &mut fec) != Ok(1) {
        return;
    }
    *fec_capability = fec[0];
    if caps.dpcd[0] & 1 == 0 {
        return;
    }
    caps.supports = true;
    caps.branch = is_branch;
    caps.throughput_quirk = throughput_quirk;
    caps.max_slice_width = u16::from(caps.dpcd.get(2).copied().unwrap_or(0)) * 320;
    caps.max_slices = caps.dpcd.get(4).copied().unwrap_or(0) & 0x0f;
    caps.slice_count_mask = framework.dsc_supported_slice_counts(caps);
    caps.sink_bpp_incr = caps.dpcd.get(6).copied().unwrap_or(0) & 0x0f;
    caps.sink_max_bpp_x16 = u16::from_le_bytes([
        caps.dpcd.get(7).copied().unwrap_or(0),
        caps.dpcd.get(8).copied().unwrap_or(0),
    ]);
    caps.input_bpc_mask = caps.dpcd.get(1).copied().unwrap_or(0) & 0x07;
    caps.supports_ycbcr420 = caps.dpcd.get(3).copied().unwrap_or(0) & 0x08 != 0;
    init_dsc_overall_throughput_limits(caps, is_branch, io, framework);
}

/// Read eDP receiver DSC caps at eDP 1.4 or later.
// upstream: intel_dp.c intel_edp_get_dsc_sink_cap()
pub fn intel_edp_get_dsc_sink_cap(
    edp_dpcd_rev: u8,
    caps: &mut DscSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    if edp_dpcd_rev < 0x04 {
        return;
    }
    if intel_dp_read_dsc_dpcd(io, caps).is_err() {
        return;
    }
    if caps.dpcd[0] & 1 != 0 {
        caps.supports = true;
        init_dsc_overall_throughput_limits(caps, false, io, framework);
    }
}

/// Gate DP DSC probing on the platform's source DSC capability.
// upstream: intel_dp.c intel_dp_detect_dsc_caps()
pub fn intel_dp_detect_dsc_caps(
    has_source_dsc: bool,
    dpcd_rev: u8,
    edp_dpcd_rev: u8,
    is_edp: bool,
    is_branch: bool,
    throughput_quirk: bool,
    caps: &mut DscSinkCaps,
    fec_capability: &mut u8,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    *caps = DscSinkCaps::default();
    *fec_capability = 0;
    if !has_source_dsc {
        return;
    }
    if is_edp {
        intel_edp_get_dsc_sink_cap(edp_dpcd_rev, caps, io, framework);
    } else {
        intel_dp_get_dsc_sink_cap(
            dpcd_rev,
            is_branch,
            throughput_quirk,
            caps,
            fec_capability,
            io,
            framework,
        );
    }
}

/// Per-connector DSC AUX reference count for shared MST topology devices.
// upstream: intel_dp.c intel_dp_dsc_aux_ref_count()
pub fn intel_dp_dsc_aux_ref_count(
    members: &[DscAuxMember],
    topology_id: u64,
    aux_id: u64,
    is_mst: bool,
    own_enabled: bool,
) -> usize {
    if !is_mst {
        return usize::from(own_enabled);
    }
    members
        .iter()
        .filter(|member| {
            member.topology_id == topology_id && member.aux_id == aux_id && member.enabled
        })
        .count()
}

/// Mark DSC AUX reference acquired and report whether hardware must be enabled.
// upstream: intel_dp.c intel_dp_dsc_aux_get_ref()
pub fn intel_dp_dsc_aux_get_ref(
    members: &mut [DscAuxMember],
    index: usize,
    topology_id: u64,
    aux_id: u64,
) -> Result<bool, DpError> {
    let should_enable = !members
        .iter()
        .any(|other| other.topology_id == topology_id && other.aux_id == aux_id && other.enabled);
    members.get_mut(index).ok_or(DpError::Invalid)?.enabled = true;
    Ok(should_enable)
}

/// Mark DSC AUX reference released and report whether hardware can be disabled.
// upstream: intel_dp.c intel_dp_dsc_aux_put_ref()
pub fn intel_dp_dsc_aux_put_ref(
    members: &mut [DscAuxMember],
    index: usize,
    topology_id: u64,
    aux_id: u64,
) -> Result<bool, DpError> {
    members.get_mut(index).ok_or(DpError::Invalid)?.enabled = false;
    Ok(!members
        .iter()
        .any(|other| other.topology_id == topology_id && other.aux_id == aux_id && other.enabled))
}

/// Read-modify-write DSC decompression or pass-through bit in DP_DSC_ENABLE.
// upstream: intel_dp.c write_dsc_decompression_flag()
pub fn write_dsc_decompression_flag(
    io: &mut impl DpAuxIo,
    flag: u8,
    set: bool,
) -> Result<(), DpError> {
    const DP_DSC_ENABLE: u32 = 0x0160;
    let mut value = [0u8; 1];
    if io.read(DP_DSC_ENABLE, &mut value)? != 1 {
        return Err(DpError::Io);
    }
    if set {
        value[0] |= flag;
    } else {
        value[0] &= !flag;
    }
    if io.write(DP_DSC_ENABLE, &value)? != 1 {
        return Err(DpError::Io);
    }
    Ok(())
}

/// Program sink decompression enable.
// upstream: intel_dp.c intel_dp_sink_set_dsc_decompression()
pub fn intel_dp_sink_set_dsc_decompression(
    io: &mut impl DpAuxIo,
    enable: bool,
) -> Result<(), DpError> {
    write_dsc_decompression_flag(io, 1 << 0, enable)
}

/// Program the last MST branch's DSC pass-through enable bit.
// upstream: intel_dp.c intel_dp_sink_set_dsc_passthrough()
pub fn intel_dp_sink_set_dsc_passthrough(
    io: Option<&mut impl DpAuxIo>,
    enable: bool,
) -> Result<(), DpError> {
    match io {
        Some(io) => write_dsc_decompression_flag(io, 1 << 1, enable),
        None => Ok(()),
    }
}

/// Enable sink/branch DSC decompression only on the first shared AUX reference.
// upstream: intel_dp.c intel_dp_sink_enable_decompression()
pub fn intel_dp_sink_enable_decompression(
    compressing: bool,
    members: &mut [DscAuxMember],
    member_index: usize,
    topology_id: u64,
    aux_id: u64,
    sink_aux: &mut impl DpAuxIo,
    mut branch_aux: Option<&mut impl DpAuxIo>,
) -> Result<(), DpError> {
    if !compressing {
        return Ok(());
    }
    if members.get(member_index).ok_or(DpError::Invalid)?.enabled {
        return Err(DpError::Invalid);
    }
    if !intel_dp_dsc_aux_get_ref(members, member_index, topology_id, aux_id)? {
        return Ok(());
    }
    let mut first_error = None;
    if let Some(branch) = branch_aux.as_deref_mut() {
        if let Err(error) = write_dsc_decompression_flag(branch, 1 << 1, true) {
            first_error = Some(error);
        }
    }
    if let Err(error) = intel_dp_sink_set_dsc_decompression(sink_aux, true) {
        if first_error.is_none() {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Disable sink/branch DSC only on the last shared AUX reference.
// upstream: intel_dp.c intel_dp_sink_disable_decompression()
pub fn intel_dp_sink_disable_decompression(
    was_compressing: bool,
    members: &mut [DscAuxMember],
    member_index: usize,
    topology_id: u64,
    aux_id: u64,
    sink_aux: &mut impl DpAuxIo,
    mut branch_aux: Option<&mut impl DpAuxIo>,
) -> Result<(), DpError> {
    if !was_compressing {
        return Ok(());
    }
    if !members.get(member_index).ok_or(DpError::Invalid)?.enabled {
        return Err(DpError::Invalid);
    }
    if !intel_dp_dsc_aux_put_ref(members, member_index, topology_id, aux_id)? {
        return Ok(());
    }
    let mut first_error = intel_dp_sink_set_dsc_decompression(sink_aux, false).err();
    if let Some(branch) = branch_aux.as_deref_mut() {
        if let Err(error) = write_dsc_decompression_flag(branch, 1 << 1, false) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

const DP_SDP_VSC: u8 = 0x07;
const DP_SDP_ADAPTIVE_SYNC: u8 = 0x22;
const DP_SDP_PPS: u8 = 0x10;
const HDMI_PACKET_TYPE_GAMUT_METADATA: u8 = 0x87;

fn intel_dp_sdp_enable_mask(packet_type: u8) -> u32 {
    match packet_type {
        0x03 => 1 << 0,
        0x0a => 1 << 1,
        0x07 => 1 << 2,
        0x22 => 1 << 3,
        0x82 => 1 << 4,
        0x83 => 1 << 5,
        0x81 => 1 << 6,
        0x87 => 1 << 7,
        _ => 0,
    }
}

/// Number of vertical lines needed for the selected DP SDP.
// upstream: intel_dp.c intel_dp_get_lines_for_sdp()
pub fn intel_dp_get_lines_for_sdp(crtc: SdpGuardbandState, packet_type: u8) -> u16 {
    match packet_type {
        0x20 | 0x21 => 10,
        HDMI_PACKET_TYPE_GAMUT_METADATA => 8,
        DP_SDP_PPS => 7,
        DP_SDP_ADAPTIVE_SYNC => crtc.vrr_vsync_start.saturating_add(1),
        _ => 0,
    }
}

/// Return the minimum vertical blanking guardband for enabled packet types.
// upstream: intel_dp.c intel_dp_sdp_min_guardband()
pub fn intel_dp_sdp_min_guardband(crtc: SdpGuardbandState, assume_all_enabled: bool) -> u16 {
    let mut guardband = 0;
    if assume_all_enabled
        || crtc.enable & intel_dp_sdp_enable_mask(HDMI_PACKET_TYPE_GAMUT_METADATA) != 0
    {
        guardband = max(
            guardband,
            intel_dp_get_lines_for_sdp(crtc, HDMI_PACKET_TYPE_GAMUT_METADATA),
        );
    }
    if assume_all_enabled || crtc.dsc_enabled {
        guardband = max(guardband, intel_dp_get_lines_for_sdp(crtc, DP_SDP_PPS));
    }
    if crtc.enable & intel_dp_sdp_enable_mask(DP_SDP_ADAPTIVE_SYNC) != 0 {
        guardband = max(
            guardband,
            intel_dp_get_lines_for_sdp(crtc, DP_SDP_ADAPTIVE_SYNC),
        );
    }
    guardband
}

/// Validate vblank guardband after late PSR/VRR/DSC computation.
// upstream: intel_dp.c intel_dp_sdp_compute_config_late()
pub fn intel_dp_sdp_compute_config_late(
    crtc: SdpGuardbandState,
    vblank_length: u16,
) -> Result<(), DpError> {
    if vblank_length < intel_dp_sdp_min_guardband(crtc, false) {
        Err(DpError::Invalid)
    } else {
        Ok(())
    }
}

/// AS SDP remains at the T1 position for ALPM/DC6v operation.
// upstream: intel_dp.c intel_dp_as_sdp_transmission_time()
pub const fn intel_dp_as_sdp_transmission_time() -> u8 {
    0
}

/// Pack AS SDP values into the exact 4-byte header plus 32-byte data block.
// upstream: intel_dp.c intel_dp_as_sdp_pack()
pub fn intel_dp_as_sdp_pack(
    as_sdp: AdaptiveSyncSdp,
    size: usize,
) -> Result<(DpSdp, usize), DpError> {
    let length = core::mem::size_of::<DpSdp>();
    if size < length {
        return Err(DpError::NoSpace);
    }
    let mut sdp = DpSdp::default();
    sdp.header = SdpHeader {
        hb0: 0,
        hb1: as_sdp.sdp_type,
        hb2: as_sdp.revision,
        hb3: as_sdp.length,
    };
    sdp.db[0] = as_sdp.mode;
    sdp.db[1] = as_sdp.vtotal as u8;
    sdp.db[2] = (as_sdp.vtotal >> 8) as u8;
    sdp.db[3] = as_sdp.target_rr as u8;
    sdp.db[4] = ((as_sdp.target_rr >> 8) as u8) & 0x03;
    if as_sdp.target_rr_divider {
        sdp.db[4] |= 0x20;
    }
    sdp.db[7] = as_sdp.coasting_vtotal as u8;
    sdp.db[8] = (as_sdp.coasting_vtotal >> 8) as u8;
    Ok((sdp, length))
}

/// Unpack and validate the AS SDP header and payload.
// upstream: intel_dp.c intel_dp_as_sdp_unpack()
pub fn intel_dp_as_sdp_unpack(sdp: DpSdp, size: usize) -> Result<AdaptiveSyncSdp, DpError> {
    if size < core::mem::size_of::<DpSdp>()
        || sdp.header.hb0 != 0
        || sdp.header.hb1 != DP_SDP_ADAPTIVE_SYNC
        || sdp.header.hb3 & 0x3f != 9
    {
        return Err(DpError::Invalid);
    }
    Ok(AdaptiveSyncSdp {
        sdp_type: sdp.header.hb1,
        length: sdp.header.hb3 & 0x3f,
        revision: sdp.header.hb2,
        mode: sdp.db[0] & 0x03,
        vtotal: u16::from(sdp.db[1]) | (u16::from(sdp.db[2]) << 8),
        target_rr: u16::from(sdp.db[3]) | (u16::from(sdp.db[4] & 0x03) << 8),
        target_rr_divider: sdp.db[4] & 0x20 != 0,
        coasting_vtotal: u16::from(sdp.db[7]) | (u16::from(sdp.db[8]) << 8),
    })
}

/// Unpack PSR/Panel Replay or colorimetry-bearing VSC SDP variants.
// upstream: intel_dp.c intel_dp_vsc_sdp_unpack()
pub fn intel_dp_vsc_sdp_unpack(sdp: DpSdp, size: usize) -> Result<VscSdp, DpError> {
    if size < core::mem::size_of::<DpSdp>() || sdp.header.hb0 != 0 || sdp.header.hb1 != DP_SDP_VSC {
        return Err(DpError::Invalid);
    }
    let mut vsc = VscSdp {
        sdp_type: sdp.header.hb1,
        revision: sdp.header.hb2,
        length: sdp.header.hb3,
        ..VscSdp::default()
    };
    match (sdp.header.hb2, sdp.header.hb3) {
        (0x02, 0x08) | (0x04, 0x0e) | (0x06, 0x10) => Ok(vsc),
        (0x05, 0x13) => {
            vsc.pixelformat = (sdp.db[16] >> 4) & 0x0f;
            vsc.colorimetry = sdp.db[16] & 0x0f;
            vsc.dynamic_range = (sdp.db[17] >> 7) & 1;
            vsc.bpc = match sdp.db[17] & 7 {
                0 => 6,
                1 => 8,
                2 => 10,
                3 => 12,
                4 => 16,
                _ => return Err(DpError::Invalid),
            };
            vsc.content_type = sdp.db[18] & 7;
            Ok(vsc)
        }
        _ => Err(DpError::Invalid),
    }
}

/// Pack a DRM static-HDR infoframe in DP SDP form.
// upstream: intel_dp.c intel_dp_hdr_metadata_infoframe_sdp_pack()
pub fn intel_dp_hdr_metadata_infoframe_sdp_pack(
    frame: HdrMetadataInfoframe,
    size: usize,
) -> Result<(DpSdp, usize), DpError> {
    let length = core::mem::size_of::<DpSdp>();
    if size < length {
        return Err(DpError::NoSpace);
    }
    if frame.length != 26 {
        return Err(DpError::Invalid);
    }
    let mut sdp = DpSdp::default();
    sdp.header = SdpHeader {
        hb0: 0,
        hb1: frame.kind,
        hb2: 0x1d,
        hb3: 0x4c,
    };
    sdp.db[0] = frame.version;
    sdp.db[1] = frame.length;
    sdp.db[2..28].copy_from_slice(&frame.payload);
    Ok((sdp, 4 + 2 + 26))
}

/// Validate DP static-HDR SDP header and decode the underlying metadata bytes.
// upstream: intel_dp.c intel_dp_hdr_metadata_infoframe_sdp_unpack()
pub fn intel_dp_hdr_metadata_infoframe_sdp_unpack(
    sdp: DpSdp,
    size: usize,
) -> Result<HdrMetadataInfoframe, DpError> {
    if size < core::mem::size_of::<DpSdp>()
        || sdp.header.hb0 != 0
        || sdp.header.hb1 != HDMI_PACKET_TYPE_GAMUT_METADATA
        || sdp.header.hb2 != 0x1d
        || sdp.header.hb3 & 3 != 0
        || (sdp.header.hb3 >> 2) & 0x3f != 0x13
        || sdp.db[0] != 1
        || sdp.db[1] != 26
    {
        return Err(DpError::Invalid);
    }
    let mut payload = [0u8; 26];
    payload.copy_from_slice(&sdp.db[2..28]);
    Ok(HdrMetadataInfoframe {
        kind: sdp.header.hb1,
        version: sdp.db[0],
        length: sdp.db[1],
        payload,
    })
}

/// Read/unpack AS SDP only when its infoframe-enable slot is set.
// upstream: intel_dp.c intel_read_dp_as_sdp()
pub fn intel_read_dp_as_sdp(
    enable: u32,
    io: &mut impl DpSdpIo,
) -> Result<Option<AdaptiveSyncSdp>, DpError> {
    if enable & intel_dp_sdp_enable_mask(DP_SDP_ADAPTIVE_SYNC) == 0 {
        return Ok(None);
    }
    let mut sdp = DpSdp::default();
    io.read_sdp(DP_SDP_ADAPTIVE_SYNC, &mut sdp);
    Ok(Some(intel_dp_as_sdp_unpack(
        sdp,
        core::mem::size_of::<DpSdp>(),
    )?))
}

/// Read/unpack VSC SDP only when its infoframe-enable slot is set.
// upstream: intel_dp.c intel_read_dp_vsc_sdp()
pub fn intel_read_dp_vsc_sdp(
    enable: u32,
    io: &mut impl DpSdpIo,
) -> Result<Option<VscSdp>, DpError> {
    if enable & intel_dp_sdp_enable_mask(DP_SDP_VSC) == 0 {
        return Ok(None);
    }
    let mut sdp = DpSdp::default();
    io.read_sdp(DP_SDP_VSC, &mut sdp);
    Ok(Some(intel_dp_vsc_sdp_unpack(
        sdp,
        core::mem::size_of::<DpSdp>(),
    )?))
}

/// Read/unpack static HDR SDP only when its infoframe-enable slot is set.
// upstream: intel_dp.c intel_read_dp_hdr_metadata_infoframe_sdp()
pub fn intel_read_dp_hdr_metadata_infoframe_sdp(
    enable: u32,
    io: &mut impl DpSdpIo,
) -> Result<Option<HdrMetadataInfoframe>, DpError> {
    if enable & intel_dp_sdp_enable_mask(HDMI_PACKET_TYPE_GAMUT_METADATA) == 0 {
        return Ok(None);
    }
    let mut sdp = DpSdp::default();
    io.read_sdp(HDMI_PACKET_TYPE_GAMUT_METADATA, &mut sdp);
    Ok(Some(intel_dp_hdr_metadata_infoframe_sdp_unpack(
        sdp,
        core::mem::size_of::<DpSdp>(),
    )?))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodedDpSdp {
    Vsc(VscSdp),
    AdaptiveSync(AdaptiveSyncSdp),
    HdrMetadata(HdrMetadataInfoframe),
}

/// Determine whether this display/port supports static HDR metadata DIP.
// upstream: intel_dp.c intel_dp_has_gamut_metadata_dip()
pub const fn intel_dp_has_gamut_metadata_dip(display_ver: u8, port_a: bool, lspcon: bool) -> bool {
    !lspcon && (display_ver >= 11 || !port_a)
}

/// DP connector is eDP only after encoder type selection has completed.
// upstream: intel_dp.c intel_dp_is_edp()
pub const fn intel_dp_is_edp(connector_is_edp: bool) -> bool {
    connector_is_edp
}

/// Source DP DSC capability must be present on this SST/MST/eDP connector.
// upstream: intel_dp.c intel_dp_has_dsc()
pub const fn intel_dp_has_dsc(
    source_has_dsc: bool,
    mst: bool,
    source_has_mst_dsc: bool,
    edp_dsc_disabled: bool,
    sink_supports_dsc: bool,
) -> bool {
    source_has_dsc && (!mst || source_has_mst_dsc) && !edp_dsc_disabled && sink_supports_dsc
}

/// Check whether receiver DPCD/EDID identify an HDMI 2.1 sink behind a branch.
// upstream: intel_dp.c intel_dp_is_hdmi_2_1_sink()
pub const fn intel_dp_is_hdmi_2_1_sink(
    is_branch: bool,
    is_hdmi_sink: bool,
    max_frl_rate: u8,
) -> bool {
    is_branch && is_hdmi_sink && max_frl_rate > 0
}

/// eDP AS SDP v2 support is implied by FAVT parse or the Panel Replay rule.
// upstream: intel_dp.c intel_dp_sink_supports_as_sdp_v2()
pub fn intel_dp_sink_supports_as_sdp_v2(
    io: &mut impl DpAuxIo,
    panel_replay_supported: bool,
    async_timing_in_pr_supported: bool,
) -> bool {
    const DP_DPRX_FEATURE_ENUMERATION_LIST_CONT_1: u32 = 0x2214;
    const DP_AS_SDP_FAVT_PAYLOAD_FIELDS_PARSING_SUPPORTED: u8 = 1 << 2;
    let mut features = [0u8; 1];
    if io.read(DP_DPRX_FEATURE_ENUMERATION_LIST_CONT_1, &mut features) == Ok(1)
        && features[0] & DP_AS_SDP_FAVT_PAYLOAD_FIELDS_PARSING_SUPPORTED != 0
    {
        return true;
    }
    panel_replay_supported && !async_timing_in_pr_supported
}

/// Read AS-SDP support and choose eDP-implied or external-sink v2 behavior.
// upstream: intel_dp.c intel_dp_detect_sdp_caps()
pub fn intel_dp_detect_sdp_caps(
    state: &mut DpRuntimeState,
    display_has_as_sdp: bool,
    standard_as_supported: bool,
    is_edp: bool,
    io: &mut impl DpAuxIo,
    async_timing_in_pr_supported: bool,
) {
    state.as_sdp_supported = display_has_as_sdp && standard_as_supported;
    if !state.as_sdp_supported {
        state.as_sdp_v2_supported = false;
        return;
    }
    if is_edp {
        state.as_sdp_v2_supported = true;
    } else {
        state.as_sdp_v2_supported = intel_dp_sink_supports_as_sdp_v2(
            io,
            state.panel_replay_supported,
            async_timing_in_pr_supported,
        );
    }
}

/// Convert pipe bpp to the minimum DP link-bpp requirement for output encoding.
// upstream: intel_dp.c intel_dp_mode_min_link_bpp_x16()
pub fn intel_dp_mode_min_link_bpp_x16(format: OutputFormat) -> u16 {
    intel_dp_output_format_link_bpp_x16(format, intel_dp_min_bpp(format))
}

/// Choose the greatest HDMI sink FRL rate allowed by regular and DSC sink caps.
// upstream: intel_dp.c intel_dp_hdmi_sink_max_frl()
pub fn intel_dp_hdmi_sink_max_frl(
    max_lanes: u8,
    rate_per_lane_gbps: u8,
    pcon_dsc12: bool,
    sink_dsc12: bool,
    dsc_max_lanes: u8,
    dsc_rate_per_lane_gbps: u8,
) -> u8 {
    let mut max_frl = max_lanes.saturating_mul(rate_per_lane_gbps);
    if pcon_dsc12 && sink_dsc12 && dsc_max_lanes != 0 && dsc_rate_per_lane_gbps != 0 {
        max_frl = min(
            max_frl,
            dsc_max_lanes.saturating_mul(dsc_rate_per_lane_gbps),
        );
    }
    max_frl
}

/// Decode highest supported FRL bandwidth bit into Gbps.
// upstream: intel_dp.c intel_dp_pcon_get_frl_mask()
pub fn intel_dp_pcon_get_frl_mask(frl_bw_mask: u8) -> u8 {
    const RATES: [u8; 6] = [9, 18, 24, 32, 40, 48];
    (0..RATES.len())
        .rev()
        .find(|i| frl_bw_mask & (1 << *i) != 0)
        .map_or(0, |i| RATES[i])
}

/// Map supported FRL rate back into PCON's FRL capability mask.
// upstream: intel_dp.c intel_dp_pcon_set_frl_mask()
pub fn intel_dp_pcon_set_frl_mask(max_frl: u8) -> u8 {
    match max_frl {
        9 => 1 << 0,
        18 => 1 << 1,
        24 => 1 << 2,
        32 => 1 << 3,
        40 => 1 << 4,
        48 => 1 << 5,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PconHdmiMode {
    #[default]
    Tmds,
    Frl,
}

/// PCON mode/training helper calls supplied by DRM's DisplayPort helper layer.
pub trait DpPconOps {
    fn hdmi_link_active(&mut self) -> bool;
    fn hdmi_link_status(&mut self) -> (PconHdmiMode, u8);
    fn frl_prepare(&mut self, sink_dsc: bool) -> Result<(), DpError>;
    fn frl_ready(&mut self) -> bool;
    fn frl_configure_1(&mut self, max_frl_gbps: u8) -> Result<(), DpError>;
    fn frl_configure_2(&mut self, max_frl_mask: u8) -> Result<(), DpError>;
    fn frl_enable(&mut self) -> Result<(), DpError>;
    fn report_frl_link_error(&mut self);
    fn convert_rgb_to_ycbcr(&mut self, enable: bool) -> Result<(), DpError>;
}

/// Test active FRL training against required rate mask.
// upstream: intel_dp.c intel_dp_pcon_is_frl_trained()
pub fn intel_dp_pcon_is_frl_trained(pcon: &mut impl DpPconOps, max_frl_bw_mask: u8) -> bool {
    if !pcon.hdmi_link_active() {
        return false;
    }
    let (mode, trained_mask) = pcon.hdmi_link_status();
    mode == PconHdmiMode::Frl && trained_mask >= max_frl_bw_mask
}

/// Start or reuse PCON FRL training, with 500-ms ready and 1-s link timeouts.
// upstream: intel_dp.c intel_dp_pcon_start_frl_training()
pub fn intel_dp_pcon_start_frl_training(
    max_pcon_frl_gbps: u8,
    max_edid_frl_gbps: u8,
    trained_rate_gbps: &mut u8,
    trained: &mut bool,
    io: &mut impl DpAuxIo,
    pcon: &mut impl DpPconOps,
) -> Result<(), DpError> {
    let max_frl = min(max_pcon_frl_gbps, max_edid_frl_gbps);
    if max_frl == 0 {
        return Err(DpError::Invalid);
    }
    let max_mask = intel_dp_pcon_set_frl_mask(max_frl);
    let trained_mask;
    if intel_dp_pcon_is_frl_trained(pcon, max_mask) {
        let (_, mask) = pcon.hdmi_link_status();
        trained_mask = mask;
    } else {
        pcon.frl_prepare(false)?;
        let mut ready = false;
        for _ in 0..500 {
            if pcon.frl_ready() {
                ready = true;
                break;
            }
            io.delay_ms(1);
        }
        if !ready {
            return Err(DpError::Io);
        }
        pcon.frl_configure_1(max_frl)?;
        pcon.frl_configure_2(max_mask)?;
        pcon.frl_enable()?;
        let mut active = false;
        for _ in 0..1000 {
            if intel_dp_pcon_is_frl_trained(pcon, max_mask) {
                active = true;
                break;
            }
            io.delay_ms(1);
        }
        if !active {
            return Err(DpError::Io);
        }
        let (_, mask) = pcon.hdmi_link_status();
        trained_mask = mask;
    }
    *trained_rate_gbps = intel_dp_pcon_get_frl_mask(trained_mask);
    *trained = true;
    Ok(())
}

/// Read protocol-converter DSC encoder capabilities, clearing stale cache first.
// upstream: intel_dp.c intel_dp_get_pcon_dsc_cap()
pub fn intel_dp_get_pcon_dsc_cap(
    state: &mut DpRuntimeState,
    is_branch: bool,
    io: &mut impl DpAuxIo,
) -> Result<(), DpError> {
    const DP_PCON_DSC_ENCODER: u32 = 0x0092;
    state.pcon_dsc_dpcd.clear();
    state.pcon_dsc_dpcd.resize(13, 0);
    if !is_branch {
        return Ok(());
    }
    if io.read(DP_PCON_DSC_ENCODER, &mut state.pcon_dsc_dpcd)? != 13 {
        state.pcon_dsc_dpcd.fill(0);
        return Err(DpError::Io);
    }
    Ok(())
}

/// eDP-specific DPCD operations whose details live in panel/PSR/VBT frameworks.
pub trait DpEdpDpcdOps {
    fn read_receiver_caps(&mut self, state: &mut DpRuntimeState) -> Result<(), DpError>;
    fn read_desc_and_apply_quirks(&mut self, state: &mut DpRuntimeState);
    fn read_edp_dpcd(&mut self) -> Option<(u8, Vec<u8>)>;
    fn init_psr_dpcd(&mut self);
    fn detect_dsc_caps(&mut self);
}

/// Initialize eDP's receiver/extended DPCD, ALPM/PSR and link-cap cache once.
// upstream: intel_dp.c intel_edp_init_dpcd()
pub fn intel_edp_init_dpcd(
    state: &mut DpRuntimeState,
    link: &mut DpLinkState,
    sink_caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    edp_rates: impl FnMut(u32) -> bool,
    hbr2_quirk: bool,
    now_ms: u64,
    ops: &mut impl DpEdpDpcdOps,
) -> bool {
    if ops.read_receiver_caps(state).is_err() {
        return false;
    }
    ops.read_desc_and_apply_quirks(state);
    state.colorimetry_support = intel_dp_get_colorimetry_status(io);
    let mut edp_rev = 0;
    if let Some((reported_rev, edp_caps)) = ops.read_edp_dpcd() {
        edp_rev = reported_rev;
        state.edp_dpcd = edp_caps;
        state.use_max_params = reported_rev < 0x04;
    }
    intel_dp_init_source_oui(state, io, now_ms);
    let mut alpm_cap = [0u8; 1];
    if io.read(0x002e, &mut alpm_cap) != Ok(1) {
        return false;
    }
    state.alpm_dpcd = alpm_cap[0];
    ops.init_psr_dpcd();
    let _rate_select = intel_edp_set_sink_rates(
        link, edp_rev, sink_caps, io, framework, hbr2_quirk, edp_rates,
    );
    intel_dp_set_max_sink_lane_count(link, sink_caps);
    ops.detect_dsc_caps();
    true
}

/// HDMI PCON DSC helpers delegated to DRM's DSC/HDMI implementation.
pub trait DpPconDscOps {
    fn pcon_encoder_dsc12(&self, pcon_dpcd: &[u8]) -> bool;
    fn hdmi_sink_dsc12(&self) -> bool;
    fn max_slices(&self, pcon_dpcd: &[u8]) -> u8;
    fn max_slice_width(&self, pcon_dpcd: &[u8]) -> u16;
    fn bpp_increment(&self, pcon_dpcd: &[u8]) -> u8;
    fn hdmi_dsc_max_slices(&self) -> u8;
    fn hdmi_dsc_clock_per_slice(&self) -> u32;
    fn hdmi_dsc_all_bpp(&self) -> bool;
    fn hdmi_dsc_max_chunk_bytes(&self) -> u32;
    fn slice_height(&self, vactive: u32) -> u16;
    fn slice_count(
        &self,
        mode: DisplayMode,
        pcon_slices: u8,
        pcon_max_width: u16,
        hdmi_slices: u8,
        hdmi_throughput: u32,
    ) -> u8;
    fn compressed_bpp(
        &self,
        increment: u8,
        slice_width: u16,
        slices: u8,
        format: OutputFormat,
        all_bpp: bool,
        max_chunk_bytes: u32,
    ) -> u16;
    fn pps_override(&mut self, bytes: &[u8; 6]) -> Result<(), DpError>;
}

/// Derive PCON DSC slice height from the HDMI sink's active vertical size.
// upstream: intel_dp.c intel_dp_pcon_dsc_enc_slice_height()
pub fn intel_dp_pcon_dsc_enc_slice_height(mode: DisplayMode, ops: &impl DpPconDscOps) -> u16 {
    ops.slice_height(mode.vdisplay)
}

/// Select the common PCON/HDMI DSC slice count and throughput.
// upstream: intel_dp.c intel_dp_pcon_dsc_enc_slices()
pub fn intel_dp_pcon_dsc_enc_slices(
    state: &DpRuntimeState,
    mode: DisplayMode,
    ops: &impl DpPconDscOps,
) -> u8 {
    ops.slice_count(
        mode,
        ops.max_slices(&state.pcon_dsc_dpcd),
        ops.max_slice_width(&state.pcon_dsc_dpcd),
        ops.hdmi_dsc_max_slices(),
        ops.hdmi_dsc_clock_per_slice(),
    )
}

/// Compute a PCON DSC bpp that satisfies format, fractional and chunk limits.
// upstream: intel_dp.c intel_dp_pcon_dsc_enc_bpp()
pub fn intel_dp_pcon_dsc_enc_bpp(
    state: &DpRuntimeState,
    output: OutputFormat,
    slices: u8,
    slice_width: u16,
    ops: &impl DpPconDscOps,
) -> u16 {
    ops.compressed_bpp(
        ops.bpp_increment(&state.pcon_dsc_dpcd),
        slice_width,
        slices,
        output,
        ops.hdmi_dsc_all_bpp(),
        ops.hdmi_dsc_max_chunk_bytes(),
    )
}

/// Configure PCON DSC and write six PPS override bytes only for DSC 1.2 peers.
// upstream: intel_dp.c intel_dp_pcon_dsc_configure()
pub fn intel_dp_pcon_dsc_configure(
    state: &DpRuntimeState,
    mode: DisplayMode,
    output: OutputFormat,
    is_hdmi_2_1_sink: bool,
    ops: &mut impl DpPconDscOps,
) -> Result<(), DpError> {
    if !is_hdmi_2_1_sink || !ops.pcon_encoder_dsc12(&state.pcon_dsc_dpcd) || !ops.hdmi_sink_dsc12()
    {
        return Ok(());
    }
    let slice_height = intel_dp_pcon_dsc_enc_slice_height(mode, ops);
    if slice_height == 0 {
        return Ok(());
    }
    let slices = intel_dp_pcon_dsc_enc_slices(state, mode, ops);
    if slices == 0 {
        return Ok(());
    }
    let slice_width = mode.hdisplay.div_ceil(u32::from(slices)) as u16;
    let bpp = intel_dp_pcon_dsc_enc_bpp(state, output, slices, slice_width, ops);
    if bpp == 0 {
        return Ok(());
    }
    let params = [
        slice_height as u8,
        (slice_height >> 8) as u8,
        slice_width as u8,
        (slice_width >> 8) as u8,
        bpp as u8,
        ((bpp >> 8) as u8) & 0x03,
    ];
    ops.pps_override(&params)
}

/// Select PCON TMDS mode with source-control then HDMI-link enable writes.
// upstream: intel_dp.c intel_dp_pcon_set_tmds_mode()
pub fn intel_dp_pcon_set_tmds_mode(io: &mut impl DpAuxIo) -> Result<(), DpError> {
    const DP_PCON_HDMI_LINK_CONFIG_1: u32 = 0x305a;
    const DP_PCON_ENABLE_SOURCE_CTL_MODE: u8 = 1 << 3;
    const DP_PCON_ENABLE_HDMI_LINK: u8 = 1 << 7;
    let mut value = DP_PCON_ENABLE_SOURCE_CTL_MODE;
    if io.write(DP_PCON_HDMI_LINK_CONFIG_1, &[value])? != 1 {
        return Err(DpError::Io);
    }
    value |= DP_PCON_ENABLE_HDMI_LINK;
    if io.write(DP_PCON_HDMI_LINK_CONFIG_1, &[value])? != 1 {
        return Err(DpError::Io);
    }
    Ok(())
}

/// Retrain FRL after a link-status change, falling back to TMDS on failure.
// upstream: intel_dp.c intel_dp_check_frl_training()
pub fn intel_dp_check_frl_training(
    source_ctl_mode: bool,
    is_hdmi_2_1: bool,
    trained_rate_gbps: &mut u8,
    trained: &mut bool,
    pcon_max_frl: u8,
    sink_max_frl: u8,
    io: &mut impl DpAuxIo,
    pcon: &mut impl DpPconOps,
) -> Result<(), DpError> {
    if !source_ctl_mode || !is_hdmi_2_1 || *trained {
        return Ok(());
    }
    match intel_dp_pcon_start_frl_training(
        pcon_max_frl,
        sink_max_frl,
        trained_rate_gbps,
        trained,
        io,
        pcon,
    ) {
        Ok(()) => Ok(()),
        Err(_) => {
            let ret = intel_dp_pcon_set_tmds_mode(io);
            let (mode, _) = pcon.hdmi_link_status();
            if ret.is_err() || mode != PconHdmiMode::Tmds {
                return Err(DpError::Io);
            }
            Ok(())
        }
    }
}

/// Program PCON HDMI mode and RGB/YCbCr conversion controls in required order.
// upstream: intel_dp.c intel_dp_configure_protocol_converter()
pub fn intel_dp_configure_protocol_converter(
    dpcd_rev: u8,
    is_branch: bool,
    has_hdmi_sink: bool,
    sink_format: OutputFormat,
    output_format: OutputFormat,
    io: &mut impl DpAuxIo,
    pcon: &mut impl DpPconOps,
) -> Result<(), DpError> {
    if dpcd_rev < 0x13 || !is_branch {
        return Ok(());
    }
    const DP_PROTOCOL_CONVERTER_CONTROL_0: u32 = 0x3050;
    const DP_PROTOCOL_CONVERTER_CONTROL_1: u32 = 0x3051;
    const DP_HDMI_DVI_OUTPUT_CONFIG: u8 = 1 << 0;
    const DP_CONVERSION_TO_YCBCR420_ENABLE: u8 = 1 << 0;
    let mut ycbcr444_to_420 = false;
    let mut rgb_to_ycbcr = false;
    if io.write(
        DP_PROTOCOL_CONVERTER_CONTROL_0,
        &[if has_hdmi_sink {
            DP_HDMI_DVI_OUTPUT_CONFIG
        } else {
            0
        }],
    )? != 1
    {
        return Err(DpError::Io);
    }
    if sink_format == OutputFormat::Ycbcr420 {
        match output_format {
            OutputFormat::Ycbcr420 => (),
            OutputFormat::Ycbcr444 => ycbcr444_to_420 = true,
            OutputFormat::Rgb => {
                rgb_to_ycbcr = true;
                ycbcr444_to_420 = true;
            }
            OutputFormat::Ycbcr422 => (),
        }
    } else if sink_format == OutputFormat::Ycbcr444 && output_format == OutputFormat::Rgb {
        rgb_to_ycbcr = true;
    }
    if io.write(
        DP_PROTOCOL_CONVERTER_CONTROL_1,
        &[if ycbcr444_to_420 {
            DP_CONVERSION_TO_YCBCR420_ENABLE
        } else {
            0
        }],
    )? != 1
    {
        return Err(DpError::Io);
    }
    pcon.convert_rgb_to_ycbcr(rgb_to_ycbcr)
}

/// Query receiver colorimetry support from the DP 2.0 feature enumeration byte.
// upstream: intel_dp.c intel_dp_get_colorimetry_status()
pub fn intel_dp_get_colorimetry_status(io: &mut impl DpAuxIo) -> bool {
    let mut feature = [0u8; 1];
    io.read(0x2210, &mut feature) == Ok(1) && feature[0] & (1 << 3) != 0
}

/// Hooks that refresh sink DPCD and DP tunnel state during init/resume.
pub trait DpSyncOps {
    fn get_dpcd(&mut self, state: &mut DpRuntimeState) -> bool;
    fn tunnel_resume(&mut self, state: &mut DpRuntimeState, active_crtc: bool, dpcd_updated: bool);
}

/// Sync cached encoder state during init/resume without clobbering prior DPCD.
// upstream: intel_dp.c intel_dp_sync_state()
pub fn intel_dp_sync_state(
    state: &mut DpRuntimeState,
    active_crtc: Option<(u32, u8)>,
    ops: &mut impl DpSyncOps,
) {
    let mut dpcd_updated = false;
    if active_crtc.is_some() && state.dpcd.first().copied().unwrap_or(0) == 0 {
        let _ = ops.get_dpcd(state);
        dpcd_updated = true;
    }
    ops.tunnel_resume(state, active_crtc.is_some(), dpcd_updated);
    if let Some((port_clock, lane_count)) = active_crtc {
        intel_dp_reset_link_params(state);
        intel_dp_set_link_params(state, port_clock, lane_count);
        state.link.active = true;
    }
}

/// Require full modeset when link rate is unsupported, DSC is active or PR state needs setup.
// upstream: intel_dp.c intel_dp_initial_fastset_check()
pub fn intel_dp_initial_fastset_check(
    source_rates: &[u32],
    state: FastsetState,
    panel_replay_capable: bool,
    crtc_state: &mut FastsetState,
) -> bool {
    let mut fastset = true;
    if intel_dp_rate_index(source_rates, state.port_clock) < 0 {
        crtc_state.connectors_changed = true;
        fastset = false;
    }
    if state.dsc_enabled {
        crtc_state.mode_changed = true;
        fastset = false;
    }
    if panel_replay_capable {
        crtc_state.mode_changed = true;
        fastset = false;
    }
    fastset
}

/// Queue one connector modeset retry, covering each active MST stream.
// upstream: intel_dp.c intel_dp_queue_modeset_retry_for_link()
pub fn intel_dp_queue_modeset_retry_for_link(
    state: &mut DpRuntimeState,
    is_mst: bool,
    mst_connector_active: &[bool],
    mut queue_work: impl FnMut(usize),
) {
    if state.needs_modeset_retry {
        return;
    }
    state.needs_modeset_retry = true;
    if !is_mst {
        queue_work(0);
        return;
    }
    for (index, active) in mst_connector_active.iter().copied().enumerate() {
        if active {
            queue_work(index);
        }
    }
}

/// Physical digital-port callbacks (locks, power and HPD sampling).
pub trait DpPortOps {
    fn lock(&mut self);
    fn unlock(&mut self);
    fn handles_hpd_glitches(&self) -> bool;
    fn connected(&mut self) -> bool;
    fn display_core_power(&mut self, enable: bool);
    fn delay_us(&mut self, microseconds: u32);
}

/// Explicit source port lock operation, when that port provides a lock hook.
// upstream: intel_dp.c intel_digital_port_lock()
pub fn intel_digital_port_lock(ops: &mut impl DpPortOps) {
    ops.lock();
}

/// Explicit source port unlock operation, when that port provides an unlock hook.
// upstream: intel_dp.c intel_digital_port_unlock()
pub fn intel_digital_port_unlock(ops: &mut impl DpPortOps) {
    ops.unlock();
}

/// Poll a digital port at 30-us intervals for up to 4 ms under display-core power.
// upstream: intel_dp.c intel_digital_port_connected_locked()
pub fn intel_digital_port_connected_locked(ops: &mut impl DpPortOps) -> bool {
    let glitch_free = ops.handles_hpd_glitches();
    ops.display_core_power(true);
    let mut connected = false;
    for _ in 0..=133 {
        connected = ops.connected();
        if connected || glitch_free {
            break;
        }
        ops.delay_us(30);
    }
    ops.display_core_power(false);
    connected
}

/// Lock, sample and unlock a digital port around its physical connection poll.
// upstream: intel_dp.c intel_digital_port_connected()
pub fn intel_digital_port_connected(ops: &mut impl DpPortOps) -> bool {
    intel_digital_port_lock(ops);
    let connected = intel_digital_port_connected_locked(ops);
    intel_digital_port_unlock(ops);
    connected
}

/// Display HPD/DPCD callbacks needed by the pulse handler.
pub trait DpHpdOps {
    fn display_rpm_suspended(&self) -> bool;
    fn panel_power_or_vdd(&self) -> bool;
    fn dpcd_set_probe(&mut self, force_external: bool);
    fn read_dprx_caps(&mut self);
    fn mst_active(&self) -> bool;
    fn check_mst_status(&mut self) -> bool;
    fn short_pulse(&mut self) -> bool;
}

/// Handle DP long/short HPD with eDP VDD safety and tunnel dummy-cap read ordering.
// upstream: intel_dp.c intel_dp_hpd_pulse()
pub fn intel_dp_hpd_pulse(
    state: &mut DpRuntimeState,
    is_edp: bool,
    long_hpd: bool,
    ops: &mut impl DpHpdOps,
) -> IrqReturn {
    if is_edp && (long_hpd || ops.display_rpm_suspended() || !ops.panel_power_or_vdd()) {
        return IrqReturn::Handled;
    }
    if long_hpd {
        ops.dpcd_set_probe(true);
        ops.read_dprx_caps();
        state.reset_link_params = true;
        intel_dp_invalidate_source_oui(state);
        return IrqReturn::None;
    }
    if ops.mst_active() {
        if !ops.check_mst_status() {
            return IrqReturn::None;
        }
    } else if !ops.short_pulse() {
        return IrqReturn::None;
    }
    IrqReturn::Handled
}

/// Compute VSC SDP revision/length for colorimetry and PSR/Panel Replay modes.
// upstream: intel_dp.c intel_dp_compute_vsc_sdp()
pub fn intel_dp_compute_vsc_sdp(
    colorimetry_support: bool,
    needs_colorimetry: bool,
    color: ColorState,
    infoframe_enable: &mut u32,
) -> Option<VscSdp> {
    if (!colorimetry_support || !needs_colorimetry) && !color.psr {
        return None;
    }
    if needs_colorimetry {
        *infoframe_enable |= intel_dp_sdp_enable_mask(DP_SDP_VSC);
        return Some(intel_dp_compute_vsc_colorimetry(color));
    }
    let mut vsc = VscSdp {
        sdp_type: DP_SDP_VSC,
        ..VscSdp::default()
    };
    if color.panel_replay {
        vsc.revision = 0x06;
        vsc.length = 0x10;
    } else if color.selective_update {
        vsc.revision = 0x04;
        vsc.length = 0x0e;
    } else {
        vsc.revision = 0x02;
        vsc.length = 0x08;
    }
    *infoframe_enable |= intel_dp_sdp_enable_mask(DP_SDP_VSC);
    Some(vsc)
}

/// Add DRM static-HDR SDP only after metadata infoframe creation succeeds.
// upstream: intel_dp.c intel_dp_compute_hdr_metadata_infoframe_sdp()
pub fn intel_dp_compute_hdr_metadata_infoframe_sdp(
    metadata: Option<HdrMetadataInfoframe>,
    infoframe_enable: &mut u32,
) -> Option<HdrMetadataInfoframe> {
    let frame = metadata
        .filter(|frame| frame.kind == HDMI_PACKET_TYPE_GAMUT_METADATA && frame.length == 26)?;
    *infoframe_enable |= intel_dp_sdp_enable_mask(HDMI_PACKET_TYPE_GAMUT_METADATA);
    Some(frame)
}

/// Per-SDP packet read/write interface implemented by display generation glue.
pub trait DpSdpIo {
    fn write_sdp(&mut self, packet_type: u8, sdp: &DpSdp, byte_len: usize);
    fn read_sdp(&mut self, packet_type: u8, sdp: &mut DpSdp);
}

/// Select and program one enabled VSC/AS/HDR packet using source packet order.
// upstream: intel_dp.c intel_write_dp_sdp()
pub fn intel_write_dp_sdp(
    enable: u32,
    packet_type: u8,
    vsc: VscSdp,
    adaptive: AdaptiveSyncSdp,
    hdr: HdrMetadataInfoframe,
    io: &mut impl DpSdpIo,
) -> Result<(), DpError> {
    if enable & intel_dp_sdp_enable_mask(packet_type) == 0 {
        return Ok(());
    }
    let (sdp, length) = match packet_type {
        DP_SDP_VSC => {
            let mut packet = DpSdp::default();
            packet.header = SdpHeader {
                hb0: 0,
                hb1: vsc.sdp_type,
                hb2: vsc.revision,
                hb3: vsc.length,
            };
            packet.db[16] = (vsc.pixelformat << 4) | (vsc.colorimetry & 0x0f);
            packet.db[17] = (vsc.dynamic_range << 7)
                | match vsc.bpc {
                    6 => 0,
                    8 => 1,
                    10 => 2,
                    12 => 3,
                    16 => 4,
                    _ => 0,
                };
            packet.db[18] = vsc.content_type & 7;
            (packet, core::mem::size_of::<DpSdp>())
        }
        DP_SDP_ADAPTIVE_SYNC => intel_dp_as_sdp_pack(adaptive, core::mem::size_of::<DpSdp>())?,
        HDMI_PACKET_TYPE_GAMUT_METADATA => {
            intel_dp_hdr_metadata_infoframe_sdp_pack(hdr, core::mem::size_of::<DpSdp>())?
        }
        _ => return Err(DpError::Unsupported),
    };
    io.write_sdp(packet_type, &sdp, length);
    Ok(())
}

/// Register and posting-read adapter for DDI DIP controls.
pub trait DpInfoframeControlIo: DpSdpIo {
    fn read_dip_control(&mut self, transcoder: u8) -> u32;
    fn write_dip_control(&mut self, transcoder: u8, value: u32);
    fn posting_read_dip_control(&mut self, transcoder: u8);
}

/// Clear stale DIP enables, preserve PPS/PSR conditions, then write DP SDPs.
// upstream: intel_dp.c intel_dp_set_infoframes()
pub fn intel_dp_set_infoframes(
    transcoder: u8,
    enable: bool,
    display_has_as_sdp: bool,
    as_sdp_control_enable_mask: u32,
    has_dsc: bool,
    has_psr: bool,
    infoframe_enable: u32,
    control_enable_mask: u32,
    pps_enable_mask: u32,
    vsc_enable_mask: u32,
    vsc: VscSdp,
    adaptive: AdaptiveSyncSdp,
    hdr: HdrMetadataInfoframe,
    io: &mut impl DpInfoframeControlIo,
) -> Result<(), DpError> {
    let dip_enable = control_enable_mask
        | if display_has_as_sdp {
            as_sdp_control_enable_mask
        } else {
            0
        };
    let mut value = io.read_dip_control(transcoder) & !dip_enable;
    if !enable && has_dsc {
        value &= !pps_enable_mask;
    }
    if !enable || !has_psr {
        value &= !vsc_enable_mask;
    }
    io.write_dip_control(transcoder, value);
    io.posting_read_dip_control(transcoder);
    if !enable {
        return Ok(());
    }
    intel_write_dp_sdp(infoframe_enable, DP_SDP_VSC, vsc, adaptive, hdr, io)?;
    intel_write_dp_sdp(
        infoframe_enable,
        DP_SDP_ADAPTIVE_SYNC,
        vsc,
        adaptive,
        hdr,
        io,
    )?;
    intel_write_dp_sdp(
        infoframe_enable,
        HDMI_PACKET_TYPE_GAMUT_METADATA,
        vsc,
        adaptive,
        hdr,
        io,
    )
}

/// Read and unpack the requested DP VSC, adaptive-sync, or static-HDR SDP.
// upstream: intel_dp.c intel_read_dp_sdp()
pub fn intel_read_dp_sdp(
    packet_type: u8,
    enable: u32,
    io: &mut impl DpSdpIo,
) -> Result<Option<DecodedDpSdp>, DpError> {
    match packet_type {
        DP_SDP_VSC => Ok(intel_read_dp_vsc_sdp(enable, io)?.map(DecodedDpSdp::Vsc)),
        DP_SDP_ADAPTIVE_SYNC => {
            Ok(intel_read_dp_as_sdp(enable, io)?.map(DecodedDpSdp::AdaptiveSync))
        }
        HDMI_PACKET_TYPE_GAMUT_METADATA => Ok(intel_read_dp_hdr_metadata_infoframe_sdp(
            enable, io,
        )?
        .map(DecodedDpSdp::HdrMetadata)),
        _ => Err(DpError::Unsupported),
    }
}

/// HDMI-infoframe enable-bit translation used by display infoframe state.
pub fn intel_hdmi_infoframe_enable(packet_type: u8) -> u32 {
    intel_dp_sdp_enable_mask(packet_type)
}

/// Set maximum source/sink/TC lane intersection and rebuild sorted configs.
// upstream: intel_dp.c intel_dp_set_common_link_params()
pub fn intel_dp_set_common_link_params(
    link: &mut DpLinkState,
    lttpr_max_lanes: u8,
    framework: &impl DpFramework,
) -> bool {
    let mut changed = intel_dp_set_common_rates(link);
    changed |= intel_dp_set_max_common_lane_count(link, lttpr_max_lanes);
    intel_dp_link_config_init(link, framework);
    changed
}

/// Set the conservative invalid-DPCD fallback maximum sink lane count.
// upstream: intel_dp.c intel_dp_set_default_max_sink_lane_count()
pub fn intel_dp_set_default_max_sink_lane_count(link: &mut DpLinkState) {
    link.max_sink_lane_count = 1;
}

/// Read and validate the receiver's maximum lane count; fallback to one lane.
// upstream: intel_dp.c intel_dp_set_max_sink_lane_count()
pub fn intel_dp_set_max_sink_lane_count(link: &mut DpLinkState, caps: &DpSinkCaps) {
    link.max_sink_lane_count = max_dprx_lane_count(caps);
    if !matches!(link.max_sink_lane_count, 1 | 2 | 4) {
        intel_dp_set_default_max_sink_lane_count(link);
    }
}

/// Populate DPCD receiver rates and fall back to RBR if no valid rates exist.
// upstream: intel_dp.c intel_dp_set_sink_rates()
pub fn intel_dp_set_sink_rates(
    link: &mut DpLinkState,
    caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
) {
    intel_dp_set_dpcd_sink_rates(link, caps, io, framework);
    if link.sink_rates.is_empty() {
        intel_dp_set_default_sink_rates(link);
    }
}

/// Choose the greatest common link rate not above the forced rate.
// upstream: intel_dp.c forced_link_rate()
pub fn forced_link_rate(link: &DpLinkState) -> u32 {
    let len = intel_dp_common_len_rate_limit(link, link.force_rate);
    if len == 0 {
        intel_dp_common_rate(link, 0)
    } else {
        intel_dp_common_rate(link, len - 1)
    }
}

/// Upper link-rate limit honoring a forced-rate request.
// upstream: intel_dp.c intel_dp_max_link_rate()
pub fn intel_dp_max_link_rate(link: &DpLinkState) -> u32 {
    if link.force_rate != 0 {
        forced_link_rate(link)
    } else {
        let len = intel_dp_common_len_rate_limit(link, link.max_rate);
        intel_dp_common_rate(link, len.saturating_sub(1))
    }
}

/// Minimum link rate, pinned to forced rate if requested.
// upstream: intel_dp.c intel_dp_min_link_rate()
pub fn intel_dp_min_link_rate(link: &DpLinkState) -> u32 {
    if link.force_rate != 0 {
        forced_link_rate(link)
    } else {
        intel_dp_common_rate(link, 0)
    }
}

/// Find sink rate's index, falling back to the first rate for invalid input.
// upstream: intel_dp.c intel_dp_rate_select()
pub fn intel_dp_rate_select(sink_rates: &[u32], rate: u32) -> usize {
    let idx = intel_dp_rate_index(sink_rates, rate);
    if idx < 0 { 0 } else { idx as usize }
}

/// Remove BIOS-rejected eDP rates from an advertised rate vector.
// upstream: intel_dp.c intel_edp_set_data_override_rates()
pub fn intel_edp_set_data_override_rates(
    sink_rates: &mut Vec<u32>,
    mut rejected: impl FnMut(u32) -> bool,
) {
    sink_rates.retain(|rate| !rejected(*rate));
}

/// Read eDP supported-link-rate table and preserve DPCD fallback selection.
// upstream: intel_dp.c intel_edp_set_sink_rates()
pub fn intel_edp_set_sink_rates(
    link: &mut DpLinkState,
    edp_dpcd_rev: u8,
    caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    hbr2_quirk: bool,
    reject_rate: impl FnMut(u32) -> bool,
) -> bool {
    link.sink_rates.clear();
    let mut use_rate_select = false;
    if edp_dpcd_rev >= 0x04 {
        const DP_SUPPORTED_LINK_RATES: u32 = 0x0010;
        let mut raw = [0u8; DP_MAX_SUPPORTED_RATES * 2];
        let read = io.read(DP_SUPPORTED_LINK_RATES, &mut raw).unwrap_or(0);
        if read == raw.len() {
            for pair in raw.chunks_exact(2) {
                let encoded = u32::from(u16::from_le_bytes([pair[0], pair[1]]));
                if encoded == 0 {
                    break;
                }
                let rate = encoded * 20;
                if hbr2_quirk && rate > 540_000 {
                    break;
                }
                link.sink_rates.push(rate);
            }
            use_rate_select = !link.sink_rates.is_empty();
        }
    }
    if !use_rate_select {
        intel_dp_set_sink_rates(link, caps, io, framework);
    }
    intel_edp_set_data_override_rates(&mut link.sink_rates, reject_rate);
    use_rate_select
}

/// Set source-side sink-rate selection method and chosen lane/rate limits.
// upstream: intel_dp.c intel_dp_update_sink_caps()
pub fn intel_dp_update_sink_caps(
    link: &mut DpLinkState,
    caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    lttpr_max_lanes: u8,
) -> bool {
    intel_dp_set_sink_rates(link, caps, io, framework);
    intel_dp_set_max_sink_lane_count(link, caps);
    intel_dp_set_common_link_params(link, lttpr_max_lanes, framework)
}

/// Human-readable MST protocol mode, including SST sideband tunneling.
// upstream: intel_dp.c intel_dp_mst_mode_str()
pub const fn intel_dp_mst_mode_str(mode: MstMode) -> &'static str {
    match mode {
        MstMode::Mst => "MST",
        MstMode::SstSideband => "SST w/ sideband messaging",
        MstMode::Sst => "SST",
    }
}

/// Select sink-reported MST mode subject to source support and feature enable.
// upstream: intel_dp.c intel_dp_mst_mode_choose()
pub fn intel_dp_mst_mode_choose(
    enable_dp_mst: bool,
    source_support: bool,
    sink_mode: MstMode,
    main_link_128b132b: bool,
) -> MstMode {
    if !enable_dp_mst || !source_support {
        return MstMode::Sst;
    }
    if sink_mode == MstMode::SstSideband && !main_link_128b132b {
        return MstMode::Sst;
    }
    sink_mode
}

/// Topology callbacks for DRM MST manager operations.
pub trait DpMstOps {
    fn source_support(&self) -> bool;
    fn read_sink_mst_mode(&mut self) -> MstMode;
    fn prepare_probe(&mut self);
    fn set_mst(&mut self, enable: bool);
    fn suspend(&mut self);
    fn resume(&mut self, full: bool) -> Result<(), DpError>;
}

/// Detect sink MST support and apply source/modparam gating.
// upstream: intel_dp.c intel_dp_mst_detect()
pub fn intel_dp_mst_detect(
    enable_dp_mst: bool,
    main_link_128b132b: bool,
    ops: &mut impl DpMstOps,
) -> MstMode {
    intel_dp_mst_mode_choose(
        enable_dp_mst,
        ops.source_support(),
        ops.read_sink_mst_mode(),
        main_link_128b132b,
    )
}

/// Set or tear down MST topology state and clear stale detection result.
// upstream: intel_dp.c intel_dp_mst_configure()
pub fn intel_dp_mst_configure(
    state: &mut DpRuntimeState,
    detected: MstMode,
    ops: &mut impl DpMstOps,
) {
    if !ops.source_support() {
        return;
    }
    let enabled = detected != MstMode::Sst;
    if enabled {
        ops.prepare_probe();
    }
    ops.set_mst(enabled);
    state.is_mst = enabled;
    state.mst_detect = MstMode::Sst;
}

/// Disconnect MST state only if it was previously active.
// upstream: intel_dp.c intel_dp_mst_disconnect()
pub fn intel_dp_mst_disconnect(state: &mut DpRuntimeState, ops: &mut impl DpMstOps) {
    if !state.is_mst {
        return;
    }
    state.is_mst = false;
    ops.set_mst(false);
}

/// Suspend an active MST manager.
// upstream: intel_dp.c intel_dp_mst_suspend()
pub fn intel_dp_mst_suspend(has_display: bool, is_mst: bool, ops: &mut impl DpMstOps) {
    if has_display && ops.source_support() && is_mst {
        ops.suspend();
    }
}

/// Resume MST manager, failing closed to SST if topology resume fails.
// upstream: intel_dp.c intel_dp_mst_resume()
pub fn intel_dp_mst_resume(
    has_display: bool,
    is_mst: &mut bool,
    ops: &mut impl DpMstOps,
) -> Result<(), DpError> {
    if !has_display || !ops.source_support() {
        return Ok(());
    }
    match ops.resume(true) {
        Ok(()) => Ok(()),
        Err(error) => {
            *is_mst = false;
            ops.set_mst(false);
            Err(error)
        }
    }
}

/// DPCD probing policy: no eDP probe, force external probe, skip MST, else EDID quirk.
// upstream: intel_dp.c intel_dp_needs_dpcd_probe()
pub const fn intel_dp_needs_dpcd_probe(
    is_edp: bool,
    force_on_external: bool,
    is_mst: bool,
    edid_probe_quirk: bool,
) -> bool {
    if is_edp {
        return false;
    }
    if force_on_external {
        return true;
    }
    !is_mst && edid_probe_quirk
}

/// Program DRM's DPCD probe policy through the AUX adapter.
// upstream: intel_dp.c intel_dp_dpcd_set_probe()
pub fn intel_dp_dpcd_set_probe(
    is_edp: bool,
    force_on_external: bool,
    is_mst: bool,
    edid_probe_quirk: bool,
    set_probe: impl FnOnce(bool),
) {
    set_probe(intel_dp_needs_dpcd_probe(
        is_edp,
        force_on_external,
        is_mst,
        edid_probe_quirk,
    ));
}

/// Narrow discovery calls whose implementations belong to DRM's DPCD helpers.
pub trait DpDiscoveryOps {
    fn init_lttpr_and_dprx_caps(&mut self, state: &mut DpRuntimeState) -> Result<(), DpError>;
    fn read_desc_and_apply_quirks(&mut self, state: &mut DpRuntimeState);
    fn read_colorimetry_support(&mut self) -> bool;
    fn sink_count_capable(&mut self, state: &DpRuntimeState) -> bool;
    fn read_sink_count(&mut self) -> Result<u8, DpError>;
    fn read_downstream_info(&mut self, state: &mut DpRuntimeState) -> Result<(), DpError>;
}

/// DPCD sink-count capability predicate with connector-attachment gate.
// upstream: intel_dp.c intel_dp_has_sink_count()
pub fn intel_dp_has_sink_count(
    has_attached_connector: bool,
    state: &DpRuntimeState,
    ops: &mut impl DpDiscoveryOps,
) -> bool {
    has_attached_connector && ops.sink_count_capable(state)
}

/// Refresh DPCD/quirks/rates/downstream data while retaining cached eDP IDs.
// upstream: intel_dp.c intel_dp_get_dpcd()
pub fn intel_dp_get_dpcd(
    state: &mut DpRuntimeState,
    is_edp: bool,
    has_connector: bool,
    caps: &DpSinkCaps,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    lttpr_max_lanes: u8,
    discovery: &mut impl DpDiscoveryOps,
) -> bool {
    if discovery.init_lttpr_and_dprx_caps(state).is_err() {
        return false;
    }
    if !is_edp {
        discovery.read_desc_and_apply_quirks(state);
        state.colorimetry_support = discovery.read_colorimetry_support();
        intel_dp_update_sink_caps(&mut state.link, caps, io, framework, lttpr_max_lanes);
    }
    if intel_dp_has_sink_count(has_connector, state, discovery) {
        let count = match discovery.read_sink_count() {
            Ok(count) => count,
            Err(_) => return false,
        };
        state.sink_count = count;
        if count == 0 {
            return false;
        }
    }
    discovery.read_downstream_info(state).is_ok()
}

/// Probe branch devices, HPD-aware sink count, DDC fallback, then reliability status.
// upstream: intel_dp.c intel_dp_detect_dpcd()
pub fn intel_dp_detect_dpcd(
    state: &mut DpRuntimeState,
    is_edp: bool,
    has_connector: bool,
    is_branch: bool,
    hpd_aware: bool,
    source_supports_mst: bool,
    enable_dp_mst: bool,
    main_link_128b132b: bool,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    caps: &DpSinkCaps,
    discovery: &mut impl DpDiscoveryOps,
    mst: &mut impl DpMstOps,
    ddc_probe: impl FnOnce() -> bool,
) -> (ConnectorStatus, MstMode) {
    if is_edp {
        return (ConnectorStatus::Connected, MstMode::Sst);
    }
    if !intel_dp_get_dpcd(
        state,
        false,
        has_connector,
        caps,
        io,
        framework,
        0,
        discovery,
    ) {
        return (ConnectorStatus::Disconnected, MstMode::Sst);
    }
    let mst_mode = if source_supports_mst {
        intel_dp_mst_detect(enable_dp_mst, main_link_128b132b, mst)
    } else {
        MstMode::Sst
    };
    if !is_branch {
        return (ConnectorStatus::Connected, mst_mode);
    }
    if hpd_aware {
        return (
            if state.sink_count != 0 {
                ConnectorStatus::Connected
            } else {
                ConnectorStatus::Disconnected
            },
            mst_mode,
        );
    }
    if mst_mode == MstMode::Mst || ddc_probe() {
        return (ConnectorStatus::Connected, mst_mode);
    }
    let rev = state.dpcd.first().copied().unwrap_or(0);
    let port = state.downstream_ports.first().copied().unwrap_or(0);
    let connector_type = if rev >= 0x11 {
        port & 0x0f
    } else {
        port & 0x06
    };
    if (rev >= 0x11 && matches!(connector_type, 0x01 | 0x04))
        || (rev < 0x11 && matches!(connector_type, 0x02 | 0x06))
    {
        return (ConnectorStatus::Unknown, mst_mode);
    }
    (ConnectorStatus::Disconnected, mst_mode)
}

/// Detection-side effects that stay in the DRM connector/AUX/PSR integration layer.
pub trait DpDetectOps {
    fn display_enabled(&self) -> bool;
    fn driver_access_allowed(&self) -> bool;
    fn current_connector_status(&self) -> ConnectorStatus;
    fn flush_connector_commits(&mut self) -> Result<(), DpError>;
    fn pps_vdd_on(&mut self);
    fn edp_detect(&mut self) -> ConnectorStatus;
    fn digital_port_connected(&mut self) -> bool;
    fn detect_dpcd(&mut self) -> ConnectorStatus;
    fn verify_mst_dpcd_state(&mut self) -> bool;
    fn reset_dp_test(&mut self);
    fn clear_disconnected_sink_caps(&mut self);
    fn mst_disconnect(&mut self);
    fn tunnel_disconnect(&mut self);
    fn init_source_oui(&mut self);
    fn tunnel_detect(&mut self) -> Result<bool, DpError>;
    fn bump_connector_epoch(&mut self);
    fn init_psr_dpcd(&mut self);
    fn detect_dsc_caps(&mut self);
    fn detect_sdp_caps(&mut self);
    fn configure_mst(&mut self, state: &mut DpRuntimeState);
    fn print_rates(&mut self);
    fn check_link_state(&mut self);
    fn reset_aux_i2c_counters(&mut self);
    fn set_edid(&mut self) -> bool;
    fn unset_edid(&mut self);
    fn dpcd_set_probe_false(&mut self);
    fn set_subconnector_property(&mut self, status: ConnectorStatus);
    fn pps_vdd_off(&mut self);
}

/// Complete the source DP detect sequence, with VDD cleanup on every exit.
// upstream: intel_dp.c intel_dp_detect()
pub fn intel_dp_detect(
    state: &mut DpRuntimeState,
    is_edp: bool,
    ops: &mut impl DpDetectOps,
) -> Result<ConnectorStatus, DpError> {
    if !ops.display_enabled() {
        return Ok(ConnectorStatus::Disconnected);
    }
    if !ops.driver_access_allowed() {
        return Ok(ops.current_connector_status());
    }
    ops.flush_connector_commits()?;
    ops.pps_vdd_on();
    let mut status = if is_edp {
        ops.edp_detect()
    } else if ops.digital_port_connected() {
        ops.detect_dpcd()
    } else {
        ConnectorStatus::Disconnected
    };
    if status != ConnectorStatus::Disconnected && !ops.verify_mst_dpcd_state() {
        status = ConnectorStatus::Disconnected;
    }
    if status == ConnectorStatus::Disconnected {
        ops.reset_dp_test();
        ops.clear_disconnected_sink_caps();
        if state.is_mst {
            state.is_mst = false;
            ops.mst_disconnect();
        }
        ops.tunnel_disconnect();
        ops.unset_edid();
        ops.dpcd_set_probe_false();
        if !is_edp {
            ops.set_subconnector_property(status);
        }
        ops.pps_vdd_off();
        return Ok(status);
    }
    ops.init_source_oui();
    match ops.tunnel_detect() {
        Err(error) => {
            ops.pps_vdd_off();
            return Err(error);
        }
        Ok(true) => ops.bump_connector_epoch(),
        Ok(false) => (),
    }
    if !is_edp {
        ops.init_psr_dpcd();
    }
    ops.detect_dsc_caps();
    ops.detect_sdp_caps();
    if state.reset_link_params {
        intel_dp_reset_link_params(state);
        state.reset_link_params = false;
    }
    ops.configure_mst(state);
    ops.print_rates();
    if state.is_mst {
        status = ConnectorStatus::Disconnected;
        ops.unset_edid();
    } else {
        if !is_edp {
            ops.check_link_state();
        }
        ops.reset_aux_i2c_counters();
        let has_edid = ops.set_edid();
        if is_edp || has_edid {
            status = ConnectorStatus::Connected;
        }
    }
    if status != ConnectorStatus::Connected && !state.is_mst {
        ops.unset_edid();
    }
    ops.dpcd_set_probe_false();
    if !is_edp {
        ops.set_subconnector_property(status);
    }
    ops.pps_vdd_off();
    Ok(status)
}

/// DRM mode-enumeration callbacks for EDID, eDP fixed modes and DP branches.
pub trait DpModesOps {
    fn add_edid_modes(&mut self) -> usize;
    fn add_panel_modes(&mut self) -> usize;
    fn has_detect_edid(&self) -> bool;
    fn downstream_mode(&mut self) -> Option<DisplayMode>;
    fn add_probe_mode(&mut self, mode: DisplayMode);
}

/// Enumerate EDID modes, eDP fixed modes, or the DP downstream fallback mode.
// upstream: intel_dp.c intel_dp_get_modes()
pub fn intel_dp_get_modes(is_edp: bool, ops: &mut impl DpModesOps) -> usize {
    let mut count = ops.add_edid_modes();
    if is_edp {
        count += ops.add_panel_modes();
    }
    if count != 0 {
        return count;
    }
    if !ops.has_detect_edid() {
        if let Some(mode) = ops.downstream_mode() {
            ops.add_probe_mode(mode);
            count += 1;
        }
    }
    count
}

/// Connector force callback refreshes EDID only while status remains connected.
// upstream: intel_dp.c intel_dp_force()
pub fn intel_dp_force(
    driver_access_allowed: bool,
    connector_connected: bool,
    mut unset_edid: impl FnMut(),
    mut set_edid: impl FnMut(),
    mut dpcd_set_probe_false: impl FnMut(),
) {
    if !driver_access_allowed {
        return;
    }
    unset_edid();
    if !connector_connected {
        return;
    }
    set_edid();
    dpcd_set_probe_false();
}

/// OOB HPD event state and scheduling adapter.
pub trait DpHotplugOps {
    fn lock_irq_state(&mut self);
    fn unlock_irq_state(&mut self);
    fn last_hpd_state(&self, pin: u8) -> bool;
    fn set_hpd_state(&mut self, pin: u8, high: bool);
    fn set_event_bit(&mut self, pin: u8);
    fn schedule_detection(&mut self);
}

/// Queue hotplug detection only when OOB level differs from recorded state.
// upstream: intel_dp.c intel_dp_oob_hotplug_event()
pub fn intel_dp_oob_hotplug_event(pin: u8, connected: bool, ops: &mut impl DpHotplugOps) {
    ops.lock_irq_state();
    let changed = connected != ops.last_hpd_state(pin);
    if changed {
        ops.set_event_bit(pin);
        ops.set_hpd_state(pin, connected);
    }
    ops.unlock_irq_state();
    if changed {
        ops.schedule_detection();
    }
}

/// Encoder teardown/suspend/shutdown actions supplied by power/MST/tunnel code.
pub trait DpEncoderLifecycleOps {
    fn link_check_flush_work(&mut self);
    fn mst_encoder_cleanup(&mut self);
    fn tunnel_destroy(&mut self);
    fn pps_vdd_off_sync(&mut self);
    fn pps_wait_power_cycle(&mut self);
    fn aux_fini(&mut self);
    fn tunnel_suspend(&mut self);
}

/// Drain asynchronous work and honor panel power-cycle timing before AUX teardown.
// upstream: intel_dp.c intel_dp_encoder_flush_work()
pub fn intel_dp_encoder_flush_work(ops: &mut impl DpEncoderLifecycleOps) {
    ops.link_check_flush_work();
    ops.mst_encoder_cleanup();
    ops.tunnel_destroy();
    ops.pps_vdd_off_sync();
    ops.pps_wait_power_cycle();
    ops.aux_fini();
}

/// Synchronously power down the panel before suspending its DP tunnel.
// upstream: intel_dp.c intel_dp_encoder_suspend()
pub fn intel_dp_encoder_suspend(ops: &mut impl DpEncoderLifecycleOps) {
    ops.pps_vdd_off_sync();
    ops.tunnel_suspend();
}

/// Keep power sequencer cycle delay honored at encoder shutdown.
// upstream: intel_dp.c intel_dp_encoder_shutdown()
pub fn intel_dp_encoder_shutdown(ops: &mut impl DpEncoderLifecycleOps) {
    ops.pps_wait_power_cycle();
}

/// Atomic-state operations shared with DRM connector/CRTC/plane code.
pub trait DpAtomicOps {
    fn mark_crtc_mode_changed(&mut self, crtc_index: usize) -> Result<(), DpError>;
    fn add_affected_connectors(&mut self, crtc_index: usize) -> Result<(), DpError>;
    fn add_affected_planes(&mut self, crtc_index: usize) -> Result<(), DpError>;
    fn digital_connector_atomic_check(&mut self) -> Result<(), DpError>;
    fn mst_root_conn_atomic_check(&mut self) -> Result<(), DpError>;
    fn connector_needs_modeset(&self) -> bool;
    fn tunnel_atomic_check(&mut self) -> Result<(), DpError>;
    fn display_ver(&self) -> u8;
    fn current_tile_group_id(&self) -> Option<u32>;
    fn current_connector_index(&self) -> usize;
    fn mst_source_support(&self) -> bool;
}

/// Mark all active CRTCs belonging to one tiled connector group.
// upstream: intel_dp.c intel_modeset_tile_group()
pub fn intel_modeset_tile_group(
    tile_group_id: u32,
    connectors: &[DpAtomicConnector],
    ops: &mut impl DpAtomicOps,
) -> Result<(), DpError> {
    for connector in connectors {
        if !connector.has_tile || connector.tile_group_id != tile_group_id {
            continue;
        }
        let Some(crtc) = connector.new_crtc else {
            continue;
        };
        ops.mark_crtc_mode_changed(crtc)?;
        ops.add_affected_planes(crtc)?;
    }
    Ok(())
}

/// Add connector/plane dependencies for every enabled transcoder in a bitmask.
// upstream: intel_dp.c intel_modeset_affected_transcoders()
pub fn intel_modeset_affected_transcoders(
    transcoders: u8,
    crtcs: &[DpAtomicCrtc],
    ops: &mut impl DpAtomicOps,
) -> Result<(), DpError> {
    let mut remaining = transcoders;
    for (index, crtc) in crtcs.iter().enumerate() {
        if !crtc.hw_enable || remaining & (1 << crtc.transcoder) == 0 {
            continue;
        }
        ops.mark_crtc_mode_changed(index)?;
        ops.add_affected_connectors(index)?;
        ops.add_affected_planes(index)?;
        remaining &= !(1 << crtc.transcoder);
    }
    let _unmatched_transcoder_bits = remaining; // source warns; it does not abort.
    Ok(())
}

/// Mark old pipe-sync master and slave CRTCs affected by a connector modeset.
// upstream: intel_dp.c intel_modeset_synced_crtcs()
pub fn intel_modeset_synced_crtcs(
    connector_old_crtc: Option<usize>,
    crtcs: &[DpAtomicCrtc],
    ops: &mut impl DpAtomicOps,
) -> Result<(), DpError> {
    let Some(index) = connector_old_crtc else {
        return Ok(());
    };
    let crtc = *crtcs.get(index).ok_or(DpError::Invalid)?;
    if !crtc.hw_active {
        return Ok(());
    }
    let mut transcoders = crtc.sync_mode_slaves_mask;
    if let Some(master) = crtc.master_transcoder {
        transcoders |= 1 << master;
    }
    intel_modeset_affected_transcoders(transcoders, crtcs, ops)
}

/// Preserve DP connector's atomic-check gates before MST/tunnel/tile-sync work.
// upstream: intel_dp.c intel_dp_connector_atomic_check()
pub fn intel_dp_connector_atomic_check(
    connectors: &[DpAtomicConnector],
    crtcs: &[DpAtomicCrtc],
    ops: &mut impl DpAtomicOps,
) -> Result<(), DpError> {
    ops.digital_connector_atomic_check()?;
    if ops.mst_source_support() {
        ops.mst_root_conn_atomic_check()?;
    }
    if !ops.connector_needs_modeset() {
        return Ok(());
    }
    ops.tunnel_atomic_check()?;
    if ops.display_ver() < 9 {
        return Ok(());
    }
    if let Some(tile_group_id) = ops.current_tile_group_id() {
        intel_modeset_tile_group(tile_group_id, connectors, ops)?;
    }
    let old_crtc = connectors
        .get(ops.current_connector_index())
        .and_then(|connector| connector.old_crtc);
    intel_modeset_synced_crtcs(old_crtc, crtcs, ops)
}

/// EDID downstream-capability helpers implemented by the DRM connector layer.
pub trait DpEdidOps {
    fn fixed_edid(&mut self) -> FixedEdid;
    fn read_ddc_edid(&mut self) -> Option<u64>;
    fn update_display_info(&mut self, edid: Option<u64>, state: &mut DpEdidState);
    fn is_vrr_capable(&mut self, state: &DpEdidState) -> bool;
    fn downstream_max_bpc(&mut self, state: &DpRuntimeState) -> u8;
    fn downstream_max_dotclock(&mut self, state: &DpRuntimeState) -> u32;
    fn downstream_min_tmds_clock(&mut self, state: &DpRuntimeState) -> u32;
    fn downstream_max_tmds_clock(&mut self, state: &DpRuntimeState, edid: Option<u64>) -> u32;
    fn downstream_pcon_frl_bw(&mut self, state: &DpRuntimeState) -> u8;
    fn downstream_420_passthrough(&mut self, state: &DpRuntimeState) -> bool;
    fn downstream_444_to_420(&mut self, state: &DpRuntimeState) -> bool;
    fn downstream_rgb_to_ycbcr(&mut self, state: &DpRuntimeState) -> bool;
    fn read_pcon_dsc_cap(&mut self, state: &DpRuntimeState);
    fn lspcon_active(&mut self) -> bool;
    fn update_vrr_property(&mut self, capable: bool);
    fn attach_cec(&mut self, physical_address: u16);
    fn unset_cec(&mut self);
    fn free_detect_edid(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixedEdid {
    Absent,
    Valid(u64),
    Invalid,
}

/// Select panel-fixed EDID first, else read DDC through the AUX I2C adapter.
// upstream: intel_dp.c intel_dp_get_edid()
pub fn intel_dp_get_edid(ops: &mut impl DpEdidOps) -> Option<u64> {
    match ops.fixed_edid() {
        FixedEdid::Valid(edid) => Some(edid),
        FixedEdid::Invalid => None,
        FixedEdid::Absent => ops.read_ddc_edid(),
    }
}

/// Refresh downstream max BPC, clock, TMDS and PCON FRL constraints.
// upstream: intel_dp.c intel_dp_update_dfp()
pub fn intel_dp_update_dfp(
    state: &DpRuntimeState,
    edid: Option<u64>,
    edid_state: &mut DpEdidState,
    ops: &mut impl DpEdidOps,
) {
    edid_state.downstream.max_bpc = ops.downstream_max_bpc(state);
    edid_state.downstream.max_dotclock = ops.downstream_max_dotclock(state);
    edid_state.downstream.min_tmds_clock = ops.downstream_min_tmds_clock(state);
    edid_state.downstream.max_tmds_clock = ops.downstream_max_tmds_clock(state, edid);
    edid_state.downstream.pcon_max_frl_bw = ops.downstream_pcon_frl_bw(state);
    ops.read_pcon_dsc_cap(state);
}

/// Determine 4:2:0 support from source output or branch conversion capability.
// upstream: intel_dp.c intel_dp_can_ycbcr420()
pub fn intel_dp_can_ycbcr420(
    display_ver: u8,
    has_gmch: bool,
    ironlake: bool,
    is_branch: bool,
    ycbcr420_passthrough: bool,
    rgb_to_ycbcr: bool,
    ycbcr444_to_420: bool,
) -> bool {
    if source_can_output(display_ver, OutputFormat::Ycbcr420, has_gmch, ironlake)
        && (!is_branch || ycbcr420_passthrough)
    {
        return true;
    }
    if source_can_output(display_ver, OutputFormat::Rgb, has_gmch, ironlake)
        && dfp_can_convert_from_rgb(
            is_branch,
            OutputFormat::Ycbcr420,
            rgb_to_ycbcr,
            ycbcr444_to_420,
        )
    {
        return true;
    }
    source_can_output(display_ver, OutputFormat::Ycbcr444, has_gmch, ironlake)
        && dfp_can_convert_from_ycbcr444(is_branch, OutputFormat::Ycbcr420, ycbcr444_to_420)
}

/// Refresh branch pass-through/conversion and publish 4:2:0 mode allowance.
// upstream: intel_dp.c intel_dp_update_420()
pub fn intel_dp_update_420(
    display_ver: u8,
    has_gmch: bool,
    ironlake: bool,
    is_branch: bool,
    state: &DpRuntimeState,
    edid_state: &mut DpEdidState,
    ops: &mut impl DpEdidOps,
) {
    edid_state.downstream.ycbcr420_passthrough = ops.downstream_420_passthrough(state);
    edid_state.downstream.ycbcr444_to_420 = ops.lspcon_active() || ops.downstream_444_to_420(state);
    edid_state.downstream.rgb_to_ycbcr = ops.downstream_rgb_to_ycbcr(state);
    edid_state.info.ycbcr_420_allowed = intel_dp_can_ycbcr420(
        display_ver,
        has_gmch,
        ironlake,
        is_branch,
        edid_state.downstream.ycbcr420_passthrough,
        edid_state.downstream.rgb_to_ycbcr,
        edid_state.downstream.ycbcr444_to_420,
    );
}

/// Clear cached EDID-dependent DFP constraints and associated CEC/VRR state.
// upstream: intel_dp.c intel_dp_unset_edid()
pub fn intel_dp_unset_edid(state: &mut DpEdidState, ops: &mut impl DpEdidOps) {
    ops.unset_cec();
    ops.free_detect_edid();
    state.has_edid = false;
    state.downstream = DpDownstreamCaps::default();
    state.info.ycbcr_420_allowed = false;
    state.vrr_capable = false;
    ops.update_vrr_property(false);
}

/// Set EDID, refresh all connector/DFP/4:2:0/VRR state, and attach CEC.
// upstream: intel_dp.c intel_dp_set_edid()
pub fn intel_dp_set_edid(
    runtime: &DpRuntimeState,
    edid_state: &mut DpEdidState,
    display_ver: u8,
    has_gmch: bool,
    ironlake: bool,
    is_branch: bool,
    ops: &mut impl DpEdidOps,
) {
    intel_dp_unset_edid(edid_state, ops);
    let edid = intel_dp_get_edid(ops);
    edid_state.has_edid = edid.is_some();
    ops.update_display_info(edid, edid_state);
    edid_state.vrr_capable = ops.is_vrr_capable(edid_state);
    ops.update_vrr_property(edid_state.vrr_capable);
    intel_dp_update_dfp(runtime, edid, edid_state, ops);
    intel_dp_update_420(
        display_ver,
        has_gmch,
        ironlake,
        is_branch,
        runtime,
        edid_state,
        ops,
    );
    ops.attach_cec(edid_state.cec_physical_address);
}

/// Check eDP panel fixed-mode BPP and override a smaller erroneous VBT value.
// upstream: intel_dp.c intel_edp_fixup_vbt_bpp()
pub fn intel_edp_fixup_vbt_bpp(vbt_bpp: &mut u8, pipe_bpp: u8) -> bool {
    if *vbt_bpp != 0 && pipe_bpp > *vbt_bpp {
        *vbt_bpp = pipe_bpp;
        true
    } else {
        false
    }
}

/// Multiply eDP timing width, sync positions, total and clock for MSO links.
// upstream: intel_dp.c intel_edp_mso_mode_fixup()
pub fn intel_edp_mso_mode_fixup(
    mode: &mut DisplayMode,
    link_count: u8,
    pixel_overlap: u16,
) -> bool {
    if link_count == 0 {
        return false;
    }
    let overlap = u32::from(pixel_overlap);
    mode.hdisplay = mode
        .hdisplay
        .saturating_sub(overlap)
        .saturating_mul(u32::from(link_count));
    mode.hsync_start = mode
        .hsync_start
        .saturating_sub(overlap)
        .saturating_mul(u32::from(link_count));
    mode.hsync_end = mode
        .hsync_end
        .saturating_sub(overlap)
        .saturating_mul(u32::from(link_count));
    mode.htotal = mode
        .htotal
        .saturating_sub(overlap)
        .saturating_mul(u32::from(link_count));
    mode.clock_khz = mode.clock_khz.saturating_mul(u32::from(link_count));
    true
}

/// Parse and validate the sink's DP eDP 1.4 MSO link count and overlap.
// upstream: intel_dp.c intel_edp_mso_init()
pub fn intel_edp_mso_init(
    edp_dpcd_rev: u8,
    advertised_links: u8,
    max_sink_lanes: u8,
    source_mso_supported: bool,
    pixel_overlap: u16,
) -> (u8, u16) {
    if edp_dpcd_rev < 0x04 {
        return (0, 0);
    }
    let mut links = advertised_links & 0x0f;
    if links % 2 != 0 || links > max_sink_lanes {
        links = 0;
    }
    if links != 0 && !source_mso_supported {
        links = 0;
    }
    (links, if links != 0 { pixel_overlap } else { 0 })
}

/// Panel/VBT/PPS integration boundaries for the eDP initialization sequence.
pub trait DpEdpConnectorOps {
    fn lvds_encoder_present(&self) -> bool;
    fn bios_init_panel_early(&mut self);
    fn pps_init(&mut self) -> bool;
    fn enable_hpd_detection(&mut self);
    fn alpm_init(&mut self);
    fn init_edp_dpcd(&mut self) -> bool;
    fn has_shared_aux_channel(&self) -> bool;
    fn digital_port_connected(&mut self) -> bool;
    fn known_legacy_vga_branch(&self) -> bool;
    fn lock_mode_config(&mut self);
    fn read_ddc_or_opregion_edid_and_modes(&mut self) -> bool;
    fn init_panel_late(&mut self);
    fn add_edid_fixed_modes(&mut self);
    fn mso_init(&mut self);
    fn fixup_all_mso_modes(&mut self);
    fn add_vbt_lfp_fixed_mode(&mut self);
    fn has_preferred_fixed_mode(&self) -> bool;
    fn unlock_mode_config(&mut self);
    fn panel_init(&mut self);
    fn backlight_setup(&mut self);
    fn add_edp_properties(&mut self);
    fn pps_init_late(&mut self);
    fn pps_vdd_off_sync(&mut self);
    fn bios_fini_panel(&mut self);
}

/// Initialize an eDP panel end-to-end; every failure takes the VDD/panel cleanup path.
// upstream: intel_dp.c intel_edp_init_connector()
pub fn intel_edp_init_connector(
    is_edp: bool,
    has_ibx_or_cpt_lvds: bool,
    ops: &mut impl DpEdpConnectorOps,
) -> bool {
    if !is_edp {
        return true;
    }
    if has_ibx_or_cpt_lvds && ops.lvds_encoder_present() {
        return false;
    }
    ops.bios_init_panel_early();
    if !ops.pps_init() {
        return edp_init_failure(ops);
    }
    ops.enable_hpd_detection();
    ops.alpm_init();
    if !ops.init_edp_dpcd() {
        return edp_init_failure(ops);
    }
    if ops.has_shared_aux_channel() {
        if !ops.digital_port_connected() {
            return edp_init_failure(ops);
        }
        if ops.known_legacy_vga_branch() {
            return edp_init_failure(ops);
        }
    }
    ops.lock_mode_config();
    let _ = ops.read_ddc_or_opregion_edid_and_modes();
    ops.init_panel_late();
    ops.add_edid_fixed_modes();
    ops.mso_init();
    ops.fixup_all_mso_modes();
    if !ops.has_preferred_fixed_mode() {
        ops.add_vbt_lfp_fixed_mode();
    }
    ops.unlock_mode_config();
    if !ops.has_preferred_fixed_mode() {
        return edp_init_failure(ops);
    }
    ops.panel_init();
    ops.backlight_setup();
    ops.add_edp_properties();
    ops.pps_init_late();
    true
}

fn edp_init_failure(ops: &mut impl DpEdpConnectorOps) -> bool {
    ops.pps_vdd_off_sync();
    ops.bios_fini_panel();
    false
}

/// Backlight power hooks preserving source sequencing.
pub trait DpEdpBacklightOps {
    fn backlight_enable(&mut self);
    fn pps_backlight_on(&mut self);
    fn pps_backlight_off(&mut self);
    fn backlight_disable(&mut self);
    fn initial_vlv_pipe(&mut self) -> Option<u8>;
    fn backlight_setup(&mut self, pipe: Option<u8>);
}

/// Enable PWM first, then PPS backlight control, only for an eDP port.
// upstream: intel_dp.c intel_edp_backlight_on()
pub fn intel_edp_backlight_on(is_edp: bool, ops: &mut impl DpEdpBacklightOps) {
    if !is_edp {
        return;
    }
    ops.backlight_enable();
    ops.pps_backlight_on();
}

/// Disable PPS control before disabling PWM, only for an eDP port.
// upstream: intel_dp.c intel_edp_backlight_off()
pub fn intel_edp_backlight_off(is_edp: bool, ops: &mut impl DpEdpBacklightOps) {
    if !is_edp {
        return;
    }
    ops.pps_backlight_off();
    ops.backlight_disable();
}

/// Initialize panel backlight using the legacy VLV/CHV initial pipe if required.
// upstream: intel_dp.c intel_edp_backlight_setup()
pub fn intel_edp_backlight_setup(is_vlv_or_chv: bool, ops: &mut impl DpEdpBacklightOps) {
    let pipe = if is_vlv_or_chv {
        ops.initial_vlv_pipe()
    } else {
        None
    };
    ops.backlight_setup(pipe);
}

/// Synchronize software DSC decompression state from active CRTC configuration.
// upstream: intel_dp.c intel_dp_connector_sync_state()
pub fn intel_dp_connector_sync_state(
    dsc_decompression_aux_exists: bool,
    dsc_enabled: bool,
) -> Result<bool, DpError> {
    if dsc_enabled && !dsc_decompression_aux_exists {
        return Err(DpError::Invalid);
    }
    Ok(dsc_enabled)
}

/// DRM/AUX/CEC/LSPCON registration operations for the DP connector callback.
pub trait DpConnectorRegisterOps {
    fn register_connector(&mut self) -> Result<(), DpError>;
    fn set_aux_device(&mut self);
    fn register_aux(&mut self) -> Result<(), DpError>;
    fn register_cec(&mut self);
    fn is_lspcon(&self) -> bool;
    fn lspcon_init(&mut self) -> bool;
    fn lspcon_hdr_capable(&mut self) -> bool;
    fn attach_hdr_metadata_property(&mut self);
    fn unregister_cec(&mut self);
    fn unregister_aux(&mut self);
    fn unregister_connector(&mut self);
}

/// Register connector/AUX/CEC and initialize LSPCON-specific HDR state.
// upstream: intel_dp.c intel_dp_connector_register()
pub fn intel_dp_connector_register(ops: &mut impl DpConnectorRegisterOps) -> Result<(), DpError> {
    ops.register_connector()?;
    ops.set_aux_device();
    let aux_result = ops.register_aux();
    if aux_result.is_ok() {
        ops.register_cec();
    }
    if !ops.is_lspcon() {
        return aux_result;
    }
    if ops.lspcon_init() && ops.lspcon_hdr_capable() {
        ops.attach_hdr_metadata_property();
    }
    aux_result
}

/// Unregister CEC and AUX before releasing the generic connector object.
// upstream: intel_dp.c intel_dp_connector_unregister()
pub fn intel_dp_connector_unregister(ops: &mut impl DpConnectorRegisterOps) {
    ops.unregister_cec();
    ops.unregister_aux();
    ops.unregister_connector();
}

/// Match DPCD/VBT port type to eDP, with the pre-Gen9 Port-A rule.
// upstream: intel_dp.c _intel_dp_is_port_edp()
pub const fn _intel_dp_is_port_edp(display_ver: u8, port_a: bool, bios_marks_edp: bool) -> bool {
    if display_ver < 5 {
        return false;
    }
    if display_ver < 9 && port_a {
        return true;
    }
    bios_marks_edp
}

/// Resolve eDP port status after VBT encoder-data lookup.
// upstream: intel_dp.c intel_dp_is_port_edp()
pub const fn intel_dp_is_port_edp(
    display_ver: u8,
    port_a: bool,
    vbt_encoder_supports_edp: bool,
) -> bool {
    _intel_dp_is_port_edp(display_ver, port_a, vbt_encoder_supports_edp)
}

/// Connector initialization callbacks for hardware and generic DRM registration.
pub trait DpConnectorInitOps {
    fn source_max_lanes(&self) -> u8;
    fn tc_port_max_lane_count(&self) -> u8;
    fn read_current_output_control(&mut self) -> u32;
    fn init_aux(&mut self) -> Result<(), DpError>;
    fn register_connector(&mut self, connector_type: DpConnectorType) -> Result<(), DpError>;
    fn init_edp_panel(&mut self, state: &mut DpRuntimeState) -> bool;
    fn init_mst_encoder(&mut self);
    fn add_connector_properties(&mut self);
    fn hdcp_supported(&self) -> bool;
    fn init_hdcp(&mut self) -> Result<(), DpError>;
    fn init_psr(&mut self);
    fn power_flush_work(&mut self);
    fn connector_cleanup(&mut self);
    fn mst_encoder_cleanup(&mut self);
    fn aux_fini(&mut self);
}

/// Initialize DP connector state, rates, AUX, eDP/MST/HDCP and properties.
// upstream: intel_dp.c intel_dp_init_connector()
pub fn intel_dp_init_connector(
    state: &mut DpRuntimeState,
    display: DisplayInfo,
    edp: bool,
    port_vbt_max_rate: u32,
    framework: &impl DpFramework,
    lttpr_max_lanes: u8,
    ops: &mut impl DpConnectorInitOps,
) -> Result<DpConnectorType, DpError> {
    if ops.source_max_lanes() < 1 {
        return Err(DpError::Invalid);
    }
    state.reset_link_params = true;
    state.hardware_control = ops.read_current_output_control();
    intel_dp_set_default_sink_rates(&mut state.link);
    intel_dp_set_default_max_sink_lane_count(&mut state.link);
    if ops.init_aux().is_err() {
        return Err(DpError::Io);
    }
    let connector_type = if edp {
        DpConnectorType::EmbeddedDisplayPort
    } else {
        DpConnectorType::DisplayPort
    };
    if ops.register_connector(connector_type).is_err() {
        ops.aux_fini();
        return Err(DpError::Io);
    }
    if edp && !ops.init_edp_panel(state) {
        ops.aux_fini();
        ops.power_flush_work();
        ops.connector_cleanup();
        return Err(DpError::Unsupported);
    }
    intel_dp_set_source_rates(&mut state.link, display, edp, port_vbt_max_rate);
    state.link.max_source_lane_count = ops.source_max_lanes();
    state.link.tc_lane_count = ops.tc_port_max_lane_count();
    intel_dp_set_common_link_params(&mut state.link, lttpr_max_lanes, framework);
    intel_dp_reset_link_params(state);
    ops.init_mst_encoder();
    ops.add_connector_properties();
    if !edp && ops.hdcp_supported() {
        let _ = ops.init_hdcp();
    }
    state.frl_is_trained = false;
    state.frl_trained_rate_gbps = 0;
    ops.init_psr();
    Ok(connector_type)
}

/// Cleanup MST, AUX and DRM connector state after flushing deferred power work.
// upstream: intel_dp.c intel_dp_cleanup_connector()
pub fn intel_dp_cleanup_connector(ops: &mut impl DpConnectorInitOps) {
    ops.power_flush_work();
    ops.mst_encoder_cleanup();
    ops.aux_fini();
    ops.connector_cleanup();
}

/// Read SST sink ESI service/link IRQ fields with DPCD 1.2 ordering.
// upstream: intel_dp.c intel_dp_get_sink_irq_esi_sst()
pub fn intel_dp_get_sink_irq_esi_sst(
    dpcd_rev: u8,
    io: &mut impl DpAuxIo,
    esi: &mut [u8; 4],
) -> bool {
    const DP_SINK_COUNT: u32 = 0x0200;
    const DP_LINK_SERVICE_IRQ_VECTOR_ESI0: u32 = 0x2005;
    esi.fill(0);
    if io.read(DP_SINK_COUNT, &mut esi[..2]) != Ok(2) {
        return false;
    }
    if dpcd_rev < DP_DPCD_REV_12 {
        return true;
    }
    io.read(DP_LINK_SERVICE_IRQ_VECTOR_ESI0, &mut esi[3..4]) == Ok(1)
}

/// Acknowledge SST service and link IRQ fields using revision-dependent writes.
// upstream: intel_dp.c intel_dp_ack_sink_irq_esi_sst()
pub fn intel_dp_ack_sink_irq_esi_sst(dpcd_rev: u8, io: &mut impl DpAuxIo, esi: &[u8; 4]) -> bool {
    const DP_DEVICE_SERVICE_IRQ_VECTOR: u32 = 0x0201;
    const DP_LINK_SERVICE_IRQ_VECTOR_ESI0: u32 = 0x2005;
    if io.write(DP_DEVICE_SERVICE_IRQ_VECTOR, &esi[1..2]) != Ok(1) {
        return false;
    }
    if dpcd_rev < DP_DPCD_REV_12 {
        return true;
    }
    io.write(DP_LINK_SERVICE_IRQ_VECTOR_ESI0, &esi[3..4]) == Ok(1)
}

/// Read general ESI vector; display-14 WA separates sink count from IRQ byte.
// upstream: intel_dp.c intel_dp_get_sink_irq_esi()
pub fn intel_dp_get_sink_irq_esi(
    display_ver: u8,
    display_subver: u8,
    battlemage: bool,
    io: &mut impl DpAuxIo,
    esi: &mut [u8; 4],
) -> bool {
    const DP_SINK_COUNT_ESI: u32 = 0x2002;
    const DP_LINK_SERVICE_IRQ_VECTOR_ESI0: u32 = 0x2005;
    if display_ver == 14 && display_subver == 20 && !battlemage {
        if io.read(DP_SINK_COUNT_ESI, &mut esi[..3]) != Ok(3) {
            return false;
        }
        return io.read(DP_LINK_SERVICE_IRQ_VECTOR_ESI0, &mut esi[3..4]) == Ok(1);
    }
    io.read(DP_SINK_COUNT_ESI, esi) == Ok(4)
}

/// Acknowledge ESI bytes 1-3, retrying three AUX writes as i915 does.
// upstream: intel_dp.c intel_dp_ack_sink_irq_esi()
pub fn intel_dp_ack_sink_irq_esi(io: &mut impl DpAuxIo, esi: &[u8; 4]) -> bool {
    for _ in 0..3 {
        if io.write(0x2003, &esi[1..4]) == Ok(3) {
            return true;
        }
    }
    false
}

/// Read/ack SST ESI and mask to the events owned by this source.
// upstream: intel_dp.c intel_dp_get_and_ack_sink_irq_esi_sst()
pub fn intel_dp_get_and_ack_sink_irq_esi_sst(
    state: &DpRuntimeState,
    io: &mut impl DpAuxIo,
    esi: &mut [u8; 4],
) -> bool {
    const DEVICE_MASK: u8 = (1 << 1) | (1 << 2) | (1 << 6);
    const LINK_MASK: u8 = (1 << 0) | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5);
    if !intel_dp_get_sink_irq_esi_sst(state.dpcd.first().copied().unwrap_or(0), io, esi) {
        return false;
    }
    esi[1] &= DEVICE_MASK;
    esi[3] &= LINK_MASK;
    if esi[1..4].iter().all(|byte| *byte == 0) {
        return true;
    }
    intel_dp_ack_sink_irq_esi_sst(state.dpcd.first().copied().unwrap_or(0), io, esi)
}

/// Read normal or MST link-status DPCD window and retain downstream-change state.
// upstream: intel_dp.c intel_dp_read_link_status()
pub fn intel_dp_read_link_status(
    io: &mut impl DpAuxIo,
    mst_active_streams: usize,
    state: &mut DpRuntimeState,
    status: &mut [u8; 6],
) -> Result<(), DpError> {
    const DP_LANE0_1_STATUS: u32 = 0x0202;
    const DP_LANE0_1_STATUS_ESI: u32 = 0x200c;
    const DP_LANE_ALIGN_STATUS_UPDATED: usize = 2;
    const DP_DOWNSTREAM_PORT_STATUS_CHANGED: u8 = 1 << 6;
    status.fill(0);
    let address = if mst_active_streams > 0 {
        DP_LANE0_1_STATUS_ESI
    } else {
        DP_LANE0_1_STATUS
    };
    if io.read(address, status)? != status.len() {
        return Err(DpError::Io);
    }
    if status[DP_LANE_ALIGN_STATUS_UPDATED] & DP_DOWNSTREAM_PORT_STATUS_CHANGED != 0 {
        state.downstream_port_changed = true;
    }
    Ok(())
}

/// Check coding-specific channel equalization for all active lanes.
// upstream: intel_dp.c intel_dp_link_ok()
pub fn intel_dp_link_ok(
    link_rate: u32,
    lanes: u8,
    status: &[u8; 6],
    framework: &impl DpFramework,
) -> bool {
    framework.link_status_ok(link_rate, lanes, status)
}

/// Preserve retraining gates and their exact short-circuit order.
// upstream: intel_dp.c intel_dp_needs_link_retrain()
pub fn intel_dp_needs_link_retrain(
    state: &mut DpRuntimeState,
    psr_enabled: bool,
    mst_active_streams: usize,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    psr_link_ok: bool,
) -> bool {
    if !state.link.active {
        return false;
    }
    if psr_enabled {
        return false;
    }
    if state.link.force_retrain {
        return true;
    }
    let mut link_status = [0u8; 6];
    if intel_dp_read_link_status(io, mst_active_streams, state, &mut link_status).is_err() {
        return false;
    }
    if !intel_dp_link_params_valid(&state.link, state.link_rate, state.lane_count) {
        return false;
    }
    if state.link.retrain_disabled {
        return false;
    }
    if state.link.seq_train_failures != 0 {
        return true;
    }
    !intel_dp_link_ok(state.link_rate, state.lane_count, &link_status, framework) && !psr_link_ok
}

/// Service connector/device and link IRQ events in source order through narrow hooks.
pub trait DpIrqHooks {
    fn test_reset(&mut self);
    fn automated_test_request(&mut self);
    fn content_protection_irq(&mut self);
    fn sink_specific_irq(&mut self);
    fn link_status_changed(&mut self);
    fn hdmi_link_status_changed(&mut self);
    fn connected_off_entry_requested(&mut self);
    fn tunnel_irq(&mut self) -> bool;
    fn mst_hpd_irq(&mut self, esi: &[u8; 4], ack: &mut [u8; 4]);
    fn send_mst_request(&mut self);
    fn psr_short_pulse(&mut self);
    fn alpm_error(&mut self) -> bool;
    fn disable_alpm(&mut self);
    fn dp_test_short_pulse(&mut self) -> bool;
    fn cec_irq(&mut self);
}

/// Process MST HPD-IRQ flags and acknowledge HDCP CP_IRQ separately.
// upstream: intel_dp.c intel_dp_mst_hpd_irq()
pub fn intel_dp_mst_hpd_irq(esi: &[u8; 4], ack: &mut [u8; 4], hooks: &mut impl DpIrqHooks) {
    hooks.mst_hpd_irq(esi, ack);
    if esi[1] & (1 << 2) != 0 {
        hooks.content_protection_irq();
        ack[1] |= 1 << 2;
    }
}

/// Handle SST device-service IRQ bits.
// upstream: intel_dp.c intel_dp_handle_device_service_irq()
pub fn intel_dp_handle_device_service_irq(irq_mask: u8, hooks: &mut impl DpIrqHooks) {
    if irq_mask & (1 << 1) != 0 {
        hooks.automated_test_request();
    }
    if irq_mask & (1 << 2) != 0 {
        hooks.content_protection_irq();
    }
    if irq_mask & (1 << 6) != 0 {
        hooks.sink_specific_irq();
    }
}

/// Handle link service IRQs; return whether a full connector reprobe is needed.
// upstream: intel_dp.c intel_dp_handle_link_service_irq()
pub fn intel_dp_handle_link_service_irq(
    state: &mut DpRuntimeState,
    irq_mask: u8,
    hooks: &mut impl DpIrqHooks,
) -> bool {
    const RX_CAP_CHANGED: u8 = 1 << 0;
    const LINK_STATUS_CHANGED: u8 = 1 << 1;
    const HDMI_LINK_STATUS_CHANGED: u8 = 1 << 3;
    const CONNECTED_OFF_ENTRY_REQUESTED: u8 = 1 << 4;
    const DP_TUNNELING_IRQ: u8 = 1 << 5;
    let mut reprobe_needed = false;
    if irq_mask & RX_CAP_CHANGED != 0 {
        state.reset_link_params = true;
        reprobe_needed = true;
    }
    if irq_mask & LINK_STATUS_CHANGED != 0 {
        hooks.link_status_changed();
    }
    if irq_mask & HDMI_LINK_STATUS_CHANGED != 0 {
        hooks.hdmi_link_status_changed();
    }
    if irq_mask & CONNECTED_OFF_ENTRY_REQUESTED != 0 {
        hooks.connected_off_entry_requested();
    }
    if irq_mask & DP_TUNNELING_IRQ != 0 && hooks.tunnel_irq() {
        reprobe_needed = true;
    }
    reprobe_needed
}

/// SST is connected when connector status is connected; MST is connected while active.
// upstream: intel_dp.c intel_dp_is_connected()
pub const fn intel_dp_is_connected(connector_connected: bool, is_mst: bool) -> bool {
    connector_connected || is_mst
}

/// Connector association test against the SST encoder or any MST stream encoder.
// upstream: intel_dp.c intel_dp_has_connector()
pub fn intel_dp_has_connector(
    best_encoder: Option<u64>,
    sst_encoder: u64,
    mst_stream_encoders: &[u64],
) -> bool {
    match best_encoder {
        Some(encoder) => encoder == sst_encoder || mst_stream_encoders.contains(&encoder),
        None => false,
    }
}

/// Modeset/connector adapter provides lock ordering and active-pipe discovery.
pub trait DpRetrainOps {
    fn is_connected(&self) -> bool;
    fn lock_connection_mutex(&mut self) -> Result<(), DpError>;
    /// Must lock candidate CRTCs and wait for their connector HW-done commits.
    fn get_active_pipes(&mut self) -> Result<u8, DpError>;
    fn commit_pipes(&mut self, pipe_mask: u8) -> Result<(), DpError>;
    fn retry_deadlock(&mut self) -> bool;
    fn queue_link_check(&mut self);
}

/// Resolve active DP pipes; the adapter performs per-CRTC locks and commit waits.
// upstream: intel_dp.c intel_dp_get_active_pipes()
pub fn intel_dp_get_active_pipes(ops: &mut impl DpRetrainOps) -> Result<u8, DpError> {
    ops.get_active_pipes()
}

/// Commit wait adapter used while the connection mutex protects connector state.
pub trait DpCommitWait {
    fn wait_hw_done(&mut self, timeout_ms: u32) -> bool;
}

/// Wait for current connector commit's hardware-done completion, when present.
// upstream: intel_dp.c wait_for_connector_hw_done()
pub fn wait_for_connector_hw_done(has_commit: bool, io: &mut impl DpCommitWait) -> bool {
    !has_commit || io.wait_hw_done(5000)
}

/// Wait for connector's current HW commit before mutating the shared DP link.
// upstream: intel_dp.c intel_dp_flush_connector_commits()
pub fn intel_dp_flush_connector_commits(has_commit: bool, io: &mut impl DpCommitWait) -> bool {
    wait_for_connector_hw_done(has_commit, io)
}

/// Lock, recheck the link, discover active pipes and atomically retrain them.
// upstream: intel_dp.c intel_dp_retrain_link()
pub fn intel_dp_retrain_link(
    state: &mut DpRuntimeState,
    psr_enabled: bool,
    mst_active_streams: usize,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    psr_link_ok: bool,
    ops: &mut impl DpRetrainOps,
) -> Result<(), DpError> {
    if !ops.is_connected() {
        return Ok(());
    }
    ops.lock_connection_mutex()?;
    if !intel_dp_needs_link_retrain(
        state,
        psr_enabled,
        mst_active_streams,
        io,
        framework,
        psr_link_ok,
    ) {
        return Ok(());
    }
    let pipe_mask = intel_dp_get_active_pipes(ops)?;
    if pipe_mask == 0 {
        return Ok(());
    }
    if !intel_dp_needs_link_retrain(
        state,
        psr_enabled,
        mst_active_streams,
        io,
        framework,
        psr_link_ok,
    ) {
        return Ok(());
    }
    let result = ops.commit_pipes(pipe_mask);
    if result == Err(DpError::Deadlock) {
        return result;
    }
    state.link.force_retrain = false;
    result
}

/// Retry the modeset lock context on deadlock, matching the kernel retry macro.
// upstream: intel_dp.c intel_dp_link_check()
pub fn intel_dp_link_check(
    state: &mut DpRuntimeState,
    psr_enabled: bool,
    mst_active_streams: usize,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    psr_link_ok: bool,
    ops: &mut impl DpRetrainOps,
) -> Result<(), DpError> {
    loop {
        match intel_dp_retrain_link(
            state,
            psr_enabled,
            mst_active_streams,
            io,
            framework,
            psr_link_ok,
            ops,
        ) {
            Err(DpError::Deadlock) if ops.retry_deadlock() => continue,
            result => return result,
        }
    }
}

/// Queue background link check only for connected ports needing recovery.
// upstream: intel_dp.c intel_dp_check_link_state()
pub fn intel_dp_check_link_state(
    state: &mut DpRuntimeState,
    psr_enabled: bool,
    mst_active_streams: usize,
    io: &mut impl DpAuxIo,
    framework: &impl DpFramework,
    psr_link_ok: bool,
    ops: &mut impl DpRetrainOps,
) {
    if !ops.is_connected() {
        return;
    }
    if intel_dp_needs_link_retrain(
        state,
        psr_enabled,
        mst_active_streams,
        io,
        framework,
        psr_link_ok,
    ) {
        ops.queue_link_check();
    }
}

/// Handle the 33-iteration MST ESI clear loop, re-requests and force-retrain deferral.
// upstream: intel_dp.c intel_dp_check_mst_status()
pub fn intel_dp_check_mst_status(
    state: &mut DpRuntimeState,
    io: &mut impl DpAuxIo,
    hooks: &mut impl DpIrqHooks,
) -> bool {
    const MST_DEVICE_MASK: u8 = (1 << 2) | (1 << 4) | (1 << 5);
    const MST_LINK_MASK: u8 = (1 << 0) | (1 << 1) | (1 << 5);
    let mut force_retrain = state.link.force_retrain;
    let mut reprobe_needed = false;
    let mut tries = 33;
    let mut cleared = false;
    while tries > 1 {
        tries -= 1;
        let mut esi = [0u8; 4];
        let mut ack = [0u8; 4];
        if !intel_dp_get_sink_irq_esi(0, 0, false, io, &mut esi) {
            reprobe_needed = true;
            break;
        }
        ack[3] |= esi[3] & MST_LINK_MASK;
        intel_dp_mst_hpd_irq(&esi, &mut ack, hooks);
        let new_irqs = ack.iter().any(|byte| *byte != 0);
        ack[1] &= MST_DEVICE_MASK;
        ack[3] &= MST_LINK_MASK;
        if new_irqs && !intel_dp_ack_sink_irq_esi(io, &ack) { /* retain protocol progress */ }
        if ack[1] & ((1 << 4) | (1 << 5)) != 0 {
            hooks.send_mst_request();
        }
        if force_retrain {
            ack[3] |= 1 << 1;
            force_retrain = false;
        }
        if intel_dp_handle_link_service_irq(state, ack[3], hooks) {
            reprobe_needed = true;
        }
        if !new_irqs {
            cleared = true;
            break;
        }
    }
    if tries == 1 && !cleared {
        reprobe_needed = true;
    }
    !reprobe_needed
}

/// Handle SST short-pulse events and trigger full detection when state changes.
// upstream: intel_dp.c intel_dp_short_pulse()
pub fn intel_dp_short_pulse(
    state: &mut DpRuntimeState,
    io: &mut impl DpAuxIo,
    hooks: &mut impl DpIrqHooks,
    stored_sink_count: u8,
    has_sink_count: bool,
) -> bool {
    let mut esi = [0u8; 4];
    hooks.test_reset();
    if !intel_dp_get_and_ack_sink_irq_esi_sst(state, io, &mut esi) {
        return false;
    }
    let sink_count = ((esi[0] & 0x80) >> 1) | (esi[0] & 0x3f);
    if has_sink_count && sink_count != stored_sink_count {
        return false;
    }
    intel_dp_handle_device_service_irq(esi[1], hooks);
    esi[3] |= 1 << 1; // always validate link status on this short pulse path
    let mut reprobe_needed = intel_dp_handle_link_service_irq(state, esi[3], hooks);
    hooks.cec_irq();
    if state.downstream_port_changed {
        state.downstream_port_changed = false;
        reprobe_needed = true;
    }
    hooks.psr_short_pulse();
    if hooks.alpm_error() {
        hooks.disable_alpm();
        state.sink_alpm_error = true;
    }
    if hooks.dp_test_short_pulse() {
        reprobe_needed = true;
    }
    !reprobe_needed
}

/// Compute bpp/candidate rate requirement for the active stream state.
// upstream: intel_dp.c intel_dp_config_required_rate()
pub fn intel_dp_config_required_rate(
    link: &SelectedLink,
    mode: DisplayMode,
    framework: &impl DpFramework,
) -> u64 {
    let bpp_x16 = if link.dsc {
        link.compressed_bpp_x16
    } else {
        u16::from(link.pipe_bpp) * 16
    };
    intel_dp_link_required(
        link.rate,
        link.lane_count,
        mode.clock_khz,
        mode.hdisplay,
        bpp_x16,
        0,
        framework,
    )
}

/// Maximum downstream TMDS clock from protocol-converter and sink metadata.
// upstream: intel_dp.c intel_dp_max_tmds_clock()
pub fn intel_dp_max_tmds_clock(pcon_max_tmds: u32, sink_max_tmds: u32) -> u32 {
    if pcon_max_tmds != 0 && sink_max_tmds != 0 {
        min(pcon_max_tmds, sink_max_tmds)
    } else {
        pcon_max_tmds
    }
}

/// Calculate the TMDS clock and compare with downstream min/max constraints.
// upstream: intel_dp.c intel_dp_tmds_clock_valid()
pub fn intel_dp_tmds_clock_valid(
    clock: u32,
    bpc: u8,
    format: OutputFormat,
    min_tmds_clock: u32,
    max_tmds_clock: u32,
    respect_downstream_limits: bool,
) -> ModeStatus {
    if !respect_downstream_limits {
        return ModeStatus::Ok;
    }
    let tmds_clock = match format {
        OutputFormat::Ycbcr420 => clock * u32::from(bpc) / 16,
        OutputFormat::Ycbcr422 => clock,
        _ => clock * u32::from(bpc) / 8,
    };
    if min_tmds_clock != 0 && tmds_clock < min_tmds_clock {
        return ModeStatus::ClockLow;
    }
    if max_tmds_clock != 0 && tmds_clock > max_tmds_clock {
        return ModeStatus::ClockHigh;
    }
    ModeStatus::Ok
}

/// Compute maximum downstream BPC by trying deep-color values in 2-bpc steps.
// upstream: intel_dp.c intel_dp_hdmi_compute_bpc()
pub fn intel_dp_hdmi_compute_bpc(
    max_bpc: u8,
    clock: u32,
    format: OutputFormat,
    is_hdmi_sink: bool,
    respect_downstream_limits: bool,
    min_tmds_clock: u32,
    max_tmds_clock: u32,
    mut bpc_possible: impl FnMut(u8, bool) -> bool,
) -> Option<u8> {
    let mut bpc = max(max_bpc, 8);
    if !respect_downstream_limits {
        bpc = 8;
    }
    while bpc >= 8 {
        if bpc_possible(bpc, is_hdmi_sink)
            && intel_dp_tmds_clock_valid(
                clock,
                bpc,
                format,
                min_tmds_clock,
                max_tmds_clock,
                respect_downstream_limits,
            ) == ModeStatus::Ok
        {
            return Some(bpc);
        }
        if bpc < 10 {
            break;
        }
        bpc -= 2;
    }
    None
}

/// eDP detect cannot report disconnected after connector registration.
// upstream: intel_dp.c edp_detect()
pub const fn edp_detect() -> ConnectorStatus {
    ConnectorStatus::Connected
}

/// A DP branch exposes HDMI only when the attached connector display info says so.
// upstream: intel_dp.c intel_dp_has_hdmi_sink()
pub const fn intel_dp_has_hdmi_sink(is_hdmi_sink: bool) -> bool {
    is_hdmi_sink
}

/// Drop dead FRL link state and restart FRL/TMDS fallback after status change.
// upstream: intel_dp.c intel_dp_handle_hdmi_link_status_change()
pub fn intel_dp_handle_hdmi_link_status_change(
    state: &mut DpRuntimeState,
    source_ctl_mode: bool,
    is_hdmi_2_1_sink: bool,
    max_pcon_frl: u8,
    max_sink_frl: u8,
    io: &mut impl DpAuxIo,
    pcon: &mut impl DpPconOps,
) -> Result<(), DpError> {
    if !state.frl_is_trained || pcon.hdmi_link_active() {
        return Ok(());
    }
    const DP_PCON_HDMI_LINK_CONFIG_1: u32 = 0x305a;
    const DP_PCON_ENABLE_HDMI_LINK: u8 = 1 << 7;
    let mut config = [0u8; 1];
    if io.read(DP_PCON_HDMI_LINK_CONFIG_1, &mut config)? != 1 {
        return Err(DpError::Io);
    }
    config[0] &= !DP_PCON_ENABLE_HDMI_LINK;
    if io.write(DP_PCON_HDMI_LINK_CONFIG_1, &config)? != 1 {
        return Err(DpError::Io);
    }
    pcon.report_frl_link_error();
    state.frl_is_trained = false;
    intel_dp_check_frl_training(
        source_ctl_mode,
        is_hdmi_2_1_sink,
        &mut state.frl_trained_rate_gbps,
        &mut state.frl_is_trained,
        max_pcon_frl,
        max_sink_frl,
        io,
        pcon,
    )
}

/// FRL payload bandwidth in kbit/s after the format's chroma subsampling.
// upstream: intel_dp.c frl_required_bw()
pub fn frl_required_bw(clock: u32, bpc: u8, format: OutputFormat) -> u64 {
    let clock = if format == OutputFormat::Ycbcr420 {
        clock / 2
    } else {
        clock
    };
    u64::from(clock) * u64::from(bpc) * 3
}

/// Apply branch FRL, max-dotclock, then TMDS downstream mode limits.
// upstream: intel_dp.c intel_dp_mode_valid_downstream()
pub fn intel_dp_mode_valid_downstream(
    target_clock: u32,
    format: OutputFormat,
    pcon_max_frl_bw_gbps: u8,
    max_dotclock: u32,
    min_tmds_clock: u32,
    max_tmds_clock: u32,
) -> ModeStatus {
    if pcon_max_frl_bw_gbps != 0 {
        let target_bw = frl_required_bw(target_clock, 8, format);
        if target_bw > u64::from(pcon_max_frl_bw_gbps) * 1_000_000 {
            ModeStatus::ClockHigh
        } else {
            ModeStatus::Ok
        }
    } else if max_dotclock != 0 && target_clock > max_dotclock {
        ModeStatus::ClockHigh
    } else {
        intel_dp_tmds_clock_valid(
            target_clock,
            8,
            format,
            min_tmds_clock,
            max_tmds_clock,
            true,
        )
    }
}

/// Select output encoding and link configuration with 4:2:0 fallback ordering.
pub trait DpFormatOps {
    fn sink_format_valid(&self, sink: OutputFormat) -> ModeStatus;
    fn output_format(&self, sink: OutputFormat) -> OutputFormat;
    fn compute_link_config(
        &mut self,
        output: OutputFormat,
        respect_downstream_limits: bool,
    ) -> Result<(), DpError>;
}

/// Compute one sink-format candidate after sink validation.
// upstream: intel_dp.c intel_dp_compute_output_format()
pub fn intel_dp_compute_output_format(
    ops: &mut impl DpFormatOps,
    sink: OutputFormat,
    respect_downstream_limits: bool,
) -> Result<OutputFormat, DpError> {
    if ops.sink_format_valid(sink) != ModeStatus::Ok {
        return Err(DpError::Invalid);
    }
    let output = ops.output_format(sink);
    ops.compute_link_config(output, respect_downstream_limits)?;
    Ok(output)
}

/// Try RGB first except 4:2:0-only modes; retain source fallback ordering.
// upstream: intel_dp.c intel_dp_compute_formats()
pub fn intel_dp_compute_formats(
    ops: &mut impl DpFormatOps,
    is_420_only: bool,
    is_420_also: bool,
    respect_downstream_limits: bool,
) -> Result<OutputFormat, DpError> {
    if is_420_only {
        match intel_dp_compute_output_format(ops, OutputFormat::Ycbcr420, respect_downstream_limits)
        {
            Ok(format) => Ok(format),
            Err(_) if !respect_downstream_limits => {
                intel_dp_compute_output_format(ops, OutputFormat::Rgb, false)
            }
            Err(error) => Err(error),
        }
    } else {
        match intel_dp_compute_output_format(ops, OutputFormat::Rgb, respect_downstream_limits) {
            Ok(format) => Ok(format),
            Err(_) if is_420_also => intel_dp_compute_output_format(
                ops,
                OutputFormat::Ycbcr420,
                respect_downstream_limits,
            ),
            Err(error) => Err(error),
        }
    }
}

/// Source-port audio capability, including G4x and pre-Gen12 port-A gates.
// upstream: intel_dp.c intel_dp_port_has_audio()
pub const fn intel_dp_port_has_audio(display_ver: u8, port_a: bool, g4x: bool) -> bool {
    !g4x && !(display_ver < 12 && port_a)
}

/// Resolve connector force-audio state against sink EDID audio support.
// upstream: intel_dp.c intel_dp_has_audio()
pub const fn intel_dp_has_audio(
    port_has_audio: bool,
    force: AudioForce,
    sink_has_audio: bool,
) -> bool {
    port_has_audio
        && match force {
            AudioForce::Auto => sink_has_audio,
            AudioForce::On => true,
            AudioForce::Off => false,
        }
}

/// Configure audio and UHBR SDP splitting through the display audio adapter.
// upstream: intel_dp.c intel_dp_audio_compute_config()
pub fn intel_dp_audio_compute_config(
    has_audio: bool,
    uhbr: bool,
    audio_compute_config: impl FnOnce() -> bool,
) -> (bool, bool) {
    let enabled = has_audio && audio_compute_config();
    (enabled, enabled && uhbr)
}

/// Seamless DRRS requires double-buffered M/N registers and seamless panel policy.
// upstream: intel_dp.c has_seamless_m_n()
pub const fn has_seamless_m_n(double_buffered_m_n: bool, seamless_drrs: bool) -> bool {
    double_buffered_m_n && seamless_drrs
}

/// Determine whether DRRS can coexist with VRR, PSR, PCH, and this transcoder.
// upstream: intel_dp.c can_enable_drrs()
pub const fn can_enable_drrs(policy: DrrsPolicy) -> bool {
    !policy.vrr_enabled
        && !policy.psr_enabled
        && !policy.has_pch_encoder
        && policy.transcoder_has_drrs
        && policy.downclock_mode_clock != 0
        && policy.seamless_m_n
}

/// Select panel highest-mode clock only for seamless M/N reprogramming.
// upstream: intel_dp.c intel_dp_mode_clock()
pub const fn intel_dp_mode_clock(
    adjusted_mode_clock: u32,
    panel_highest_mode_clock: u32,
    seamless_m_n: bool,
) -> u32 {
    if seamless_m_n {
        panel_highest_mode_clock
    } else {
        adjusted_mode_clock
    }
}

/// Compute M/N after applying panel DRRS eligibility and split-link scaling.
// upstream: intel_dp.c intel_dp_drrs_compute_config()
pub fn intel_dp_drrs_compute_config(
    policy: DrrsPolicy,
    mode: DisplayMode,
    link_bpp_x16: u16,
    lane_count: u8,
    port_clock: u32,
    fec_enabled: bool,
    has_m2_n2: bool,
    mut result: DrrsResult,
    mut compute_m_n: impl FnMut(u16, u8, u32, u32, u32) -> DpMn,
) -> DrrsResult {
    if policy.seamless_m_n && !policy.joined_pipes {
        result.update_m_n = true;
    }
    if !can_enable_drrs(policy) {
        if has_m2_n2 {
            result.m2_n2 = DpMn::default();
        }
        return result;
    }
    if policy.ironlake_or_sandybridge_or_ivybridge {
        result.msa_timing_delay = policy.vbt_msa_timing_delay;
    }
    result.has_drrs = true;
    let pixel_clock = if policy.splitter_links != 0 {
        policy.downclock_mode_clock / u32::from(policy.splitter_links)
    } else {
        policy.downclock_mode_clock
    };
    result.m2_n2 = compute_m_n(
        link_bpp_x16,
        lane_count,
        pixel_clock,
        port_clock,
        intel_dp_bw_fec_overhead(fec_enabled),
    );
    if policy.splitter_links != 0 {
        result.m2_n2.data_m = result
            .m2_n2
            .data_m
            .saturating_mul(u32::from(policy.splitter_links));
    }
    let _ = mode;
    result
}

/// Display 12/13 never programs the later MIN_HBLANK register.
// upstream: intel_dp.c intel_dp_compute_min_hblank()
pub fn intel_dp_compute_min_hblank(
    display_ver: u8,
    later_generation: impl FnOnce() -> Result<u16, DpError>,
) -> Result<u16, DpError> {
    if display_ver < 30 {
        Ok(0)
    } else {
        later_generation()
    }
}

/// DP atomic config callbacks for operations owned by adjacent display subsystems.
pub trait DpComputeConfigOps {
    fn panel_compute_config(&mut self, config: &mut DpPipeConfig) -> Result<(), DpError>;
    fn compute_formats(
        &mut self,
        config: &mut DpPipeConfig,
        respect_downstream_limits: bool,
    ) -> Result<(), DpError>;
    fn pfit_compute_config(&mut self, config: &mut DpPipeConfig) -> Result<(), DpError>;
    fn compute_min_hblank(&mut self, config: &mut DpPipeConfig) -> Result<u16, DpError>;
    fn audio_compute_config(&mut self, config: &mut DpPipeConfig, uhbr: bool);
    fn compute_m_n(&mut self, config: &mut DpPipeConfig, link_bpp_x16: u16);
    fn compute_vrr(&mut self, config: &mut DpPipeConfig);
    fn compute_psr(&mut self, config: &mut DpPipeConfig);
    fn compute_as_sdp(&mut self, config: &mut DpPipeConfig);
    fn compute_alpm_lobf(&mut self, config: &mut DpPipeConfig);
    fn compute_drrs(&mut self, config: &mut DpPipeConfig, link_bpp_x16: u16);
    fn compute_vsc_sdp(&mut self, config: &mut DpPipeConfig);
    fn compute_hdr_metadata_sdp(&mut self, config: &mut DpPipeConfig);
    fn tunnel_atomic_compute_stream_bw(&mut self, config: &mut DpPipeConfig)
    -> Result<(), DpError>;
}

/// Preserve the full atomic DP mode-config ordering and fallback boundaries.
// upstream: intel_dp.c intel_dp_compute_config()
pub fn intel_dp_compute_config(
    config: &mut DpPipeConfig,
    display_ver: u8,
    is_edp: bool,
    interlace_allowed: bool,
    has_ddi: bool,
    enhanced_frame_cap: bool,
    mso_link_count: u8,
    mso_pixel_overlap: u16,
    broadcast_rgb_auto: bool,
    limited_range_requested: bool,
    default_range_limited: bool,
    ops: &mut impl DpComputeConfigOps,
) -> Result<(), DpError> {
    if is_edp {
        ops.panel_compute_config(config)?;
    }
    if config.mode_dblscan || (config.mode_interlaced && !interlace_allowed) || config.mode_dblclk {
        return Err(DpError::Invalid);
    }
    if intel_dp_hdisplay_bad(has_ddi, config.mode.hdisplay) {
        return Err(DpError::Invalid);
    }
    if ops.compute_formats(config, true).is_err() {
        ops.compute_formats(config, false)?;
    }
    ops.pfit_compute_config(config)?;
    config.limited_color_range = intel_dp_limited_color_range(
        config.output_format,
        config.pipe_bpp,
        broadcast_rgb_auto,
        limited_range_requested,
        default_range_limited,
    );
    let uhbr = intel_dp_is_uhbr(config.port_clock);
    config.mst_master_transcoder = uhbr;
    config.enhanced_framing = !uhbr && enhanced_frame_cap;
    let link_bpp_x16 = if config.dsc {
        config.compressed_bpp_x16
    } else {
        intel_dp_output_format_link_bpp_x16(config.output_format, config.pipe_bpp)
    };
    if mso_link_count != 0 {
        config.splitter_links = mso_link_count;
        config.splitter_overlap = mso_pixel_overlap;
        let links = u32::from(mso_link_count);
        let overlap = u32::from(mso_pixel_overlap);
        config.mode.hdisplay = config.mode.hdisplay / links + overlap;
        config.mode.hsync_start = config.mode.hsync_start / links + overlap;
        config.mode.hsync_end = config.mode.hsync_end / links + overlap;
        config.mode.htotal = config.mode.htotal / links + overlap;
        config.mode.clock_khz /= links;
    }
    ops.audio_compute_config(config, uhbr);
    if !uhbr {
        ops.compute_m_n(config, link_bpp_x16);
    }
    config.min_hblank =
        intel_dp_compute_min_hblank(display_ver, || ops.compute_min_hblank(config))?;
    if config.splitter_links != 0 && config.m_n_valid {
        config.dp_m_n.data_m = config
            .dp_m_n
            .data_m
            .saturating_mul(u32::from(config.splitter_links));
    }
    ops.compute_vrr(config);
    ops.compute_psr(config);
    ops.compute_as_sdp(config);
    ops.compute_alpm_lobf(config);
    ops.compute_drrs(config, link_bpp_x16);
    ops.compute_vsc_sdp(config);
    ops.compute_hdr_metadata_sdp(config);
    ops.tunnel_atomic_compute_stream_bw(config)
}

/// Late-config callbacks for PSR and ALPM protocols.
pub trait DpLateConfigOps {
    fn psr_compute_config_late(&mut self, config: &mut DpPipeConfig);
    fn alpm_lobf_compute_config_late(&mut self, config: &mut DpPipeConfig);
}

/// Run late PSR setup, validate SDP guardband, then finalize ALPM/LOBF.
// upstream: intel_dp.c intel_dp_compute_config_late()
pub fn intel_dp_compute_config_late(
    config: &mut DpPipeConfig,
    vblank_length: u16,
    guardband: SdpGuardbandState,
    ops: &mut impl DpLateConfigOps,
) -> Result<(), DpError> {
    ops.psr_compute_config_late(config);
    intel_dp_sdp_compute_config_late(guardband, vblank_length)?;
    ops.alpm_lobf_compute_config_late(config);
    Ok(())
}

/// Return whether HDR metadata requests HDR10/PQ transfer characteristics.
// upstream: intel_dp.c intel_dp_in_hdr_mode()
pub const fn intel_dp_in_hdr_mode(has_metadata: bool, eotf: u8) -> bool {
    has_metadata && eotf == 2
}

/// Clamp max pipe bpp by requested BPC, DP++ TMDS limits and eDP VBT bpp.
// upstream: intel_dp.c intel_dp_max_bpp()
pub fn intel_dp_max_bpp(
    max_pipe_bpp: u8,
    dfp_max_bpc: u8,
    edp_vbt_bpp: u8,
    edid_bpc: u8,
    edp: bool,
    dfp_min_tmds_clock: u32,
    respect_downstream_limits: bool,
    clock: u32,
    output_format: OutputFormat,
    is_hdmi_sink: bool,
    min_tmds_clock: u32,
    max_tmds_clock: u32,
    bpc_possible: impl FnMut(u8, bool) -> bool,
) -> u8 {
    let mut bpc = max_pipe_bpp / 3;
    if dfp_max_bpc != 0 {
        bpc = min(bpc, dfp_max_bpc);
    }
    if dfp_min_tmds_clock != 0 {
        let Some(max_hdmi_bpc) = intel_dp_hdmi_compute_bpc(
            bpc,
            clock,
            output_format,
            is_hdmi_sink,
            respect_downstream_limits,
            min_tmds_clock,
            max_tmds_clock,
            bpc_possible,
        ) else {
            return 0;
        };
        bpc = min(bpc, max_hdmi_bpc);
    }
    let mut bpp = bpc * 3;
    if edp && edid_bpc == 0 && edp_vbt_bpp != 0 && edp_vbt_bpp < bpp {
        bpp = edp_vbt_bpp;
    }
    bpp
}

/// Choose RGB full/limited-range policy; all YCbCr streams remain limited.
// upstream: intel_dp.c intel_dp_limited_color_range()
pub fn intel_dp_limited_color_range(
    output: OutputFormat,
    pipe_bpp: u8,
    broadcast_rgb_auto: bool,
    limited_requested: bool,
    default_range_limited: bool,
) -> bool {
    if output != OutputFormat::Rgb {
        return false;
    }
    if broadcast_rgb_auto {
        pipe_bpp != 18 && default_range_limited
    } else {
        limited_requested
    }
}

/// Reject the known legacy 4096-wide DP modes on non-DDI hardware.
// upstream: intel_dp.c intel_dp_hdisplay_bad()
pub const fn intel_dp_hdisplay_bad(has_ddi: bool, hdisplay: u32) -> bool {
    hdisplay == 4096 && !has_ddi
}

/// Validate 4:2:0 and RGB sink-format constraints.
// upstream: intel_dp.c intel_dp_sink_format_valid()
pub fn intel_dp_sink_format_valid(
    format: OutputFormat,
    min_tmds_clock: u32,
    is_hdmi_sink: bool,
    ycbcr_420_allowed: bool,
    mode_is_420: bool,
) -> ModeStatus {
    match format {
        OutputFormat::Ycbcr420 => {
            if min_tmds_clock != 0 && !is_hdmi_sink {
                return ModeStatus::No420;
            }
            if !ycbcr_420_allowed || !mode_is_420 {
                return ModeStatus::No420;
            }
            ModeStatus::Ok
        }
        OutputFormat::Rgb => ModeStatus::Ok,
        _ => ModeStatus::Bad,
    }
}

/// DRM mode/DSC/joiner hooks needed to retain the connector-mode validation walk.
pub trait DpModeValidationOps {
    fn cpu_transcoder_mode_valid(&self, mode: DisplayMode) -> ModeStatus;
    fn panel_mode_valid(&self, mode: DisplayMode, target_clock: &mut u32) -> ModeStatus;
    fn joiner_candidates(&self, mode: DisplayMode) -> Vec<u8>;
    fn pfit_mode_valid(
        &self,
        mode: DisplayMode,
        output: OutputFormat,
        joined_pipes: u8,
    ) -> ModeStatus;
    fn dsc_max_pipe_bpp(&self) -> u8;
    fn joiner_needs_dsc(&self, joined_pipes: u8) -> bool;
    fn max_plane_size_valid(&self, mode: DisplayMode, joined_pipes: u8) -> ModeStatus;
    fn dotclk_valid(&self, mode: DisplayMode, slices: u8, joined_pipes: u8) -> bool;
    fn downstream_mode_valid(&self, target_clock: u32, sink_format: OutputFormat) -> ModeStatus;
}

/// Validate one sink/output-format candidate, including DSC/joiner iteration.
// upstream: intel_dp.c intel_dp_mode_valid_format()
pub fn intel_dp_mode_valid_format(
    request: DpModeRequest,
    sink_format: OutputFormat,
    dsc_caps: &DscSinkCaps,
    link_caps: &DpSinkCaps,
    framework: &impl DpFramework,
    ops: &impl DpModeValidationOps,
) -> ModeStatus {
    let format_status = intel_dp_sink_format_valid(
        sink_format,
        request.min_tmds_clock,
        request.is_hdmi_sink,
        request.ycbcr_420_allowed,
        request.sink_420_only || request.sink_420_also,
    );
    if format_status != ModeStatus::Ok {
        return format_status;
    }
    let output = intel_dp_output_format(
        sink_format,
        request.forced_output_format,
        request.display_ver,
        request.has_gmch,
        request.ironlake,
        request.is_branch,
        request.rgb_to_ycbcr,
        request.ycbcr444_to_420,
    );
    let max_rate = request.max_link_rate;
    let max_lanes = request.max_lane_count;
    let max_data = intel_dp_max_link_data_rate(link_caps, max_rate, max_lanes, framework);
    let link_bpp_x16 = intel_dp_output_format_link_bpp_x16(output, intel_dp_min_bpp(output));
    let mode_rate = intel_dp_link_required(
        max_rate,
        max_lanes,
        request.target_clock,
        request.mode.hdisplay,
        link_bpp_x16,
        0,
        framework,
    );
    let mut status = ModeStatus::ClockHigh;
    for joined in ops.joiner_candidates(request.mode) {
        let mut slices = 0;
        status = ops.pfit_mode_valid(request.mode, output, joined);
        if status != ModeStatus::Ok {
            continue;
        }
        let mut dsc = false;
        if request.dsc_supported {
            slices = intel_dp_dsc_get_slice_count(
                dsc_caps,
                request.target_clock,
                request.mode.hdisplay,
                joined,
                request.is_edp,
                request.max_cdclk,
                framework,
            );
            let pipe_bpp = ops.dsc_max_pipe_bpp();
            if request.is_edp {
                let compressed =
                    (u16::from(dsc_caps.dpcd[7]) | (u16::from(dsc_caps.dpcd[8] & 0x03) << 8)) >> 4;
                dsc = compressed != 0 && slices != 0;
            } else if request.fec_supported {
                let overhead_flags = if intel_dp_is_uhbr(request.max_link_rate) {
                    0
                } else {
                    BW_OVERHEAD_FEC
                };
                dsc = intel_dp_mode_valid_with_dsc(
                    request.display_ver,
                    dsc_caps,
                    link_caps,
                    request.max_link_rate,
                    request.max_lane_count,
                    request.mode,
                    joined,
                    output,
                    pipe_bpp,
                    overhead_flags,
                    request.max_cdclk,
                    request.dsc_bpp_step_x16,
                    request.force_dsc,
                    framework,
                );
            }
        }
        if ops.joiner_needs_dsc(joined) && !dsc {
            status = ModeStatus::ClockHigh;
            continue;
        }
        if mode_rate > max_data && !dsc {
            status = ModeStatus::ClockHigh;
            continue;
        }
        status = ops.max_plane_size_valid(request.mode, joined);
        if status != ModeStatus::Ok {
            continue;
        }
        if !dsc {
            slices = 0;
        }
        if !ops.dotclk_valid(request.mode, slices, joined) {
            status = ModeStatus::ClockHigh;
            continue;
        }
        break;
    }
    if status != ModeStatus::Ok {
        return status;
    }
    ops.downstream_mode_valid(request.target_clock, sink_format)
}

/// Validate a DP display mode's flags, panel adjustment, and 4:2:0 fallback path.
// upstream: intel_dp.c intel_dp_mode_valid()
pub fn intel_dp_mode_valid(
    mut request: DpModeRequest,
    dsc_caps: &DscSinkCaps,
    link_caps: &DpSinkCaps,
    framework: &impl DpFramework,
    ops: &impl DpModeValidationOps,
) -> ModeStatus {
    let status = ops.cpu_transcoder_mode_valid(request.mode);
    if status != ModeStatus::Ok {
        return status;
    }
    if request.dbldclk {
        return ModeStatus::HorizontalIllegal;
    }
    if request.mode.clock_khz < 10_000 {
        return ModeStatus::ClockLow;
    }
    if intel_dp_hdisplay_bad(request.has_ddi, request.mode.hdisplay) {
        return ModeStatus::HorizontalIllegal;
    }
    if request.is_edp {
        let status = ops.panel_mode_valid(request.mode, &mut request.target_clock);
        if status != ModeStatus::Ok {
            return status;
        }
    }
    if request.sink_420_only {
        intel_dp_mode_valid_format(
            request,
            OutputFormat::Ycbcr420,
            dsc_caps,
            link_caps,
            framework,
            ops,
        )
    } else {
        let status = intel_dp_mode_valid_format(
            request,
            OutputFormat::Rgb,
            dsc_caps,
            link_caps,
            framework,
            ops,
        );
        if status != ModeStatus::Ok && request.sink_420_also {
            intel_dp_mode_valid_format(
                request,
                OutputFormat::Ycbcr420,
                dsc_caps,
                link_caps,
                framework,
                ops,
            )
        } else {
            status
        }
    }
}

/// Test whether the source can join pipes and keep each pipe within its limit.
// upstream: intel_dp.c intel_dp_can_join()
pub fn intel_dp_can_join(
    _display_ver: u8,
    has_bigjoiner: bool,
    has_uncompressed_joiner: bool,
    has_ultrajoiner: bool,
    has_joiner: bool,
    num_joined_pipes: u8,
) -> bool {
    if num_joined_pipes > 1 && !has_joiner {
        return false;
    }
    match num_joined_pipes {
        1 => true,
        2 => has_bigjoiner || has_uncompressed_joiner,
        4 => has_ultrajoiner,
        _ => false,
    }
}

/// Validate max active width and connector-forced joiner count.
// upstream: intel_dp.c intel_dp_joiner_candidate_valid()
pub fn intel_dp_joiner_candidate_valid(
    can_join: bool,
    hdisplay: u32,
    joined_pipes: u8,
    max_hdisplay_per_pipe: u32,
    force_joined_pipes: u8,
) -> bool {
    can_join
        && hdisplay <= u32::from(joined_pipes) * max_hdisplay_per_pipe
        && (force_joined_pipes == 0 || force_joined_pipes == joined_pipes)
}

/// Calculate link min/max bpp after pipe and DSC constraints.
// upstream: intel_dp.c intel_dp_compute_config_link_bpp_limits()
pub fn intel_dp_compute_config_link_bpp_limits(
    policy: &DpConfigPolicy,
    caps: &DscSinkCaps,
    limits: &mut LinkConfigLimits,
) -> bool {
    let mut max_link_bpp_x16 = min(policy.max_link_bpp_x16, u16::from(limits.max_pipe_bpp) * 16);
    if !policy.dsc {
        max_link_bpp_x16 = max_link_bpp_x16 / (16 * 6) * (16 * 6);
        if max_link_bpp_x16 < u16::from(limits.min_pipe_bpp) * 16 {
            return false;
        }
        limits.min_link_bpp_x16 = i32::from(limits.min_pipe_bpp) * 16;
    } else {
        limits.min_link_bpp_x16 = i32::from(intel_dp_compute_min_compressed_bpp_x16(
            policy.display_ver,
            policy.output,
            policy.dsc_bpp_step_x16,
        ));
        max_link_bpp_x16 = compute_max_compressed_bpp_x16(
            policy.display_ver,
            caps,
            policy.mode.clock_khz,
            policy.mode.hdisplay,
            policy.joined_pipes,
            policy.output,
            limits.max_pipe_bpp,
            max_link_bpp_x16,
            policy.max_cdclk,
            policy.force_dsc,
            policy.dsc_bpp_step_x16,
        );
    }
    limits.max_link_bpp_x16 = i32::from(max_link_bpp_x16);
    limits.min_link_bpp_x16 > 0 && limits.min_link_bpp_x16 <= limits.max_link_bpp_x16
}

/// Compute DP rate/lane/pipe/DSC intervals before selecting a specific link.
// upstream: intel_dp.c intel_dp_compute_config_limits()
pub fn intel_dp_compute_config_limits(
    link: &DpLinkState,
    caps: &DscSinkCaps,
    policy: &DpConfigPolicy,
    hdr_dsc_supported: bool,
) -> Option<LinkConfigLimits> {
    let max_rate = intel_dp_max_link_rate(link);
    let mut limits = LinkConfigLimits {
        min_rate: min(intel_dp_min_link_rate(link), max_rate),
        max_rate,
        min_lane_count: intel_dp_min_lane_count(link),
        max_lane_count: intel_dp_max_lane_count(link),
        min_pipe_bpp: intel_dp_min_bpp(policy.output),
        max_pipe_bpp: if policy.mst {
            min(policy.connector_max_pipe_bpp, 24)
        } else {
            policy.connector_max_pipe_bpp
        },
        min_link_bpp_x16: 0,
        max_link_bpp_x16: 0,
    };
    if !policy.dsc && policy.hdr {
        if hdr_dsc_supported && limits.max_pipe_bpp >= 30 {
            limits.min_pipe_bpp = max(limits.min_pipe_bpp, 30);
        }
    }
    if limits.min_pipe_bpp == 0 || limits.min_pipe_bpp > limits.max_pipe_bpp {
        return None;
    }
    if policy.dsc && !intel_dp_dsc_compute_pipe_bpp_limits(caps, policy.display_ver, &mut limits) {
        return None;
    }
    limits.max_pipe_bpp = policy
        .requested_max_pipe_bpp
        .clamp(limits.min_pipe_bpp, limits.max_pipe_bpp);
    if policy.dsc {
        limits.max_pipe_bpp = align_max_sink_dsc_input_bpp(caps, limits.max_pipe_bpp);
    }
    if policy.mst || policy.use_max_params {
        limits.min_lane_count = limits.max_lane_count;
        limits.min_rate = limits.max_rate;
    }
    if !intel_dp_compute_config_link_bpp_limits(policy, caps, &mut limits) {
        return None;
    }
    Some(limits)
}

/// Compute the Display-12/13 link tuple for one pipe-joiner candidate.
// upstream: intel_dp.c intel_dp_compute_link_for_joined_pipes()
pub fn intel_dp_compute_link_for_joined_pipes(
    link: &DpLinkState,
    link_caps: &DpSinkCaps,
    dsc_caps: &DscSinkCaps,
    policy: &DpConfigPolicy,
    output: OutputFormat,
    supports_dsc: bool,
    fec_supported: bool,
    has_uncompressed_joiner: bool,
    framework: &impl DpFramework,
) -> Result<SelectedLink, DpError> {
    let joined = policy.joined_pipes.max(1);
    let joiner_needs_dsc = intel_dp_joiner_needs_dsc(has_uncompressed_joiner, joined);
    let no_dsc_limits = intel_dp_compute_config_limits(link, dsc_caps, policy, false);
    let mut dsc_needed = joiner_needs_dsc || policy.force_dsc;
    if !dsc_needed {
        if let Some(limits) = no_dsc_limits {
            match intel_dp_compute_link_config_wide(
                link,
                limits,
                policy.mode,
                output,
                |rate, lanes| intel_dp_max_link_data_rate(link_caps, rate, lanes, framework),
                0,
                framework,
            ) {
                Ok(config)
                    if (!intel_dp_is_uhbr(config.rate)
                        || framework.mtp_tu_fits(
                            config.rate,
                            config.lane_count,
                            policy.mode,
                            u16::from(config.pipe_bpp) * 16,
                        ))
                        && intel_dp_dotclk_valid(
                            policy.max_dotclk,
                            framework.max_uncompressed_dotclock(),
                            policy.mode.clock_khz,
                            policy.mode.htotal,
                            0,
                            joined,
                            |_, _, _| 0,
                        ) =>
                {
                    return Ok(config);
                }
                Err(_) | Ok(_) => dsc_needed = true,
            }
        } else {
            dsc_needed = true;
        }
    }
    if !dsc_needed || !supports_dsc || !fec_supported {
        return Err(DpError::NoLinkConfig);
    }
    let mut dsc_policy = *policy;
    dsc_policy.dsc = true;
    let limits = intel_dp_compute_config_limits(link, dsc_caps, &dsc_policy, true)
        .ok_or(DpError::NoLinkConfig)?;
    let min_bpp_x16 = intel_dp_compute_min_compressed_bpp_x16(
        policy.display_ver,
        output,
        policy.dsc_bpp_step_x16,
    );
    let max_bpp_x16 = compute_max_compressed_bpp_x16(
        policy.display_ver,
        dsc_caps,
        policy.mode.clock_khz,
        policy.mode.hdisplay,
        joined,
        output,
        limits.max_pipe_bpp,
        if limits.max_link_bpp_x16 > 0 {
            limits.max_link_bpp_x16 as u16
        } else {
            u16::from(limits.max_pipe_bpp) * 16
        },
        policy.max_cdclk,
        policy.force_dsc,
        policy.dsc_bpp_step_x16,
    );
    let slices = intel_dp_dsc_get_slice_count(
        dsc_caps,
        policy.mode.clock_khz,
        policy.mode.hdisplay,
        joined,
        false,
        policy.max_cdclk,
        framework,
    );
    let fec = !intel_dp_is_uhbr(intel_dp_max_link_rate(link));
    let dsc = dsc_compute_compressed_bpp(
        policy.display_ver,
        link,
        limits,
        policy.mode,
        output,
        limits.max_pipe_bpp,
        max_bpp_x16,
        min_bpp_x16,
        policy.dsc_bpp_step_x16.max(1),
        joined,
        false,
        fec,
        policy.force_dsc_fractional_bpp,
        slices,
        link_caps,
        framework,
    )?;
    if slices == 0
        || !intel_dp_dotclk_valid(
            policy.max_dotclk,
            framework.max_uncompressed_dotclock(),
            policy.mode.clock_khz,
            policy.mode.htotal,
            slices,
            joined,
            |clock, htotal, slice_count| {
                framework.dsc_pixel_rate_with_bubbles(clock, htotal, slice_count)
            },
        )
    {
        return Err(DpError::NoLinkConfig);
    }
    Ok(dsc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_dprx_lane_count_ignores_non_lane_capability_bits() {
        let mut caps = DpSinkCaps::default();
        caps.dpcd = alloc::vec![0; 16];
        caps.dpcd[2] = 4 | 0x80 | 0x20;
        assert_eq!(max_dprx_lane_count(&caps), 4);
        caps.dpcd[2] = 0x03;
        assert_eq!(max_dprx_lane_count(&caps), 3);
    }

    #[test]
    fn source_rate_selection_distinguishes_platform_and_edp() {
        assert_eq!(
            intel_dp_source_max_rate(
                DisplayInfo {
                    display_ver: 12,
                    ..DisplayInfo::default()
                },
                false,
            ),
            540_000
        );
        assert_eq!(
            intel_dp_source_max_rate(
                DisplayInfo {
                    display_ver: 13,
                    platform_alderlake: true,
                    ..DisplayInfo::default()
                },
                true,
            ),
            810_000
        );
        assert_eq!(
            intel_dp_source_max_rate(
                DisplayInfo {
                    display_ver: 12,
                    platform_ehl_jsl: true,
                    ..DisplayInfo::default()
                },
                true,
            ),
            540_000
        );
    }

    #[test]
    fn symbol_clock_uses_dpcd_rate_coding() {
        assert_eq!(intel_dp_link_symbol_size(810_000), 10);
        assert_eq!(intel_dp_link_symbol_size(1_000_000), 32);
        assert_eq!(intel_dp_link_symbol_clock(810_000), 810_000);
        assert_eq!(intel_dp_link_symbol_clock(1_000_000), 312_500);
    }

    #[test]
    fn conservative_sink_fallback_is_single_rbr_rate() {
        let mut link = DpLinkState::default();
        link.sink_rates.extend_from_slice(&[270_000, 540_000]);
        intel_dp_set_default_sink_rates(&mut link);
        assert_eq!(link.sink_rates, [162_000]);
    }
}

/// Iterate joiner candidates, retaining only a fully viable link configuration.
// upstream: intel_dp.c intel_dp_compute_link_config()
pub fn intel_dp_compute_link_config(
    link: &DpLinkState,
    link_caps: &DpSinkCaps,
    dsc_caps: &DscSinkCaps,
    mut policy: DpConfigPolicy,
    candidates: &[u8],
    fec_requested: bool,
    supports_fec: bool,
    supports_dsc: bool,
    has_uncompressed_joiner: bool,
    framework: &impl DpFramework,
) -> Result<SelectedLink, DpError> {
    if fec_requested && !supports_fec {
        return Err(DpError::Invalid);
    }
    let mut last_error = DpError::NoLinkConfig;
    for &num_joined_pipes in candidates {
        policy.joined_pipes = num_joined_pipes;
        match intel_dp_compute_link_for_joined_pipes(
            link,
            link_caps,
            dsc_caps,
            &policy,
            policy.output,
            supports_dsc,
            supports_fec,
            has_uncompressed_joiner,
            framework,
        ) {
            Ok(config) => return Ok(config),
            Err(DpError::Deadlock) => return Err(DpError::Deadlock),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}
