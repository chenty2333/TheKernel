/*
 * Copyright © 2012 Intel Corporation
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
 *
 * Authors:
 *    Eugeni Dodonov <eugeni.dodonov@intel.com>
 */

//! Source-ordered Rust translation of Linux 7.2.3
//! `drivers/gpu/drm/i915/display/intel_ddi.c`.
//!
//! DRM-owned object lookup, atomic-state plumbing, connector/encoder registration,
//! and cross-subsystem algorithms are represented at explicit trait boundaries.
//! This module intentionally has no default MMIO or display-framework backend.

#![allow(dead_code, non_camel_case_types, non_snake_case, clippy::too_many_arguments)]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_0: u8 = 0;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_1: u8 = 1;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_2: u8 = 2;
pub const DP_TRAIN_VOLTAGE_SWING_LEVEL_3: u8 = 3;
pub const DP_TRAIN_PRE_EMPH_LEVEL_0: u8 = 0 << 3;
pub const DP_TRAIN_PRE_EMPH_LEVEL_1: u8 = 1 << 3;
pub const DP_TRAIN_PRE_EMPH_LEVEL_2: u8 = 2 << 3;
pub const DP_TRAIN_PRE_EMPH_LEVEL_3: u8 = 3 << 3;
pub const DP_TRAIN_VOLTAGE_SWING_MASK: u8 = 0x3;
pub const DP_TRAIN_PRE_EMPHASIS_MASK: u8 = 0x18;
pub const DP_TRAIN_PRE_EMPH_LEVEL_3_MASK: u8 = 0x18;
pub const DP_TX_FFE_PRESET_VALUE_MASK: u8 = 0xf;

pub const DDI_BUF_IS_IDLE: u32 = 1 << 7;
pub const DDI_BUF_CTL_ENABLE: u32 = 1 << 31;
pub const DDI_BUF_BALANCE_LEG_ENABLE: u32 = 1 << 31;
pub const DDI_BUF_TRANS_SELECT_MASK: u32 = 0xf << 24;
pub const DDI_BUF_TRANS_SELECT_SHIFT: u32 = 24;
pub const DDI_BUF_EMP_MASK: u32 = DDI_BUF_TRANS_SELECT_MASK;
pub const DDI_BUF_PORT_REVERSAL: u32 = 1 << 16;
pub const DDI_A_4_LANES: u32 = 1 << 4;
pub const DDI_BUF_PORT_DATA_40BIT: u32 = 2 << 18;
pub const DDI_BUF_PORT_DATA_10BIT: u32 = 0;
pub const DDI_BUF_PORT_DATA_MASK: u32 = 0x3 << 18;
pub const DDI_PORT_WIDTH_MASK: u32 = 0x7 << 1;
pub const DDI_BUF_CTL_TC_PHY_OWNERSHIP: u32 = 1 << 6;
pub const DDI_CLK_SEL_MASK: u32 = 0xf << 28;
pub const DDI_CLK_SEL_NONE: u32 = 0;
pub const DDI_CLK_SEL_MG: u32 = 8 << 28;
pub const DDI_CLK_SEL_TBT_162: u32 = 12 << 28;
pub const DDI_CLK_SEL_TBT_270: u32 = 13 << 28;
pub const DDI_CLK_SEL_TBT_540: u32 = 14 << 28;
pub const DDI_CLK_SEL_TBT_810: u32 = 15 << 28;

