/*
 * Copyright © 2008 Intel Corporation
 *             2014 Red Hat Inc.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice (including the next
 * paragraph) shall be included in all copies or substantial portions of the
 * Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
 * IN THE SOFTWARE.
 */
// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dp_mst.c.
// DP payload arithmetic, MST link/atomic policy, and sequencing live here.
// DRM object/topology storage, sideband transport, AUX, and MMIO are explicit
// backend interfaces; this file does not pretend to perform hardware I/O.

#![allow(dead_code, clippy::too_many_arguments)]

use alloc::vec;
use alloc::vec::Vec;

pub const DP_MST_SLOTS: u8 = 64;
pub const DP_CAP_ANSI_8B10B: u8 = 0;
pub const DP_CAP_ANSI_128B132B: u8 = 1;
pub const DRM_DP_BW_OVERHEAD_MST: u32 = 1 << 0;
pub const DRM_DP_BW_OVERHEAD_SSC_REF_CLK: u32 = 1 << 1;
pub const DRM_DP_BW_OVERHEAD_FEC: u32 = 1 << 2;
pub const EINVAL: i32 = -22;
pub const ENOSPC: i32 = -28;
pub const EAGAIN: i32 = -11;
pub const EDEADLK: i32 = -35;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayMode {
    pub clock_khz: u32,
    pub crtc_clock_khz: u32,
    pub hdisplay: u32,
    pub htotal: u32,
    pub doublescan: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkMN {
    pub data_m: u64,
    pub data_n: u64,
    pub tu: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub mode: DisplayMode,
    pub port_clock_khz: u32,
    pub lane_count: u8,
    pub fec_enable: bool,
    pub dsc_enable: bool,
    pub is_mst: bool,
    pub dsc_bpp_x16: u32,
    pub pipe_bpp: u8,
    pub output_bpp_x16: u32,
    pub joined_pipe_count: u8,
    pub cpu_transcoder: u8,
    pub pipe: u8,
    pub active: bool,
    pub mst_master_transcoder: u8,
    pub mst_slave_transcoder: bool,
    pub dp_m_n: LinkMN,
    pub mode_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkLimits {
    pub min_rate_khz: u32,
    pub max_rate_khz: u32,
    pub max_lane_count: u8,
    pub min_bpp_x16: u32,
    pub max_bpp_x16: u32,
    pub max_pipe_bpp: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PbnDiv {
    /// Fixed-point 20.12 value in the kernel. Integer units are retained here.
    pub full: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MstTopologyState {
    pub pbn_div: PbnDiv,
    pub total_avail_slots: i32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MstLink {
    pub active_streams: u32,
    pub probed_rate_khz: u32,
    pub probed_lane_count: u8,
    pub display_version: u8,
    pub dpcd_128b132b: bool,
    pub force_dsc: bool,
    pub force_dsc_bpc: bool,
    pub dsc_hblank_expansion_quirk: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MstConnector {
    pub port_id: u32,
    pub pipe: Option<u8>,
    pub transcoder: Option<u8>,
    pub is_mst: bool,
    pub has_dsc_hblank_quirk: bool,
    pub connected: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LinkConfig {
    pub pipe: CrtcState,
    pub limits: LinkLimits,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModeValidationCaps {
    pub unregistered: bool,
    pub transcoder_status: i32,
    pub double_clock: bool,
    pub min_link_bpp_x16: u32,
    pub max_link_clock_khz: u32,
    pub max_lanes: u8,
    pub max_link_data_rate: u64,
    pub port_full_pbn: u32,
    pub supports_dsc: bool,
    pub max_pipe_bpp: u8,
}

/// Narrow algorithm/hardware boundary. Implementations own connector objects,
/// atomic-state lookup, topology, AUX sideband messages, and register access.
pub trait MstBackend {
    fn display_version(&self) -> u8 { 0 }
    fn is_uhbr(&self, _crtc: &CrtcState) -> bool { false }
    fn link_symbol_clock_khz(&self, rate_khz: u32) -> u64 { rate_khz as u64 }
    fn bandwidth_overhead(&self, _rate: u32, _lanes: u8, _width: u32,
                          _slices: u32, _bpp_x16: u32, _flags: u32) -> i32 { 0 }
    fn compute_link_m_n(&self, bpp_x16: u32, lanes: u8, pixel_clock: u32,
                        port_clock: u32, overhead: i32) -> Option<LinkMN> {
        let numerator = (pixel_clock as u64).checked_mul(bpp_x16 as u64)?;
        let denominator = (port_clock as u64).checked_mul(lanes as u64)?.checked_mul(24 * 16)?;
        if denominator == 0 || overhead < 0 { return None; }
        let scale = 1u64 << 24;
        let data_m = numerator.checked_mul(scale)?.checked_div(denominator)?;
        Some(LinkMN { data_m, data_n: scale, tu: 0 })
    }
    fn effective_data_rate(&self, pixel_clock: u32, bpp_x16: u32, overhead: i32) -> i32 {
        if overhead < 0 { return 0; }
        let raw = (pixel_clock as u64).saturating_mul(bpp_x16 as u64) / 16;
        let scaled = raw.saturating_mul(1000 + overhead as u64) / 1000;
        scaled.min(i32::MAX as u64) as i32
    }
    fn dsc_slice_count(&self, _port_id: u32, _mode_clock: u32,
                       _hdisplay: u32, _joined_pipes: u8) -> u32 { 0 }
    fn dsc_supported(&self, _port_id: u32, _crtc: &CrtcState) -> bool { false }
    fn dsc_bpp_valid(&self, _bpp_x16: u32) -> bool { true }
    fn dsc_bpp_step_x16(&self, _port_id: u32) -> u32 { 16 }
    fn needs_8b10b_fec(&self, _crtc: &CrtcState, _dsc: bool) -> bool { false }
    fn fec_supported(&self, _port_id: u32, _crtc: &CrtcState) -> bool { true }
    fn output_link_bpp_x16(&self, _crtc: &CrtcState, pipe_bpp: u32) -> u32 { pipe_bpp * 16 }
    fn atomic_topology_state(&mut self) -> Result<MstTopologyState, i32> { Err(EINVAL) }
    fn get_vc_payload_bw(&self, _port_clock: u32, lanes: u8) -> u64 {
        lanes as u64
    }
    fn update_slots(&mut self, _link_coding_cap: u8) {}
    fn find_time_slots(&mut self, _port_id: u32, _pbn: u32) -> i32 { ENOSPC }
    fn pipe_dsc_config(&mut self, _crtc: &mut CrtcState, _limits: LinkLimits,
                       _tu: u32) -> i32 { EINVAL }
    fn compute_min_hblank(&mut self, _crtc: &mut CrtcState, _port_id: u32) -> i32 { 0 }
    fn dotclk_valid(&self, _mode: DisplayMode, _slices: u32, _joined: u8) -> bool { true }
    fn compute_config_limits(&mut self, _crtc: &CrtcState, _port_id: u32,
                             _dsc: bool) -> Option<LinkLimits> { None }
    fn is_uhbr_rate(&self, rate: u32) -> bool { rate >= 10_000_000 }
    fn transcoder_mask(&self) -> u32 { 0 }
    fn pipes_downstream(&self, _port_id: u32) -> u8 { 0 }
    fn dsc_pipes(&self, _pipe_mask: u8) -> u8 { 0 }
    fn force_modeset_for_pipes(&mut self, _pipe_mask: u8) -> i32 { 0 }
    fn check_manager_bandwidth(&mut self, _port_id: u32) -> i32 { 0 }
    fn reduce_bpp_for_pipes(&mut self, _pipe_mask: u8) -> i32 { 0 }
    fn stream_count_for_port(&self, _port_id: u32) -> usize { 0 }
    fn primary_encoder(&self) -> u32 { 0 }
    fn primary_dp(&self) -> u32 { 0 }
    fn topology_check(&mut self, _port_id: u32) -> i32 { 0 }
    fn connector_atomic_check(&mut self, _port_id: u32) -> i32 { 0 }
    fn tunnel_atomic_check(&mut self, _port_id: u32) -> i32 { 0 }
    fn release_time_slots(&mut self, _port_id: u32) -> i32 { 0 }
    /// Executes one named i915 display action (e.g. "remove_payload_part1").
    /// The implementation must map these to the correct DRM/sideband/MMIO API.
    fn action(&mut self, _name: &'static str, _port_id: u32, _crtc: &CrtcState) -> i32 { 0 }
    fn probe_topology(&mut self, _port_id: u32) {}
    fn connector_modes(&mut self, _port_id: u32) -> i32 { 0 }
    fn connector_mode_valid(&self, _port_id: u32, _mode: DisplayMode) -> i32 { 0 }
    fn connector_detect(&mut self, _port_id: u32) -> bool { false }
    fn connector_properties(&mut self, _port_id: u32) -> i32 { 0 }
    fn read_dsc_caps(&mut self, _port_id: u32) -> i32 { 0 }
    fn add_topology_connector(&mut self, _port_id: u32) -> i32 { 0 }
    fn poll_hpd_irq(&mut self, _port_id: u32) {}
    fn create_stream_encoder(&mut self, _pipe: u8) -> i32 { 0 }
    fn cleanup_encoders(&mut self) {}
    fn init_topology_manager(&mut self) -> i32 { 0 }
    fn source_supports_mst(&self) -> bool { false }
    fn connector_state_for_crtc(&self, _pipe: u8) -> Option<u32> { None }
    fn add_topology_state(&mut self, _connector: u32) -> i32 { 0 }
    fn crtc_needs_modeset(&self, _pipe: u8) -> bool { false }
    fn prepare_probe(&mut self) {}
    fn verify_dpcd_state(&mut self) -> bool { true }
    fn force_dsc(&self) -> bool { false }
    fn force_dsc_bpc(&self) -> bool { false }
    fn joiner_needs_dsc(&self, joined: u8) -> bool { joined > 1 }
    fn candidate_joined_pipes(&self, _port_id: u32, _mode: DisplayMode) -> Vec<u8> { vec![1] }
    fn tunnel_atomic_compute_stream_bw(&mut self, _port_id: u32, _crtc: &CrtcState) -> i32 { 0 }
    fn limited_color_range(&self, _port_id: u32, _crtc: &CrtcState) -> bool { false }
    fn connector_needs_modeset(&self, _port_id: u32) -> bool { false }
    fn topology_has_active_crtcs(&self, _port_id: u32) -> Vec<u8> { Vec::new() }
    fn connector_register(&mut self, _port_id: u32) -> i32 { 0 }
    fn connector_unregister(&mut self, _port_id: u32) {}
    fn connector_late_register(&mut self, _port_id: u32) -> i32 { 0 }
    fn connector_early_unregister(&mut self, _port_id: u32) {}
    fn connector_is_unregistered(&self, _port_id: u32) -> bool { false }
    fn display_access_allowed(&self) -> bool { true }
    fn connector_atomic_best_encoder(&self, _port_id: u32) -> Option<u32> { None }
    fn connector_hw_state(&self, _port_id: u32) -> bool { false }
    fn mst_transcoder_mask(&self, _port_id: u32) -> u32 { 0 }
    fn register_encoder(&mut self, _pipe: u8) -> i32 { 0 }
    fn source_mst_supported(&self) -> bool { false }
    fn connector_modeset_required_for_crtc(&self, _pipe: u8) -> bool { false }
    fn dsc_wa_bits(&self, _crtc: &CrtcState) -> (u32, u32) { (0, 0) }
    fn mode_validation_caps(&self, _port_id: u32, _mode: DisplayMode) -> ModeValidationCaps { ModeValidationCaps::default() }
    fn mode_required_rate(&self, _mode: DisplayMode, _min_bpp_x16: u32, _fec: bool) -> u64 { 0 }
    fn mode_required_pbn(&self, _mode: DisplayMode, _min_bpp_x16: u32) -> u32 { 0 }
    fn mode_dsc_valid(&self, _port_id: u32, _mode: DisplayMode, _joined: u8,
                      _pipe_bpp: u8, _fec: bool) -> bool { false }
    fn mode_plane_status(&self, _mode: DisplayMode, _joined: u8) -> i32 { 0 }
    fn lock_modeset(&mut self, _port_id: u32) -> i32 { 0 }
    fn mode_dsc_slice_count(&self, _port_id: u32, _mode: DisplayMode, _joined: u8) -> u32 { 0 }
    fn connector_pipe(&self, _port_id: u32) -> Option<u8> { None }
    fn stream_pipe(&self, _port_id: u32) -> Option<u8> { None }
    fn connector_state_exists(&self, _port_id: u32) -> bool { true }
    fn get_primary_config(&mut self, _port_id: u32, _crtc: &mut CrtcState) {}
    fn initial_fastset_check(&self, _port_id: u32, _crtc: &CrtcState) -> bool { true }
    fn encoder_create(&mut self, _pipe: u8) -> i32 { 0 }
    fn encoder_count(&self) -> u8 { 0 }
    fn manager_init(&mut self, _conn_base_id: i32) -> i32 { 0 }
    fn is_edp(&self) -> bool { false }
    fn has_dp_mst(&self) -> bool { false }
    fn port_id_value(&self) -> u8 { 0 }
    fn display_pipe_count(&self) -> u8 { 0 }
    fn mst_mode_enabled(&self) -> bool { false }
    fn dpcd_mst_ctrl_matches(&mut self, _port_id: u32) -> bool { true }
    fn link_active(&self) -> bool { false }
    fn max_link_rate(&self) -> u32 { 0 }
    fn max_lane_count(&self) -> u8 { 0 }
    fn mst_manager_ids(&self) -> Vec<u32> { Vec::new() }
    fn manager_port_for_state(&self, _mgr: u32) -> u32 { 0 }
    fn connector_count(&self) -> usize { 0 }
    fn connector_at(&self, _index: usize) -> Option<MstConnector> { None }
    fn connector_state_present(&self, _port_id: u32) -> bool { true }
    fn connector_state_pipe(&self, _port_id: u32) -> Option<u8> { None }
    fn crtc_active(&self, _pipe: u8) -> bool { false }
    fn add_affected_planes(&mut self, _pipe: u8) -> i32 { 0 }
    fn release_mgr_slots(&mut self, _port_id: u32) -> i32 { 0 }
    fn check_mgr_bandwidth(&mut self, _mgr: u32) -> i32 { 0 }
    fn reduce_bpp(&mut self, _pipe_mask: u8) -> i32 { 0 }
    fn link_dsc_pipe_mask(&mut self, _mgr: u32) -> u8 { 0 }
}

fn div_round_up_u64(n: u64, d: u64) -> u64 {
    if d == 0 { return u64::MAX; }
    n / d + u64::from(n % d != 0)
}

// upstream: intel_dp_mst.c to_primary_encoder()
pub fn to_primary_encoder<B: MstBackend>(backend: &B) -> u32 { backend.primary_encoder() }
// upstream: intel_dp_mst.c to_primary_dp()
pub fn to_primary_dp<B: MstBackend>(backend: &B) -> u32 { backend.primary_dp() }

// upstream: intel_dp_mst.c intel_dp_mst_active_streams()
pub fn intel_dp_mst_active_streams(link: &MstLink) -> u32 { link.active_streams }

// upstream: intel_dp_mst.c intel_dp_mst_dec_active_streams()
pub fn intel_dp_mst_dec_active_streams(link: &mut MstLink) -> bool {
    if link.active_streams == 0 { return true; }
    link.active_streams -= 1;
    link.active_streams == 0
}

// upstream: intel_dp_mst.c intel_dp_mst_inc_active_streams()
pub fn intel_dp_mst_inc_active_streams(link: &mut MstLink) -> bool {
    let first = link.active_streams == 0;
    link.active_streams = link.active_streams.saturating_add(1);
    first
}

// upstream: intel_dp_mst.c intel_dp_mst_max_dpt_bpp()
pub fn intel_dp_mst_max_dpt_bpp<B: MstBackend>(backend: &B, crtc: &CrtcState,
                                                dsc: bool) -> u32 {
    if !backend.is_uhbr(crtc) || backend.display_version() >= 20 || !dsc { return 0; }
    let symbol = backend.link_symbol_clock_khz(crtc.port_clock_khz);
    let numerator = symbol.saturating_mul(72).saturating_mul(128);
    let denominator = (crtc.mode.crtc_clock_khz as u64).saturating_mul(103);
    if denominator == 0 { return 0; }
    (numerator / denominator).min(u32::MAX as u64) as u32
}

// upstream: intel_dp_mst.c intel_dp_mst_bw_overhead()
pub fn intel_dp_mst_bw_overhead<B: MstBackend>(backend: &B, crtc: &CrtcState,
                                                ssc: bool, slices: u32,
                                                bpp_x16: u32) -> i32 {
    let mut flags = DRM_DP_BW_OVERHEAD_MST;
    if ssc { flags |= DRM_DP_BW_OVERHEAD_SSC_REF_CLK; }
    if crtc.fec_enable { flags |= DRM_DP_BW_OVERHEAD_FEC; }
    backend.bandwidth_overhead(crtc.port_clock_khz, crtc.lane_count,
                               crtc.mode.hdisplay, slices, bpp_x16, flags)
}

// upstream: intel_dp_mst.c intel_dp_mst_compute_m_n()
pub fn intel_dp_mst_compute_m_n<B: MstBackend>(backend: &B, crtc: &CrtcState,
                                                overhead: i32, bpp_x16: u32) -> Option<LinkMN> {
    let mut mn = backend.compute_link_m_n(bpp_x16, crtc.lane_count,
                                         crtc.mode.crtc_clock_khz,
                                         crtc.port_clock_khz, overhead)?;
    // TU = ceil(data_m * 64 / data_n), identical to DIV_ROUND_UP_ULL.
    mn.tu = div_round_up_u64(mn.data_m.saturating_mul(64), mn.data_n)
        .min(u32::MAX as u64) as u32;
    Some(mn)
}

// upstream: intel_dp_mst.c intel_dp_mst_calc_pbn()
pub fn intel_dp_mst_calc_pbn<B: MstBackend>(backend: &B, pixel_clock: u32,
                                            bpp_x16: u32, overhead: i32) -> u32 {
    let rate = backend.effective_data_rate(pixel_clock, bpp_x16, overhead).max(0) as u64;
    div_round_up_u64(rate.saturating_mul(64), 54 * 1000).min(u32::MAX as u64) as u32
}

// upstream: intel_dp_mst.c intel_dp_mst_dsc_get_slice_count()
pub fn intel_dp_mst_dsc_get_slice_count<B: MstBackend>(backend: &B, port: u32,
                                                       crtc: &CrtcState) -> u32 {
    backend.dsc_slice_count(port, crtc.mode.clock_khz, crtc.mode.hdisplay,
                            crtc.joined_pipe_count.max(1))
}

// upstream: intel_dp_mst.c mst_stream_update_slots()
pub fn mst_stream_update_slots<B: MstBackend>(backend: &mut B, crtc: &CrtcState) {
    backend.update_slots(if backend.is_uhbr(crtc) { DP_CAP_ANSI_128B132B }
                         else { DP_CAP_ANSI_8B10B });
}

/// Compute MST/SST transport-unit configuration, trying candidate bpp from
/// highest to lowest and reserving a time-slot payload only after its arithmetic
/// is valid. The topology table and atomic payload reservation stay in the DRM
/// backend; all candidate ordering/PBN/TU invariants are local Rust policy.
// upstream: intel_dp_mst.c intel_dp_mtp_tu_compute_config()
pub fn intel_dp_mtp_tu_compute_config<B: MstBackend>(backend: &mut B, _link: &MstLink,
        port: u32, crtc: &mut CrtcState, min_bpp_x16: u32, mut max_bpp_x16: u32,
        mut step_x16: u32, dsc: bool) -> i32 {
    let is_mst = crtc.is_mst;
    if step_x16 == 0 {
        if min_bpp_x16 != max_bpp_x16 { return EINVAL; }
        step_x16 = 1;
    }
    if min_bpp_x16 > max_bpp_x16 || step_x16 == 0 { return EINVAL; }
    let topology = if is_mst {
        match backend.atomic_topology_state() { Ok(mut s) => {
            s.pbn_div.full = backend.get_vc_payload_bw(crtc.port_clock_khz, crtc.lane_count);
            s
        }, Err(e) => return e }
    } else { MstTopologyState::default() };
    if is_mst {
        if topology.pbn_div.full == 0 { return EINVAL; }
        mst_stream_update_slots(backend, crtc);
    }

    crtc.fec_enable = backend.needs_8b10b_fec(crtc, dsc);
    if crtc.fec_enable && dsc && !backend.fec_supported(port, crtc) { return EINVAL; }

    let max_dpt_x16 = intel_dp_mst_max_dpt_bpp(backend, crtc, dsc).saturating_mul(16);
    if max_dpt_x16 != 0 { max_bpp_x16 = max_bpp_x16.min(max_dpt_x16); }
    if dsc && intel_dp_mst_dsc_get_slice_count(backend, port, crtc) == 0 { return ENOSPC; }

    let slices = if dsc { intel_dp_mst_dsc_get_slice_count(backend, port, crtc) } else { 0 };
    let mut bpp = max_bpp_x16;
    let mut last_error = ENOSPC;
    loop {
        if dsc && !backend.dsc_bpp_valid(bpp) {
            // SST validates its single compressed bpp before this point.
            if !is_mst { return EINVAL; }
        } else {
            let link_bpp = if dsc { bpp } else { backend.output_link_bpp_x16(crtc, bpp / 16) };
            let local_overhead = intel_dp_mst_bw_overhead(backend, crtc, false, slices, link_bpp);
            let mut mn = match intel_dp_mst_compute_m_n(backend, crtc, local_overhead, link_bpp) {
                Some(v) => v, None => return EINVAL,
            };
            let slots = if is_mst {
                let remote_overhead = intel_dp_mst_bw_overhead(backend, crtc, true, slices, link_bpp);
                let pbn = intel_dp_mst_calc_pbn(backend, crtc.mode.crtc_clock_khz,
                                                 link_bpp, remote_overhead);
                let mut remote_tu = div_round_up_u64(pbn as u64, topology.pbn_div.full);
                let alignment = 4u64 / crtc.lane_count.max(1) as u64;
                remote_tu = div_round_up_u64(remote_tu, alignment) * alignment;
                let aligned_pbn = remote_tu.saturating_mul(topology.pbn_div.full);
                if remote_tu < mn.tu as u64 { return EINVAL; }
                mn.tu = remote_tu.min(u32::MAX as u64) as u32;
                let allocated = backend.find_time_slots(port, aligned_pbn.min(u32::MAX as u64) as u32);
                if allocated > topology.total_avail_slots { EINVAL } else { allocated }
            } else {
                let alignment = 4u32 / crtc.lane_count.max(1) as u32;
                mn.tu = div_round_up_u64(mn.tu as u64, alignment as u64)
                    .saturating_mul(alignment as u64).min(u32::MAX as u64) as u32;
                if mn.tu <= DP_MST_SLOTS as u32 { mn.tu as i32 } else { EINVAL }
            };
            last_error = slots;
            if last_error == EDEADLK { return last_error; }
            if last_error >= 0 {
                if last_error as u32 != mn.tu { return EINVAL; }
                crtc.dp_m_n = mn;
                if dsc { crtc.dsc_bpp_x16 = bpp; }
                else { crtc.pipe_bpp = (bpp / 16).min(u8::MAX as u32) as u8; }
                crtc.output_bpp_x16 = link_bpp;
                return 0;
            }
        }
        if bpp <= min_bpp_x16 || bpp - min_bpp_x16 < step_x16 { break; }
        bpp -= step_x16;
    }
    last_error
}

// upstream: intel_dp_mst.c mst_stream_compute_link_config()
pub fn mst_stream_compute_link_config<B: MstBackend>(backend: &mut B, link: &MstLink,
        port: u32, crtc: &mut CrtcState, limits: LinkLimits) -> i32 {
    crtc.lane_count = limits.max_lane_count;
    crtc.port_clock_khz = limits.max_rate_khz;
    // FIXME retained from upstream: YUV420 link bpp is not yet accounted for.
    intel_dp_mtp_tu_compute_config(backend, link, port, crtc, limits.min_bpp_x16,
                                   limits.max_bpp_x16, 2 * 3 * 16, false)
}

// upstream: intel_dp_mst.c mst_stream_dsc_compute_link_config()
pub fn mst_stream_dsc_compute_link_config<B: MstBackend>(backend: &mut B, link: &MstLink,
        port: u32, crtc: &mut CrtcState, limits: LinkLimits) -> i32 {
    crtc.pipe_bpp = limits.max_pipe_bpp;
    crtc.lane_count = limits.max_lane_count;
    crtc.port_clock_khz = limits.max_rate_khz;
    let step = backend.dsc_bpp_step_x16(port);
    intel_dp_mtp_tu_compute_config(backend, link, port, crtc, limits.min_bpp_x16,
                                   limits.max_bpp_x16, step, true)
}

// upstream: intel_dp_mst.c mode_hblank_period_ns()
pub fn mode_hblank_period_ns(mode: DisplayMode) -> u32 {
    if mode.crtc_clock_khz == 0 || mode.htotal < mode.hdisplay { return u32::MAX; }
    let hblank = (mode.htotal - mode.hdisplay) as u64;
    div_round_up_u64(hblank.saturating_mul(1_000_000), mode.crtc_clock_khz as u64)
        .min(u32::MAX as u64) as u32
}

// upstream: intel_dp_mst.c hblank_expansion_quirk_needs_dsc()
pub fn hblank_expansion_quirk_needs_dsc<B: MstBackend>(backend: &B, link: &MstLink,
        connector: &MstConnector, crtc: &CrtcState, limits: LinkLimits) -> bool {
    let uhbr_sink = link.dpcd_128b132b;
    let hblank_limit = if uhbr_sink { 500 } else { 300 };
    if !connector.has_dsc_hblank_quirk { return false; }
    if uhbr_sink && !backend.is_uhbr_rate(limits.max_rate_khz) { return false; }
    if mode_hblank_period_ns(crtc.mode) > hblank_limit { return false; }
    intel_dp_mst_dsc_get_slice_count(backend, connector.port_id, crtc) != 0
}

// upstream: intel_dp_mst.c adjust_limits_for_dsc_hblank_expansion_quirk()
pub fn adjust_limits_for_dsc_hblank_expansion_quirk<B: MstBackend>(backend: &B,
        link: &MstLink, connector: &MstConnector, crtc: &CrtcState,
        limits: &mut LinkLimits, dsc: bool) -> bool {
    if !hblank_expansion_quirk_needs_dsc(backend, link, connector, crtc, *limits) { return true; }
    if !dsc {
        if backend.dsc_supported(connector.port_id, crtc) { return false; }
        let min = 24 * 16;
        if limits.max_bpp_x16 < min { return false; }
        limits.min_bpp_x16 = min;
        return true;
    }
    let mut min_bpp = limits.min_bpp_x16;
    if limits.max_rate_khz < 540_000 { min_bpp = 13 * 16; }
    else if limits.max_rate_khz < 810_000 { min_bpp = 10 * 16; }
    if limits.min_bpp_x16 >= min_bpp { return true; }
    if limits.max_bpp_x16 < min_bpp { return false; }
    limits.min_bpp_x16 = min_bpp;
    true
}

// upstream: intel_dp_mst.c mst_stream_compute_config_limits()
pub fn mst_stream_compute_config_limits<B: MstBackend>(backend: &mut B, link: &MstLink,
        connector: &MstConnector, crtc: &CrtcState, dsc: bool) -> Option<LinkLimits> {
    let mut limits = backend.compute_config_limits(crtc, connector.port_id, dsc)?;
    if adjust_limits_for_dsc_hblank_expansion_quirk(backend, link, connector,
                                                     crtc, &mut limits, dsc) {
        Some(limits)
    } else { None }
}

// upstream: intel_dp_mst.c mst_stream_compute_link_for_joined_pipes()
pub fn mst_stream_compute_link_for_joined_pipes<B: MstBackend>(backend: &mut B,
        link: &MstLink, connector: &MstConnector, crtc: &mut CrtcState,
        joined: u8) -> i32 {
    crtc.dsc_enable = false;
    crtc.dsc_bpp_x16 = 0;
    let mut ret = 0;
    let mut limits = LinkLimits::default();
    let joiner_needs_dsc = backend.joiner_needs_dsc(joined);
    let mut dsc_needed = joiner_needs_dsc || link.force_dsc ||
        match mst_stream_compute_config_limits(backend, link, connector, crtc, false) {
            Some(v) => { limits = v; false }, None => true,
        };
    if !dsc_needed {
        ret = mst_stream_compute_link_config(backend, link, connector.port_id, crtc, limits);
        if ret == EDEADLK { return ret; }
        if ret != 0 || !backend.dotclk_valid(crtc.mode, 0, joined) { dsc_needed = true; }
    }
    if dsc_needed && !backend.dsc_supported(connector.port_id, crtc) { return EINVAL; }
    if dsc_needed {
        limits = match mst_stream_compute_config_limits(backend, link, connector, crtc, true) {
            Some(v) => v, None => return EINVAL,
        };
        if backend.force_dsc_bpc() { /* upstream warns; MST still uses the pipe bpc */ }
        ret = mst_stream_dsc_compute_link_config(backend, link, connector.port_id, crtc, limits);
        if ret < 0 { return ret; }
        ret = backend.pipe_dsc_config(crtc, limits, crtc.dp_m_n.tu);
        if ret != 0 { return ret; }
        crtc.dsc_enable = true;
        let slices = intel_dp_mst_dsc_get_slice_count(backend, connector.port_id, crtc);
        if !backend.dotclk_valid(crtc.mode, slices, joined) { return EINVAL; }
    }
    if ret != 0 { return ret; }
    backend.compute_min_hblank(crtc, connector.port_id)
}

// upstream: intel_dp_mst.c mst_stream_compute_config()
pub fn mst_stream_compute_config<B: MstBackend>(backend: &mut B, link: &MstLink,
        connector: &MstConnector, crtc: &mut CrtcState) -> i32 {
    if crtc.fec_enable && !backend.fec_supported(connector.port_id, crtc) { return EINVAL; }
    if crtc.mode.doublescan { return EINVAL; }
    crtc.output_bpp_x16 = if crtc.pipe_bpp == 0 { 8 * 16 } else { crtc.pipe_bpp as u32 * 16 };
    if backend.action("pfit_compute_config", connector.port_id, crtc) != 0 { return EINVAL; }
    if backend.action("pfit_compute_config", connector.port_id, crtc) != 0 { return EINVAL; }
    let candidates = backend.candidate_joined_pipes(connector.port_id, crtc.mode);
    let mut ret = EINVAL;
    let base_pipe = crtc.pipe;
    for joined in candidates {
        crtc.joined_pipe_count = joined;
        crtc.pipe = base_pipe;
        ret = mst_stream_compute_link_for_joined_pipes(backend, link, connector, crtc, joined);
        if ret == 0 || ret == EDEADLK { break; }
    }
    if ret != 0 { return ret; }
    let _limited_range = backend.limited_color_range(connector.port_id, crtc);
    for action in ["vrr_compute_config", "audio_compute_config", "min_voltage_level",
                   "psr_compute_config"] {
        let ret = backend.action(action, connector.port_id, crtc);
        if ret != 0 { return ret; }
    }
    backend.tunnel_atomic_compute_stream_bw(connector.port_id, crtc)
}

// upstream: intel_dp_mst.c intel_dp_mst_transcoder_mask()
pub fn intel_dp_mst_transcoder_mask<B: MstBackend>(backend: &B, active: &[CrtcState]) -> u32 {
    if backend.display_version() < 12 { return 0; }
    active.iter().filter(|s| s.active).fold(0u32, |mask, s| mask | (1u32 << (s.cpu_transcoder & 31)))
}

// upstream: intel_dp_mst.c get_pipes_downstream_of_mst_port()
pub fn get_pipes_downstream_of_mst_port<B: MstBackend>(backend: &B, port: u32) -> u8 {
    backend.pipes_downstream(port)
}

// upstream: intel_dp_mst.c intel_dp_mst_check_dsc_change()
pub fn intel_dp_mst_check_dsc_change<B: MstBackend>(backend: &mut B, port: u32,
        link_dsc_pipes: &mut u8) -> i32 {
    let mst_pipe_mask = backend.pipes_downstream(port);
    let dsc_pipe_mask = backend.dsc_pipes(mst_pipe_mask);
    if dsc_pipe_mask == 0 || mst_pipe_mask == dsc_pipe_mask { return 0; }
    *link_dsc_pipes |= mst_pipe_mask;
    let ret = backend.force_modeset_for_pipes(mst_pipe_mask);
    if ret != 0 { ret } else { EAGAIN }
}

// upstream: intel_dp_mst.c intel_dp_mst_check_bw()
pub fn intel_dp_mst_check_bw<B: MstBackend>(backend: &mut B, port: u32,
        link_bw_pipe_mask: &mut u8) -> i32 {
    let ret = backend.check_manager_bandwidth(port);
    if ret != ENOSPC { return ret; }
    let pipes = backend.pipes_downstream(port);
    *link_bw_pipe_mask |= pipes;
    let ret = backend.reduce_bpp(pipes);
    if ret != 0 { ret } else { EAGAIN }
}

// upstream: intel_dp_mst.c intel_dp_mst_atomic_check_link()
pub fn intel_dp_mst_atomic_check_link<B: MstBackend>(backend: &mut B,
        link_dsc_pipes: &mut u8, link_bw_pipe_mask: &mut u8) -> i32 {
    for mgr in backend.mst_manager_ids() {
        let port = backend.manager_port_for_state(mgr);
        let ret = intel_dp_mst_check_dsc_change(backend, port, link_dsc_pipes);
        if ret != 0 { return ret; }
        let ret = intel_dp_mst_check_bw(backend, port, link_bw_pipe_mask);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c mst_stream_compute_config_late()
pub fn mst_stream_compute_config_late<B: MstBackend>(backend: &B, crtc: &mut CrtcState) -> i32 {
    let mask = backend.transcoder_mask();
    if mask == 0 { return EINVAL; }
    crtc.mst_master_transcoder = mask.trailing_zeros() as u8;
    0
}

// upstream: intel_dp_mst.c mst_connector_atomic_topology_check()
pub fn mst_connector_atomic_topology_check<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    if !backend.connector_needs_modeset(connector.port_id) { return 0; }
    let mut affected = Vec::new();
    for pipe in backend.topology_has_active_crtcs(connector.port_id) {
        if backend.add_affected_planes(pipe) != 0 { return EINVAL; }
        affected.push(pipe);
    }
    for pipe in affected {
        let _ = backend.action("mark_mode_changed", connector.port_id,
                              &CrtcState { pipe, mode_changed: true, ..CrtcState::default() });
    }
    0
}

// upstream: intel_dp_mst.c mst_connector_atomic_check()
pub fn mst_connector_atomic_check<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    let mut ret = backend.connector_atomic_check(connector.port_id);
    if ret != 0 { return ret; }
    ret = mst_connector_atomic_topology_check(backend, connector);
    if ret != 0 { return ret; }
    if backend.connector_needs_modeset(connector.port_id) {
        ret = backend.tunnel_atomic_check(connector.port_id);
        if ret != 0 { return ret; }
    }
    backend.release_mgr_slots(connector.port_id)
}

// upstream: intel_dp_mst.c mst_stream_disable()
pub fn mst_stream_disable<B: MstBackend>(backend: &mut B, link: &mut MstLink,
        connector: &MstConnector, crtc: &CrtcState) -> i32 {
    let _last_stream = intel_dp_mst_active_streams(link) == 1;
    for action in ["hdcp_disable", "sink_disable_decompression"] {
        let ret = backend.action(action, connector.port_id, crtc);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c mst_stream_post_disable()
pub fn mst_stream_post_disable<B: MstBackend>(backend: &mut B, link: &mut MstLink,
        connector: &MstConnector, crtc: &CrtcState) -> i32 {
    let last = intel_dp_mst_dec_active_streams(link);
    if backend.display_version() >= 12 && last && !intel_dp_mst_is_master_trans(crtc) { return EINVAL; }
    // Keep the source's significant ordering: disable pipe, update VC payload,
    // send ACT, retire decompression/scaler state, power down the path, then clock.
    let mut actions: Vec<&'static str> = vec!["vblank_off_all_modeset_pipes", "disable_transcoder",
        "remove_payload_part1", "clear_act_sent", "clear_vc_payload_alloc", "wait_act_sent",
        "check_act_status", "remove_payload_part2", "vrr_transcoder_disable",
        "disable_transcoder_func", "disable_dsc_scalers", "power_down_mst_phy",
        "disable_infoframes"];
    if backend.display_version() < 12 || !last { actions.push("disable_transcoder_clock"); }
    if last { actions.push("primary_post_disable"); }
    for action in actions {
        let ret = backend.action(action, connector.port_id, crtc);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c mst_stream_post_pll_disable()
pub fn mst_stream_post_pll_disable<B: MstBackend>(backend: &mut B, link: &MstLink,
        port: u32, crtc: &CrtcState) -> i32 {
    if link.active_streams == 0 { backend.action("primary_post_pll_disable", port, crtc) }
    else { 0 }
}

// upstream: intel_dp_mst.c mst_stream_pre_pll_enable()
pub fn mst_stream_pre_pll_enable<B: MstBackend>(backend: &mut B, link: &MstLink,
        port: u32, crtc: &CrtcState) -> i32 {
    if link.active_streams == 0 { backend.action("primary_pre_pll_enable", port, crtc) }
    else { backend.action("update_active_dpll", port, crtc) }
}

// upstream: intel_dp_mst.c intel_mst_probed_link_params_valid()
pub fn intel_mst_probed_link_params_valid(link: &MstLink, rate: u32, lanes: u8) -> bool {
    link.probed_rate_khz == rate && link.probed_lane_count == lanes
}

// upstream: intel_dp_mst.c intel_mst_set_probed_link_params()
pub fn intel_mst_set_probed_link_params(link: &mut MstLink, rate: u32, lanes: u8) {
    link.probed_rate_khz = rate;
    link.probed_lane_count = lanes;
}

// upstream: intel_dp_mst.c intel_mst_reprobe_topology()
pub fn intel_mst_reprobe_topology<B: MstBackend>(backend: &mut B, link: &mut MstLink,
        port: u32, crtc: &CrtcState) {
    if intel_mst_probed_link_params_valid(link, crtc.port_clock_khz, crtc.lane_count) { return; }
    backend.probe_topology(port);
    intel_mst_set_probed_link_params(link, crtc.port_clock_khz, crtc.lane_count);
}

// upstream: intel_dp_mst.c mst_stream_pre_enable()
pub fn mst_stream_pre_enable<B: MstBackend>(backend: &mut B, link: &mut MstLink,
        connector: &MstConnector, crtc: &CrtcState) -> i32 {
    let first = intel_dp_mst_inc_active_streams(link);
    if backend.display_version() >= 12 && first && !intel_dp_mst_is_master_trans(crtc) { return EINVAL; }
    let mut actions = Vec::new();
    if first { actions.push("set_power_d0"); }
    actions.push("power_up_mst_phy");
    actions.push("sink_enable_decompression");
    if first { actions.push("primary_pre_enable"); }
    if first { actions.push("queue_topology_probe_if_link_changed"); }
    actions.push("add_payload_part1");
    if backend.display_version() < 12 || !first { actions.push("enable_transcoder_clock"); }
    if backend.display_version() >= 13 && !first { actions.push("config_transcoder_func"); }
    actions.push("write_dsc_pps");
    actions.push("set_dp_msa");
    for action in actions {
        let ret = backend.action(action, connector.port_id, crtc);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c enable_bs_jitter_was()
pub fn enable_bs_jitter_was<B: MstBackend>(backend: &mut B, alderlake_p: bool,
        step_d0_or_later: bool, crtc: &CrtcState, uhbr: bool) -> i32 {
    if !alderlake_p || !step_d0_or_later { return 0; }
    let (clear, mut set) = backend.dsc_wa_bits(crtc);
    if crtc.fec_enable || uhbr { set |= 1; }
    if clear == 0 && set == 0 { return 0; }
    backend.action("write_chicken_misc_3_rmw", 0, &CrtcState { ..*crtc })
}

// upstream: intel_dp_mst.c mst_stream_enable()
pub fn mst_stream_enable<B: MstBackend>(backend: &mut B, link: &MstLink,
        connector: &MstConnector, crtc: &CrtcState) -> i32 {
    let first = link.active_streams == 1;
    if backend.action("enable_bs_jitter_was", connector.port_id, crtc) != 0 { return EINVAL; }
    let mut actions = Vec::new();
    if backend.is_uhbr(crtc) { actions.push("write_vfreq_high_low"); }
    actions.extend(["enable_transcoder_func", "enable_vrr_transcoder", "clear_act_sent",
                    "set_vc_payload_alloc", "wait_act_sent", "check_act_status"]);
    if first { actions.push("wait_fec_status"); }
    actions.push("add_payload_part2");
    if backend.display_version() >= 12 { actions.push("update_fec_stall_wa"); }
    actions.push("enable_transcoder");
    actions.push("vblank_on_all_modeset_pipes");
    actions.push("hdcp_enable");
    for action in actions {
        let ret = backend.action(action, connector.port_id, crtc);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c mst_stream_get_hw_state()
pub fn mst_stream_get_hw_state<B: MstBackend>(backend: &B, port: u32, pipe: &mut u8) -> bool {
    *pipe = backend.stream_pipe(port).unwrap_or(0);
    backend.connector_state_exists(port)
}

// upstream: intel_dp_mst.c mst_stream_get_config()
pub fn mst_stream_get_config<B: MstBackend>(backend: &mut B, port: u32,
        pipe_config: &mut CrtcState) {
    backend.get_primary_config(port, pipe_config);
}

// upstream: intel_dp_mst.c mst_stream_initial_fastset_check()
pub fn mst_stream_initial_fastset_check<B: MstBackend>(backend: &B, port: u32,
        crtc: &CrtcState) -> bool {
    backend.initial_fastset_check(port, crtc)
}

// upstream: intel_dp_mst.c mst_connector_get_ddc_modes()
pub fn mst_connector_get_ddc_modes<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    if backend.connector_is_unregistered(connector.port_id) {
        return backend.action("update_modes_without_edid", connector.port_id, &CrtcState::default());
    }
    if !backend.display_access_allowed() {
        return backend.action("add_modes_without_edid_read", connector.port_id, &CrtcState::default());
    }
    let ret = backend.action("read_mst_edid_and_update_modes", connector.port_id, &CrtcState::default());
    let _ = backend.action("free_edid", connector.port_id, &CrtcState::default());
    ret
}

// upstream: intel_dp_mst.c mst_connector_late_register()
pub fn mst_connector_late_register<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    let ret = backend.connector_late_register(connector.port_id);
    if ret < 0 { return ret; }
    let ret = backend.connector_register(connector.port_id);
    if ret < 0 { backend.connector_early_unregister(connector.port_id); }
    ret
}

// upstream: intel_dp_mst.c mst_connector_early_unregister()
pub fn mst_connector_early_unregister<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) {
    backend.connector_unregister(connector.port_id);
    backend.connector_early_unregister(connector.port_id);
}

// upstream: intel_dp_mst.c mst_connector_get_modes()
pub fn mst_connector_get_modes<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    mst_connector_get_ddc_modes(backend, connector)
}

pub const MODE_OK: i32 = 0;
pub const MODE_ERROR: i32 = 1;
pub const MODE_H_ILLEGAL: i32 = 2;
pub const MODE_CLOCK_LOW: i32 = 3;
pub const MODE_CLOCK_HIGH: i32 = 4;

// upstream: intel_dp_mst.c mst_connector_mode_valid_ctx()
pub fn mst_connector_mode_valid_ctx<B: MstBackend>(backend: &mut B,
        connector: &MstConnector, mode: DisplayMode) -> (i32, i32) {
    let caps = backend.mode_validation_caps(connector.port_id, mode);
    if caps.unregistered { return (0, MODE_ERROR); }
    if caps.transcoder_status != MODE_OK { return (0, caps.transcoder_status); }
    if caps.double_clock { return (0, MODE_H_ILLEGAL); }
    if mode.clock_khz < 10_000 { return (0, MODE_CLOCK_LOW); }
    let mut min_bpp_x16 = if caps.min_link_bpp_x16 == 0 { 18 * 16 } else { caps.min_link_bpp_x16 };
    if caps.supports_dsc && connector.is_mst {
        let compressed_min = backend.action("compute_min_compressed_bpp", connector.port_id,
                                           &CrtcState::default());
        if compressed_min > 0 { min_bpp_x16 = compressed_min as u32; }
    }
    let max_rate = caps.max_link_data_rate;
    let mode_rate = backend.mode_required_rate(mode, min_bpp_x16, false);
    let lock_ret = backend.lock_modeset(connector.port_id);
    if lock_ret != 0 { return (lock_ret, MODE_OK); }
    if mode_rate > max_rate || backend.mode_required_pbn(mode, min_bpp_x16) > caps.port_full_pbn {
        return (0, MODE_CLOCK_HIGH);
    }
    let mut status = MODE_CLOCK_HIGH;
    for joined in backend.candidate_joined_pipes(connector.port_id, mode) {
        let mut dsc = false;
        let mut slices = 0;
        if caps.supports_dsc {
            slices = backend.mode_dsc_slice_count(connector.port_id, mode, joined);
            dsc = backend.mode_dsc_valid(connector.port_id, mode, joined,
                                          caps.max_pipe_bpp, true);
        }
        if backend.joiner_needs_dsc(joined) && !dsc { status = MODE_CLOCK_HIGH; continue; }
        if mode_rate > max_rate && !dsc { status = MODE_CLOCK_HIGH; continue; }
        status = backend.mode_plane_status(mode, joined);
        if status != MODE_OK { continue; }
        if !dsc { slices = 0; }
        if !backend.dotclk_valid(mode, slices, joined) { status = MODE_CLOCK_HIGH; continue; }
        break;
    }
    (0, status)
}

// upstream: intel_dp_mst.c mst_connector_atomic_best_encoder()
pub fn mst_connector_atomic_best_encoder<B: MstBackend>(backend: &B,
        connector: &MstConnector) -> Option<u32> {
    backend.connector_atomic_best_encoder(connector.port_id)
}

// upstream: intel_dp_mst.c mst_connector_detect_ctx()
pub fn mst_connector_detect_ctx<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> bool {
    if !backend.display_access_allowed() || backend.connector_is_unregistered(connector.port_id) {
        return false;
    }
    let _ = backend.action("flush_connector_commits", connector.port_id, &CrtcState::default());
    backend.connector_detect(connector.port_id)
}

// upstream: intel_dp_mst.c mst_stream_encoder_destroy()
pub fn mst_stream_encoder_destroy<B: MstBackend>(backend: &mut B, pipe: u8) -> i32 {
    backend.action("drm_encoder_cleanup", pipe as u32, &CrtcState::default())
}

// upstream: intel_dp_mst.c mst_connector_get_hw_state()
pub fn mst_connector_get_hw_state<B: MstBackend>(backend: &B,
        connector: &MstConnector) -> bool {
    if backend.connector_pipe(connector.port_id).is_none() ||
       !backend.connector_state_exists(connector.port_id) { return false; }
    backend.connector_hw_state(connector.port_id)
}

// upstream: intel_dp_mst.c mst_topology_add_connector_properties()
pub fn mst_topology_add_connector_properties<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> i32 {
    let ret = backend.connector_properties(connector.port_id);
    if ret != 0 { return ret; }
    backend.action("set_path_property", connector.port_id, &CrtcState::default())
}

// upstream: intel_dp_mst.c intel_dp_mst_read_decompression_port_dsc_caps()
pub fn intel_dp_mst_read_decompression_port_dsc_caps<B: MstBackend>(backend: &mut B,
        connector: &MstConnector, has_decompression_aux: bool) -> i32 {
    if !has_decompression_aux { return 0; }
    backend.read_dsc_caps(connector.port_id)
}

// upstream: intel_dp_mst.c detect_dsc_hblank_expansion_quirk()
pub fn detect_dsc_hblank_expansion_quirk<B: MstBackend>(backend: &mut B,
        connector: &MstConnector) -> bool {
    backend.action("read_parent_dpcd_caps_and_desc_for_hblank_quirk",
                   connector.port_id, &CrtcState::default()) == 1
}

// upstream: intel_dp_mst.c mst_topology_add_connector()
pub fn mst_topology_add_connector<B: MstBackend>(backend: &mut B, port: u32) -> i32 {
    // DRM dynamic connector allocation, encoder attachment, and port refs are
    // delegated as one transactional topology callback. On error the callback
    // owns the upstream err_cleanup_connector/err_put_port unwind.
    backend.add_topology_connector(port)
}

// upstream: intel_dp_mst.c mst_topology_poll_hpd_irq()
pub fn mst_topology_poll_hpd_irq<B: MstBackend>(backend: &mut B, port: u32) {
    backend.poll_hpd_irq(port)
}

// upstream: intel_dp_mst.c mst_stream_encoder_create()
pub fn mst_stream_encoder_create<B: MstBackend>(backend: &mut B, pipe: u8) -> i32 {
    backend.encoder_create(pipe)
}

// upstream: intel_dp_mst.c mst_stream_encoders_create()
pub fn mst_stream_encoders_create<B: MstBackend>(backend: &mut B) -> bool {
    for pipe in 0..backend.display_pipe_count() {
        if backend.encoder_create(pipe) != 0 { return false; }
    }
    true
}

// upstream: intel_dp_mst.c intel_dp_mst_encoder_init()
pub fn intel_dp_mst_encoder_init<B: MstBackend>(backend: &mut B, conn_base_id: i32) -> i32 {
    if !backend.has_dp_mst() || backend.is_edp() { return 0; }
    let port = backend.port_id_value();
    if backend.display_version() < 12 && port == 0 { return 0; }
    if backend.display_version() < 11 && port == 4 { return 0; }
    for pipe in 0..backend.display_pipe_count() {
        let ret = backend.encoder_create(pipe);
        if ret != 0 { return ret; }
    }
    let ret = backend.manager_init(conn_base_id);
    if ret != 0 { backend.action("clear_topology_callbacks", 0, &CrtcState::default()); }
    ret
}

// upstream: intel_dp_mst.c intel_dp_mst_source_support()
pub fn intel_dp_mst_source_support<B: MstBackend>(backend: &B) -> bool {
    backend.source_supports_mst()
}

// upstream: intel_dp_mst.c intel_dp_mst_encoder_cleanup()
pub fn intel_dp_mst_encoder_cleanup<B: MstBackend>(backend: &mut B) {
    if !backend.source_supports_mst() { return; }
    let _ = backend.action("destroy_topology_manager", 0, &CrtcState::default());
    backend.cleanup_encoders();
}

// upstream: intel_dp_mst.c intel_dp_mst_is_master_trans()
pub fn intel_dp_mst_is_master_trans(crtc: &CrtcState) -> bool {
    crtc.mst_master_transcoder == crtc.cpu_transcoder
}

// upstream: intel_dp_mst.c intel_dp_mst_is_slave_trans()
pub fn intel_dp_mst_is_slave_trans(crtc: &CrtcState) -> bool {
    crtc.mst_master_transcoder != u8::MAX &&
    crtc.mst_master_transcoder != crtc.cpu_transcoder
}

// upstream: intel_dp_mst.c intel_dp_mst_add_topology_state_for_connector()
pub fn intel_dp_mst_add_topology_state_for_connector<B: MstBackend>(backend: &mut B,
        connector: &MstConnector, pipe: u8) -> i32 {
    if !connector.is_mst { return 0; }
    let ret = backend.add_topology_state(connector.port_id);
    if ret != 0 { return ret; }
    backend.action("set_pending_crtc_mask", connector.port_id,
                   &CrtcState { pipe, ..CrtcState::default() })
}

// upstream: intel_dp_mst.c intel_dp_mst_add_topology_state_for_crtc()
pub fn intel_dp_mst_add_topology_state_for_crtc<B: MstBackend>(backend: &mut B,
        pipe: u8) -> i32 {
    for index in 0..backend.connector_count() {
        let Some(connector) = backend.connector_at(index) else { continue; };
        if backend.connector_state_pipe(connector.port_id) != Some(pipe) { continue; }
        let ret = intel_dp_mst_add_topology_state_for_connector(backend, &connector, pipe);
        if ret != 0 { return ret; }
    }
    0
}

// upstream: intel_dp_mst.c get_connector_in_state_for_crtc()
pub fn get_connector_in_state_for_crtc<B: MstBackend>(backend: &B, pipe: u8) -> Option<MstConnector> {
    (0..backend.connector_count()).filter_map(|i| backend.connector_at(i))
        .find(|c| backend.connector_state_pipe(c.port_id) == Some(pipe))
}

// upstream: intel_dp_mst.c intel_dp_mst_crtc_needs_modeset()
pub fn intel_dp_mst_crtc_needs_modeset<B: MstBackend>(backend: &B, pipe: u8,
        connector: Option<&MstConnector>) -> bool {
    if !backend.connector_modeset_required_for_crtc(pipe) { return false; }
    let Some(connector) = connector else { return false; };
    backend.topology_has_active_crtcs(connector.port_id).into_iter()
        .any(|other_pipe| other_pipe != pipe && backend.crtc_needs_modeset(other_pipe))
}

// upstream: intel_dp_mst.c intel_dp_mst_prepare_probe()
pub fn intel_dp_mst_prepare_probe<B: MstBackend>(backend: &mut B, link: &mut MstLink,
        port: u32) {
    if backend.link_active() { return; }
    let rate = backend.max_link_rate();
    let lanes = backend.max_lane_count();
    if intel_mst_probed_link_params_valid(link, rate, lanes) { return; }
    let _ = backend.action("set_training_mode_and_bandwidth_for_probe", port, &CrtcState::default());
    intel_mst_set_probed_link_params(link, rate, lanes);
    backend.prepare_probe();
}

// upstream: intel_dp_mst.c intel_dp_mst_verify_dpcd_state()
pub fn intel_dp_mst_verify_dpcd_state<B: MstBackend>(backend: &mut B,
        _link: &MstLink, port: u32) -> bool {
    if !backend.mst_mode_enabled() { return true; }
    backend.dpcd_mst_ctrl_matches(port) && backend.verify_dpcd_state()
}
