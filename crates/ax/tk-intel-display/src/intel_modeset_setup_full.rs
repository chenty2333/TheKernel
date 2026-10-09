// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_modeset_setup.c translation.
// DRM, atomic, connector, power, and MMIO services are explicit in ModesetOps.
#![allow(dead_code, clippy::too_many_arguments, clippy::type_complexity)]

extern crate alloc;
use alloc::vec::Vec;

pub const INVALID_TRANSCODER: u8 = 0xff;
pub const PIPE_A: u8 = 0;
pub const DRM_MODE_DPMS_ON: i32 = 0;
pub const DRM_MODE_DPMS_OFF: i32 = 3;
pub const DRM_PLANE_TYPE_PRIMARY: u8 = 1;
const POWER_DOMAIN_NUM: u32 = 128;
const GEN9_CLKGATE_DIS_0: u32 = 0x46530;
const DARBF_GATING_DIS: u32 = 1 << 27;
const CHICKEN_PAR1_1: u32 = 0x42080;
const FORCE_ARB_IDLE_PLANES: u32 = 1 << 14;
const KBL_ARB_FILL_SPARE_22: u32 = 1 << 22;
const CHICKEN_MISC_2: u32 = 0x42084;
const KBL_ARB_FILL_SPARE_13: u32 = 1 << 13;
const KBL_ARB_FILL_SPARE_14: u32 = 1 << 14;