const INDEX_TO_DP_SIGNAL_LEVELS: [u8; 10] = [
    DP_TRAIN_VOLTAGE_SWING_LEVEL_0 | DP_TRAIN_PRE_EMPH_LEVEL_0,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_0 | DP_TRAIN_PRE_EMPH_LEVEL_1,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_0 | DP_TRAIN_PRE_EMPH_LEVEL_2,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_0 | DP_TRAIN_PRE_EMPH_LEVEL_3,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_1 | DP_TRAIN_PRE_EMPH_LEVEL_0,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_1 | DP_TRAIN_PRE_EMPH_LEVEL_1,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_1 | DP_TRAIN_PRE_EMPH_LEVEL_2,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_2 | DP_TRAIN_PRE_EMPH_LEVEL_0,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_2 | DP_TRAIN_PRE_EMPH_LEVEL_1,
    DP_TRAIN_VOLTAGE_SWING_LEVEL_3 | DP_TRAIN_PRE_EMPH_LEVEL_0,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Port { #[default] A, B, C, D, E, F, G, H, I, J }
impl Port { pub const fn index(self) -> u8 { self as u8 } }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pipe { #[default] A, B, C, D, E, F }
impl Pipe { pub const fn index(self) -> u8 { self as u8 } }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum Transcoder { #[default] A = 0, B = 1, C = 2, D = 3, Edp = 4, Dsi0 = 5, Dsi1 = 6, E = 7, F = 8, Invalid = 255 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputType { #[default] DisplayPort, EmbeddedDisplayPort, Hdmi, Dvi, Analog, DpMst, Dsi, Unused }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DpllId { #[default] None, Wrpll1, Wrpll2, Spll, Lcpll810, Lcpll1350, Lcpll2700, IclTbt,
    IclMg1, IclMg2, IclMg3, IclMg4, TglMg5, TglMg6, IclDpll0, IclDpll1,
    SklDpll0, SklDpll1, SklDpll2, SklDpll3, Dg1Dpll0, Dg1Dpll1, Dg1Dpll2, Dg1Dpll3 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Platform {
    pub display_ver: u8,
    pub broxton: bool,
    pub geminilake: bool,
    pub alderlake_p: bool,
    pub has_dp20: bool,
    pub wa_16011342517: bool,
    pub increase_ddi_disabled_time: bool,
    pub has_mso: bool,
    pub has_dpll_manager: bool,
    pub jasperlake: bool,
    pub elkhartlake: bool,
    pub haswell: bool,
    pub tigerlake: bool,
    pub has_edp_transcoder: bool,
    pub has_pch_tgp: bool,
    pub dg2: bool,
    pub alderlake_s: bool,
    pub rocketlake: bool,
    pub dg1: bool,
    pub broadwell: bool,
    pub has_lt_phy: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub pipe: Pipe,
    pub cpu_transcoder: Transcoder,
    pub master_transcoder: Transcoder,
    pub mst_master_transcoder: Transcoder,
    pub output: OutputType,
    pub port_clock: u32,
    pub lane_count: u8,
    pub fdi_lanes: u8,
    pub pipe_bpp: u8,
    /// Raw DRM mode flags from `adjusted_mode.flags` (PHSYNC bit 0, PVSYNC bit 2).
    pub mode_flags: u32,
    pub limited_color_range: bool,
    pub has_pch_encoder: bool,
    pub has_hdmi_sink: bool,
    pub hdmi_scrambling: bool,
    pub hdmi_high_tmds_clock_ratio: bool,
    pub pfit_force_thru: bool,
    pub uhbr: bool,
    pub mst: bool,
    pub mst_slave: bool,
    pub mst_master: bool,
    pub dp_needs_vsc_sdp: bool,
    pub has_vrr: bool,
    pub fec_enable: bool,
    pub splitter: SplitterState,
    pub has_infoframe: bool,
    pub sync_mode: bool,
    pub sync_mode_slaves_mask: u32,
    pub payload_tu: u8,
    pub enhanced_framing: bool,
    pub min_voltage_level: u8,
    pub has_audio: bool,
    pub active: bool,
    pub joiner_pipes: u8,
    pub output_types: u32,
    pub output_format: u8,
    pub adjusted_mode: DisplayMode,
    pub dp_m_n: LinkMN,
    pub crc_enabled: bool,
    pub pfit_enabled: bool,
    pub lane_lat_optim_mask: u8,
    pub pll_id: DpllId,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdiEncoder {
    pub port: Port,
    pub phy: u8,
    pub output: OutputType,
    pub display: Platform,
    pub power_domain: u32,
    pub aux_channel: u8,
    pub is_tc: bool,
    pub in_tbt_alt_mode: bool,
    pub lane_reversal: bool,
    pub ddi_a_4_lanes: bool,
    pub max_lanes: u8,
    pub bios_dp_boost: u8,
    pub bios_hdmi_boost: u8,
    pub bios_hdmi_level: i8,
    pub buffer_entries: u8,
    pub buffer_level: u8,
    pub hdmi_default_entry: u8,
    pub train_set: [u8; 4],
    pub buffer_ctl: u32,
    pub hobl_active: bool,
    pub dp_state: u32,
    pub has_crtc: bool,
    pub hotplug_retries: u8,
    pub hpd_pin: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdiBufTransEntry {
    pub hsw_trans1: u32,
    pub hsw_trans2: u32,
    pub hsw_i_boost: u8,
    pub icl_dw2_swing_sel: u8,
    pub icl_dw4_post_cursor_1: u8,
    pub icl_dw4_post_cursor_2: u8,
    pub icl_dw4_cursor_coeff: u8,
    pub icl_dw7_n_scalar: u8,
    pub mg_cri_txdeemph_override_17_12: u8,
    pub mg_cri_txdeemph_override_11_6: u8,
    pub mg_cri_txdeemph_override_5_0: u8,
    pub dkl_preshoot: u8,
    pub dkl_de_emphasis: u8,
    pub dkl_vswing: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SplitterState { pub enable: bool, pub link_count: u8, pub pixel_overlap: u8 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayMode { pub clock: u32, pub flags: u32, pub hdisplay: u16, pub hsync_start: u16, pub hsync_end: u16,
    pub htotal: u16, pub vdisplay: u16, pub vsync_start: u16, pub vsync_end: u16, pub vtotal: u16, pub vscan: u16 }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkMN { pub tu: u8, pub data_m: u32, pub data_n: u32, pub link_m: u32, pub link_n: u32 }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortSyncCandidate { pub has_tile: bool, pub tile_group_id: i32, pub state: CrtcState }

/// Register selectors for the combo PHY recipes in `intel_combo_phy_regs.h`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComboPhyRegister { ClDw5, ClDw10, PcsDw1Lane(u8), PcsDw1Group, TxDw2Lane(u8), TxDw4Lane(u8), TxDw5Lane(u8), TxDw5Group, TxDw7Lane(u8) }

/// MG PHY selectors corresponding to the `MG_*` register macros.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MgPhyRegister { Tx1LinkParams(u8), Tx2LinkParams(u8), Tx1SwingCtrl(u8), Tx2SwingCtrl(u8), Tx1DrvCtrl(u8), Tx2DrvCtrl(u8), ClkHub(u8), Tx1Dcc(u8), Tx2Dcc(u8), Tx1PisoReadload(u8), Tx2PisoReadload(u8) }

/// DKL PHY message-bus selectors corresponding to `DKL_*` register macros.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DklPhyRegister { DpMode(u8), TxDpcntl0(u8), TxDpcntl1(u8), TxDpcntl2(u8), TxPmdLaneSus(u8), PcsDw5(u8) }

/// Explicit display/MMIO/AUX boundary. The helpers mirror operations consumed by
/// this translation; the caller owns the actual display framework and hardware.
pub trait DdiIo {
    fn read(&mut self, reg: u32) -> u32;
    fn write(&mut self, reg: u32, value: u32);
    fn lock_dpll(&mut self) {}
    fn unlock_dpll(&mut self) {}
    fn get_dpll(&mut self, id: DpllId) -> Option<DpllId> { (id != DpllId::None).then_some(id) }
    fn get_dpll_index(&mut self, display: Platform, index: u32) -> Option<DpllId> {
        let id = if display.dg1 { match index { 0 => DpllId::Dg1Dpll0, 1 => DpllId::Dg1Dpll1, 2 => DpllId::Dg1Dpll2, 3 => DpllId::Dg1Dpll3, _ => DpllId::None } }
            else if display.display_ver < 11 || display.alderlake_s || display.rocketlake { match index { 0 => DpllId::SklDpll0, 1 => DpllId::SklDpll1, 2 => DpllId::SklDpll2, 3 => DpllId::SklDpll3, _ => DpllId::None } }
            else { match index { 0 => DpllId::IclDpll0, 1 => DpllId::IclDpll1, 2 => DpllId::IclTbt, 3 => DpllId::IclMg1, 4 => DpllId::IclMg2, 5 => DpllId::IclMg3, 6 => DpllId::IclMg4, 7 => DpllId::TglMg5, 8 => DpllId::TglMg6, _ => DpllId::None } };
        self.get_dpll(id)
    }
    fn rmw(&mut self, reg: u32, clear: u32, set: u32) {
        let value = self.read(reg);
        self.write(reg, (value & !clear) | set);
    }
    fn combo_phy_read(&mut self, _phy: u8, _reg: ComboPhyRegister) -> u32;
    fn combo_phy_write(&mut self, _phy: u8, _reg: ComboPhyRegister, _value: u32);
    fn combo_phy_rmw(&mut self, _phy: u8, _reg: ComboPhyRegister, _clear: u32, _set: u32);
    fn mg_phy_rmw(&mut self, _port: Port, _reg: MgPhyRegister, _clear: u32, _set: u32);
    fn dkl_phy_read(&mut self, _port: Port, _reg: DklPhyRegister) -> u32;
    fn dkl_phy_write(&mut self, _port: Port, _reg: DklPhyRegister, _value: u32);
    fn dkl_phy_rmw(&mut self, _port: Port, _reg: DklPhyRegister, _clear: u32, _set: u32);
    fn mg_dp_mode_read(&mut self, _port: Port, _lane: u8) -> u32;
    fn mg_dp_mode_write(&mut self, _port: Port, _lane: u8, _value: u32);
    fn posting_read(&mut self, reg: u32) { let _ = self.read(reg); }
    fn wait_set(&mut self, _reg: u32, _mask: u32, _timeout_ms: u32) -> bool { false }
    fn wait_clear(&mut self, _reg: u32, _mask: u32, _timeout_ms: u32) -> bool { false }
    fn wait_set_us(&mut self, _reg: u32, _mask: u32, _timeout_us: u32) -> bool { false }
    fn wait_clear_us(&mut self, _reg: u32, _mask: u32, _timeout_us: u32) -> bool { false }
    fn delay_us(&mut self, _min_us: u32, _max_us: u32) {}
    fn delay_ms(&mut self, _ms: u32) {}
    fn warning(&mut self, _code: u32) {}
    fn log(&mut self, _code: u32, _value: u32) {}
    fn buffer_trans(&mut self, _encoder: &DdiEncoder, _state: &CrtcState) -> Vec<DdiBufTransEntry> { Vec::new() }
    fn dp_symbol_size(&mut self, port_clock: u32) -> u32 { if port_clock >= 1_000_000 { 132 } else { 10 } }
    fn crtc_dotclock(&mut self, _state: &CrtcState) -> u32 { 0 }
    fn is_uhbr(&self, state: &CrtcState) -> bool { state.uhbr }
    fn needs_vsc_sdp(&self, state: &CrtcState) -> bool { state.dp_needs_vsc_sdp }
    fn signal_level(&mut self, _encoder: &DdiEncoder, _state: &CrtcState, lane: usize) -> usize { lane.min(9) }
    fn enable_power(&mut self, _domain: u32) -> Option<u64> { Some(1) }
    fn power_is_enabled(&mut self, _domain: u32) -> bool { true }
    fn enable_power_if_enabled(&mut self, _domain: u32) -> Option<u64> { None }
    fn disable_power(&mut self, _domain: u32, _cookie: u64) {}
    fn aux_write(&mut self, _port: Port, _address: u32, _bytes: &[u8]) -> Result<i32, i32> { Ok(0) }
    fn aux_read(&mut self, _port: Port, _address: u32, _bytes: &mut [u8]) -> Result<i32, i32> { Ok(0) }
    fn external(&mut self, _operation: ExternalOperation, _value: u32) -> u32 { 0 }
    fn encoder_clock_enable(&mut self, encoder: &DdiEncoder, state: &CrtcState) {
        self.external(ExternalOperation::Dpll, ((encoder.port.index() as u32) << 24) | state.pll_id as u32 | (1 << 31));
    }
    fn encoder_clock_disable(&mut self, encoder: &DdiEncoder) {
        self.external(ExternalOperation::Dpll, (encoder.port.index() as u32) << 24);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExternalOperation { LinkTraining, DpAux, PowerDomain, Dpll, Hdmi, Connector, AtomicState, Hotplug, Psr, TcPort, Phy,
    LinkParams, PanelPower, ProtocolConverter, SinkDecompression, FrlTraining, PconDsc, LinkTrainStart,
    LinkTrainStop, LinkPayload, DscPps, PanelReplay, DpSdpCrc, MstRetry, SignalLevels,
    Infoframes, DualModeTmds, FifoUnderrun, PrivacyScreen, Backlight, Scaler, Hdcp, Alpm, Vrr, Vblank,
    LaneCount, DpllUpdate, Audio, Init }

const fn ddi_buf_ctl(port: Port) -> u32 { 0x64000 + (port.index() as u32) * 0x100 }
const fn DDI_BUF_TRANS_SELECT(level: u32) -> u32 { (level & 0xf) << DDI_BUF_TRANS_SELECT_SHIFT }
const fn ddi_buf_port_width(lanes: u8) -> u32 { (if lanes == 3 { 4 } else { lanes.saturating_sub(1) } as u32) << 1 }
const fn transcoder_port_width(lanes: u8) -> u32 { (lanes.saturating_sub(1) as u32) << 1 }
const fn ddi_buf_status(port: Port, display_ver: u8) -> u32 {
    if display_ver >= 14 {
        if port.index() < 3 { 0x64004 + (port.index() as u32) * 0x100 }
        else { 0x16f200 + (port.index() as u32 - 3) * 0x200 }
    } else { ddi_buf_ctl(port) }
}
const fn ddi_buf_trans_lo(port: Port, entry: u32) -> u32 { 0x64e00 + (port.index() as u32) * 0x60 + entry * 8 }
const fn ddi_buf_trans_hi(port: Port, entry: u32) -> u32 { ddi_buf_trans_lo(port, entry) + 4 }
const fn trans_offset(cpu: Transcoder) -> u32 {
    match cpu { Transcoder::A => 0x0000, Transcoder::B => 0x1000, Transcoder::C => 0x2000, Transcoder::D => 0x3000,
        Transcoder::Edp => 0x6f000, Transcoder::Dsi0 => 0x0b000, Transcoder::Dsi1 => 0x0b800,
        Transcoder::E => 0x4000, Transcoder::F => 0x5000, Transcoder::Invalid => 0 }
}
const fn trans_ddi_func_ctl(cpu: Transcoder) -> u32 { 0x60400 + trans_offset(cpu) }
const fn trans_ddi_func_ctl2(cpu: Transcoder) -> u32 { trans_ddi_func_ctl(cpu) + 4 }
const fn trans_clk_sel(cpu: Transcoder) -> u32 { 0x46140 + trans_index(cpu) * 4 }
const fn trans_dp2_ctl(cpu: Transcoder) -> u32 { 0x600a0 + trans_offset(cpu) }
const fn port_clk_sel(port: Port) -> u32 { 0x46100 + (port.index() as u32) * 4 }
const fn msa_misc_reg(cpu: Transcoder) -> u32 { 0x60410 + trans_offset(cpu) }
const fn dp_tp_transcoder_reg(cpu: Transcoder, status: bool) -> u32 { 0x60540 + trans_offset(cpu) + if status { 4 } else { 0 } }
const fn dp_tp_port_reg(port: Port, status: bool) -> u32 { 0x64040 + (port.index() as u32) * 0x100 + if status { 4 } else { 0 } }
const fn trans_index(cpu: Transcoder) -> u32 { cpu as u32 }

fn port_clock_sel(pll: DpllId) -> Option<u32> {
    match pll { DpllId::Wrpll1 => Some(4 << 29), DpllId::Wrpll2 => Some(5 << 29), DpllId::Spll => Some(3 << 29),
        DpllId::Lcpll810 => Some(2 << 29), DpllId::Lcpll1350 => Some(1 << 29), DpllId::Lcpll2700 => Some(0), _ => None }
}

// upstream: intel_ddi.c intel_ddi_hdmi_level()
pub fn intel_ddi_hdmi_level(encoder: &DdiEncoder, trans: &[DdiBufTransEntry]) -> usize {
    if encoder.bios_hdmi_level >= 0 { encoder.bios_hdmi_level as usize } else { encoder.hdmi_default_entry.min(trans.len().saturating_sub(1) as u8) as usize }
}

// upstream: intel_ddi.c has_buf_trans_select()
pub const fn has_buf_trans_select(display: Platform) -> bool { display.display_ver < 10 && !display.broxton }

// upstream: intel_ddi.c has_iboost()
pub const fn has_iboost(display: Platform) -> bool { display.display_ver == 9 && !display.broxton }

// upstream: intel_ddi.c hsw_prepare_dp_ddi_buffers()
pub fn hsw_prepare_dp_ddi_buffers(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    let boost = if has_iboost(encoder.display) && encoder.bios_dp_boost != 0 { DDI_BUF_BALANCE_LEG_ENABLE } else { 0 };
    for (i, entry) in entries.iter().enumerate() {
        io.write(ddi_buf_trans_lo(encoder.port, i as u32), entry.hsw_trans1 | boost);
        io.write(ddi_buf_trans_hi(encoder.port, i as u32), entry.hsw_trans2);
    }
}

// upstream: intel_ddi.c hsw_prepare_hdmi_ddi_buffers()
pub fn hsw_prepare_hdmi_ddi_buffers(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    let level = intel_ddi_level(io, encoder, state, 0);
    let boost = if has_iboost(encoder.display) && encoder.bios_hdmi_boost != 0 { DDI_BUF_BALANCE_LEG_ENABLE } else { 0 };
    if let Some(entry) = entries.get(level) {
        io.write(ddi_buf_trans_lo(encoder.port, 9), entry.hsw_trans1 | boost);
        io.write(ddi_buf_trans_hi(encoder.port, 9), entry.hsw_trans2);
    } else { io.warning(level as u32); }
}

// upstream: intel_ddi.c intel_ddi_buf_status_reg()
pub const fn intel_ddi_buf_status_reg(display: Platform, port: Port) -> u32 { ddi_buf_status(port, display.display_ver) }

// upstream: intel_ddi.c intel_wait_ddi_buf_idle()
pub fn intel_wait_ddi_buf_idle(io: &mut impl DdiIo, display: Platform, port: Port) {
    if display.broxton { io.delay_us(16, 16); return; }
    if io.wait_set(intel_ddi_buf_status_reg(display, port), DDI_BUF_IS_IDLE, 10) { io.log(1, port.index() as u32); }
}

// upstream: intel_ddi.c intel_wait_ddi_buf_active()
pub fn intel_wait_ddi_buf_active(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if encoder.display.display_ver < 10 { io.delay_us(518, 1000); return; }
    if io.wait_clear(intel_ddi_buf_status_reg(encoder.display, encoder.port), DDI_BUF_IS_IDLE, 10) { io.log(2, encoder.port.index() as u32); }
}

// upstream: intel_ddi.c hsw_pll_to_ddi_pll_sel()
pub fn hsw_pll_to_ddi_pll_sel(pll: DpllId) -> u32 { port_clock_sel(pll).unwrap_or(PORT_CLK_SEL_NONE) }

// upstream: intel_ddi.c icl_pll_to_ddi_clk_sel()
pub fn icl_pll_to_ddi_clk_sel(pll: DpllId, port_clock: u32) -> u32 {
    match pll { DpllId::IclTbt => match port_clock { 162_000 => DDI_CLK_SEL_TBT_162, 270_000 => DDI_CLK_SEL_TBT_270,
            540_000 => DDI_CLK_SEL_TBT_540, 810_000 => DDI_CLK_SEL_TBT_810, _ => 0 },
        DpllId::IclMg1 | DpllId::IclMg2 | DpllId::IclMg3 | DpllId::IclMg4 | DpllId::TglMg5 | DpllId::TglMg6 => DDI_CLK_SEL_MG,
        _ => 0 }
}

// upstream: intel_ddi.c ddi_buf_phy_link_rate()
pub const fn ddi_buf_phy_link_rate(port_clock: u32) -> u32 {
    let code = match port_clock { 162_000 => 0, 216_000 => 4, 243_000 => 5, 270_000 => 1, 324_000 => 6, 432_000 => 7,
        540_000 => 2, 810_000 => 3, _ => 0 };
    code << 20
}

// upstream: intel_ddi.c dp_phy_lane_stagger_delay()
pub fn dp_phy_lane_stagger_delay(io: &mut impl DdiIo, port_clock: u32) -> u32 {
    let symbol_size = io.dp_symbol_size(port_clock).max(1);
    port_clock.div_ceil(symbol_size * 1000)
}

// upstream: intel_ddi.c intel_ddi_init_dp_buf_reg()
pub fn intel_ddi_init_dp_buf_reg(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) -> u32 {
    let mut dp = ddi_buf_port_width(state.lane_count) | DDI_BUF_TRANS_SELECT(0);
    if encoder.lane_reversal { dp |= DDI_BUF_PORT_REVERSAL; }
    if encoder.ddi_a_4_lanes { dp |= DDI_A_4_LANES; }
    if encoder.display.display_ver >= 14 { dp |= if io.is_uhbr(state) { DDI_BUF_PORT_DATA_40BIT } else { DDI_BUF_PORT_DATA_10BIT }; }
    if encoder.display.alderlake_p && encoder.is_tc {
        dp |= ddi_buf_phy_link_rate(state.port_clock);
        if !encoder.in_tbt_alt_mode { dp |= DDI_BUF_CTL_TC_PHY_OWNERSHIP; }
    }
    if (11..=13).contains(&encoder.display.display_ver) && encoder.is_tc {
        dp |= (dp_phy_lane_stagger_delay(io, state.port_clock) & 0xff) << 8;
    }
    dp
}

// upstream: intel_ddi.c icl_calc_tbt_pll_link()
pub fn icl_calc_tbt_pll_link(io: &mut impl DdiIo, _display: Platform, port: Port) -> u32 {
    match io.read(port_clk_sel(port)) & DDI_CLK_SEL_MASK { DDI_CLK_SEL_NONE => 0,
        DDI_CLK_SEL_TBT_162 => 162_000, DDI_CLK_SEL_TBT_270 => 270_000, DDI_CLK_SEL_TBT_540 => 540_000,
        DDI_CLK_SEL_TBT_810 => 810_000, value => { io.warning(value); 0 } }
}

// upstream: intel_ddi.c ddi_dotclock_get()
pub fn ddi_dotclock_get(io: &mut impl DdiIo, state: &mut CrtcState) {
    if !state.has_pch_encoder { state.adjusted_mode.clock = io.crtc_dotclock(state); }
}

// upstream: intel_ddi.c intel_ddi_set_dp_msa()
pub fn intel_ddi_set_dp_msa(io: &mut impl DdiIo, state: &CrtcState, is_dp_encoder: bool) {
    if !is_dp_encoder { return; }
    let mut misc = 1 << 0;
    misc |= match state.pipe_bpp { 18 => 0, 24 => 1 << 5, 30 => 2 << 5, 36 => 3 << 5, other => { io.warning(other as u32); 0 } };
    if state.limited_color_range && state.output_format != 0 { io.warning(state.output_format as u32); }
    if state.limited_color_range { misc |= 1 << 3; }
    if state.output_format == 2 { misc |= (2 << 1) | (1 << 3) | (1 << 4); }
    if io.needs_vsc_sdp(state) { misc |= 1 << 2; }
    io.write(msa_misc_reg(state.cpu_transcoder), misc);
}

// upstream: intel_ddi.c bdw_trans_port_sync_master_select()
pub const fn bdw_trans_port_sync_master_select(master: Transcoder) -> u32 { if matches!(master, Transcoder::Edp) { 0 } else { master as u32 + 1 } }

// upstream: intel_ddi.c intel_ddi_config_transcoder_dp2()
pub fn intel_ddi_config_transcoder_dp2(io: &mut impl DdiIo, display: Platform, state: &CrtcState, enable: bool) {
    if !display.has_dp20 { return; }
    let value = if enable && io.is_uhbr(state) { 1 << 31 } else { 0 };
    io.write(trans_dp2_ctl(state.cpu_transcoder), value);
}

// upstream: intel_ddi.c intel_ddi_transcoder_func_reg_val_get()
pub fn intel_ddi_transcoder_func_reg_val_get(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) -> u32 {
    const FUNC_ENABLE: u32 = 1 << 31;
    const DRM_MODE_FLAG_PHSYNC: u32 = 1 << 0;
    const DRM_MODE_FLAG_PVSYNC: u32 = 1 << 2;
    const BPC_6: u32 = 2 << 20; const BPC_8: u32 = 0 << 20; const BPC_10: u32 = 1 << 20; const BPC_12: u32 = 3 << 20;
    const MODE_HDMI: u32 = 0 << 24; const MODE_DVI: u32 = 1 << 24; const MODE_DP_SST: u32 = 2 << 24;
    const MODE_DP_MST: u32 = 3 << 24; const MODE_FDI_DP2: u32 = 4 << 24;
    let mut value = FUNC_ENABLE;
    if encoder.display.display_ver >= 12 { value |= ((encoder.port.index() as u32 + 1) & 0xf) << 27; }
    else { value |= ((encoder.port.index() as u32) & 0x7) << 28; }
    value |= match state.pipe_bpp { 18 => BPC_6, 24 => BPC_8, 30 => BPC_10, 36 => BPC_12, other => { io.warning(other as u32); BPC_6 } };
    if state.mode_flags & DRM_MODE_FLAG_PVSYNC != 0 { value |= 1 << 17; }
    if state.mode_flags & DRM_MODE_FLAG_PHSYNC != 0 { value |= 1 << 16; }
    if state.cpu_transcoder == Transcoder::Edp {
        value |= match state.pipe { Pipe::A => if state.pfit_force_thru { 4 << 12 } else { 0 },
            Pipe::B => 5 << 12, Pipe::C => 6 << 12, Pipe::D => 7 << 12, Pipe::E | Pipe::F => 0 };
    }
    if state.output == OutputType::Hdmi || state.output == OutputType::Dvi {
        value |= if state.has_hdmi_sink { MODE_HDMI } else { MODE_DVI };
        if state.hdmi_scrambling { value |= 1 << 0; }
        if state.hdmi_high_tmds_clock_ratio { value |= 1 << 4; }
        if encoder.display.display_ver >= 14 { value |= transcoder_port_width(state.lane_count); }
    } else if state.output == OutputType::Analog {
        value |= MODE_FDI_DP2 | ((state.fdi_lanes.saturating_sub(1) as u32) << 1);
    } else if state.output == OutputType::DpMst || io.is_uhbr(state) {
        value |= if io.is_uhbr(state) { MODE_FDI_DP2 } else { MODE_DP_MST };
        value |= transcoder_port_width(state.lane_count);
        if encoder.display.display_ver >= 12 {
            if state.mst_master_transcoder == Transcoder::Invalid { io.warning(0); }
            else { value |= (trans_index(state.mst_master_transcoder) & 0x3) << 10; }
        }
    } else { value |= MODE_DP_SST | transcoder_port_width(state.lane_count); }
    if (8..=10).contains(&encoder.display.display_ver) && state.master_transcoder != Transcoder::Invalid {
        let master = bdw_trans_port_sync_master_select(state.master_transcoder);
        value |= (1 << 15) | ((master & 0x3) << 18);
    }
    value
}

// upstream: intel_ddi.c intel_ddi_enable_transcoder_func()
pub fn intel_ddi_enable_transcoder_func(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.display.display_ver >= 11 {
        let ctl2 = if state.master_transcoder != Transcoder::Invalid {
            let select = bdw_trans_port_sync_master_select(state.master_transcoder);
            (1 << 4) | (select & 0x7)
        } else { 0 };
        io.write(trans_ddi_func_ctl2(state.cpu_transcoder), ctl2);
    }
    let ctl = intel_ddi_transcoder_func_reg_val_get(io, encoder, state);
    io.write(trans_ddi_func_ctl(state.cpu_transcoder), ctl);
}

// upstream: intel_ddi.c intel_ddi_config_transcoder_func()
pub fn intel_ddi_config_transcoder_func(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    intel_ddi_config_transcoder_dp2(io, encoder.display, state, true);
    let ctl = intel_ddi_transcoder_func_reg_val_get(io, encoder, state) & !(1 << 31);
    io.write(trans_ddi_func_ctl(state.cpu_transcoder), ctl);
}

// upstream: intel_ddi.c intel_ddi_disable_transcoder_func()
pub fn intel_ddi_disable_transcoder_func(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.display.display_ver >= 11 { io.write(trans_ddi_func_ctl2(state.cpu_transcoder), 0); }
    let reg = trans_ddi_func_ctl(state.cpu_transcoder);
    let mut ctl = io.read(reg);
    io.warning((ctl >> 9) & 1);
    ctl &= !(1 << 31);
    if (8..=10).contains(&encoder.display.display_ver) { ctl &= !((1 << 15) | (0x3 << 18)); }
    if encoder.display.display_ver >= 12 {
        if !state.mst_master { ctl &= !((0xf << 27) | (0x7 << 24)); }
    } else { ctl &= !((0x7 << 28) | (0x7 << 24)); }
    io.write(reg, ctl);
    if state.mst_slave { intel_ddi_config_transcoder_dp2(io, encoder.display, state, false); }
    if encoder.display.increase_ddi_disabled_time && state.output == OutputType::Hdmi { io.delay_ms(100); }
}

// upstream: intel_ddi.c intel_ddi_toggle_hdcp_bits()
pub fn intel_ddi_toggle_hdcp_bits(io: &mut impl DdiIo, encoder: &DdiEncoder, cpu: Transcoder, enable: bool, hdcp_mask: u32) -> i32 {
    let Some(wakeref) = io.enable_power(encoder.power_domain) else { return -6; };
    let reg = trans_ddi_func_ctl(cpu);
    let old = io.read(reg);
    io.write(reg, (old & !hdcp_mask) | if enable { hdcp_mask } else { 0 });
    io.disable_power(encoder.power_domain, wakeref);
    0
}

// upstream: intel_ddi.c intel_ddi_connector_get_hw_state()
pub fn intel_ddi_connector_get_hw_state(io: &mut impl DdiIo, encoder: &DdiEncoder, connector_type: u32, pipe: Option<Pipe>) -> bool {
    let Some(wakeref) = io.enable_power(encoder.power_domain) else { return false; };
    let result = if let Some(pipe) = pipe {
        let cpu = if encoder.port == Port::A && encoder.display.has_edp_transcoder { Transcoder::Edp }
            else { match pipe { Pipe::A => Transcoder::A, Pipe::B => Transcoder::B, Pipe::C => Transcoder::C,
                Pipe::D => Transcoder::D, Pipe::E | Pipe::F => Transcoder::Invalid } };
        let mode = io.read(trans_ddi_func_ctl(cpu)) & (0x7 << 24);
        if mode == 0 || mode == (1 << 24) { connector_type == 11 }
        else if mode == (4 << 24) && !encoder.display.has_dp20 { connector_type == 1 }
        else if mode == (2 << 24) { connector_type == 10 || connector_type == 14 }
        else if mode == (4 << 24) { connector_type == 10 }
        else if mode == (3 << 24) { io.warning(mode); false }
        else { false }
    } else { false };
    io.disable_power(encoder.power_domain, wakeref);
    result
}

// upstream: intel_ddi.c intel_ddi_get_encoder_pipes()
pub fn intel_ddi_get_encoder_pipes(io: &mut impl DdiIo, encoder: &DdiEncoder) -> (u8, bool) {
    let Some(wakeref) = io.enable_power(encoder.power_domain) else { return (0, false); };
    let mut pipe_mask = 0u8;
    let mut mst_mask = 0u8;
    let mut dp2_mask = 0u8;
    let mut is_mst = false;
    if io.read(ddi_buf_ctl(encoder.port)) & DDI_BUF_CTL_ENABLE == 0 { io.disable_power(encoder.power_domain, wakeref); return (0, false); }
    if encoder.display.has_edp_transcoder && encoder.port == Port::A {
        let edp_ctl = io.read(trans_ddi_func_ctl(Transcoder::Edp));
        match edp_ctl & (0x7 << 12) {
            0 => pipe_mask = 1 << Pipe::A.index(),
            value if value == (4 << 12) => pipe_mask = 1 << Pipe::A.index(),
            value if value == (5 << 12) => pipe_mask = 1 << Pipe::B.index(),
            value if value == (6 << 12) => pipe_mask = 1 << Pipe::C.index(),
            value if value == (7 << 12) => pipe_mask = 1 << Pipe::D.index(),
            invalid => { io.warning(invalid); pipe_mask = 1 << Pipe::A.index(); }
        }
    } else {
        for pipe in [Pipe::A, Pipe::B, Pipe::C, Pipe::D] {
            let cpu = match pipe { Pipe::A => Transcoder::A, Pipe::B => Transcoder::B, Pipe::C => Transcoder::C, _ => Transcoder::D };
            let Some(trans_wakeref) = io.enable_power_if_enabled(0x100 + cpu as u32) else { continue; };
            let ctl = io.read(trans_ddi_func_ctl(cpu));
            io.disable_power(0x100 + cpu as u32, trans_wakeref);
            let (port_mask, ddi_select) = if encoder.display.display_ver >= 12 { (0xf << 27, (encoder.port.index() as u32 + 1) << 27) }
                else { (0x7 << 28, (encoder.port.index() as u32) << 28) };
            if ctl & port_mask != ddi_select { continue; }
            let mode = ctl & (0x7 << 24);
            if mode == 3 << 24 { mst_mask |= 1 << pipe.index(); }
            else if mode == 4 << 24 && encoder.display.has_dp20 { dp2_mask |= 1 << pipe.index(); }
            pipe_mask |= 1 << pipe.index();
        }
    }
    if mst_mask == 0 && dp2_mask != 0 && (dp2_mask.count_ones() > 1 || io.external(ExternalOperation::LinkTraining, encoder.port.index() as u32) != 0) { mst_mask = dp2_mask; }
    if mst_mask == 0 && pipe_mask.count_ones() > 1 {
        io.log(3, pipe_mask as u32);
        pipe_mask &= pipe_mask.wrapping_neg();
    }
    if mst_mask != 0 && mst_mask != pipe_mask { io.log(4, ((mst_mask as u32) << 8) | pipe_mask as u32); }
    else { is_mst = mst_mask != 0; }
    if pipe_mask != 0 && (encoder.display.broxton || encoder.display.geminilake) {
        let phy = io.read(0x64c00 + encoder.port.index() as u32 * 4);
        if phy & 0x7 != 0x4 { io.log(5, phy); }
    }
    io.disable_power(encoder.power_domain, wakeref);
    (pipe_mask, is_mst)
}

// upstream: intel_ddi.c intel_ddi_get_hw_state()
pub fn intel_ddi_get_hw_state(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<Pipe> {
    let (mask, is_mst) = intel_ddi_get_encoder_pipes(io, encoder);
    if is_mst || mask == 0 { return None; }
    match mask.trailing_zeros() { 0 => Some(Pipe::A), 1 => Some(Pipe::B), 2 => Some(Pipe::C),
        3 => Some(Pipe::D), 4 => Some(Pipe::E), 5 => Some(Pipe::F), _ => None }
}

// upstream: intel_ddi.c intel_ddi_main_link_aux_domain()
pub fn intel_ddi_main_link_aux_domain(encoder: &DdiEncoder, state: &CrtcState, psr_needs_aux_io: bool) -> Option<u32> {
    if psr_needs_aux_io { Some(0x200 + encoder.aux_channel as u32) }
    else if encoder.display.display_ver < 14 && (state.output == OutputType::DisplayPort || state.output == OutputType::EmbeddedDisplayPort || encoder.is_tc) { Some(0x300 + encoder.port.index() as u32) }
    else { None }
}

// upstream: intel_ddi.c main_link_aux_power_domain_get()
pub fn main_link_aux_power_domain_get(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, aux_wakeref: &mut Option<u64>, psr_needs_aux_io: bool) {
    if let Some(domain) = intel_ddi_main_link_aux_domain(encoder, state, psr_needs_aux_io) { *aux_wakeref = io.enable_power(domain); }
}

// upstream: intel_ddi.c main_link_aux_power_domain_put()
pub fn main_link_aux_power_domain_put(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, aux_wakeref: &mut Option<u64>, psr_needs_aux_io: bool) {
    if let (Some(domain), Some(cookie)) = (intel_ddi_main_link_aux_domain(encoder, state, psr_needs_aux_io), aux_wakeref.take()) { io.disable_power(domain, cookie); }
}

// upstream: intel_ddi.c intel_ddi_get_power_domains()
pub fn intel_ddi_get_power_domains(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, io_wakeref: &mut Option<u64>, aux_wakeref: &mut Option<u64>, aux_needs_aux_io: bool) {
    if state.output == OutputType::DpMst { io.warning(state.output as u32); return; }
    if !encoder.in_tbt_alt_mode { *io_wakeref = io.enable_power(0x400 + encoder.port.index() as u32); }
    main_link_aux_power_domain_get(io, encoder, state, aux_wakeref, aux_needs_aux_io);
}

// upstream: intel_ddi.c intel_ddi_enable_transcoder_clock()
pub fn intel_ddi_enable_transcoder_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.cpu_transcoder == Transcoder::Edp { return; }
    let port = if encoder.display.display_ver >= 13 { encoder.phy } else { encoder.port.index() };
    let shift = if encoder.display.display_ver >= 12 { 28 } else { 29 };
    io.write(trans_clk_sel(state.cpu_transcoder), (port as u32 + 1) << shift);
}

// upstream: intel_ddi.c intel_ddi_disable_transcoder_clock()
pub fn intel_ddi_disable_transcoder_clock(io: &mut impl DdiIo, _encoder: &DdiEncoder, state: &CrtcState) {
    if state.cpu_transcoder == Transcoder::Edp { return; }
    io.write(trans_clk_sel(state.cpu_transcoder), 0);
}

// upstream: intel_ddi.c _skl_ddi_set_iboost()
fn _skl_ddi_set_iboost(io: &mut impl DdiIo, port: Port, iboost: u8) {
    let shift = 8 + 3 * port.index() as u32;
    let mask = (0x7 << shift) | (1 << (23 + port.index() as u32));
    let value = if iboost != 0 { (iboost as u32) << shift } else { 1 << (23 + port.index() as u32) };
    io.rmw(0x8a180, mask, value);
}

// upstream: intel_ddi.c skl_ddi_set_iboost()
pub fn skl_ddi_set_iboost(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, level: usize) {
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    let mut boost = if state.output == OutputType::Hdmi { encoder.bios_hdmi_boost } else { encoder.bios_dp_boost };
    if boost == 0 {
        let Some(entry) = entries.get(level) else { io.warning(level as u32); return; };
        boost = entry.hsw_i_boost;
    }
    if boost != 0 && boost != 1 && boost != 3 && boost != 7 { io.log(6, boost as u32); return; }
    _skl_ddi_set_iboost(io, encoder.port, boost);
    if encoder.port == Port::A && encoder.max_lanes == 4 { _skl_ddi_set_iboost(io, Port::E, boost); }
}

// upstream: intel_ddi.c intel_ddi_dp_voltage_max()
pub fn intel_ddi_dp_voltage_max(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) -> u8 {
    let entries = io.buffer_trans(encoder, state);
    let n_entries = entries.len().clamp(1, INDEX_TO_DP_SIGNAL_LEVELS.len());
    if entries.is_empty() { io.warning(0); }
    INDEX_TO_DP_SIGNAL_LEVELS[n_entries - 1] & DP_TRAIN_VOLTAGE_SWING_MASK
}

// upstream: intel_ddi.c intel_ddi_dp_preemph_max()
pub const fn intel_ddi_dp_preemph_max() -> u8 { DP_TRAIN_PRE_EMPH_LEVEL_3_MASK }

// upstream: intel_ddi.c icl_combo_phy_loadgen_select()
pub const fn icl_combo_phy_loadgen_select(state: &CrtcState, lane: u8) -> u32 {
    if state.port_clock > 600_000 { 0 }
    else if state.lane_count == 4 { if lane >= 1 { 1 << 11 } else { 0 } }
    else if lane == 1 || lane == 2 { 1 << 11 } else { 0 }
}

const COMMON_KEEPER_EN: u32 = 1 << 26;
const LOADGEN_SELECT: u32 = 1 << 31;
const TX_TRAINING_EN: u32 = 1 << 31;
const EDP4K2K_MODE_OVRD_EN: u32 = 1 << 3;
const EDP4K2K_MODE_OVRD_OPTIMIZED: u32 = 1 << 2;
const SCALING_MODE_SEL_MASK: u32 = 0x7 << 18;
const RTERM_SELECT_MASK: u32 = 0x7 << 3;
const COEFF_POLARITY: u32 = 1 << 25;
const CURSOR_PROGRAM: u32 = 1 << 26;
const TAP2_DISABLE: u32 = 1 << 30;
const TAP3_DISABLE: u32 = 1 << 29;
const SWING_SEL_UPPER_MASK: u32 = 1 << 15;
const SWING_SEL_LOWER_MASK: u32 = 0x7 << 11;
const RCOMP_SCALAR_MASK: u32 = 0xff;
const POST_CURSOR_1_MASK: u32 = 0x3f << 12;
const POST_CURSOR_2_MASK: u32 = 0x3f << 6;
const CURSOR_COEFF_MASK: u32 = 0x3f;
const N_SCALAR_MASK: u32 = 0x7f << 24;

// upstream: intel_ddi.c icl_ddi_combo_vswing_program()
pub fn icl_ddi_combo_vswing_program(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    if state.output == OutputType::EmbeddedDisplayPort {
        let value = EDP4K2K_MODE_OVRD_EN | EDP4K2K_MODE_OVRD_OPTIMIZED;
        encoder.hobl_active = io.external(ExternalOperation::Phy, 0x484f424c) != 0;
        io.combo_phy_rmw(encoder.phy, ComboPhyRegister::ClDw10, value, if encoder.hobl_active { value } else { 0 });
    }
    let mut value = io.combo_phy_read(encoder.phy, ComboPhyRegister::TxDw5Lane(0));
    value &= !(SCALING_MODE_SEL_MASK | RTERM_SELECT_MASK | COEFF_POLARITY | CURSOR_PROGRAM | TAP2_DISABLE | TAP3_DISABLE);
    value |= 2 << 18 | 6 << 3 | TAP3_DISABLE;
    io.combo_phy_write(encoder.phy, ComboPhyRegister::TxDw5Group, value);
    for lane in 0..4u8 {
        let level = io.signal_level(encoder, state, lane as usize).min(entries.len() - 1);
        let entry = entries[level];
        io.combo_phy_rmw(encoder.phy, ComboPhyRegister::TxDw2Lane(lane), SWING_SEL_UPPER_MASK | SWING_SEL_LOWER_MASK | RCOMP_SCALAR_MASK,
            ((entry.icl_dw2_swing_sel as u32 >> 3) << 15) | ((entry.icl_dw2_swing_sel as u32 & 0x7) << 11) | 0x98);
    }
    for lane in 0..4u8 {
        let entry = entries[io.signal_level(encoder, state, lane as usize).min(entries.len() - 1)];
        io.combo_phy_rmw(encoder.phy, ComboPhyRegister::TxDw4Lane(lane), POST_CURSOR_1_MASK | POST_CURSOR_2_MASK | CURSOR_COEFF_MASK,
            (entry.icl_dw4_post_cursor_1 as u32) << 12 | (entry.icl_dw4_post_cursor_2 as u32) << 6 | entry.icl_dw4_cursor_coeff as u32);
    }
    for lane in 0..4u8 {
        let entry = entries[io.signal_level(encoder, state, lane as usize).min(entries.len() - 1)];
        io.combo_phy_rmw(encoder.phy, ComboPhyRegister::TxDw7Lane(lane), N_SCALAR_MASK, (entry.icl_dw7_n_scalar as u32) << 24);
    }
}

// upstream: intel_ddi.c icl_combo_phy_set_signal_levels()
pub fn icl_combo_phy_set_signal_levels(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let mut value = io.combo_phy_read(encoder.phy, ComboPhyRegister::PcsDw1Lane(0));
    if state.output == OutputType::Hdmi { value &= !COMMON_KEEPER_EN; } else { value |= COMMON_KEEPER_EN; }
    io.combo_phy_write(encoder.phy, ComboPhyRegister::PcsDw1Group, value);
    for lane in 0..4 { io.combo_phy_rmw(encoder.phy, ComboPhyRegister::TxDw4Lane(lane as u8), LOADGEN_SELECT, icl_combo_phy_loadgen_select(state, lane as u8)); }
    io.combo_phy_rmw(encoder.phy, ComboPhyRegister::ClDw5, 0, 3);
    value = io.combo_phy_read(encoder.phy, ComboPhyRegister::TxDw5Lane(0)) & !TX_TRAINING_EN;
    io.combo_phy_write(encoder.phy, ComboPhyRegister::TxDw5Group, value);
    icl_ddi_combo_vswing_program(io, encoder, state);
    value = io.combo_phy_read(encoder.phy, ComboPhyRegister::TxDw5Lane(0)) | TX_TRAINING_EN;
    io.combo_phy_write(encoder.phy, ComboPhyRegister::TxDw5Group, value);
}

// upstream: intel_ddi.c icl_mg_phy_set_signal_levels()
pub fn icl_mg_phy_set_signal_levels(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.in_tbt_alt_mode { return; }
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    const CRI_USE_FS32: u32 = 1 << 5;
    const DEEMPH17_MASK: u32 = 0x3f;
    const DEEMPH11_MASK: u32 = 0x3f << 24;
    const DEEMPH5_MASK: u32 = 0x3f << 16;
    const DEEMPH_EN: u32 = 1 << 22;
    for lane in 0..2u8 {
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx1LinkParams(lane), CRI_USE_FS32, 0);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx2LinkParams(lane), CRI_USE_FS32, 0);
    }
    for lane in 0..2u8 {
        let a = entries[io.signal_level(encoder, state, (lane * 2) as usize).min(entries.len()-1)];
        let b = entries[io.signal_level(encoder, state, (lane * 2 + 1) as usize).min(entries.len()-1)];
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx1SwingCtrl(lane), DEEMPH17_MASK, a.mg_cri_txdeemph_override_17_12 as u32);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx2SwingCtrl(lane), DEEMPH17_MASK, b.mg_cri_txdeemph_override_17_12 as u32);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx1DrvCtrl(lane), DEEMPH11_MASK | DEEMPH5_MASK,
            (a.mg_cri_txdeemph_override_11_6 as u32) << 24 | (a.mg_cri_txdeemph_override_5_0 as u32) << 16 | DEEMPH_EN);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx2DrvCtrl(lane), DEEMPH11_MASK | DEEMPH5_MASK,
            (b.mg_cri_txdeemph_override_11_6 as u32) << 24 | (b.mg_cri_txdeemph_override_5_0 as u32) << 16 | DEEMPH_EN);
    }
    for lane in 0..2u8 { io.mg_phy_rmw(encoder.port, MgPhyRegister::ClkHub(lane), 1 << 11, if state.port_clock < 300_000 { 1 << 11 } else { 0 }); }
    for lane in 0..2u8 {
        let dcc = if state.port_clock > 500_000 { (1 << 25) | (1 << 24) } else { 0 };
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx1Dcc(lane), (3 << 25) | (1 << 24), dcc);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx2Dcc(lane), (3 << 25) | (1 << 24), dcc);
    }
    for lane in 0..2u8 {
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx1PisoReadload(lane), 0, 1 << 1);
        io.mg_phy_rmw(encoder.port, MgPhyRegister::Tx2PisoReadload(lane), 0, 1 << 1);
    }
}

// upstream: intel_ddi.c tgl_dkl_phy_set_signal_levels()
pub fn tgl_dkl_phy_set_signal_levels(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.in_tbt_alt_mode { return; }
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return; }
    for lane in 0..2u8 {
        if encoder.display.wa_16011342517 {
            let affected = (state.output == OutputType::Hdmi && state.port_clock == 594_000) ||
                ((state.output == OutputType::DisplayPort || state.output == OutputType::EmbeddedDisplayPort) && state.port_clock == 162_000);
            io.dkl_phy_rmw(encoder.port, DklPhyRegister::TxDpcntl2(lane), 1 << 12, if affected { 1 << 12 } else { 0 });
        }
        io.dkl_phy_write(encoder.port, DklPhyRegister::TxPmdLaneSus(lane), 0);
        for (tx, index) in [(0u8, lane as usize * 2), (1u8, lane as usize * 2 + 1)] {
            let entry = entries[io.signal_level(encoder, state, index).min(entries.len()-1)];
            let reg = if tx == 0 { DklPhyRegister::TxDpcntl0(lane) } else { DklPhyRegister::TxDpcntl1(lane) };
            io.dkl_phy_rmw(encoder.port, reg, (0x1f << 13) | (0x1f << 8) | 0x7,
                (entry.dkl_preshoot as u32) << 13 | (entry.dkl_de_emphasis as u32) << 8 | entry.dkl_vswing as u32);
        }
        io.dkl_phy_rmw(encoder.port, DklPhyRegister::TxDpcntl2(lane), 1 << 2, 0);
        if encoder.display.alderlake_p {
            let value = if state.output == OutputType::Hdmi {
                if lane == 0 { 2 << 5 } else { (3 << 3) | (3 << 5) }
            } else { 0 };
            io.dkl_phy_rmw(encoder.port, DklPhyRegister::TxDpcntl2(lane), (3 << 3) | (3 << 5), value);
        }
    }
}

// upstream: intel_ddi.c translate_signal_level()
pub fn translate_signal_level(io: &mut impl DdiIo, signal_levels: u8) -> usize {
    INDEX_TO_DP_SIGNAL_LEVELS.iter().position(|&level| level == signal_levels).unwrap_or_else(|| { io.warning(signal_levels as u32); 0 })
}

// upstream: intel_ddi.c intel_ddi_dp_level()
pub fn intel_ddi_dp_level(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, lane: usize) -> usize {
    let train_set = encoder.train_set[lane.min(3)];
    if io.is_uhbr(state) { (train_set & DP_TX_FFE_PRESET_VALUE_MASK) as usize }
    else { translate_signal_level(io, train_set & (DP_TRAIN_VOLTAGE_SWING_MASK | DP_TRAIN_PRE_EMPHASIS_MASK)) }
}

// upstream: intel_ddi.c intel_ddi_level()
pub fn intel_ddi_level(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, lane: usize) -> usize {
    let entries = io.buffer_trans(encoder, state);
    if entries.is_empty() { io.warning(1); return 0; }
    let level = if state.output == OutputType::Hdmi { intel_ddi_hdmi_level(encoder, &entries) }
        else { intel_ddi_dp_level(io, encoder, state, lane) };
    level.min(entries.len() - 1)
}

// upstream: intel_ddi.c hsw_set_signal_levels()
pub fn hsw_set_signal_levels(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let level = intel_ddi_level(io, encoder, state, 0);
    if has_iboost(encoder.display) { skl_ddi_set_iboost(io, encoder, state, level); }
    if state.output == OutputType::Hdmi { return; }
    let signal = DDI_BUF_TRANS_SELECT(level as u32);
    encoder.dp_state &= !DDI_BUF_EMP_MASK;
    encoder.dp_state |= signal;
    io.write(ddi_buf_ctl(encoder.port), encoder.dp_state);
    io.posting_read(ddi_buf_ctl(encoder.port));
}

// upstream: intel_ddi.c _icl_ddi_enable_clock()
fn _icl_ddi_enable_clock(io: &mut impl DdiIo, reg: u32, clk_sel_mask: u32, clk_sel: u32, clk_off: u32) {
    io.lock_dpll();
    io.rmw(reg, clk_sel_mask, clk_sel);
    // The selector update and clock-on update must remain separate writes.
    io.rmw(reg, clk_off, 0);
    io.unlock_dpll();
}

// upstream: intel_ddi.c _icl_ddi_disable_clock()
fn _icl_ddi_disable_clock(io: &mut impl DdiIo, reg: u32, clk_off: u32) {
    io.lock_dpll();
    io.rmw(reg, 0, clk_off);
    io.unlock_dpll();
}

// upstream: intel_ddi.c _icl_ddi_is_clock_enabled()
fn _icl_ddi_is_clock_enabled(io: &mut impl DdiIo, reg: u32, clk_off: u32) -> bool { io.read(reg) & clk_off == 0 }

// upstream: intel_ddi.c _icl_ddi_get_pll()
fn _icl_ddi_get_pll(io: &mut impl DdiIo, display: Platform, reg: u32, clk_sel_mask: u32, clk_sel_shift: u32) -> Option<DpllId> {
    let id = (io.read(reg) & clk_sel_mask) >> clk_sel_shift;
    io.get_dpll_index(display, id)
}

const ICL_DPCLKA_CFGCR0: u32 = 0x164280;
const DPLL_CTRL2: u32 = 0x6c05c;
const PORT_CLK_SEL_MASK: u32 = 0x7 << 29;
const PORT_CLK_SEL_NONE: u32 = 0x7 << 29;
const PORT_CLK_SEL_LCPLL_2700: u32 = 0;
const PORT_CLK_SEL_LCPLL_1350: u32 = 1 << 29;
const PORT_CLK_SEL_LCPLL_810: u32 = 2 << 29;
const PORT_CLK_SEL_SPLL: u32 = 3 << 29;
const PORT_CLK_SEL_WRPLL1: u32 = 4 << 29;
const PORT_CLK_SEL_WRPLL2: u32 = 5 << 29;