#[inline]
const fn bit(n: u8) -> u8 { 1u8 << n }

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModeState {
    pub mode: u64,
    pub adjusted_mode: u64,
    pub enable: bool,
    pub active: bool,
    pub scaling_filter: u32,
    pub sharpness_strength: u32,
    pub background_color: u64,
    pub degamma_lut: Option<u64>,
    pub gamma_lut: Option<u64>,
    pub ctm: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrtcState {
    pub pipe: u8,
    pub cpu_transcoder: u8,
    pub master_transcoder: u8,
    pub joiner_secondary: bool,
    pub joiner_pipes: u8,
    pub joiner_secondary_pipes: u8,
    pub port_sync_mode: bool,
    pub port_sync_master: bool,
    pub sync_mode_slaves_mask: u8,
    pub hw: ModeState,
    pub uapi: ModeState,
    pub pipe_bpp: u32,
    pub pre_csc_lut: Option<u64>,
    pub post_csc_lut: Option<u64>,
    pub degamma_lut_size: u32,
    pub intel_dpll: Option<u64>,
    pub port_clock: u32,
    pub pixel_rate: u32,
    pub double_wide: bool,
    pub vrr_enable: bool,
    pub inherited: bool,
    pub connector_mask: u32,
    pub encoder_mask: u32,
    pub active_planes: u32,
    pub data_rate: [u32; 32],
    pub plane_min_cdclk: [u32; 32],
    pub min_cdclk: u32,
    pub uapi_crtc: u32,
}
impl Default for CrtcState {
    fn default() -> Self {
        Self { pipe: PIPE_A, cpu_transcoder: INVALID_TRANSCODER,
            master_transcoder: INVALID_TRANSCODER, joiner_secondary: false,
            joiner_pipes: 0, joiner_secondary_pipes: 0, port_sync_mode: false,
            port_sync_master: false, sync_mode_slaves_mask: 0, hw: ModeState::default(),
            uapi: ModeState::default(), pipe_bpp: 0, pre_csc_lut: None, post_csc_lut: None,
            degamma_lut_size: 0, intel_dpll: None, port_clock: 0, pixel_rate: 0,
            double_wide: false, vrr_enable: false, inherited: false, connector_mask: 0,
            encoder_mask: 0, active_planes: 0, data_rate: [0; 32], plane_min_cdclk: [0; 32],
            min_cdclk: 0, uapi_crtc: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneState {
    pub crtc: Option<usize>,
    pub visible: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Plane {
    pub id: u32,
    pub name: &'static str,
    pub plane_type: u8,
    pub crtc: Option<usize>,
    pub state: PlaneState,
    pub min_cdclk: bool,
}
impl Default for Plane {
    fn default() -> Self {
        Self { id: 0, name: "", plane_type: 0, crtc: None, state: PlaneState::default(), min_cdclk: false }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crtc {
    pub id: u32,
    pub name: &'static str,
    pub pipe: u8,
    pub drm_mask: u32,
    pub primary_plane: Option<usize>,
    pub enabled: bool,
    pub active: bool,
    pub state: CrtcState,
    pub enabled_power_domains: u128,
}
impl Default for Crtc {
    fn default() -> Self {
        Self { id: 0, name: "", pipe: PIPE_A, drm_mask: 0, primary_plane: None,
            enabled: false, active: false, state: CrtcState::default(), enabled_power_domains: 0 }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Encoder {
    pub id: u32,
    pub name: &'static str,
    pub base_crtc: Option<usize>,
    pub digital_port: Option<u32>,
    pub drm_mask: u32,
    pub phys_mask_bit: u32,
    pub has_disable: bool,
    pub has_post_disable: bool,
    pub has_sync_state: bool,
    pub has_get_power_domains: bool,
}
impl Default for Encoder {
    fn default() -> Self {
        Self { id: 0, name: "", base_crtc: None, digital_port: None, drm_mask: 0,
            phys_mask_bit: 0, has_disable: false, has_post_disable: false,
            has_sync_state: false, has_get_power_domains: false }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorState {
    pub crtc: Option<usize>,
    pub best_encoder: Option<usize>,
    pub max_bpc: u32,
}
impl Default for ConnectorState {
    fn default() -> Self { Self { crtc: None, best_encoder: None, max_bpc: 0 } }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Connector {
    pub id: u32,
    pub name: &'static str,
    pub encoder: Option<usize>,
    pub attached_encoder: Option<usize>,
    pub drm_mask: u32,
    pub dpms: i32,
    pub state: ConnectorState,
    pub has_sync_state: bool,
}
impl Default for Connector {
    fn default() -> Self {
        Self { id: 0, name: "", encoder: None, attached_encoder: None, drm_mask: 0,
            dpms: DRM_MODE_DPMS_OFF, state: ConnectorState::default(), has_sync_state: false }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Display {
    pub display_ver: u8,
    pub sandybridge: bool,
    pub haswell: bool,
    pub kabylake: bool,
    pub coffeelake: bool,
    pub cometlake: bool,
    pub has_gmch: bool,
    pub has_ddi: bool,
    pub color_degamma_lut_size: u32,
    pub pipe_mask: u8,
    pub crtcs: [Option<Crtc>; 8],
    pub crtc_count: u8,
    pub planes: Vec<Plane>,
    pub encoders: Vec<Encoder>,
    pub connectors: Vec<Connector>,
    pub pmdemand_phys_mask: u32,
    pub pmdemand_port_clock: [u32; 8],
}

/// Upstream side effects kept at explicit call sites, in source order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Debug(&'static str, u32, u64),
    Warn(&'static str, u32),
    PlaneDisableNoatomic(usize, usize),
    AtomicAcquireCtx(u64, u64),
    AtomicSetInternal(u64, bool),
    AtomicCrtcState(u64, usize),
    AtomicAddAffectedConnectors(u64, usize),
    CrtcDisable(u64, usize),
    AtomicCommitPut(u64),
    DpllCrtcPut(usize, u64),
    ConnectorPut(usize),
    ConnectorGet(usize),
    PmdemandUpdatePhysMask(usize, bool),
    ConnectorListIterBegin,
    ConnectorListIterEnd,
    DestroyCrtcUapiState(usize),
    FreeCrtcHwState(usize),
    ResetCrtcState(usize),
    FbcDisable(usize),
    UpdateWatermarks,
    DisplayPowerPutAll(u128),
    CdclkCrtcDisable(usize),
    WatermarkCrtcDisable(usize),
    BandwidthCrtcDisable(usize),
    DbufBandwidthCrtcDisable(usize),
    PmdemandUpdatePortClock(u8, u32),
    SetCrtcMode(u32, u64),
    ReplaceBlob(&'static str, Option<u64>),
    FixupPlaneBitmasks(usize),
    FifoUnderrunInit(usize, bool),
    ColorCommitNoarm(usize),
    ColorCommitArm(usize),
    SleepMs(u32),
    CrtcStateDump(usize, &'static str),
    EncoderDisable(usize, usize, usize),
    EncoderPostDisable(usize, usize, usize),
    OpregionNotifyEncoder(usize, bool),
    DdiSanitizeEncoderPllMapping(usize),
    DpllReadoutHwState,
    WatermarkGetHwState,
    BandwidthUpdateHwState,
    DbufBandwidthUpdateHwState,
    CdclkUpdateHwState,
    PmdemandInitParams,
    VgaDisable,
    PchSanitize,
    CmtgSanitize,
    DmcEnablePipe(usize),
    CrtcVblankReset(usize),
    CrtcVblankOn(usize),
    FbcSanitize,
    DpllSanitizeState,
    WatermarkSanitize,
    ModesetPutCrtcPowerDomains(usize, u128),
    DisplayPowerPutInit(u64),
    DisplayPowerSanitizeState,
    DisplayWa14010480278,
    DeRmw(u32, u32, u32),
}

/// DRM/object and hardware operations invoked by translated routines.
/// Defaults are inert so a caller must opt in to real effects explicitly.
pub trait ModesetOps {
    fn action(&mut self, _action: Action) {}
    fn link_needs_reset(&mut self, _digital_port: u32) -> bool { false }
    fn plane_get_hw_state(&mut self, _plane: usize, _pipe: &mut u8) -> bool { false }
    fn encoder_get_hw_state(&mut self, _encoder: usize, _pipe: &mut u8) -> bool { false }
    fn connector_get_hw_state(&mut self, _connector: usize) -> bool { false }
    fn get_pipe_config(&mut self, _crtc: usize, _state: &mut CrtcState) {}
    fn encoder_get_config(&mut self, _encoder: usize, _state: &mut CrtcState) {}
    fn encoder_sync_state(&mut self, _encoder: usize, _state: Option<&CrtcState>) {}
    fn connector_sync_state(&mut self, _connector: usize, _state: Option<&CrtcState>) {}
    fn crtc_min_cdclk(&mut self, _state: &CrtcState) -> u32 { 0 }
    fn crtc_update_active_timings(&mut self, _crtc: usize, _state: &mut CrtcState, _vrr: bool) {}
    fn encoder_get_power_domains(&mut self, _encoder: usize, _state: &mut CrtcState) {}
    fn alloc_atomic_commit(&mut self) -> Option<u64> { None }
    fn atomic_get_crtc_state(&mut self, _commit: u64, _crtc: usize) -> bool { false }
    fn atomic_add_affected_connectors(&mut self, _commit: u64, _crtc: usize) -> i32 { 0 }
    fn drm_atomic_set_mode_for_crtc(&mut self, _crtc: u32, _mode: u64) -> i32 { 0 }
    fn color_background_color_hw_to_drm(&mut self, color: u64) -> u64 { color }
    fn intel_display_wa_14010480278(&mut self) -> bool { false }
    fn display_power_get_init(&mut self) -> u64 { 0 }
    fn modeset_get_crtc_power_domains(&mut self, _crtc: usize) -> u128 { 0 }
}

#[inline]
fn crtcs(display: &Display) -> impl Iterator<Item = usize> + '_ {
    (0..display.crtc_count as usize).filter(|&i| display.crtcs[i].is_some())
}
#[inline]
fn crtc_for_pipe(display: &Display, pipe: u8) -> Option<usize> {
    crtcs(display).find(|&i| display.crtcs[i].unwrap().pipe == pipe)
}
#[inline]
fn crtc_for_pipe_mask(display: &Display, mask: u8) -> Vec<usize> {
    crtcs(display).filter(|&i| mask & bit(display.crtcs[i].unwrap().pipe) != 0).collect()
}
#[inline]
fn joiner_secondary_pipes(state: &CrtcState) -> u8 { state.joiner_secondary_pipes }

#[inline]
fn reset_local_crtc_state(display: &mut Display, crtc: usize) {
    let object = display.crtcs[crtc].unwrap();
    let mut state = CrtcState::default();
    state.pipe = object.pipe;
    state.uapi_crtc = object.id;
    display.crtcs[crtc].as_mut().unwrap().state = state;
}

#[inline]
fn update_pmdemand_phys_mask(display: &mut Display, ops: &mut impl ModesetOps,
                             encoder: usize, active: bool) {
    let bit = display.encoders[encoder].phys_mask_bit;
    if active { display.pmdemand_phys_mask |= bit; }
    else { display.pmdemand_phys_mask &= !bit; }
    ops.action(Action::PmdemandUpdatePhysMask(encoder, active));
}

#[inline]
fn update_pmdemand_port_clock(display: &mut Display, ops: &mut impl ModesetOps,
                              pipe: u8, port_clock: u32) {
    if (pipe as usize) < display.pmdemand_port_clock.len() {
        display.pmdemand_port_clock[pipe as usize] = port_clock;
    }
    ops.action(Action::PmdemandUpdatePortClock(pipe, port_clock));
}

// upstream: intel_modeset_setup.c intel_crtc_disable_noatomic_begin()
fn intel_crtc_disable_noatomic_begin(display: &mut Display, ops: &mut impl ModesetOps,
                                     crtc: usize, ctx: u64) {
    let crtc_state = display.crtcs[crtc].unwrap().state;
    if !crtc_state.hw.active { return; }

    let plane_ids: Vec<usize> = display.planes.iter().enumerate()
        .filter(|(_, p)| p.state.crtc == Some(crtc) && p.state.visible)
        .map(|(i, _)| i).collect();
    for plane in plane_ids { ops.action(Action::PlaneDisableNoatomic(crtc, plane)); }

    let Some(commit) = ops.alloc_atomic_commit() else {
        let c = display.crtcs[crtc].unwrap();
        ops.action(Action::Debug("failed to disable [CRTC:%d:%s], out of memory", c.id, 0));
        return;
    };
    ops.action(Action::AtomicAcquireCtx(commit, ctx));
    ops.action(Action::AtomicSetInternal(commit, true));
    let pipe_mask = bit(display.crtcs[crtc].unwrap().pipe) | joiner_secondary_pipes(&crtc_state);
    for temp_crtc in crtc_for_pipe_mask(display, pipe_mask) {
        let state_error = ops.atomic_get_crtc_state(commit, temp_crtc);
        let connector_error = ops.atomic_add_affected_connectors(commit, temp_crtc);
        if state_error || connector_error != 0 {
            ops.action(Action::Warn("IS_ERR(temp_crtc_state) || ret", display.crtcs[temp_crtc].unwrap().id));
        }
    }
    ops.action(Action::CrtcDisable(commit, crtc));
    ops.action(Action::AtomicCommitPut(commit));
    let c = display.crtcs[crtc].unwrap();
    ops.action(Action::Debug("[CRTC:%d:%s] hw state adjusted, was enabled, now disabled", c.id, 0));
    let crtc_data = display.crtcs[crtc].as_mut().unwrap();
    crtc_data.active = false;
    crtc_data.enabled = false;
    if let Some(dpll) = crtc_state.intel_dpll {
        ops.action(Action::DpllCrtcPut(crtc, dpll));
    }
}

// upstream: intel_modeset_setup.c set_encoder_for_connector()
fn set_encoder_for_connector(display: &mut Display, ops: &mut impl ModesetOps,
                             connector: usize, encoder: Option<usize>) {
    if display.connectors[connector].state.crtc.is_some() {
        ops.action(Action::ConnectorPut(connector));
    }
    if let Some(encoder_id) = encoder {
        let crtc = display.encoders[encoder_id].base_crtc;
        let conn = &mut display.connectors[connector];
        conn.state.best_encoder = Some(encoder_id);
        conn.state.crtc = crtc;
        ops.action(Action::ConnectorGet(connector));
    } else {
        let conn = &mut display.connectors[connector];
        conn.state.best_encoder = None;
        conn.state.crtc = None;
    }
}

// upstream: intel_modeset_setup.c reset_encoder_connector_state()
fn reset_encoder_connector_state(display: &mut Display, ops: &mut impl ModesetOps,
                                 encoder: usize) {
    ops.action(Action::ConnectorListIterBegin);
    let connector_ids: Vec<usize> = display.connectors.iter().enumerate()
        .filter(|(_, c)| c.encoder == Some(encoder)).map(|(i, _)| i).collect();
    for connector in connector_ids {
        update_pmdemand_phys_mask(display, ops, encoder, false);
        set_encoder_for_connector(display, ops, connector, None);
        let conn = &mut display.connectors[connector];
        conn.dpms = DRM_MODE_DPMS_OFF;
        conn.encoder = None;
    }
    ops.action(Action::ConnectorListIterEnd);
}

// upstream: intel_modeset_setup.c reset_crtc_encoder_state()
fn reset_crtc_encoder_state(display: &mut Display, ops: &mut impl ModesetOps, crtc: usize) {
    let encoders: Vec<usize> = display.encoders.iter().enumerate()
        .filter(|(_, e)| e.base_crtc == Some(crtc)).map(|(i, _)| i).collect();
    for encoder in encoders {
        reset_encoder_connector_state(display, ops, encoder);
        display.encoders[encoder].base_crtc = None;
    }
}

// upstream: intel_modeset_setup.c intel_crtc_disable_noatomic_complete()
fn intel_crtc_disable_noatomic_complete(display: &mut Display, ops: &mut impl ModesetOps,
                                        crtc: usize) {
    let pipe = display.crtcs[crtc].unwrap().pipe;
    ops.action(Action::DestroyCrtcUapiState(crtc));
    ops.action(Action::FreeCrtcHwState(crtc));
    ops.action(Action::ResetCrtcState(crtc));
    reset_local_crtc_state(display, crtc);
    reset_crtc_encoder_state(display, ops, crtc);
    ops.action(Action::FbcDisable(crtc));
    ops.action(Action::UpdateWatermarks);
    let domains = display.crtcs[crtc].unwrap().enabled_power_domains;
    ops.action(Action::DisplayPowerPutAll(domains));
    display.crtcs[crtc].as_mut().unwrap().enabled_power_domains = 0;
    ops.action(Action::CdclkCrtcDisable(crtc));
    ops.action(Action::WatermarkCrtcDisable(crtc));
    ops.action(Action::BandwidthCrtcDisable(crtc));
    ops.action(Action::DbufBandwidthCrtcDisable(crtc));
    update_pmdemand_port_clock(display, ops, pipe, 0);
}

// upstream: intel_modeset_setup.c get_transcoder_pipes()
fn get_transcoder_pipes(display: &Display, transcoder_mask: u8) -> u8 {
    let mut pipes = 0;
    for crtc in crtcs(display) {
        let state = display.crtcs[crtc].unwrap().state;
        if state.cpu_transcoder == INVALID_TRANSCODER || state.joiner_secondary { continue; }
        if transcoder_mask & bit(state.cpu_transcoder) != 0 {
            pipes |= bit(display.crtcs[crtc].unwrap().pipe);
        }
    }
    pipes
}

// upstream: intel_modeset_setup.c get_portsync_pipes()
fn get_portsync_pipes(display: &Display, crtc: usize) -> (u8, u8) {
    let crtc_state = display.crtcs[crtc].unwrap().state;
    if !crtc_state.port_sync_mode { return (bit(display.crtcs[crtc].unwrap().pipe), 0); }
    let master_transcoder = if crtc_state.port_sync_master {
        crtc_state.cpu_transcoder
    } else { crtc_state.master_transcoder };
    let master_pipe_mask = get_transcoder_pipes(display, bit(master_transcoder));
    debug_assert!(master_pipe_mask.count_ones() == 1);
    let master_pipe = master_pipe_mask.trailing_zeros() as u8;
    let master_crtc = crtc_for_pipe(display, master_pipe).expect("upstream invariant: port-sync master exists");
    let master_crtc_state = display.crtcs[master_crtc].unwrap().state;
    let slave_pipes_mask = get_transcoder_pipes(display, master_crtc_state.sync_mode_slaves_mask);
    (master_pipe_mask, slave_pipes_mask)
}

// upstream: intel_modeset_setup.c get_joiner_secondary_pipes()
fn get_joiner_secondary_pipes(display: &Display, primary_pipes_mask: u8) -> u8 {
    let mut pipes = 0;
    for primary_crtc in crtc_for_pipe_mask(display, primary_pipes_mask) {
        pipes |= joiner_secondary_pipes(&display.crtcs[primary_crtc].unwrap().state);
    }
    pipes
}

// upstream: intel_modeset_setup.c intel_crtc_disable_noatomic()
fn intel_crtc_disable_noatomic(display: &mut Display, ops: &mut impl ModesetOps,
                               crtc: usize, ctx: u64) {
    let (portsync_master_mask, portsync_slaves_mask) = get_portsync_pipes(display, crtc);
    let joiner_secondaries_mask = get_joiner_secondary_pipes(display,
        portsync_master_mask | portsync_slaves_mask);
    if portsync_master_mask & portsync_slaves_mask != 0 ||
       portsync_master_mask & joiner_secondaries_mask != 0 ||
       portsync_slaves_mask & joiner_secondaries_mask != 0 {
        ops.action(Action::Warn("overlapping port-sync/joiner pipe masks", 0));
    }
    for c in crtc_for_pipe_mask(display, joiner_secondaries_mask) {
        intel_crtc_disable_noatomic_begin(display, ops, c, ctx);
    }
    for c in crtc_for_pipe_mask(display, portsync_slaves_mask) {
        intel_crtc_disable_noatomic_begin(display, ops, c, ctx);
    }
    for c in crtc_for_pipe_mask(display, portsync_master_mask) {
        intel_crtc_disable_noatomic_begin(display, ops, c, ctx);
    }
    let all = joiner_secondaries_mask | portsync_slaves_mask | portsync_master_mask;
    for c in crtc_for_pipe_mask(display, all) {
        intel_crtc_disable_noatomic_complete(display, ops, c);
    }
}

// upstream: intel_modeset_setup.c intel_modeset_update_connector_atomic_state()
fn intel_modeset_update_connector_atomic_state(display: &mut Display, ops: &mut impl ModesetOps) {
    ops.action(Action::ConnectorListIterBegin);
    for connector in 0..display.connectors.len() {
        let encoder = display.connectors[connector].encoder;
        set_encoder_for_connector(display, ops, connector, encoder);
        if let Some(encoder) = encoder {
            let crtc = display.encoders[encoder].base_crtc
                .expect("upstream invariant: connector encoder has a CRTC");
            let pipe_bpp = display.crtcs[crtc].unwrap().state.pipe_bpp;
            display.connectors[connector].state.max_bpc = (if pipe_bpp != 0 { pipe_bpp } else { 24 }) / 3;
        }
    }
    ops.action(Action::ConnectorListIterEnd);
}

// upstream: intel_modeset_setup.c intel_crtc_copy_hw_to_uapi_state()
fn intel_crtc_copy_hw_to_uapi_state(display: &mut Display, ops: &mut impl ModesetOps, crtc: usize) {
    let mut state = display.crtcs[crtc].unwrap().state;
    if state.joiner_secondary { return; }
    let result = ops.drm_atomic_set_mode_for_crtc(state.uapi_crtc, state.hw.mode);
    if result < 0 { ops.action(Action::Warn("drm_atomic_set_mode_for_crtc() < 0", state.uapi_crtc)); }
    let mut uapi = state.uapi;
    uapi.enable = state.hw.enable;
    uapi.active = state.hw.active;
    uapi.mode = state.hw.mode;
    uapi.adjusted_mode = state.hw.adjusted_mode;
    uapi.scaling_filter = state.hw.scaling_filter;
    uapi.sharpness_strength = state.hw.sharpness_strength;
    uapi.background_color = ops.color_background_color_hw_to_drm(state.hw.background_color);
    let (hw_degamma, hw_gamma) = if display.color_degamma_lut_size != 0 {
        (state.pre_csc_lut, state.post_csc_lut)
    } else {
        if state.post_csc_lut.is_some() && state.pre_csc_lut.is_some() {
            ops.action(Action::Warn("post_csc_lut && pre_csc_lut", state.uapi_crtc));
        }
        (None, state.post_csc_lut.or(state.pre_csc_lut))
    };
    state.hw.degamma_lut = hw_degamma;
    state.hw.gamma_lut = hw_gamma;
    ops.action(Action::ReplaceBlob("hw.degamma_lut", hw_degamma));
    ops.action(Action::ReplaceBlob("hw.gamma_lut", hw_gamma));
    uapi.degamma_lut = hw_degamma;
    uapi.gamma_lut = hw_gamma;
    uapi.ctm = state.hw.ctm;
    ops.action(Action::ReplaceBlob("uapi.degamma_lut", uapi.degamma_lut));
    ops.action(Action::ReplaceBlob("uapi.gamma_lut", uapi.gamma_lut));
    ops.action(Action::ReplaceBlob("uapi.ctm", uapi.ctm));
    state.uapi = uapi;
    display.crtcs[crtc].as_mut().unwrap().state = state;
}

// upstream: intel_modeset_setup.c intel_sanitize_plane_mapping()
fn intel_sanitize_plane_mapping(display: &mut Display, ops: &mut impl ModesetOps) {
    if display.display_ver >= 4 { return; }
    for crtc in crtcs(display).collect::<Vec<_>>() {
        let Some(plane) = display.crtcs[crtc].unwrap().primary_plane else { continue; };
        let mut pipe = PIPE_A;
        if !ops.plane_get_hw_state(plane, &mut pipe) { continue; }
        if pipe == display.crtcs[crtc].unwrap().pipe { continue; }
        let p = display.planes[plane];
        ops.action(Action::Debug("[PLANE:%d:%s] attached to the wrong pipe, disabling plane", p.id, pipe as u64));
        let plane_crtc = crtc_for_pipe(display, pipe).expect("upstream invariant: mapped pipe has a CRTC");
        ops.action(Action::PlaneDisableNoatomic(plane_crtc, plane));
    }
}

// upstream: intel_modeset_setup.c intel_crtc_has_encoders()
fn intel_crtc_has_encoders(display: &Display, crtc: usize) -> bool {
    display.encoders.iter().any(|encoder| encoder.base_crtc == Some(crtc))
}

// upstream: intel_modeset_setup.c intel_crtc_needs_link_reset()
fn intel_crtc_needs_link_reset(display: &Display, ops: &mut impl ModesetOps, crtc: usize) -> bool {
    display.encoders.iter().any(|encoder| {
        encoder.base_crtc == Some(crtc) && encoder.digital_port
            .is_some_and(|port| ops.link_needs_reset(port))
    })
}

// upstream: intel_modeset_setup.c intel_encoder_find_connector()
fn intel_encoder_find_connector(display: &Display, ops: &mut impl ModesetOps,
                                encoder: usize) -> Option<usize> {
    ops.action(Action::ConnectorListIterBegin);
    let found = display.connectors.iter().position(|connector| connector.encoder == Some(encoder));
    ops.action(Action::ConnectorListIterEnd);
    found
}

// upstream: intel_modeset_setup.c intel_sanitize_fifo_underrun_reporting()
fn intel_sanitize_fifo_underrun_reporting(display: &Display, ops: &mut impl ModesetOps,
                                          crtc: usize) {
    let active = display.crtcs[crtc].unwrap().state.hw.active;
    ops.action(Action::FifoUnderrunInit(crtc, !active && !display.has_gmch));
}

// upstream: intel_modeset_setup.c intel_sanitize_crtc()
fn intel_sanitize_crtc(display: &mut Display, ops: &mut impl ModesetOps,
                       crtc: usize, ctx: u64) -> bool {
    let crtc_state = display.crtcs[crtc].unwrap().state;
    if crtc_state.hw.active {
        let plane_ids: Vec<usize> = display.planes.iter().enumerate()
            .filter(|(_, p)| p.state.crtc == Some(crtc) && p.state.visible &&
                p.plane_type != DRM_PLANE_TYPE_PRIMARY).map(|(i, _)| i).collect();
        for plane in plane_ids { ops.action(Action::PlaneDisableNoatomic(crtc, plane)); }
        ops.action(Action::ColorCommitNoarm(crtc));
        ops.action(Action::ColorCommitArm(crtc));
    }
    if !crtc_state.hw.active || crtc_state.joiner_secondary { return false; }
    let needs_link_reset = intel_crtc_needs_link_reset(display, ops, crtc);
    if !needs_link_reset && intel_crtc_has_encoders(display, crtc) { return false; }
    intel_crtc_disable_noatomic(display, ops, crtc, ctx);
    if needs_link_reset { ops.action(Action::SleepMs(20)); }
    true
}

// upstream: intel_modeset_setup.c intel_sanitize_all_crtcs()
fn intel_sanitize_all_crtcs(display: &mut Display, ops: &mut impl ModesetOps, ctx: u64) {
    let mut crtcs_forced_off = 0u32;
    loop {
        let old_mask = crtcs_forced_off;
        for crtc in crtcs(display).collect::<Vec<_>>() {
            let crtc_mask = display.crtcs[crtc].unwrap().drm_mask;
            if crtcs_forced_off & crtc_mask != 0 { continue; }
            if intel_sanitize_crtc(display, ops, crtc, ctx) { crtcs_forced_off |= crtc_mask; }
        }
        if crtcs_forced_off == old_mask { break; }
    }
    for crtc in crtcs(display) { ops.action(Action::CrtcStateDump(crtc, "setup_hw_state")); }
}

// upstream: intel_modeset_setup.c has_bogus_dpll_config()
fn has_bogus_dpll_config(display: &Display, crtc_state: &CrtcState) -> bool {
    display.sandybridge && crtc_state.hw.active && crtc_state.intel_dpll.is_some() && crtc_state.port_clock == 0
}

// upstream: intel_modeset_setup.c intel_sanitize_encoder()
fn intel_sanitize_encoder(display: &mut Display, ops: &mut impl ModesetOps, encoder: usize) {
    let crtc = display.encoders[encoder].base_crtc;
    let crtc_state = crtc.map(|i| display.crtcs[i].unwrap().state);
    let mut has_active_crtc = crtc_state.is_some_and(|state| state.hw.active);
    if let (Some(crtc), Some(state)) = (crtc, crtc_state) {
        if has_bogus_dpll_config(display, &state) {
            ops.action(Action::Debug("BIOS has misprogrammed the hardware. Disabling pipe %c", display.crtcs[crtc].unwrap().pipe as u32, 0));
            has_active_crtc = false;
        }
    }
    let connector = intel_encoder_find_connector(display, ops, encoder);
    if let Some(connector) = connector.filter(|_| !has_active_crtc) {
        let e = display.encoders[encoder];
        ops.action(Action::Debug("[ENCODER:%d:%s] has active connectors but no active pipe!", e.id, 0));
        update_pmdemand_phys_mask(display, ops, encoder, false);
        if let (Some(crtc), Some(_)) = (crtc, crtc_state) {
            ops.action(Action::Debug("[ENCODER:%d:%s] manually disabled", e.id, 0));
            let best_encoder = display.connectors[connector].state.best_encoder;
            display.connectors[connector].state.best_encoder = Some(encoder);
            if e.has_disable { ops.action(Action::EncoderDisable(encoder, crtc, connector)); }
            if e.has_post_disable { ops.action(Action::EncoderPostDisable(encoder, crtc, connector)); }
            display.connectors[connector].state.best_encoder = best_encoder;
        }
        display.encoders[encoder].base_crtc = None;
        display.connectors[connector].dpms = DRM_MODE_DPMS_OFF;
        display.connectors[connector].encoder = None;
    }
    ops.action(Action::OpregionNotifyEncoder(encoder, connector.is_some() && has_active_crtc));
    if display.has_ddi { ops.action(Action::DdiSanitizeEncoderPllMapping(encoder)); }
}

// upstream: intel_modeset_setup.c readout_plane_state()
fn readout_plane_state(display: &mut Display, ops: &mut impl ModesetOps) {
    for plane in 0..display.planes.len() {
        let mut pipe = PIPE_A;
        let visible = ops.plane_get_hw_state(plane, &mut pipe);
        let crtc = crtc_for_pipe(display, pipe).expect("upstream invariant: hardware plane pipe has a CRTC");
        display.planes[plane].state.crtc = Some(crtc);
        display.planes[plane].state.visible = visible;
        let mask = 1u32 << display.planes[plane].id;
        let state = &mut display.crtcs[crtc].as_mut().unwrap().state;
        if visible { state.active_planes |= mask; } else { state.active_planes &= !mask; }
        ops.action(Action::Debug("[PLANE:%d:%s] hw state readout: %s, pipe %c", display.planes[plane].id, pipe as u64));
    }
    for crtc in crtcs(display).collect::<Vec<_>>() { ops.action(Action::FixupPlaneBitmasks(crtc)); }
}

// upstream: intel_modeset_setup.c intel_modeset_readout_hw_state()
fn intel_modeset_readout_hw_state(display: &mut Display, ops: &mut impl ModesetOps) {
    for crtc in crtcs(display).collect::<Vec<_>>() {
        ops.action(Action::DestroyCrtcUapiState(crtc));
        ops.action(Action::FreeCrtcHwState(crtc));
        ops.action(Action::ResetCrtcState(crtc));
        reset_local_crtc_state(display, crtc);
        let mut state = display.crtcs[crtc].unwrap().state;
        ops.get_pipe_config(crtc, &mut state);
        state.hw.enable = state.hw.active;
        display.crtcs[crtc].as_mut().unwrap().state = state;
        display.crtcs[crtc].as_mut().unwrap().enabled = state.hw.enable;
        display.crtcs[crtc].as_mut().unwrap().active = state.hw.active;
        ops.action(Action::Debug("[CRTC:%d:%s] hw state readout: %s", display.crtcs[crtc].unwrap().id, state.hw.active as u64));
    }

    readout_plane_state(display, ops);
    let mut pipe = 0u8;
    for encoder in 0..display.encoders.len() {
        let mut crtc_state = None;
        pipe = 0;
        if ops.encoder_get_hw_state(encoder, &mut pipe) {
            let crtc = crtc_for_pipe(display, pipe)
                .expect("upstream invariant: enabled encoder pipe has a CRTC");
                crtc_state = Some(crtc);
                display.encoders[encoder].base_crtc = Some(crtc);
                let mut state = display.crtcs[crtc].unwrap().state;
                ops.encoder_get_config(encoder, &mut state);
                display.crtcs[crtc].as_mut().unwrap().state = state;
                if state.joiner_pipes != 0 {
                    if state.joiner_secondary {
                        ops.action(Action::Warn("encoder should be linked to joiner primary", display.crtcs[crtc].unwrap().id));
                    }
                    for secondary_crtc in crtc_for_pipe_mask(display, joiner_secondary_pipes(&state)) {
                        let mut secondary = display.crtcs[secondary_crtc].unwrap().state;
                        ops.encoder_get_config(encoder, &mut secondary);
                        display.crtcs[secondary_crtc].as_mut().unwrap().state = secondary;
                    }
                }
            update_pmdemand_phys_mask(display, ops, encoder, true);
        } else {
            update_pmdemand_phys_mask(display, ops, encoder, false);
            display.encoders[encoder].base_crtc = None;
        }
        if display.encoders[encoder].has_sync_state {
            let state = crtc_state.map(|crtc| &display.crtcs[crtc].as_ref().unwrap().state);
            ops.encoder_sync_state(encoder, state);
        }
        let e = display.encoders[encoder];
        ops.action(Action::Debug("[ENCODER:%d:%s] hw state readout: %s, pipe %c", e.id, pipe as u64));
    }

    ops.action(Action::DpllReadoutHwState);
    ops.action(Action::ConnectorListIterBegin);
    for connector in 0..display.connectors.len() {
        let mut crtc_state = None;
        if ops.connector_get_hw_state(connector) {
            display.connectors[connector].dpms = DRM_MODE_DPMS_ON;
            let encoder = display.connectors[connector].attached_encoder;
            display.connectors[connector].encoder = encoder;
            let encoder = encoder.expect("upstream invariant: connected connector has attached encoder");
            if let Some(crtc) = display.encoders[encoder].base_crtc {
                let state = display.crtcs[crtc].unwrap().state;
                crtc_state = Some(crtc);
                if state.hw.active {
                    display.crtcs[crtc].as_mut().unwrap().state.connector_mask |= display.connectors[connector].drm_mask;
                    display.crtcs[crtc].as_mut().unwrap().state.encoder_mask |= display.encoders[encoder].drm_mask;
                }
            }
        } else {
            display.connectors[connector].dpms = DRM_MODE_DPMS_OFF;
            display.connectors[connector].encoder = None;
        }
        if display.connectors[connector].has_sync_state {
            let state = crtc_state.map(|crtc| &display.crtcs[crtc].as_ref().unwrap().state);
            ops.connector_sync_state(connector, state);
        }
        ops.action(Action::Debug("[CONNECTOR:%d:%s] hw state readout: %s", display.connectors[connector].id, display.connectors[connector].encoder.is_some() as u64));
    }
    ops.action(Action::ConnectorListIterEnd);

    for crtc in crtcs(display).collect::<Vec<_>>() {
        let mut state = display.crtcs[crtc].unwrap().state;
        state.inherited = true;
        if state.hw.active {
            let vrr_enable = state.vrr_enable;
            ops.crtc_update_active_timings(crtc, &mut state, vrr_enable);
            display.crtcs[crtc].as_mut().unwrap().state = state;
            intel_crtc_copy_hw_to_uapi_state(display, ops, crtc);
            state = display.crtcs[crtc].unwrap().state;
        }
        let planes: Vec<usize> = display.planes.iter().enumerate()
            .filter(|(_, p)| p.state.crtc == Some(crtc)).map(|(i, _)| i).collect();
        for plane in planes {
            let p = display.planes[plane];
            if p.state.visible { state.data_rate[p.id as usize] = 4 * state.pixel_rate; }
            if p.state.visible && p.min_cdclk {
                if state.double_wide || display.display_ver >= 10 {
                    state.plane_min_cdclk[p.id as usize] = (state.pixel_rate + 1) / 2;
                } else { state.plane_min_cdclk[p.id as usize] = state.pixel_rate; }
            }
            ops.action(Action::Debug("[PLANE:%d:%s] min_cdclk %d kHz", p.id, state.plane_min_cdclk[p.id as usize] as u64));
        }
        state.min_cdclk = ops.crtc_min_cdclk(&state);
        ops.action(Action::Debug("[CRTC:%d:%s] min_cdclk %d kHz", display.crtcs[crtc].unwrap().id, state.min_cdclk as u64));
        display.crtcs[crtc].as_mut().unwrap().state = state;
        update_pmdemand_port_clock(display, ops, pipe, state.port_clock);
    }

    if display.display_ver >= 9 { ops.action(Action::WatermarkGetHwState); }
    ops.action(Action::BandwidthUpdateHwState);
    ops.action(Action::DbufBandwidthUpdateHwState);
    ops.action(Action::CdclkUpdateHwState);
    ops.action(Action::PmdemandInitParams);
}

// upstream: intel_modeset_setup.c get_encoder_power_domains()
fn get_encoder_power_domains(display: &mut Display, ops: &mut impl ModesetOps) {
    for encoder in 0..display.encoders.len() {
        if !display.encoders[encoder].has_get_power_domains { continue; }
        let Some(crtc) = display.encoders[encoder].base_crtc else { continue; };
        let mut state = display.crtcs[crtc].unwrap().state;
        ops.encoder_get_power_domains(encoder, &mut state);
        display.crtcs[crtc].as_mut().unwrap().state = state;
    }
}

// upstream: intel_modeset_setup.c intel_early_display_was()
fn intel_early_display_was(display: &Display, ops: &mut impl ModesetOps) {
    if ops.intel_display_wa_14010480278() {
        ops.action(Action::DeRmw(GEN9_CLKGATE_DIS_0, 0, DARBF_GATING_DIS));
    }
    if display.haswell { ops.action(Action::DeRmw(CHICKEN_PAR1_1, 0, FORCE_ARB_IDLE_PLANES)); }
    if display.kabylake || display.coffeelake || display.cometlake {
        ops.action(Action::DeRmw(CHICKEN_PAR1_1, KBL_ARB_FILL_SPARE_22, KBL_ARB_FILL_SPARE_22));
        ops.action(Action::DeRmw(CHICKEN_MISC_2,
            KBL_ARB_FILL_SPARE_13 | KBL_ARB_FILL_SPARE_14, KBL_ARB_FILL_SPARE_14));
    }
}

// upstream: intel_modeset_setup.c intel_modeset_setup_hw_state()
pub fn intel_modeset_setup_hw_state(display: &mut Display, ops: &mut impl ModesetOps,
                                    ctx: u64) {
    let wakeref = ops.display_power_get_init();
    intel_early_display_was(display, ops);
    ops.action(Action::VgaDisable);
    intel_modeset_readout_hw_state(display, ops);
    get_encoder_power_domains(display, ops);
    ops.action(Action::PchSanitize);
    ops.action(Action::CmtgSanitize);
    for crtc in crtcs(display).collect::<Vec<_>>() {
        intel_sanitize_fifo_underrun_reporting(display, ops, crtc);
        ops.action(Action::CrtcVblankReset(crtc));
        if display.crtcs[crtc].unwrap().state.hw.active {
            ops.action(Action::DmcEnablePipe(crtc));
            ops.action(Action::CrtcVblankOn(crtc));
        }
    }
    ops.action(Action::FbcSanitize);
    intel_sanitize_plane_mapping(display, ops);
    for encoder in 0..display.encoders.len() { intel_sanitize_encoder(display, ops, encoder); }
    intel_modeset_update_connector_atomic_state(display, ops);
    intel_sanitize_all_crtcs(display, ops, ctx);
    ops.action(Action::DpllSanitizeState);
    if display.display_ver < 9 { ops.action(Action::WatermarkGetHwState); }
    ops.action(Action::WatermarkSanitize);
    for crtc in crtcs(display) {
        let put_domains = ops.modeset_get_crtc_power_domains(crtc);
        if put_domains != 0 {
            ops.action(Action::Warn("!bitmap_empty(put_domains.bits, POWER_DOMAIN_NUM)", display.crtcs[crtc].unwrap().id));
            ops.action(Action::ModesetPutCrtcPowerDomains(crtc, put_domains));
        }
    }
    ops.action(Action::DisplayPowerPutInit(wakeref));
    ops.action(Action::DisplayPowerSanitizeState);
}