fn phy_clk_reg(phy: u8, family: u32) -> u32 {
    match family { 0x46000 => if phy < 3 { 0x164280 } else { 0x1642bc },
        0x46020 => if phy < 2 { 0x164280 } else { 0x16c280 }, _ => family }
}
fn phy_clk_off(phy: u8) -> u32 {
    1 << match phy { 0 => 10, 1 => 11, 2 => 24, 3 => 4, 4 => 5, other => other as u32 + 10 }
}
fn phy_clk_sel_mask(phy: u8) -> u32 { 3 << (phy as u32 * 2) }
fn pll_hw_id(pll: DpllId) -> u32 {
    match pll { DpllId::IclDpll0 | DpllId::Dg1Dpll0 | DpllId::SklDpll0 => 0,
        DpllId::IclDpll1 | DpllId::Dg1Dpll1 | DpllId::SklDpll1 => 1,
        DpllId::Dg1Dpll2 | DpllId::SklDpll2 => 2, DpllId::Dg1Dpll3 | DpllId::SklDpll3 => 3,
        _ => pll as u32 }
}
fn phy_clk_sel(pll: DpllId, phy: u8) -> u32 { pll_hw_id(pll) << (phy as u32 * 2) }
fn rkl_phy_clk_off(phy: u8) -> u32 { 1 << (phy as u32 + 10) }
fn rkl_phy_clk_shift(phy: u8) -> u32 { match phy { 0 => 0, 1 => 2, 2 => 4, _ => 27 } }
fn rkl_phy_clk_mask(phy: u8) -> u32 { 3 << rkl_phy_clk_shift(phy) }
fn rkl_phy_clk_sel(pll: DpllId, phy: u8) -> u32 { pll_hw_id(pll) << rkl_phy_clk_shift(phy) }
fn dg1_phy_clk_off(phy: u8) -> u32 { 1 << ((phy as u32 % 2) + 10) }
fn dg1_phy_clk_mask(phy: u8) -> u32 { 3 << ((phy as u32 % 2) * 2) }
fn dg1_phy_clk_sel(pll: DpllId, phy: u8) -> u32 { (pll_hw_id(pll) % 2) << ((phy as u32 % 2) * 2) }
fn tc_phy_clk_off(port: Port) -> u32 {
    let tc_port = port.index().saturating_sub(Port::D.index()) as u32;
    1 << if tc_port < 3 { tc_port + 12 } else { tc_port - 3 + 21 }
}

// upstream: intel_ddi.c adls_ddi_enable_clock()
pub fn adls_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    let shift = (encoder.phy as u32 % 3) * 2;
    _icl_ddi_enable_clock(io, phy_clk_reg(encoder.phy, 0x46000), 3 << shift, pll_hw_id(state.pll_id) << shift, phy_clk_off(encoder.phy));
}

// upstream: intel_ddi.c adls_ddi_disable_clock()
pub fn adls_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { _icl_ddi_disable_clock(io, phy_clk_reg(encoder.phy, 0x46000), phy_clk_off(encoder.phy)); }

// upstream: intel_ddi.c adls_ddi_is_clock_enabled()
pub fn adls_ddi_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { _icl_ddi_is_clock_enabled(io, phy_clk_reg(encoder.phy, 0x46000), phy_clk_off(encoder.phy)) }

// upstream: intel_ddi.c adls_ddi_get_pll()
pub fn adls_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> { let shift = (encoder.phy as u32 % 3) * 2; _icl_ddi_get_pll(io, encoder.display, phy_clk_reg(encoder.phy, 0x46000), 3 << shift, shift) }

// upstream: intel_ddi.c rkl_ddi_enable_clock()
pub fn rkl_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    _icl_ddi_enable_clock(io, ICL_DPCLKA_CFGCR0, rkl_phy_clk_mask(encoder.phy), rkl_phy_clk_sel(state.pll_id, encoder.phy), rkl_phy_clk_off(encoder.phy));
}

// upstream: intel_ddi.c rkl_ddi_disable_clock()
pub fn rkl_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { _icl_ddi_disable_clock(io, ICL_DPCLKA_CFGCR0, rkl_phy_clk_off(encoder.phy)); }

// upstream: intel_ddi.c rkl_ddi_is_clock_enabled()
pub fn rkl_ddi_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { _icl_ddi_is_clock_enabled(io, ICL_DPCLKA_CFGCR0, rkl_phy_clk_off(encoder.phy)) }

// upstream: intel_ddi.c rkl_ddi_get_pll()
pub fn rkl_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> { let shift = rkl_phy_clk_shift(encoder.phy); _icl_ddi_get_pll(io, encoder.display, ICL_DPCLKA_CFGCR0, rkl_phy_clk_mask(encoder.phy), shift) }

// upstream: intel_ddi.c dg1_ddi_enable_clock()
pub fn dg1_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    let pll_lo = matches!(state.pll_id, DpllId::Dg1Dpll0 | DpllId::Dg1Dpll1);
    if (pll_lo && encoder.phy >= 2) || (!pll_lo && encoder.phy < 2) { io.warning(encoder.phy as u32); return; }
    _icl_ddi_enable_clock(io, phy_clk_reg(encoder.phy, 0x46020), dg1_phy_clk_mask(encoder.phy), dg1_phy_clk_sel(state.pll_id, encoder.phy), dg1_phy_clk_off(encoder.phy));
}

// upstream: intel_ddi.c dg1_ddi_disable_clock()
pub fn dg1_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { _icl_ddi_disable_clock(io, phy_clk_reg(encoder.phy, 0x46020), dg1_phy_clk_off(encoder.phy)); }

// upstream: intel_ddi.c dg1_ddi_is_clock_enabled()
pub fn dg1_ddi_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { _icl_ddi_is_clock_enabled(io, phy_clk_reg(encoder.phy, 0x46020), dg1_phy_clk_off(encoder.phy)) }

// upstream: intel_ddi.c dg1_ddi_get_pll()
pub fn dg1_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> {
    let id = _icl_ddi_get_pll(io, encoder.display, phy_clk_reg(encoder.phy, 0x46020), dg1_phy_clk_mask(encoder.phy), (encoder.phy as u32 % 2) * 2)?;
    if encoder.phy >= 2 { io.get_dpll_index(encoder.display, pll_hw_id(id) + 2) } else { Some(id) }
}

// upstream: intel_ddi.c icl_ddi_combo_enable_clock()
pub fn icl_ddi_combo_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    _icl_ddi_enable_clock(io, ICL_DPCLKA_CFGCR0, phy_clk_sel_mask(encoder.phy), phy_clk_sel(state.pll_id, encoder.phy), phy_clk_off(encoder.phy));
}

// upstream: intel_ddi.c icl_ddi_combo_disable_clock()
pub fn icl_ddi_combo_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { _icl_ddi_disable_clock(io, ICL_DPCLKA_CFGCR0, phy_clk_off(encoder.phy)); }

// upstream: intel_ddi.c icl_ddi_combo_is_clock_enabled()
pub fn icl_ddi_combo_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { _icl_ddi_is_clock_enabled(io, ICL_DPCLKA_CFGCR0, phy_clk_off(encoder.phy)) }

// upstream: intel_ddi.c icl_ddi_combo_get_pll()
pub fn icl_ddi_combo_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> { let shift = encoder.phy as u32 * 2; _icl_ddi_get_pll(io, encoder.display, ICL_DPCLKA_CFGCR0, 3 << shift, shift) }

// upstream: intel_ddi.c jsl_ddi_tc_enable_clock()
pub fn jsl_ddi_tc_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    io.write(port_clk_sel(encoder.port), DDI_CLK_SEL_MG);
    icl_ddi_combo_enable_clock(io, encoder, state);
}

// upstream: intel_ddi.c jsl_ddi_tc_disable_clock()
pub fn jsl_ddi_tc_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    icl_ddi_combo_disable_clock(io, encoder);
    io.write(port_clk_sel(encoder.port), DDI_CLK_SEL_NONE);
}

// upstream: intel_ddi.c jsl_ddi_tc_is_clock_enabled()
pub fn jsl_ddi_tc_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool {
    if io.read(port_clk_sel(encoder.port)) & DDI_CLK_SEL_MASK == DDI_CLK_SEL_NONE { return false; }
    icl_ddi_combo_is_clock_enabled(io, encoder)
}

// upstream: intel_ddi.c icl_ddi_tc_enable_clock()
pub fn icl_ddi_tc_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    io.write(port_clk_sel(encoder.port), icl_pll_to_ddi_clk_sel(state.pll_id, state.port_clock));
    io.lock_dpll(); io.rmw(ICL_DPCLKA_CFGCR0, tc_phy_clk_off(encoder.port), 0); io.unlock_dpll();
}

// upstream: intel_ddi.c icl_ddi_tc_disable_clock()
pub fn icl_ddi_tc_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.lock_dpll(); io.rmw(ICL_DPCLKA_CFGCR0, 0, tc_phy_clk_off(encoder.port)); io.unlock_dpll();
    io.write(port_clk_sel(encoder.port), DDI_CLK_SEL_NONE);
}

// upstream: intel_ddi.c icl_ddi_tc_is_clock_enabled()
pub fn icl_ddi_tc_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool {
    if io.read(port_clk_sel(encoder.port)) & DDI_CLK_SEL_MASK == DDI_CLK_SEL_NONE { return false; }
    io.read(ICL_DPCLKA_CFGCR0) & tc_phy_clk_off(encoder.port) == 0
}

// upstream: intel_ddi.c icl_ddi_tc_get_pll()
pub fn icl_ddi_tc_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> {
    let clock = io.read(port_clk_sel(encoder.port)) & DDI_CLK_SEL_MASK;
    let id = match clock { DDI_CLK_SEL_TBT_162 | DDI_CLK_SEL_TBT_270 | DDI_CLK_SEL_TBT_540 | DDI_CLK_SEL_TBT_810 => DpllId::IclTbt,
        DDI_CLK_SEL_MG => match encoder.port { Port::D => DpllId::IclMg1, Port::E => DpllId::IclMg2, Port::F => DpllId::IclMg3,
            Port::G => DpllId::IclMg4, Port::H => DpllId::TglMg5, Port::I => DpllId::TglMg6, _ => DpllId::None },
        DDI_CLK_SEL_NONE => DpllId::None, other => { io.warning(other); DpllId::None } };
    io.get_dpll(id)
}

// upstream: intel_ddi.c bxt_ddi_get_pll()
pub fn bxt_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> {
    let id = match encoder.port { Port::A => DpllId::SklDpll0, Port::B => DpllId::SklDpll1, Port::C => DpllId::SklDpll2,
        _ => { io.warning(encoder.port.index() as u32); return None; } };
    io.get_dpll(id)
}

// upstream: intel_ddi.c skl_ddi_enable_clock()
pub fn skl_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    let shift = encoder.port.index() as u32 * 3 + 1;
    let mask = (0x3 << shift) | (1 << (encoder.port.index() as u32 * 3));
    let select = (pll_hw_id(state.pll_id) << shift) | (1 << (encoder.port.index() as u32 * 3));
    io.lock_dpll(); io.rmw(DPLL_CTRL2, (1 << (encoder.port.index() as u32 + 15)) | mask, select); io.unlock_dpll();
}

// upstream: intel_ddi.c skl_ddi_disable_clock()
pub fn skl_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.lock_dpll(); io.rmw(DPLL_CTRL2, 0, 1 << (encoder.port.index() as u32 + 15)); io.unlock_dpll();
}

// upstream: intel_ddi.c skl_ddi_is_clock_enabled()
pub fn skl_ddi_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { io.read(DPLL_CTRL2) & (1 << (encoder.port.index() as u32 + 15)) == 0 }

// upstream: intel_ddi.c skl_ddi_get_pll()
pub fn skl_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> {
    let value = io.read(DPLL_CTRL2);
    if value & (1 << (encoder.port.index() as u32 * 3)) == 0 { return None; }
    let id = (value >> (encoder.port.index() as u32 * 3 + 1)) & 0x3;
    io.get_dpll_index(encoder.display, id)
}

// upstream: intel_ddi.c hsw_ddi_enable_clock()
pub fn hsw_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.pll_id == DpllId::None { io.warning(0); return; }
    io.write(0x46100 + encoder.port.index() as u32 * 4, hsw_pll_to_ddi_pll_sel(state.pll_id));
}

// upstream: intel_ddi.c hsw_ddi_disable_clock()
pub fn hsw_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { io.write(0x46100 + encoder.port.index() as u32 * 4, PORT_CLK_SEL_NONE); }

// upstream: intel_ddi.c hsw_ddi_is_clock_enabled()
pub fn hsw_ddi_is_clock_enabled(io: &mut impl DdiIo, encoder: &DdiEncoder) -> bool { io.read(0x46100 + encoder.port.index() as u32 * 4) != PORT_CLK_SEL_NONE }

// upstream: intel_ddi.c hsw_ddi_get_pll()
pub fn hsw_ddi_get_pll(io: &mut impl DdiIo, encoder: &DdiEncoder) -> Option<DpllId> {
    let id = match io.read(0x46100 + encoder.port.index() as u32 * 4) & PORT_CLK_SEL_MASK {
        PORT_CLK_SEL_LCPLL_2700 => DpllId::Lcpll2700, PORT_CLK_SEL_LCPLL_1350 => DpllId::Lcpll1350, PORT_CLK_SEL_LCPLL_810 => DpllId::Lcpll810,
        PORT_CLK_SEL_SPLL => DpllId::Spll, PORT_CLK_SEL_WRPLL1 => DpllId::Wrpll1, PORT_CLK_SEL_WRPLL2 => DpllId::Wrpll2,
        PORT_CLK_SEL_NONE => DpllId::None, value => { io.warning(value); DpllId::None } };
    io.get_dpll(id)
}

// upstream: intel_ddi.c intel_ddi_enable_clock()
pub fn intel_ddi_enable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) { io.encoder_clock_enable(encoder, state); }

// upstream: intel_ddi.c intel_ddi_disable_clock()
pub fn intel_ddi_disable_clock(io: &mut impl DdiIo, encoder: &DdiEncoder) { io.encoder_clock_disable(encoder); }

// upstream: intel_ddi.c intel_ddi_sanitize_encoder_pll_mapping()
pub fn intel_ddi_sanitize_encoder_pll_mapping(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if encoder.output == OutputType::DpMst { return; }
    if !encoder.has_crtc && (encoder.output == OutputType::DisplayPort || encoder.output == OutputType::EmbeddedDisplayPort) {
        let (_, is_mst) = intel_ddi_get_encoder_pipes(io, encoder);
        if is_mst { io.warning(1); return; }
    }
    let mut ddi_clk_needed = encoder.has_crtc;
    if encoder.output == OutputType::Dsi { ddi_clk_needed = false; }
    let clock_enabled = io.external(ExternalOperation::Dpll, ((encoder.port.index() as u32) << 16) | 1) != 0;
    if ddi_clk_needed || !clock_enabled { return; }
    io.log(7, encoder.port.index() as u32);
    intel_ddi_disable_clock(io, encoder);
}

// upstream: intel_ddi.c tgl_dkl_phy_check_and_rewrite()
pub fn tgl_dkl_phy_check_and_rewrite(io: &mut impl DdiIo, port: Port, lane0: u32, lane1: u32) {
    for (lane, expected) in [(0u8, lane0), (1, lane1)] {
        let reg = DklPhyRegister::DpMode(lane);
        if io.dkl_phy_read(port, reg) != expected { io.dkl_phy_write(port, reg, expected); }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TcPinAssignment { #[default] None, A, B, C, D, E, F }

// upstream: intel_ddi.c icl_program_mg_dp_mode()
pub fn icl_program_mg_dp_mode(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.display.display_ver >= 14 || !encoder.is_tc || encoder.in_tbt_alt_mode { return; }
    let pin = match io.external(ExternalOperation::TcPort, 0xc00 | encoder.port.index() as u32) {
        1 => TcPinAssignment::A, 2 => TcPinAssignment::B, 3 => TcPinAssignment::C,
        4 => TcPinAssignment::D, 5 => TcPinAssignment::E, 6 => TcPinAssignment::F,
        _ => TcPinAssignment::None,
    };
    let dkl = encoder.display.display_ver >= 12;
    let mut lane0 = if dkl { io.dkl_phy_read(encoder.port, DklPhyRegister::DpMode(0)) } else { io.mg_dp_mode_read(encoder.port, 0) };
    let mut lane1 = if dkl { io.dkl_phy_read(encoder.port, DklPhyRegister::DpMode(1)) } else { io.mg_dp_mode_read(encoder.port, 1) };
    const X1: u32 = 1 << 6; const X2: u32 = 1 << 7;
    lane0 &= !(X1 | X2); lane1 &= !(X1 | X2);
    let width = state.lane_count;
    match pin {
        TcPinAssignment::None => { if width == 1 { lane1 |= X1; } else { lane0 |= X2; lane1 |= X2; } }
        TcPinAssignment::A => if width == 4 { lane0 |= X2; lane1 |= X2; },
        TcPinAssignment::B => if width == 2 { lane0 |= X2; lane1 |= X2; },
        TcPinAssignment::C | TcPinAssignment::D | TcPinAssignment::E | TcPinAssignment::F => {
            if width == 1 { lane0 |= X1; lane1 |= X1; } else { lane0 |= X2; lane1 |= X2; }
        }
    }
    if dkl {
        io.dkl_phy_write(encoder.port, DklPhyRegister::DpMode(0), lane0);
        io.dkl_phy_write(encoder.port, DklPhyRegister::DpMode(1), lane1);
    } else {
        io.mg_dp_mode_write(encoder.port, 0, lane0);
        io.mg_dp_mode_write(encoder.port, 1, lane1);
    }
    if (12..=13).contains(&encoder.display.display_ver) { tgl_dkl_phy_check_and_rewrite(io, encoder.port, lane0, lane1); }
}

// upstream: intel_ddi.c tgl_dp_tp_transcoder()
pub const fn tgl_dp_tp_transcoder(state: &CrtcState) -> Transcoder { if matches!(state.output, OutputType::DpMst) { state.mst_master_transcoder } else { state.cpu_transcoder } }

// upstream: intel_ddi.c dp_tp_ctl_reg()
pub fn dp_tp_ctl_reg(encoder: &DdiEncoder, state: &CrtcState) -> u32 { if encoder.display.display_ver >= 12 { dp_tp_transcoder_reg(tgl_dp_tp_transcoder(state), false) } else { dp_tp_port_reg(encoder.port, false) } }

// upstream: intel_ddi.c dp_tp_status_reg()
pub fn dp_tp_status_reg(encoder: &DdiEncoder, state: &CrtcState) -> u32 { if encoder.display.display_ver >= 12 { dp_tp_transcoder_reg(tgl_dp_tp_transcoder(state), true) } else { dp_tp_port_reg(encoder.port, true) } }

// upstream: intel_ddi.c intel_ddi_clear_act_sent()
pub fn intel_ddi_clear_act_sent(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) { io.write(dp_tp_status_reg(encoder, state), 1 << 24); }

// upstream: intel_ddi.c intel_ddi_wait_for_act_sent()
pub fn intel_ddi_wait_for_act_sent(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) { if io.wait_set(dp_tp_status_reg(encoder, state), 1 << 24, 1) { io.log(8, encoder.port.index() as u32); } }

// upstream: intel_ddi.c intel_dp_sink_set_msa_timing_par_ignore_state()
pub fn intel_dp_sink_set_msa_timing_par_ignore_state(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, enable: bool) {
    if !state.has_vrr { return; }
    let value = if enable { 1 << 7 } else { 0 };
    if io.aux_write(encoder.port, 0x107, &[value]).map_or(true, |count| count <= 0) { io.log(9, value as u32); }
}

// upstream: intel_ddi.c intel_dp_sink_set_fec_ready()
pub fn intel_dp_sink_set_fec_ready(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, enable: bool) {
    if !state.fec_enable { return; }
    let value = if enable { 1 } else { 0 };
    if io.aux_write(encoder.port, 0x120, &[value]).map_or(true, |count| count <= 0) { io.log(10, value as u32); }
    if enable && io.aux_write(encoder.port, 0x280, &[0x3]).map_or(true, |count| count <= 0) { io.log(11, 3); }
}

// upstream: intel_ddi.c wait_for_fec_detected()
pub fn wait_for_fec_detected(io: &mut impl DdiIo, port: Port, enabled: bool) -> i32 {
    let mask = if enabled { 1 << 0 } else { 1 << 1 };
    let mut status = 0u8;
    let mut last_error = 0;
    for attempt in 0..=20 {
        let mut byte = [0u8; 1];
        match io.aux_read(port, 0x280, &mut byte) {
            Ok(1) => { status = byte[0]; last_error = 0; }
            Ok(_) => { last_error = -5; }
            Err(error) => { last_error = error; }
        }
        if last_error != 0 || status & mask != 0 { break; }
        if attempt != 20 { io.delay_us(10_000, 10_000); }
    }
    if last_error != 0 { io.log(12, last_error as u32); return last_error; }
    if status & mask == 0 { io.log(12, status as u32); -110 } else { 0 }
}

// upstream: intel_ddi.c intel_ddi_wait_for_fec_status()
pub fn intel_ddi_wait_for_fec_status(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, enabled: bool) -> i32 {
    if !state.fec_enable { return 0; }
    let status = dp_tp_status_reg(encoder, state);
    let timeout = if enabled { io.wait_set(status, 1 << 28, 1) } else { io.wait_clear(status, 1 << 28, 1) };
    if timeout { io.log(13, enabled as u32); return -110; }
    if enabled { wait_for_fec_detected(io, encoder.port, enabled) } else { 0 }
}

// upstream: intel_ddi.c intel_ddi_enable_fec()
pub fn intel_ddi_enable_fec(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if !state.fec_enable { return; }
    let reg = dp_tp_ctl_reg(encoder, state);
    io.rmw(reg, 0, 1 << 30);
    if encoder.display.display_ver < 30 || intel_ddi_wait_for_fec_status(io, encoder, state, true) == 0 { return; }
    for attempt in 0..3 {
        io.log(14, attempt);
        io.rmw(reg, 1 << 30, 0);
        if intel_ddi_wait_for_fec_status(io, encoder, state, false) != 0 { continue; }
        io.rmw(reg, 0, 1 << 30);
        if intel_ddi_wait_for_fec_status(io, encoder, state, true) == 0 { return; }
    }
    io.log(15, encoder.port.index() as u32);
}

// upstream: intel_ddi.c intel_ddi_disable_fec()
pub fn intel_ddi_disable_fec(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if !state.fec_enable { return; }
    let reg = dp_tp_ctl_reg(encoder, state);
    io.rmw(reg, 1 << 30, 0);
    io.posting_read(reg);
}

// upstream: intel_ddi.c intel_ddi_power_up_lanes()
pub fn intel_ddi_power_up_lanes(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.output == OutputType::Hdmi || encoder.phy == 0xff { return; }
    let args = encoder.phy as u32 | ((state.lane_count as u32) << 8) | ((encoder.lane_reversal as u32) << 16);
    io.external(ExternalOperation::Phy, args);
}

// upstream: intel_ddi.c intel_ddi_splitter_pipe_mask()
pub const fn intel_ddi_splitter_pipe_mask(display: Platform) -> u8 {
    if display.display_ver > 20 { u8::MAX } else if display.alderlake_p { 0b11 } else { 0b1 }
}

// upstream: intel_ddi.c intel_ddi_mso_get_config()
pub fn intel_ddi_mso_get_config(io: &mut impl DdiIo, display: Platform, state: &mut CrtcState) {
    if !display.has_mso { return; }
    const ENABLE: u32 = 1 << 31; const CONFIG_MASK: u32 = 0x3 << 25; const OVERLAP_MASK: u32 = 0xf << 16;
    let reg = 0x78000 + state.pipe.index() as u32 * 0x200;
    let value = io.read(reg);
    state.splitter.enable = value & ENABLE != 0;
    if !state.splitter.enable { return; }
    if intel_ddi_splitter_pipe_mask(display) & (1 << state.pipe.index()) == 0 { io.warning(value); state.splitter.enable = false; return; }
    state.splitter.link_count = if value & CONFIG_MASK == (1 << 25) { 4 } else { 2 };
    state.splitter.pixel_overlap = ((value & OVERLAP_MASK) >> 16) as u8;
}

// upstream: intel_ddi.c intel_ddi_mso_configure()
pub fn intel_ddi_mso_configure(io: &mut impl DdiIo, display: Platform, state: &CrtcState) {
    if !display.has_mso { return; }
    const ENABLE: u32 = 1 << 31; const CONFIG_MASK: u32 = 0x3 << 25; const OVERLAP_MASK: u32 = 0xf << 16;
    let mut value = 0;
    if state.splitter.enable {
        value |= ENABLE | ((state.splitter.pixel_overlap as u32) << 16);
        value |= if state.splitter.link_count == 2 { 0 } else { 1 << 25 };
    }
    io.rmw(0x78000 + state.pipe.index() as u32 * 0x200, ENABLE | CONFIG_MASK | OVERLAP_MASK, value);
}

// upstream: intel_ddi.c mtl_ddi_enable_d2d()
pub fn mtl_ddi_enable_d2d(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if encoder.display.display_ver < 14 { return; }
    let (reg, set, status) = if encoder.display.display_ver >= 20 { (ddi_buf_ctl(encoder.port), 1 << 29, 1 << 28) }
        else { (ddi_buf_status(encoder.port, 14), 1 << 29, 1 << 28) };
    io.rmw(reg, 0, set);
    if io.wait_set_us(reg, status, 100) { io.log(16, encoder.port.index() as u32); }
}

// upstream: intel_ddi.c mtl_port_buf_ctl_program()
pub fn mtl_port_buf_ctl_program(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    let mut value = ddi_buf_port_width(state.lane_count);
    value |= if io.is_uhbr(state) { DDI_BUF_PORT_DATA_40BIT } else { DDI_BUF_PORT_DATA_10BIT };
    if encoder.lane_reversal { value |= DDI_BUF_PORT_REVERSAL; }
    io.rmw(ddi_buf_status(encoder.port, 14), DDI_PORT_WIDTH_MASK | DDI_BUF_PORT_DATA_MASK, value);
}

// upstream: intel_ddi.c mtl_port_buf_ctl_io_selection()
pub fn mtl_port_buf_ctl_io_selection(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.rmw(ddi_buf_status(encoder.port, 14), 1 << 11, if encoder.in_tbt_alt_mode { 1 << 11 } else { 0 });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DpEnablePath { Mtl, Tgl, Hsw }

fn pre_enable_dp_path(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState, path: DpEnablePath) {
    let mst = state.output == OutputType::DpMst;
    let params = state.port_clock | ((state.lane_count as u32) << 24);
    io.external(ExternalOperation::LinkParams, params);
    encoder.dp_state = intel_ddi_init_dp_buf_reg(io, encoder, state);

    match path {
        DpEnablePath::Mtl => {
            mtl_port_buf_ctl_io_selection(io, encoder);
            intel_ddi_enable_clock(io, encoder, state);
            intel_ddi_enable_transcoder_clock(io, encoder, state);
            intel_ddi_config_transcoder_func(io, encoder, state);
            intel_ddi_mso_configure(io, encoder.display, state);
            if !mst { io.external(ExternalOperation::DpAux, 0x6000); }
            io.external(ExternalOperation::DpAux, 0x100 | (state.output as u32));
            io.external(ExternalOperation::ProtocolConverter, state.port_clock);
            if !mst { io.external(ExternalOperation::SinkDecompression, state.pipe_bpp as u32); }
            intel_dp_sink_set_fec_ready(io, encoder, state, true);
            io.external(ExternalOperation::FrlTraining, 0);
            io.external(ExternalOperation::PconDsc, state.pipe_bpp as u32);
            io.external(ExternalOperation::LinkTrainStart, state.port_clock);
            if !state.sync_mode { io.external(ExternalOperation::LinkTrainStop, state.port_clock); }
            intel_ddi_enable_fec(io, encoder, state);
            if !mst && io.is_uhbr(state) {
                let ret = io.external(ExternalOperation::LinkPayload, state.payload_tu as u32) as i32;
                if ret < 0 { io.external(ExternalOperation::MstRetry, encoder.port.index() as u32); }
            }
            if !mst { io.external(ExternalOperation::DscPps, state.pipe_bpp as u32); }
        }
        DpEnablePath::Tgl => {
            io.external(ExternalOperation::PanelPower, 1);
            intel_ddi_enable_clock(io, encoder, state);
            if !encoder.in_tbt_alt_mode { let _ = io.enable_power(0x400 + encoder.port.index() as u32); }
            icl_program_mg_dp_mode(io, encoder, state);
            intel_ddi_enable_transcoder_clock(io, encoder, state);
            intel_ddi_config_transcoder_func(io, encoder, state);
            io.external(ExternalOperation::SignalLevels, state.port_clock);
            intel_ddi_power_up_lanes(io, encoder, state);
            intel_ddi_mso_configure(io, encoder.display, state);
            if !mst { io.external(ExternalOperation::DpAux, 0x6000); }
            io.external(ExternalOperation::ProtocolConverter, state.port_clock);
            if !mst { io.external(ExternalOperation::SinkDecompression, state.pipe_bpp as u32); }
            intel_dp_sink_set_fec_ready(io, encoder, state, true);
            io.external(ExternalOperation::FrlTraining, 0);
            io.external(ExternalOperation::PconDsc, state.pipe_bpp as u32);
            io.external(ExternalOperation::LinkTrainStart, state.port_clock);
            if !state.sync_mode { io.external(ExternalOperation::LinkTrainStop, state.port_clock); }
            intel_ddi_enable_fec(io, encoder, state);
            if !mst && io.is_uhbr(state) {
                let ret = io.external(ExternalOperation::LinkPayload, state.payload_tu as u32) as i32;
                if ret < 0 { io.external(ExternalOperation::MstRetry, encoder.port.index() as u32); }
            }
            if !mst { io.external(ExternalOperation::DscPps, state.pipe_bpp as u32); }
        }
        DpEnablePath::Hsw => {
            if encoder.display.display_ver < 11 && mst && (encoder.port == Port::A || encoder.port == Port::E) { io.warning(1); }
            else if encoder.display.display_ver >= 11 && mst && encoder.port == Port::A { io.warning(1); }
            io.external(ExternalOperation::PanelPower, 1);
            intel_ddi_enable_clock(io, encoder, state);
            if !encoder.in_tbt_alt_mode { let _ = io.enable_power(0x400 + encoder.port.index() as u32); }
            icl_program_mg_dp_mode(io, encoder, state);
            if has_buf_trans_select(encoder.display) { hsw_prepare_dp_ddi_buffers(io, encoder, state); }
            io.external(ExternalOperation::SignalLevels, state.port_clock);
            intel_ddi_power_up_lanes(io, encoder, state);
            if !mst { io.external(ExternalOperation::DpAux, 0x6000); }
            io.external(ExternalOperation::ProtocolConverter, state.port_clock);
            if !mst { io.external(ExternalOperation::SinkDecompression, state.pipe_bpp as u32); }
            intel_dp_sink_set_fec_ready(io, encoder, state, true);
            io.external(ExternalOperation::LinkTrainStart, state.port_clock);
            if (encoder.port != Port::A || encoder.display.display_ver >= 9) && !state.sync_mode { io.external(ExternalOperation::LinkTrainStop, state.port_clock); }
            intel_ddi_enable_fec(io, encoder, state);
            if !mst { intel_ddi_enable_transcoder_clock(io, encoder, state); io.external(ExternalOperation::DscPps, state.pipe_bpp as u32); }
        }
    }
}

// upstream: intel_ddi.c mtl_ddi_pre_enable_dp()
pub fn mtl_ddi_pre_enable_dp(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) { pre_enable_dp_path(io, encoder, state, DpEnablePath::Mtl); }

// upstream: intel_ddi.c tgl_ddi_pre_enable_dp()
pub fn tgl_ddi_pre_enable_dp(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) { pre_enable_dp_path(io, encoder, state, DpEnablePath::Tgl); }

// upstream: intel_ddi.c hsw_ddi_pre_enable_dp()
pub fn hsw_ddi_pre_enable_dp(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) { pre_enable_dp_path(io, encoder, state, DpEnablePath::Hsw); }

// upstream: intel_ddi.c intel_ddi_pre_enable_dp()
pub fn intel_ddi_pre_enable_dp(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    if encoder.display.has_dp20 { io.external(ExternalOperation::DpSdpCrc, state.port_clock); }
    io.external(ExternalOperation::PanelReplay, 1);
    if encoder.display.display_ver >= 14 { mtl_ddi_pre_enable_dp(io, encoder, state); }
    else if encoder.display.display_ver >= 12 { tgl_ddi_pre_enable_dp(io, encoder, state); }
    else { hsw_ddi_pre_enable_dp(io, encoder, state); }
    if state.output != OutputType::DpMst { intel_ddi_set_dp_msa(io, state, true); }
}

// upstream: intel_ddi.c intel_ddi_pre_enable_hdmi()
pub fn intel_ddi_pre_enable_hdmi(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::DualModeTmds, 1);
    intel_ddi_enable_clock(io, encoder, state);
    let _ = io.enable_power(0x400 + encoder.port.index() as u32);
    icl_program_mg_dp_mode(io, encoder, state);
    intel_ddi_enable_transcoder_clock(io, encoder, state);
    io.external(ExternalOperation::Infoframes, state.has_infoframe as u32);
}

// upstream: intel_ddi.c intel_ddi_pre_enable()
pub fn intel_ddi_pre_enable(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState, lspcon_active: bool) {
    if state.has_pch_encoder { io.warning(1); }
    io.external(ExternalOperation::FifoUnderrun, state.pipe.index() as u32 | 1 << 8);
    if matches!(state.output, OutputType::Hdmi | OutputType::Dvi) {
        intel_ddi_pre_enable_hdmi(io, encoder, state);
    } else {
        intel_ddi_pre_enable_dp(io, encoder, state);
        if lspcon_active && state.has_hdmi_sink { io.external(ExternalOperation::Infoframes, state.has_infoframe as u32); }
    }
}

// upstream: intel_ddi.c mtl_ddi_disable_d2d()
pub fn mtl_ddi_disable_d2d(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if encoder.display.display_ver < 14 { return; }
    let (reg, clear, status) = if encoder.display.display_ver >= 20 { (ddi_buf_ctl(encoder.port), 1 << 29, 1 << 28) }
        else { (ddi_buf_status(encoder.port, 14), 1 << 29, 1 << 28) };
    io.rmw(reg, clear, 0);
    if io.wait_clear_us(reg, status, 100) { io.log(17, encoder.port.index() as u32); }
}

// upstream: intel_ddi.c intel_ddi_buf_enable()
pub fn intel_ddi_buf_enable(io: &mut impl DdiIo, encoder: &DdiEncoder, buf_ctl: u32) {
    let reg = ddi_buf_ctl(encoder.port);
    io.write(reg, buf_ctl | DDI_BUF_CTL_ENABLE);
    io.posting_read(reg);
    intel_wait_ddi_buf_active(io, encoder);
}

// upstream: intel_ddi.c intel_ddi_buf_disable()
pub fn intel_ddi_buf_disable(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.rmw(ddi_buf_ctl(encoder.port), DDI_BUF_CTL_ENABLE, 0);
    if encoder.display.display_ver >= 14 { intel_wait_ddi_buf_idle(io, encoder.display, encoder.port); }
    mtl_ddi_disable_d2d(io, encoder);
    if matches!(state.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst | OutputType::Analog) {
        io.rmw(dp_tp_ctl_reg(encoder, state), 1 << 31, 0);
    }
    intel_ddi_disable_fec(io, encoder, state);
    if encoder.display.display_ver < 14 { intel_wait_ddi_buf_idle(io, encoder.display, encoder.port); }
    let _ = intel_ddi_wait_for_fec_status(io, encoder, state, false);
}

// upstream: intel_ddi.c intel_ddi_post_disable_dp()
pub fn intel_ddi_post_disable_dp(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    let mst = state.output == OutputType::DpMst;
    if !mst { io.external(ExternalOperation::Infoframes, 0); }
    io.external(ExternalOperation::DpAux, 0x6003);
    if encoder.display.display_ver >= 12 {
        if mst || io.is_uhbr(state) { io.rmw(trans_ddi_func_ctl(state.cpu_transcoder), (0xf << 28) | (0x7 << 24), 0); }
    } else if !mst { intel_ddi_disable_transcoder_clock(io, encoder, state); }
    intel_ddi_buf_disable(io, encoder, state);
    intel_dp_sink_set_fec_ready(io, encoder, state, false);
    intel_ddi_config_transcoder_dp2(io, encoder.display, state, false);
    if encoder.display.display_ver >= 12 { intel_ddi_disable_transcoder_clock(io, encoder, state); }
    io.external(ExternalOperation::PanelPower, 0x100);
    io.external(ExternalOperation::PowerDomain, 0x800 + encoder.port.index() as u32);
    intel_ddi_disable_clock(io, encoder);
    if encoder.display.display_ver >= 14 { io.rmw(ddi_buf_status(encoder.port, 14), 1 << 11, 0); }
}

// upstream: intel_ddi.c intel_ddi_post_disable_hdmi()
pub fn intel_ddi_post_disable_hdmi(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::Infoframes, 0);
    if encoder.display.display_ver < 12 { intel_ddi_disable_transcoder_clock(io, encoder, state); }
    intel_ddi_buf_disable(io, encoder, state);
    if encoder.display.display_ver >= 12 { intel_ddi_disable_transcoder_clock(io, encoder, state); }
    io.external(ExternalOperation::PowerDomain, 0x800 + encoder.port.index() as u32);
    intel_ddi_disable_clock(io, encoder);
    io.external(ExternalOperation::DualModeTmds, 0);
}

// upstream: intel_ddi.c intel_ddi_post_disable_hdmi_or_sst()
pub fn intel_ddi_post_disable_hdmi_or_sst(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::AtomicState, 0x1000 | state.pipe.index() as u32);
    io.external(ExternalOperation::AtomicState, 0x2000 | state.cpu_transcoder as u32);
    if state.output != OutputType::Hdmi && io.is_uhbr(state) {
        io.external(ExternalOperation::LinkPayload, 0);
        intel_ddi_clear_act_sent(io, encoder, state);
        io.rmw(trans_ddi_func_ctl(state.cpu_transcoder), 0, 1 << 21);
        intel_ddi_wait_for_act_sent(io, encoder, state);
        io.external(ExternalOperation::DpAux, 0x3000);
    }
    io.external(ExternalOperation::Psr, 0);
    intel_ddi_disable_transcoder_func(io, encoder, state);
    io.external(ExternalOperation::DscPps, 0);
    io.external(ExternalOperation::Scaler, state.pipe.index() as u32);
}

// upstream: intel_ddi.c intel_ddi_post_disable()
pub fn intel_ddi_post_disable(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if state.output != OutputType::DpMst { intel_ddi_post_disable_hdmi_or_sst(io, encoder, state); }
    if matches!(state.output, OutputType::Hdmi | OutputType::Dvi) { intel_ddi_post_disable_hdmi(io, encoder, state); }
    else { intel_ddi_post_disable_dp(io, encoder, state); }
}

// upstream: intel_ddi.c intel_ddi_post_pll_disable()
pub fn intel_ddi_post_pll_disable(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, aux_wakeref: &mut Option<u64>) {
    main_link_aux_power_domain_put(io, encoder, state, aux_wakeref, false);
    if encoder.is_tc { io.external(ExternalOperation::TcPort, 0); }
}

// upstream: intel_ddi.c trans_port_sync_stop_link_train()
pub fn trans_port_sync_stop_link_train(io: &mut impl DdiIo, _encoder: &DdiEncoder, state: &CrtcState) {
    if state.sync_mode_slaves_mask == 0 { return; }
    for transcoder in 0..8 {
        if state.sync_mode_slaves_mask & (1 << transcoder) != 0 { io.external(ExternalOperation::LinkTrainStop, transcoder); }
    }
    io.delay_us(200, 400);
    io.external(ExternalOperation::LinkTrainStop, state.cpu_transcoder as u32);
}

// upstream: intel_ddi.c intel_ddi_enable_dp()
pub fn intel_ddi_enable_dp(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, lspcon_active: bool) {
    if encoder.port == Port::A && encoder.display.display_ver < 9 { io.external(ExternalOperation::LinkTrainStop, 0); }
    io.external(ExternalOperation::PrivacyScreen, 1);
    io.external(ExternalOperation::Backlight, 1);
    io.external(ExternalOperation::PanelPower, 2);
    if !lspcon_active || state.has_hdmi_sink { io.external(ExternalOperation::Infoframes, 1); }
    trans_port_sync_stop_link_train(io, encoder, state);
}

// upstream: intel_ddi.c gen9_chicken_trans_reg_by_port()
pub fn gen9_chicken_trans_reg_by_port(io: &mut impl DdiIo, display: Platform, port: Port) -> u32 {
    if display.display_ver < 9 { io.warning(display.display_ver as u32); }
    let transcoder = match port { Port::A => Transcoder::Edp, Port::B | Port::E => Transcoder::A,
        Port::C => Transcoder::B, Port::D => Transcoder::C, _ => { io.warning(port.index() as u32); Transcoder::Edp } };
    0x42000 + transcoder as u32 * 0x100
}

// upstream: intel_ddi.c intel_ddi_enable_hdmi()
pub fn intel_ddi_enable_hdmi(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    if io.external(ExternalOperation::Hdmi, (state.hdmi_high_tmds_clock_ratio as u32) | ((state.hdmi_scrambling as u32) << 1)) == 0 {
        io.log(18, encoder.port.index() as u32);
    }
    if has_buf_trans_select(encoder.display) { hsw_prepare_hdmi_ddi_buffers(io, encoder, state); }
    mtl_ddi_enable_d2d(io, encoder);
    io.external(ExternalOperation::SignalLevels, state.port_clock);
    if encoder.display.display_ver == 9 && !encoder.display.broxton {
        let reg = gen9_chicken_trans_reg_by_port(io, encoder.display, encoder.port);
        let mask = if encoder.port == Port::E { (1 << 29) | (1 << 28) } else { (1 << 27) | (1 << 26) };
        let value = io.read(reg);
        io.write(reg, value | mask);
        io.posting_read(reg);
        io.delay_us(1, 1);
        io.write(reg, value & !mask);
    }
    intel_ddi_power_up_lanes(io, encoder, state);
    let mut buf_ctl = 0;
    if encoder.lane_reversal { buf_ctl |= DDI_BUF_PORT_REVERSAL; }
    if encoder.ddi_a_4_lanes { buf_ctl |= DDI_A_4_LANES; }
    if encoder.display.display_ver >= 14 {
        let port_buf = ddi_buf_port_width(state.lane_count) | if encoder.lane_reversal { DDI_BUF_PORT_REVERSAL } else { 0 };
        io.rmw(ddi_buf_status(encoder.port, 14), DDI_PORT_WIDTH_MASK | DDI_BUF_PORT_REVERSAL, port_buf);
        buf_ctl |= ddi_buf_port_width(state.lane_count);
        if encoder.display.display_ver >= 20 { buf_ctl |= 1 << 29; }
    } else if encoder.display.alderlake_p && encoder.is_tc {
        if !encoder.in_tbt_alt_mode { io.warning(encoder.port.index() as u32); }
        buf_ctl |= DDI_BUF_CTL_TC_PHY_OWNERSHIP;
    }
    intel_ddi_buf_enable(io, encoder, buf_ctl);
    io.external(ExternalOperation::Hdmi, 0x8000);
}

// upstream: intel_ddi.c intel_ddi_enable()
pub fn intel_ddi_enable(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let is_hdmi = matches!(state.output, OutputType::Hdmi | OutputType::Dvi);
    if !is_hdmi && io.is_uhbr(state) {
        let clock_hz = state.adjusted_mode.clock as u64 * 1000;
        io.write(0x600a4 + trans_offset(state.cpu_transcoder), ((clock_hz >> 24) as u32) << 8);
        io.write(0x600a8 + trans_offset(state.cpu_transcoder), ((clock_hz as u32) & 0x00ff_ffff) << 8);
    }
    intel_ddi_enable_transcoder_func(io, encoder, state);
    io.external(ExternalOperation::Vrr, 1);
    if !is_hdmi && io.is_uhbr(state) {
        intel_ddi_clear_act_sent(io, encoder, state);
        io.rmw(trans_ddi_func_ctl(state.cpu_transcoder), 0, 1 << 21);
        intel_ddi_wait_for_act_sent(io, encoder, state);
        io.external(ExternalOperation::DpAux, 0x3000);
    }
    io.external(ExternalOperation::AtomicState, 0x3001);
    let _ = intel_ddi_wait_for_fec_status(io, encoder, state, true);
    io.external(ExternalOperation::Vblank, 1 << state.pipe.index());
    if is_hdmi { intel_ddi_enable_hdmi(io, encoder, state); } else { intel_ddi_enable_dp(io, encoder, state, false); }
    io.external(ExternalOperation::Hdcp, 1);
}

// upstream: intel_ddi.c intel_ddi_disable_dp()
pub fn intel_ddi_disable_dp(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::LinkTraining, 0);
    io.external(ExternalOperation::PanelPower, 0x10);
    io.external(ExternalOperation::Psr, 0);
    io.external(ExternalOperation::Alpm, 0);
    io.external(ExternalOperation::Backlight, 0);
    io.external(ExternalOperation::SinkDecompression, 0);
    intel_dp_sink_set_msa_timing_par_ignore_state(io, encoder, state, false);
}

// upstream: intel_ddi.c intel_ddi_disable_hdmi()
pub fn intel_ddi_disable_hdmi(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if io.external(ExternalOperation::Hdmi, 0) == 0 { io.log(19, encoder.port.index() as u32); }
}

// upstream: intel_ddi.c intel_ddi_disable()
pub fn intel_ddi_disable(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::TcPort, 0x100);
    io.external(ExternalOperation::Hdcp, 0);
    if matches!(state.output, OutputType::Hdmi | OutputType::Dvi) { intel_ddi_disable_hdmi(io, encoder); }
    else { intel_ddi_disable_dp(io, encoder, state); }
}

// upstream: intel_ddi.c intel_ddi_update_pipe_dp()
pub fn intel_ddi_update_pipe_dp(io: &mut impl DdiIo, _encoder: &DdiEncoder, state: &CrtcState) {
    intel_ddi_set_dp_msa(io, state, true);
    io.external(ExternalOperation::Infoframes, 1);
    io.external(ExternalOperation::Backlight, state.pipe_bpp as u32);
    io.external(ExternalOperation::PrivacyScreen, 1);
}

// upstream: intel_ddi.c intel_ddi_update_pipe_hdmi()
pub fn intel_ddi_update_pipe_hdmi(io: &mut impl DdiIo, _encoder: &DdiEncoder, state: &CrtcState) {
    io.external(ExternalOperation::Infoframes, 0x100 | state.pipe_bpp as u32);
}

// upstream: intel_ddi.c intel_ddi_update_pipe()
pub fn intel_ddi_update_pipe(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, is_mst_encoder: bool) {
    let is_hdmi = matches!(state.output, OutputType::Hdmi | OutputType::Dvi);
    if !is_hdmi && !is_mst_encoder { intel_ddi_update_pipe_dp(io, encoder, state); }
    if is_hdmi { intel_ddi_update_pipe_hdmi(io, encoder, state); }
    io.external(ExternalOperation::Hdcp, 0x10);
}

// upstream: intel_ddi.c intel_ddi_update_active_dpll()
pub fn intel_ddi_update_active_dpll(io: &mut impl DdiIo, encoder: &DdiEncoder, _state: &CrtcState, joined_pipe_mask: u8) {
    if !encoder.is_tc || !encoder.display.has_dpll_manager { return; }
    for pipe in 0..6 { if joined_pipe_mask & (1 << pipe) != 0 { io.external(ExternalOperation::DpllUpdate, pipe | ((encoder.port.index() as u32) << 8)); } }
}

// upstream: intel_ddi.c intel_ddi_pre_pll_enable()
pub fn intel_ddi_pre_pll_enable(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    if encoder.is_tc {
        io.external(ExternalOperation::TcPort, 0x200 | state.lane_count as u32);
        intel_ddi_update_active_dpll(io, encoder, state, 1 << state.pipe.index());
    }
    let _ = io.enable_power(intel_ddi_main_link_aux_domain(encoder, state, state.has_vrr).unwrap_or(0));
    if encoder.is_tc && !encoder.in_tbt_alt_mode { io.external(ExternalOperation::LaneCount, state.lane_count as u32); }
    else if encoder.display.geminilake || encoder.display.broxton { io.external(ExternalOperation::Phy, 0x100 | state.lane_count as u32); }
    if encoder.display.display_ver >= 14 { io.external(ExternalOperation::PanelPower, 1); }
}

// upstream: intel_ddi.c adlp_tbt_to_dp_alt_switch_wa()
pub fn adlp_tbt_to_dp_alt_switch_wa(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    for lane in 0..2u8 { io.dkl_phy_rmw(encoder.port, DklPhyRegister::PcsDw5(lane), 1 << 31, 0); }
}

// upstream: intel_ddi.c mtl_ddi_prepare_link_retrain()
pub fn mtl_ddi_prepare_link_retrain(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let reg = dp_tp_ctl_reg(encoder, state);
    let old = io.read(reg);
    io.warning((old >> 31) & 1);
    let mut ctl = 1 << 31;
    if state.output == OutputType::DpMst || io.is_uhbr(state) { ctl |= 1 << 27; }
    else if state.enhanced_framing { ctl |= 1 << 18; }
    io.write(reg, ctl); io.posting_read(reg);
    mtl_ddi_enable_d2d(io, encoder);
    io.external(ExternalOperation::SignalLevels, state.port_clock);
    mtl_port_buf_ctl_program(io, encoder, state);
    if encoder.display.display_ver >= 20 { encoder.dp_state |= 1 << 29; }
    intel_ddi_buf_enable(io, encoder, encoder.dp_state);
    encoder.dp_state |= DDI_BUF_CTL_ENABLE;
    io.external(ExternalOperation::Alpm, 0x100);
    io.external(ExternalOperation::Phy, 0x200);
}

// upstream: intel_ddi.c intel_ddi_prepare_link_retrain()
pub fn intel_ddi_prepare_link_retrain(io: &mut impl DdiIo, encoder: &mut DdiEncoder, state: &CrtcState) {
    let reg = dp_tp_ctl_reg(encoder, state);
    let old = io.read(reg);
    io.warning((old >> 31) & 1);
    let mut ctl = 1 << 31;
    if state.output == OutputType::DpMst || io.is_uhbr(state) { ctl |= 1 << 27; }
    else if state.enhanced_framing { ctl |= 1 << 18; }
    io.write(reg, ctl); io.posting_read(reg);
    if encoder.display.alderlake_p && encoder.is_tc && !encoder.in_tbt_alt_mode { adlp_tbt_to_dp_alt_switch_wa(io, encoder); }
    intel_ddi_buf_enable(io, encoder, encoder.dp_state);
    encoder.dp_state |= DDI_BUF_CTL_ENABLE;
}

// upstream: intel_ddi.c intel_ddi_set_link_train()
pub fn intel_ddi_set_link_train(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState, pattern: u8) {
    const PATTERN_MASK: u32 = 0x7 << 8;
    let reg = dp_tp_ctl_reg(encoder, state);
    let mut value = io.read(reg) & !PATTERN_MASK;
    value |= match pattern & 0x7 { 0 => 3 << 8, 1 => 0, 2 => 1 << 8, 3 => 4 << 8, 4 => 5 << 8, _ => 3 << 8 };
    io.write(reg, value);
}

// upstream: intel_ddi.c intel_ddi_set_idle_link_train()
pub fn intel_ddi_set_idle_link_train(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) {
    io.rmw(dp_tp_ctl_reg(encoder, state), 0x7 << 8, 2 << 8);
    if encoder.port == Port::A && encoder.display.display_ver < 12 { return; }
    if io.wait_set(dp_tp_status_reg(encoder, state), 1 << 25, 2) { io.log(20, encoder.port.index() as u32); }
}

// upstream: intel_ddi.c intel_ddi_is_audio_enabled()
pub fn intel_ddi_is_audio_enabled(io: &mut impl DdiIo, cpu_transcoder: Transcoder) -> bool {
    if cpu_transcoder == Transcoder::Edp || !io.power_is_enabled(0x700) { return false; }
    io.read(0x650c0) & (1 << (2 + cpu_transcoder as u32 * 4)) != 0
}

// upstream: intel_ddi.c tgl_ddi_min_voltage_level()
pub const fn tgl_ddi_min_voltage_level(state: &CrtcState) -> u8 { if state.port_clock > 594_000 { 2 } else { 0 } }

// upstream: intel_ddi.c jsl_ddi_min_voltage_level()
pub const fn jsl_ddi_min_voltage_level(state: &CrtcState) -> u8 { if state.port_clock > 594_000 { 3 } else { 0 } }

// upstream: intel_ddi.c icl_ddi_min_voltage_level()
pub const fn icl_ddi_min_voltage_level(state: &CrtcState) -> u8 { if state.port_clock > 594_000 { 1 } else { 0 } }

// upstream: intel_ddi.c intel_ddi_compute_min_voltage_level()
pub fn intel_ddi_compute_min_voltage_level(display: Platform, state: &mut CrtcState) {
    if display.display_ver >= 14 { state.min_voltage_level = icl_ddi_min_voltage_level(state); }
    else if display.display_ver >= 12 { state.min_voltage_level = tgl_ddi_min_voltage_level(state); }
    else if display.jasperlake || display.elkhartlake { state.min_voltage_level = jsl_ddi_min_voltage_level(state); }
    else if display.display_ver >= 11 { state.min_voltage_level = icl_ddi_min_voltage_level(state); }
}

// upstream: intel_ddi.c bdw_transcoder_master_readout()
pub fn bdw_transcoder_master_readout(io: &mut impl DdiIo, display: Platform, cpu: Transcoder) -> Transcoder {
    let (reg, enable_mask, shift) = if display.display_ver >= 11 { (trans_ddi_func_ctl2(cpu), 1 << 4, 0) } else { (trans_ddi_func_ctl(cpu), 1 << 15, 18) };
    let value = io.read(reg);
    if value & enable_mask == 0 { return Transcoder::Invalid; }
    let master = (value >> shift) & 0xf;
    match master { 0 => Transcoder::Edp, 1 => Transcoder::A, 2 => Transcoder::B, 3 => Transcoder::C, 4 => Transcoder::D, _ => Transcoder::Invalid }
}

// upstream: intel_ddi.c bdw_get_trans_port_sync_config()
pub fn bdw_get_trans_port_sync_config(io: &mut impl DdiIo, display: Platform, state: &mut CrtcState) {
    let transcoders = [Transcoder::A, Transcoder::B, Transcoder::C, Transcoder::D];
    state.master_transcoder = bdw_transcoder_master_readout(io, display, state.cpu_transcoder);
    state.sync_mode_slaves_mask = 0;
    for cpu in transcoders {
        let Some(cookie) = io.enable_power_if_enabled(0x100 + cpu as u32) else { continue; };
        if bdw_transcoder_master_readout(io, display, cpu) == state.cpu_transcoder { state.sync_mode_slaves_mask |= 1 << cpu as u32; }
        io.disable_power(0x100 + cpu as u32, cookie);
    }
    if state.master_transcoder != Transcoder::Invalid && state.sync_mode_slaves_mask != 0 { io.warning(state.sync_mode_slaves_mask); }
}

const TRANS_DDI_MODE_MASK: u32 = 0x7 << 24;
const TRANS_DDI_BPC_MASK: u32 = 0x7 << 20;
const TRANS_DDI_PHSYNC: u32 = 1 << 16;
const TRANS_DDI_PVSYNC: u32 = 1 << 17;

// upstream: intel_ddi.c intel_ddi_read_func_ctl_dvi()
pub fn intel_ddi_read_func_ctl_dvi(encoder: &DdiEncoder, state: &mut CrtcState, func: u32) {
    state.output = OutputType::Hdmi;
    state.lane_count = if encoder.display.display_ver >= 14 {
        let encoded = ((func & DDI_PORT_WIDTH_MASK) >> 1) as u8;
        if encoded == 4 { 3 } else { encoded + 1 }
    } else { 4 };
}

// upstream: intel_ddi.c intel_ddi_read_func_ctl_hdmi()
pub fn intel_ddi_read_func_ctl_hdmi(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, func: u32) {
    state.has_hdmi_sink = true;
    state.has_infoframe |= io.external(ExternalOperation::Infoframes, state.pipe.index() as u32) != 0;
    if func & (1 << 13) != 0 { state.hdmi_scrambling = true; }
    if func & (1 << 12) != 0 { state.hdmi_high_tmds_clock_ratio = true; }
    intel_ddi_read_func_ctl_dvi(encoder, state, func);
}

// upstream: intel_ddi.c intel_ddi_read_func_ctl_fdi()
pub fn intel_ddi_read_func_ctl_fdi(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, _func: u32) {
    state.output = OutputType::Analog;
    state.enhanced_framing = io.read(dp_tp_ctl_reg(encoder, state)) & (1 << 18) != 0;
}

// upstream: intel_ddi.c intel_ddi_read_func_ctl_dp_sst()
pub fn intel_ddi_read_func_ctl_dp_sst(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, func: u32) {
    state.output = if encoder.output == OutputType::EmbeddedDisplayPort { OutputType::EmbeddedDisplayPort } else { OutputType::DisplayPort };
    state.lane_count = (((func & DDI_PORT_WIDTH_MASK) >> 1) as u8) + 1;
    if encoder.display.display_ver >= 12 && func & TRANS_DDI_MODE_MASK == (4 << 24) {
        state.mst_master_transcoder = match (func >> 10) & 0x3 { 0 => Transcoder::A, 1 => Transcoder::B, 2 => Transcoder::C, _ => Transcoder::D };
    }
    io.external(ExternalOperation::AtomicState, 0x4d4e | state.cpu_transcoder as u32);
    state.enhanced_framing = io.read(dp_tp_ctl_reg(encoder, state)) & (1 << 18) != 0;
    if encoder.display.display_ver >= 11 { state.fec_enable = io.read(dp_tp_ctl_reg(encoder, state)) & (1 << 30) != 0; }
    state.has_infoframe |= io.external(ExternalOperation::Infoframes, state.pipe.index() as u32) != 0;
}

// upstream: intel_ddi.c intel_ddi_read_func_ctl_dp_mst()
pub fn intel_ddi_read_func_ctl_dp_mst(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, func: u32) {
    state.output = OutputType::DpMst;
    state.lane_count = (((func & DDI_PORT_WIDTH_MASK) >> 1) as u8) + 1;
    if encoder.display.display_ver >= 12 { state.mst_master_transcoder = match (func >> 10) & 0x3 { 0 => Transcoder::A, 1 => Transcoder::B, 2 => Transcoder::C, _ => Transcoder::D }; }
    io.external(ExternalOperation::AtomicState, 0x4d4e | state.cpu_transcoder as u32);
    if encoder.display.display_ver >= 11 { state.fec_enable = io.read(dp_tp_ctl_reg(encoder, state)) & (1 << 30) != 0; }
    state.has_infoframe |= io.external(ExternalOperation::Infoframes, state.pipe.index() as u32) != 0;
}

// upstream: intel_ddi.c intel_ddi_read_func_ctl()
pub fn intel_ddi_read_func_ctl(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) {
    let func = io.read(trans_ddi_func_ctl(state.cpu_transcoder));
    state.mode_flags |= if func & TRANS_DDI_PHSYNC != 0 { 1 << 2 } else { 1 << 3 };
    state.mode_flags |= if func & TRANS_DDI_PVSYNC != 0 { 1 << 4 } else { 1 << 5 };
    state.pipe_bpp = if func & TRANS_DDI_BPC_MASK == (2 << 20) { 18 }
        else if func & TRANS_DDI_BPC_MASK == 0 { 24 }
        else if func & TRANS_DDI_BPC_MASK == (1 << 20) { 30 }
        else if func & TRANS_DDI_BPC_MASK == (3 << 20) { 36 }
        else { state.pipe_bpp };
    let mode = func & TRANS_DDI_MODE_MASK;
    if mode == 0 { intel_ddi_read_func_ctl_hdmi(io, encoder, state, func); }
    else if mode == (1 << 24) { intel_ddi_read_func_ctl_dvi(encoder, state, func); }
    else if mode == (4 << 24) && !encoder.display.has_dp20 { intel_ddi_read_func_ctl_fdi(io, encoder, state, func); }
    else if mode == (2 << 24) { intel_ddi_read_func_ctl_dp_sst(io, encoder, state, func); }
    else if mode == (3 << 24) { intel_ddi_read_func_ctl_dp_mst(io, encoder, state, func); }
    else if mode == (4 << 24) && encoder.display.has_dp20 {
            if io.external(ExternalOperation::LinkTraining, 0x4d5354) != 0 { intel_ddi_read_func_ctl_dp_mst(io, encoder, state, func); }
            else { intel_ddi_read_func_ctl_dp_sst(io, encoder, state, func); }
    }
}

// upstream: intel_ddi.c intel_ddi_get_config()
pub fn intel_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) {
    if matches!(state.cpu_transcoder, Transcoder::Edp | Transcoder::Dsi0 | Transcoder::Dsi1) { io.warning(state.cpu_transcoder as u32); return; }
    intel_ddi_read_func_ctl(io, encoder, state);
    intel_ddi_mso_get_config(io, encoder.display, state);
    state.has_audio = intel_ddi_is_audio_enabled(io, state.cpu_transcoder);
    if encoder.output == OutputType::EmbeddedDisplayPort { io.external(ExternalOperation::AtomicState, state.pipe_bpp as u32); }
    ddi_dotclock_get(io, state);
    if encoder.display.geminilake || encoder.display.broxton { io.external(ExternalOperation::Phy, 0x400); }
    intel_ddi_compute_min_voltage_level(encoder.display, state);
    for infoframe in [0x82u32, 0x83, 0x81, 0x87] { io.external(ExternalOperation::Infoframes, infoframe); }
    if encoder.display.display_ver >= 8 { bdw_get_trans_port_sync_config(io, encoder.display, state); }
    io.external(ExternalOperation::Psr, 0x10);
    for sdp in [0x0a, 0x07, 0x22] { io.external(ExternalOperation::DpAux, sdp); }
    io.external(ExternalOperation::Audio, state.has_audio as u32);
}

// upstream: intel_ddi.c intel_ddi_get_clock()
pub fn intel_ddi_get_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, pll: Option<DpllId>) {
    let Some(pll) = pll else { io.warning(0); return; };
    state.pll_id = pll;
    let active = io.external(ExternalOperation::Dpll, 0x100 | pll as u32);
    if active == 0 { io.warning(pll as u32); }
    state.port_clock = io.external(ExternalOperation::Dpll, 0x200 | pll as u32);
    let _ = encoder;
}

// upstream: intel_ddi.c icl_ddi_tc_pll_is_tbt()
pub const fn icl_ddi_tc_pll_is_tbt(pll: DpllId) -> bool { matches!(pll, DpllId::IclTbt) }

// upstream: intel_ddi.c mtl_ddi_cx0_get_config()
pub fn mtl_ddi_cx0_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, _port_dpll_id: u8, pll_id: DpllId) {
    let Some(pll) = io.get_dpll(pll_id) else { io.warning(pll_id as u32); return; };
    state.pll_id = pll;
    let active = io.external(ExternalOperation::Dpll, 0x100 | pll as u32);
    if active == 0 { io.warning(pll as u32); }
    if icl_ddi_tc_pll_is_tbt(state.pll_id) { state.port_clock = io.external(ExternalOperation::Dpll, 0x300 | encoder.port.index() as u32); }
    else { state.port_clock = io.external(ExternalOperation::Dpll, 0x200 | pll as u32); }
    intel_ddi_get_config(io, encoder, state);
}

// upstream: intel_ddi.c mtl_ddi_non_tc_phy_get_config()
pub fn mtl_ddi_non_tc_phy_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) {
    let pll = match encoder.port { Port::A => DpllId::IclDpll0, Port::B => DpllId::IclDpll1, Port::C => DpllId::IclMg1, _ => DpllId::IclMg2 };
    mtl_ddi_cx0_get_config(io, encoder, state, 0, pll);
}

// upstream: intel_ddi.c mtl_ddi_tc_phy_get_config()
pub fn mtl_ddi_tc_phy_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) {
    if encoder.in_tbt_alt_mode { mtl_ddi_cx0_get_config(io, encoder, state, 0, DpllId::IclTbt); }
    else { mtl_ddi_cx0_get_config(io, encoder, state, 1, DpllId::IclMg1); }
}

// upstream: intel_ddi.c dg2_ddi_get_config()
pub fn dg2_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) {
    io.external(ExternalOperation::Dpll, 0x400);
    state.port_clock = io.external(ExternalOperation::Dpll, 0x500 | encoder.port.index() as u32);
    intel_ddi_get_config(io, encoder, state);
}

// upstream: intel_ddi.c adls_ddi_get_config()
pub fn adls_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = adls_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c rkl_ddi_get_config()
pub fn rkl_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = rkl_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c dg1_ddi_get_config()
pub fn dg1_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = dg1_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c icl_ddi_combo_get_config()
pub fn icl_ddi_combo_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = icl_ddi_combo_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c icl_ddi_tc_port_pll_type()
pub fn icl_ddi_tc_port_pll_type(io: &mut impl DdiIo, state: &CrtcState) -> u8 {
    if state.pll_id == DpllId::None { io.warning(0); return 0; }
    if icl_ddi_tc_pll_is_tbt(state.pll_id) { 0 } else { 1 }
}

// upstream: intel_ddi.c intel_ddi_port_pll_type()
pub fn intel_ddi_port_pll_type(io: &mut impl DdiIo, encoder: &DdiEncoder, _state: &CrtcState) -> u8 {
    io.external(ExternalOperation::Dpll, 0x600 | encoder.port.index() as u32) as u8
}

// upstream: intel_ddi.c icl_ddi_tc_get_clock()
pub fn icl_ddi_tc_get_clock(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, pll: Option<DpllId>) {
    let Some(pll) = pll else { io.warning(0); return; };
    state.pll_id = pll;
    let _active = io.external(ExternalOperation::Dpll, 0x100 | pll as u32);
    if icl_ddi_tc_pll_is_tbt(pll) { state.port_clock = icl_calc_tbt_pll_link(io, encoder.display, encoder.port); }
    else { state.port_clock = io.external(ExternalOperation::Dpll, 0x200 | pll as u32); }
}

// upstream: intel_ddi.c icl_ddi_tc_get_config()
pub fn icl_ddi_tc_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = icl_ddi_tc_get_pll(io, encoder); icl_ddi_tc_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c bxt_ddi_get_config()
pub fn bxt_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = bxt_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c skl_ddi_get_config()
pub fn skl_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = skl_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c hsw_ddi_get_config()
pub fn hsw_ddi_get_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) { let pll = hsw_ddi_get_pll(io, encoder); intel_ddi_get_clock(io, encoder, state, pll); intel_ddi_get_config(io, encoder, state); }

// upstream: intel_ddi.c intel_ddi_sync_state()
pub fn intel_ddi_sync_state(io: &mut impl DdiIo, encoder: &DdiEncoder, state: Option<&CrtcState>) {
    if encoder.is_tc { io.external(ExternalOperation::TcPort, 0x300 | state.map_or(0, |s| s.output as u32)); }
    if state.is_some_and(|s| matches!(s.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst)) ||
        (state.is_none() && matches!(encoder.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst)) {
        io.external(ExternalOperation::LinkTraining, 0x400 | state.map_or(0, |s| s.port_clock));
    }
}

// upstream: intel_ddi.c intel_ddi_initial_fastset_check()
pub fn intel_ddi_initial_fastset_check(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) -> bool {
    let mut fastset = true;
    if encoder.is_tc { io.log(21, encoder.port.index() as u32); state.mode_flags |= 1 << 31; fastset = false; }
    if matches!(state.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst) &&
        io.external(ExternalOperation::LinkTraining, 0x500) == 0 { fastset = false; }
    fastset
}

// upstream: intel_ddi.c intel_ddi_compute_output_type()
pub fn intel_ddi_compute_output_type(io: &mut impl DdiIo, connector_type: u32) -> OutputType {
    match connector_type { 11 => OutputType::Hdmi, 14 => OutputType::EmbeddedDisplayPort, 10 => OutputType::DisplayPort,
        other => { io.warning(other); OutputType::Unused } }
}

// upstream: intel_ddi.c intel_ddi_compute_config()
pub fn intel_ddi_compute_config(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState) -> i32 {
    if encoder.display.has_edp_transcoder && encoder.port == Port::A { state.cpu_transcoder = Transcoder::Edp; }
    let ret = if matches!(state.output, OutputType::Hdmi | OutputType::Dvi) {
        state.has_hdmi_sink = io.external(ExternalOperation::Hdmi, 0x100) != 0;
        io.external(ExternalOperation::Hdmi, 0x200) as i32
    } else { io.external(ExternalOperation::LinkTraining, 0x200) as i32 };
    if ret != 0 { return ret; }
    if encoder.display.haswell && state.pipe == Pipe::A && state.cpu_transcoder == Transcoder::Edp { state.pfit_force_thru = state.pfit_enabled || state.crc_enabled; }
    if encoder.display.geminilake || encoder.display.broxton { state.lane_lat_optim_mask = io.external(ExternalOperation::Phy, state.lane_count as u32) as u8; }
    intel_ddi_compute_min_voltage_level(encoder.display, state);
    0
}

// upstream: intel_ddi.c mode_equal()
pub const fn mode_equal(a: &DisplayMode, b: &DisplayMode) -> bool {
    a.clock == b.clock && a.flags == b.flags && a.hdisplay == b.hdisplay && a.hsync_start == b.hsync_start &&
        a.hsync_end == b.hsync_end && a.htotal == b.htotal && a.vdisplay == b.vdisplay && a.vsync_start == b.vsync_start &&
        a.vsync_end == b.vsync_end && a.vtotal == b.vtotal && a.vscan == b.vscan
}

// upstream: intel_ddi.c m_n_equal()
pub const fn m_n_equal(a: &LinkMN, b: &LinkMN) -> bool {
    a.tu == b.tu && a.data_m == b.data_m && a.data_n == b.data_n && a.link_m == b.link_m && a.link_n == b.link_n
}

// upstream: intel_ddi.c crtcs_port_sync_compatible()
pub const fn crtcs_port_sync_compatible(a: &CrtcState, b: &CrtcState) -> bool {
    a.active && b.active && a.joiner_pipes == 0 && b.joiner_pipes == 0 && a.output_types == b.output_types &&
        a.output_format == b.output_format && a.lane_count == b.lane_count && a.port_clock == b.port_clock &&
        mode_equal(&a.adjusted_mode, &b.adjusted_mode) && m_n_equal(&a.dp_m_n, &b.dp_m_n)
}

// upstream: intel_ddi.c intel_ddi_port_sync_transcoders()
pub fn intel_ddi_port_sync_transcoders(display: Platform, state: &CrtcState, tile_group_id: i32, candidates: &[PortSyncCandidate]) -> u8 {
    if !(9..=20).contains(&display.display_ver) || state.output != OutputType::DisplayPort { return 0; }
    let mut mask = 0;
    for candidate in candidates {
        if !candidate.has_tile || candidate.tile_group_id != tile_group_id || !crtcs_port_sync_compatible(state, &candidate.state) { continue; }
        mask |= 1 << candidate.state.cpu_transcoder as u32;
    }
    mask
}

// upstream: intel_ddi.c intel_ddi_compute_config_late()
pub fn intel_ddi_compute_config_late(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &mut CrtcState, tile_group_id: Option<i32>, candidates: &[PortSyncCandidate]) -> i32 {
    if matches!(state.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst) {
        let ret = io.external(ExternalOperation::LinkTraining, 0x201) as i32;
        if ret != 0 { return ret; }
    }
    io.log(22, ((encoder.port.index() as u32) << 16) | state.pipe.index() as u32);
    let mask = tile_group_id.map_or(0, |tile| intel_ddi_port_sync_transcoders(encoder.display, state, tile, candidates));
    state.master_transcoder = if mask & (1 << Transcoder::Edp as u32) != 0 { Transcoder::Edp }
        else { match mask.trailing_zeros() { 0 => Transcoder::A, 1 => Transcoder::B, 2 => Transcoder::C, 3 => Transcoder::D, _ => Transcoder::Invalid } };
    if state.master_transcoder == state.cpu_transcoder {
        state.master_transcoder = Transcoder::Invalid;
        state.sync_mode_slaves_mask = u32::from(mask) & !(1 << state.cpu_transcoder as u32);
    }
    0
}

// upstream: intel_ddi.c intel_ddi_encoder_destroy()
pub fn intel_ddi_encoder_destroy(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.external(ExternalOperation::LinkTraining, 0xdead);
    if encoder.is_tc { io.external(ExternalOperation::TcPort, 0xdead); }
    io.external(ExternalOperation::PowerDomain, 0xdead);
    io.external(ExternalOperation::Connector, encoder.port.index() as u32);
}

// upstream: intel_ddi.c intel_ddi_encoder_reset()
pub fn intel_ddi_encoder_reset(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.external(ExternalOperation::LinkTraining, 0x100);
    io.external(ExternalOperation::DpAux, 0x4f5549);
    io.external(ExternalOperation::PanelPower, 0x200);
    if encoder.is_tc { io.external(ExternalOperation::TcPort, 0x400); }
}

// upstream: intel_ddi.c intel_ddi_encoder_late_register()
pub fn intel_ddi_encoder_late_register(io: &mut impl DdiIo, encoder: &DdiEncoder) -> i32 {
    io.external(ExternalOperation::TcPort, 0x500 | encoder.port.index() as u32) as i32
}

// upstream: intel_ddi.c intel_ddi_init_dp_connector()
pub fn intel_ddi_init_dp_connector(io: &mut impl DdiIo, encoder: &DdiEncoder) -> i32 {
    let allocated = io.external(ExternalOperation::Connector, 0x100 | encoder.port.index() as u32);
    if allocated == 0 { return -12; }
    io.external(ExternalOperation::Connector, 0x200 | encoder.port.index() as u32);
    if encoder.output == OutputType::EmbeddedDisplayPort { io.external(ExternalOperation::PrivacyScreen, encoder.port.index() as u32); }
    0
}

// upstream: intel_ddi.c intel_ddi_cleanup_dp_connector()
pub fn intel_ddi_cleanup_dp_connector(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.external(ExternalOperation::Connector, 0x300 | encoder.port.index() as u32);
}

// upstream: intel_ddi.c intel_hdmi_reset_link()
pub fn intel_hdmi_reset_link(io: &mut impl DdiIo, encoder: &DdiEncoder, state: &CrtcState) -> i32 {
    if io.external(ExternalOperation::Connector, 0x400 | encoder.port.index() as u32) == 0 { return 0; }
    let lock_ret = io.external(ExternalOperation::AtomicState, 0x100) as i32;
    if lock_ret != 0 { return lock_ret; }
    if !state.active { return 0; }
    if !matches!(state.output, OutputType::Hdmi | OutputType::Dvi) { io.warning(state.output as u32); }
    if !state.hdmi_high_tmds_clock_ratio && !state.hdmi_scrambling { return 0; }
    if io.external(ExternalOperation::AtomicState, 0x200) == 0 { return 0; }
    let config = io.external(ExternalOperation::Hdmi, 0x500) as u8;
    if (config & (1 << 1) != 0) == state.hdmi_high_tmds_clock_ratio &&
        (config & 1 != 0) == state.hdmi_scrambling { return 0; }
    io.external(ExternalOperation::AtomicState, 0x300 | (1 << state.pipe.index())) as i32
}

// upstream: intel_ddi.c intel_ddi_link_check()
pub fn intel_ddi_link_check(io: &mut impl DdiIo, encoder: &DdiEncoder, dp_connector_attached: bool) {
    if !dp_connector_attached { io.warning(encoder.port.index() as u32); }
    io.external(ExternalOperation::LinkTraining, 0x600 | encoder.port.index() as u32);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HotplugState { #[default] Unchanged, Changed, Retry }

// upstream: intel_ddi.c intel_ddi_hotplug()
pub fn intel_ddi_hotplug(io: &mut impl DdiIo, encoder: &DdiEncoder, connector_type: u32, is_mst: bool, retries: u8) -> HotplugState {
    if io.external(ExternalOperation::Phy, 0x700) != 0 { return HotplugState::Unchanged; }
    let state = match io.external(ExternalOperation::Hotplug, encoder.port.index() as u32) { 1 => HotplugState::Changed, 2 => HotplugState::Retry, _ => HotplugState::Unchanged };
    let link_reset_failed = io.external(ExternalOperation::TcPort, 0x600) == 0;
    if link_reset_failed {
        if connector_type == 11 { if intel_hdmi_reset_link(io, encoder, &CrtcState::default()) != 0 { io.warning(0); } }
        else { io.external(ExternalOperation::LinkTraining, 0x601); }
    }
    if state == HotplugState::Unchanged && retries < if encoder.is_tc { 5 } else { 1 } && !is_mst { HotplugState::Retry } else { state }
}

// upstream: intel_ddi.c lpt_digital_port_connected()
pub fn lpt_digital_port_connected(io: &mut impl DdiIo, hpd_bit: u32) -> bool { io.read(0x44400) & hpd_bit != 0 }

// upstream: intel_ddi.c hsw_digital_port_connected()
pub fn hsw_digital_port_connected(io: &mut impl DdiIo, hpd_bit: u32) -> bool { io.read(0x44000) & hpd_bit != 0 }

// upstream: intel_ddi.c bdw_digital_port_connected()
pub fn bdw_digital_port_connected(io: &mut impl DdiIo, hpd_bit: u32) -> bool { io.read(0x44400) & hpd_bit != 0 }

// upstream: intel_ddi.c intel_ddi_init_hdmi_connector()
pub fn intel_ddi_init_hdmi_connector(io: &mut impl DdiIo, encoder: &DdiEncoder) -> i32 {
    if io.external(ExternalOperation::Connector, 0x500 | encoder.port.index() as u32) == 0 { return -12; }
    io.external(ExternalOperation::Connector, 0x600 | encoder.port.index() as u32);
    0
}

// upstream: intel_ddi.c intel_ddi_a_force_4_lanes()
pub fn intel_ddi_a_force_4_lanes(encoder: &DdiEncoder) -> bool {
    encoder.port == Port::A && !encoder.ddi_a_4_lanes && (encoder.display.geminilake || encoder.display.broxton)
}

// upstream: intel_ddi.c intel_ddi_max_lanes()
pub fn intel_ddi_max_lanes(io: &mut impl DdiIo, encoder: &mut DdiEncoder) -> u8 {
    if encoder.display.display_ver >= 11 { return 4; }
    let mut max_lanes = 4;
    if encoder.port == Port::A || encoder.port == Port::E {
        if io.read(ddi_buf_ctl(Port::A)) & DDI_A_4_LANES != 0 { max_lanes = if encoder.port == Port::A { 4 } else { 0 }; }
        else { max_lanes = 2; }
    }
    if intel_ddi_a_force_4_lanes(encoder) { io.log(23, encoder.port.index() as u32); encoder.ddi_a_4_lanes = true; max_lanes = 4; }
    max_lanes
}

// upstream: intel_ddi.c xelpd_hpd_pin()
pub const fn xelpd_hpd_pin(_display: Platform, port: Port) -> u8 {
    if port.index() >= Port::H.index() { 7 + port.index() - Port::H.index() }
    else if port.index() >= Port::D.index() { 9 + port.index() - Port::D.index() }
    else { 4 + port.index() }
}

// upstream: intel_ddi.c dg1_hpd_pin()
pub const fn dg1_hpd_pin(port: Port) -> u8 { if port.index() >= Port::D.index() { 6 + port.index() - Port::D.index() } else { 4 + port.index() } }

// upstream: intel_ddi.c tgl_hpd_pin()
pub const fn tgl_hpd_pin(port: Port) -> u8 { if port.index() >= Port::D.index() { 9 + port.index() - Port::D.index() } else { 4 + port.index() } }

// upstream: intel_ddi.c rkl_hpd_pin()
pub const fn rkl_hpd_pin(display: Platform, port: Port) -> u8 {
    if display.has_pch_tgp { tgl_hpd_pin(port) }
    else if port.index() >= Port::D.index() { 6 + port.index() - Port::D.index() }
    else { 4 + port.index() }
}

// upstream: intel_ddi.c icl_hpd_pin()
pub const fn icl_hpd_pin(port: Port) -> u8 { if port.index() >= Port::C.index() { 9 + port.index() - Port::C.index() } else { 4 + port.index() } }

// upstream: intel_ddi.c ehl_hpd_pin()
pub fn ehl_hpd_pin(display: Platform, port: Port) -> u8 {
    if port.index() == Port::D.index() { 4 } else if display.has_pch_tgp { icl_hpd_pin(port) } else { 4 + port.index() }
}

// upstream: intel_ddi.c skl_hpd_pin()
pub const fn skl_hpd_pin(display: Platform, port: Port) -> u8 { if display.has_pch_tgp { icl_hpd_pin(port) } else { 4 + port.index() } }

// upstream: intel_ddi.c intel_ddi_is_tc()
pub const fn intel_ddi_is_tc(display: Platform, port: Port) -> bool {
    if display.display_ver >= 12 { port.index() >= Port::D.index() }
    else if display.display_ver >= 11 { port.index() >= Port::C.index() }
    else { false }
}

// upstream: intel_ddi.c intel_ddi_encoder_suspend()
pub fn intel_ddi_encoder_suspend(io: &mut impl DdiIo, encoder: &DdiEncoder) { io.external(ExternalOperation::LinkTraining, 0x700 | encoder.port.index() as u32); }

// upstream: intel_ddi.c intel_ddi_tc_encoder_suspend_complete()
pub fn intel_ddi_tc_encoder_suspend_complete(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    io.external(ExternalOperation::LinkTraining, 0x800 | encoder.port.index() as u32);
    io.external(ExternalOperation::TcPort, 0x700 | encoder.port.index() as u32);
}

// upstream: intel_ddi.c intel_ddi_encoder_shutdown()
pub fn intel_ddi_encoder_shutdown(io: &mut impl DdiIo, encoder: &DdiEncoder) {
    if matches!(encoder.output, OutputType::DisplayPort | OutputType::EmbeddedDisplayPort | OutputType::DpMst) { io.external(ExternalOperation::LinkTraining, 0x900); }
    if matches!(encoder.output, OutputType::Hdmi | OutputType::Dvi) { io.external(ExternalOperation::Hdmi, 0x900); }
}

// upstream: intel_ddi.c intel_ddi_tc_encoder_shutdown_complete()
pub fn intel_ddi_tc_encoder_shutdown_complete(io: &mut impl DdiIo, encoder: &DdiEncoder) { io.external(ExternalOperation::TcPort, 0x800 | encoder.port.index() as u32); }

// upstream: intel_ddi.c port_strap_detected()
pub fn port_strap_detected(io: &mut impl DdiIo, display: Platform, port: Port) -> bool {
    if display.display_ver >= 9 { return true; }
    match port {
        Port::A => io.read(ddi_buf_ctl(Port::A)) & 1 != 0,
        Port::B => io.read(0xc2014) & (1 << 1) != 0,
        Port::C => io.read(0xc2014) & (1 << 2) != 0,
        Port::D => io.read(0xc2014) & (1 << 3) != 0,
        Port::E => true,
        _ => { io.warning(port.index() as u32); false }
    }
}

// upstream: intel_ddi.c need_aux_ch()
pub const fn need_aux_ch(encoder: &DdiEncoder, init_dp: bool) -> bool { init_dp || intel_ddi_is_tc(encoder.display, encoder.port) }

// upstream: intel_ddi.c assert_has_icl_dsi()
pub fn assert_has_icl_dsi(io: &mut impl DdiIo, display: Platform) -> bool {
    let supported = display.alderlake_p || display.tigerlake || display.display_ver == 11;
    if !supported { io.warning(display.display_ver as u32); }
    supported
}

// upstream: intel_ddi.c port_in_use()
pub fn port_in_use(port: Port, ports_in_use: &[Port]) -> bool { ports_in_use.iter().any(|used| *used == port) }

// upstream: intel_ddi.c intel_ddi_encoder_name()
pub fn intel_ddi_encoder_name(io: &mut impl DdiIo, display: Platform, port: Port, phy: u8) -> String {
    let pchar = (b'A' + port.index()) as char;
    let phyc = (b'A' + phy) as char;
    if display.display_ver >= 13 && port.index() >= Port::H.index() {
        alloc::format!("DDI {}/PHY {}", (b'A' + port.index() - Port::H.index() + 3) as char, phyc)
    } else if display.display_ver >= 12 {
        let tc_port = io.external(ExternalOperation::TcPort, 0x900 | port.index() as u32);
        let port_tc = intel_ddi_is_tc(display, port);
        let tc_phy = tc_port != 0xffff;
        alloc::format!("DDI {}{}/PHY {}{}", if port_tc { "TC" } else { "" }, if port_tc { (b'1' + port.index() - Port::D.index()) as char } else { pchar },
            if tc_phy { "TC" } else { "" }, if tc_phy { (b'1' + tc_port as u8) as char } else { phyc })
    } else if display.display_ver >= 11 {
        let tc_port = io.external(ExternalOperation::TcPort, 0x900 | port.index() as u32);
        alloc::format!("DDI {}{}/PHY {}{}", pchar, if port.index() >= Port::C.index() { " (TC)" } else { "" }, if tc_port != 0xffff { "TC" } else { "" }, if tc_port != 0xffff { (b'1' + tc_port as u8) as char } else { phyc })
    } else { alloc::format!("DDI {}/PHY {}", pchar, phyc) }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdiInitRequest {
    pub port: Option<Port>,
    pub used_ports: u16,
    pub supports_dsi: bool,
    pub supports_dvi: bool,
    pub supports_hdmi: bool,
    pub supports_dp: bool,
    pub lspcon: bool,
    pub dedicated_external: bool,
    pub supports_typec_usb: bool,
    pub supports_tbt: bool,
    pub aux_channel_available: bool,
    pub hti_phy_mask: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DdiClockKind { #[default] None, Hsw, Skl, Bxt, IclCombo, IclTc, JslTc, Dg1, Rkl, Adls, Dg2, Mtl }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DdiSignalKind { #[default] Hsw, Bxt, IclMg, IclCombo, TglDkl, Dg2Snps, MtlCx0, Lt }

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DdiInitPlan {
    pub encoder: DdiEncoder,
    pub name: String,
    pub init_dp: bool,
    pub init_hdmi: bool,
    pub aux_channel: Option<u8>,
    pub clock_kind: DdiClockKind,
    pub signal_kind: DdiSignalKind,
    pub connected_kind: u8,
}

// upstream: intel_ddi.c intel_ddi_init()
pub fn intel_ddi_init(io: &mut impl DdiIo, display: Platform, request: DdiInitRequest) -> Option<DdiInitPlan> {
    let port = request.port?;
    if !port_strap_detected(io, display, port) { io.log(24, port.index() as u32); return None; }
    if io.external(ExternalOperation::Init, 0x100 | port.index() as u32) == 0 { return None; }
    if request.used_ports & (1 << port.index()) != 0 { return None; }
    if request.supports_dsi {
        if !assert_has_icl_dsi(io, display) { return None; }
        io.external(ExternalOperation::Init, 0x200 | port.index() as u32);
        return None;
    }
    let phy = io.external(ExternalOperation::Init, 0x300 | port.index() as u32) as u8;
    if request.hti_phy_mask & (1 << phy) != 0 { io.log(25, phy as u32); return None; }
    let mut init_hdmi = request.supports_dvi || request.supports_hdmi;
    let mut init_dp = request.supports_dp;
    if request.lspcon { init_dp = true; init_hdmi = false; io.log(26, port.index() as u32); }
    if !init_dp && !init_hdmi { io.log(27, port.index() as u32); return None; }
    let mut encoder = DdiEncoder { port, phy, display, is_tc: intel_ddi_is_tc(display, port), ..DdiEncoder::default() };
    encoder.output = if init_dp { OutputType::DisplayPort } else { OutputType::Hdmi };
    let buf_ctl = io.read(ddi_buf_ctl(port));
    encoder.lane_reversal = io.external(ExternalOperation::Init, 0x400 | port.index() as u32) != 0 || buf_ctl & DDI_BUF_PORT_REVERSAL != 0;
    encoder.ddi_a_4_lanes = display.display_ver < 11 && buf_ctl & DDI_A_4_LANES != 0;
    encoder.max_lanes = intel_ddi_max_lanes(io, &mut encoder);
    let aux_channel = if need_aux_ch(&encoder, init_dp) {
        if !request.aux_channel_available { io.log(28, port.index() as u32); return None; }
        Some(io.external(ExternalOperation::Init, 0x500 | port.index() as u32) as u8)
    } else { None };
    encoder.aux_channel = aux_channel.unwrap_or(0xff);
    let mut is_legacy = !request.supports_typec_usb && !request.supports_tbt;
    if encoder.is_tc {
        if !is_legacy && init_hdmi { is_legacy = !init_dp; }
        if (io.external(ExternalOperation::TcPort, 0xa00 | (port.index() as u32) | ((is_legacy as u32) << 8)) as i32) < 0 { return None; }
    }
    let connected_kind = if display.display_ver >= 11 { if encoder.is_tc { 1 } else { 2 } }
        else if display.geminilake || display.broxton { 3 }
        else if display.display_ver == 9 { 4 }
        else if display.broadwell { if port == Port::A { 5 } else { 4 } }
        else if display.haswell { if port == Port::A { 6 } else { 4 } } else { 0 };
    io.external(ExternalOperation::Init, 0x600 | connected_kind as u32);
    io.external(ExternalOperation::Init, 0x700 | port.index() as u32);
    if init_dp && intel_ddi_init_dp_connector(io, &encoder) != 0 {
        if encoder.is_tc { io.external(ExternalOperation::TcPort, 0xb00); }
        return None;
    }
    if encoder.output != OutputType::EmbeddedDisplayPort && init_hdmi && intel_ddi_init_hdmi_connector(io, &encoder) != 0 { init_hdmi = false; }
    let clock_kind = if display.has_lt_phy { DdiClockKind::Mtl }
        else if display.display_ver >= 14 { DdiClockKind::Mtl }
        else if display.dg2 { DdiClockKind::Dg2 }
        else if display.alderlake_s { DdiClockKind::Adls }
        else if display.rocketlake { DdiClockKind::Rkl }
        else if display.dg1 { DdiClockKind::Dg1 }
        else if display.jasperlake || display.elkhartlake { if encoder.is_tc { DdiClockKind::JslTc } else { DdiClockKind::IclCombo } }
        else if display.display_ver >= 11 { if encoder.is_tc { DdiClockKind::IclTc } else { DdiClockKind::IclCombo } }
        else if display.geminilake || display.broxton { DdiClockKind::Bxt }
        else if display.display_ver == 9 { DdiClockKind::Skl }
        else if display.haswell || display.broadwell { DdiClockKind::Hsw } else { DdiClockKind::None };
    let signal_kind = if display.has_lt_phy { DdiSignalKind::Lt }
        else if display.display_ver >= 14 { DdiSignalKind::MtlCx0 }
        else if display.dg2 { DdiSignalKind::Dg2Snps }
        else if display.display_ver >= 12 { if io.external(ExternalOperation::Init, 0xe00 | phy as u32) != 0 { DdiSignalKind::IclCombo } else { DdiSignalKind::TglDkl } }
        else if display.display_ver >= 11 { if encoder.is_tc { DdiSignalKind::IclMg } else { DdiSignalKind::IclCombo } }
        else if display.geminilake || display.broxton { DdiSignalKind::Bxt } else { DdiSignalKind::Hsw };
    let hpd_pin = if display.display_ver >= 13 { xelpd_hpd_pin(display, port) }
        else if display.dg1 { dg1_hpd_pin(port) }
        else if display.rocketlake { rkl_hpd_pin(display, port) }
        else if display.display_ver >= 12 { tgl_hpd_pin(port) }
        else if display.jasperlake || display.elkhartlake { ehl_hpd_pin(display, port) }
        else if display.display_ver == 11 { icl_hpd_pin(port) }
        else if display.display_ver == 9 && !display.broxton { skl_hpd_pin(display, port) }
        else { 1 + port.index() };
    encoder.hpd_pin = hpd_pin;
    Some(DdiInitPlan { encoder, name: intel_ddi_encoder_name(io, display, port, phy), init_dp, init_hdmi, aux_channel, clock_kind, signal_kind, connected_kind })
}

#[cfg(test)]
mod focused_tests {
    use super::*;

    struct NoIo;
    impl DdiIo for NoIo {
        fn read(&mut self, _reg: u32) -> u32 { 0 }
        fn write(&mut self, _reg: u32, _value: u32) {}
        fn combo_phy_read(&mut self, _phy: u8, _reg: ComboPhyRegister) -> u32 { 0 }
        fn combo_phy_write(&mut self, _phy: u8, _reg: ComboPhyRegister, _value: u32) {}
        fn combo_phy_rmw(&mut self, _phy: u8, _reg: ComboPhyRegister, _clear: u32, _set: u32) {}
        fn mg_phy_rmw(&mut self, _port: Port, _reg: MgPhyRegister, _clear: u32, _set: u32) {}
        fn dkl_phy_read(&mut self, _port: Port, _reg: DklPhyRegister) -> u32 { 0 }
        fn dkl_phy_write(&mut self, _port: Port, _reg: DklPhyRegister, _value: u32) {}
        fn dkl_phy_rmw(&mut self, _port: Port, _reg: DklPhyRegister, _clear: u32, _set: u32) {}
        fn mg_dp_mode_read(&mut self, _port: Port, _lane: u8) -> u32 { 0 }
        fn mg_dp_mode_write(&mut self, _port: Port, _lane: u8, _value: u32) {}
    }

    #[test]
    fn transcoder_func_ctl_uses_drm_sync_flag_bits_and_gen13_width_rules() {
        const DRM_MODE_FLAG_PHSYNC: u32 = 1 << 0;
        const DRM_MODE_FLAG_PVSYNC: u32 = 1 << 2;
        let encoder = DdiEncoder {
            port: Port::A,
            output: OutputType::Hdmi,
            display: Platform { display_ver: 13, alderlake_p: true, ..Platform::default() },
            ..DdiEncoder::default()
        };
        let state = CrtcState {
            output: OutputType::Hdmi,
            has_hdmi_sink: true,
            pipe_bpp: 24,
            mode_flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
            ..CrtcState::default()
        };
        assert_eq!(intel_ddi_transcoder_func_reg_val_get(&mut NoIo, &encoder, &state), 0x8803_0000);
        let negative_sync = CrtcState { mode_flags: 0, ..state };
        assert_eq!(intel_ddi_transcoder_func_reg_val_get(&mut NoIo, &encoder, &negative_sync), 0x8800_0000);
    }
}
