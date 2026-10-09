// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_modeset_verify.c translation.
// DRM object lookup, state allocation/readout, and diagnostic output are hooks.

#![allow(dead_code, clippy::too_many_arguments)]

extern crate alloc;
use alloc::vec::Vec;

pub type DrmId = u32;
pub type ConnectorId = u32;
pub type EncoderId = u32;
pub type CrtcId = u32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Pipe {
    #[default]
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
}

impl Pipe {
    fn name(self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
            Self::E => 'E',
            Self::F => 'F',
            Self::G => 'G',
            Self::H => 'H',
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Display {
    pub drm_id: DrmId,
    pub i830: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncoderType {
    Other,
    DpMst,
}

impl Default for EncoderType {
    fn default() -> Self {
        Self::Other
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Connector {
    pub id: ConnectorId,
    pub name: &'static str,
    /// Legacy DRM connector-to-encoder association (`connector->encoder`).
    pub encoder: Option<EncoderId>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConnectorState {
    pub crtc: Option<CrtcId>,
    pub best_encoder: Option<EncoderId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectorChange {
    pub connector: Connector,
    pub old_state: ConnectorState,
    pub new_state: ConnectorState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Encoder {
    pub id: EncoderId,
    pub name: &'static str,
    pub crtc: Option<CrtcId>,
    pub encoder_type: EncoderType,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FdiMn {
    pub data_m: u32,
    pub data_n: u32,
    pub link_m: u32,
    pub link_n: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcHwState {
    pub active: bool,
    pub enable: bool,
    pub adjusted_mode_crtc_clock: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub hw: CrtcHwState,
    pub needs_modeset: bool,
    pub needs_fastset: bool,
    pub has_pch_encoder: bool,
    pub fdi_link_frequency: u32,
    pub fdi_m_n: FdiMn,
    /// `intel_primary_crtc()` result for this state.
    pub primary_crtc_id: CrtcId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Crtc {
    pub id: CrtcId,
    pub name: &'static str,
    pub pipe: Pipe,
    /// Transitional `crtc->active` state, distinct from atomic hw.active.
    pub active: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CrtcChange {
    pub crtc: Crtc,
    pub new_state: CrtcState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntelAtomicState {
    pub display: Display,
    /// DRM atomic-state connector changes, in iterator order.
    pub connectors: Vec<ConnectorChange>,
    /// DRM device encoder list, in `for_each_intel_encoder()` order.
    pub encoders: Vec<Encoder>,
    /// New CRTC states and CRTC objects needed by this verification pass.
    pub crtcs: Vec<CrtcChange>,
}

/// External DRM/i915 operations used by the source routines. Warning hooks
/// receive the original format string and its integer arguments so a backend
/// can preserve the kernel diagnostic text exactly.
pub trait ModesetVerifyIo {
    fn drm_dbg_kms(&mut self, _drm: DrmId, _format: &'static str, _values: &[i64], _name: &str) {}
    fn display_state_warn(
        &mut self,
        _display: &Display,
        _condition: bool,
        _format: &'static str,
        _values: &[i64],
    ) {
    }
    fn drm_warn(&mut self, _drm: DrmId, _condition: bool, _format: &'static str, _values: &[i64]) {}

    fn connector_get_hw_state(&mut self, _connector: &Connector) -> bool {
        false
    }
    fn attached_encoder(&mut self, _connector: &Connector) -> Option<EncoderId> {
        None
    }
    fn encoder_get_hw_state(&mut self, _encoder: &Encoder) -> (bool, Pipe) {
        (false, Pipe::A)
    }
    fn alloc_crtc_state(&mut self, _crtc: &Crtc) -> Option<CrtcState> {
        None
    }
    fn get_pipe_config(&mut self, _state: &mut CrtcState) {}
    fn encoder_get_config(&mut self, _encoder: &Encoder, _state: &mut CrtcState) {}
    fn dotclock_calculate(
        &mut self,
        _display: &Display,
        _link_frequency: u32,
        _fdi_m_n: &FdiMn,
    ) -> i32 {
        0
    }
    fn pipe_config_compare(&mut self, _sw: &CrtcState, _hw: &CrtcState, _fastset: bool) -> bool {
        true
    }
    fn dump_crtc_state(
        &mut self,
        _state: &CrtcState,
        _context: Option<&str>,
        _label: &'static str,
    ) {
    }
    fn destroy_crtc_state(&mut self, _crtc: &Crtc, _state: CrtcState) {}

    fn wm_state_verify(&mut self, _state: &IntelAtomicState, _crtc: &Crtc) {}
    fn dpll_state_verify(&mut self, _state: &IntelAtomicState, _crtc: &Crtc) {}
    fn mpllb_state_verify(&mut self, _state: &IntelAtomicState, _crtc: &Crtc) {}
    fn dpll_verify_disabled(&mut self, _state: &IntelAtomicState) {}
}

fn new_crtc_state<'a>(state: &'a IntelAtomicState, crtc: &Crtc) -> &'a CrtcState {
    &state
        .crtcs
        .iter()
        .find(|change| change.crtc.id == crtc.id)
        .expect("CRTC must be present in the atomic state")
        .new_state
}

fn encoder_by_id(state: &IntelAtomicState, id: EncoderId) -> Option<&Encoder> {
    state.encoders.iter().find(|encoder| encoder.id == id)
}

// upstream: intel_modeset_verify.c intel_connector_verify_state()
fn intel_connector_verify_state<I: ModesetVerifyIo>(
    io: &mut I,
    display: &Display,
    crtc_state: Option<&CrtcState>,
    connector: &Connector,
    conn_state: &ConnectorState,
    state: &IntelAtomicState,
) {
    io.drm_dbg_kms(
        display.drm_id,
        "[CONNECTOR:%d:%s]\n",
        &[connector.id as i64],
        connector.name,
    );

    if io.connector_get_hw_state(connector) {
        let attached_encoder_id = io.attached_encoder(connector);

        io.display_state_warn(
            display,
            crtc_state.is_none(),
            "connector enabled without attached crtc\n",
            &[],
        );

        let Some(crtc_state) = crtc_state else {
            return;
        };

        io.display_state_warn(
            display,
            !crtc_state.hw.active,
            "connector is active, but attached crtc isn't\n",
            &[],
        );

        let Some(encoder_id) = attached_encoder_id else {
            return;
        };
        let Some(encoder) = encoder_by_id(state, encoder_id) else {
            return;
        };
        if encoder.encoder_type == EncoderType::DpMst {
            return;
        }

        io.display_state_warn(
            display,
            conn_state.best_encoder != Some(encoder.id),
            "atomic encoder doesn't match attached encoder\n",
            &[],
        );
        io.display_state_warn(
            display,
            conn_state.crtc != encoder.crtc,
            "attached encoder crtc differs from connector crtc\n",
            &[],
        );
    } else {
        io.display_state_warn(
            display,
            crtc_state.is_some_and(|state| state.hw.active),
            "attached crtc is active, but connector isn't\n",
            &[],
        );
        io.display_state_warn(
            display,
            crtc_state.is_none() && conn_state.best_encoder.is_some(),
            "best encoder set without crtc!\n",
            &[],
        );
    }
}

// upstream: intel_modeset_verify.c verify_connector_state()
fn verify_connector_state<I: ModesetVerifyIo>(
    io: &mut I,
    state: &IntelAtomicState,
    crtc: Option<&Crtc>,
) {
    let display = &state.display;

    for change in &state.connectors {
        let connector = &change.connector;
        let new_conn_state = &change.new_state;
        if new_conn_state.crtc != crtc.map(|crtc| crtc.id) {
            continue;
        }

        let crtc_state = crtc.map(|crtc| new_crtc_state(state, crtc));
        intel_connector_verify_state(io, display, crtc_state, connector, new_conn_state, state);

        io.display_state_warn(
            display,
            new_conn_state.best_encoder != connector.encoder,
            "connector's atomic encoder doesn't match legacy encoder\n",
            &[],
        );
    }
}

// upstream: intel_modeset_verify.c intel_pipe_config_sanity_check()
fn intel_pipe_config_sanity_check<I: ModesetVerifyIo>(
    io: &mut I,
    display: &Display,
    crtc_state: &CrtcState,
) {
    if crtc_state.has_pch_encoder {
        let fdi_dotclock =
            io.dotclock_calculate(display, crtc_state.fdi_link_frequency, &crtc_state.fdi_m_n);
        let dotclock = crtc_state.hw.adjusted_mode_crtc_clock;

        io.drm_warn(
            display.drm_id,
            (fdi_dotclock - dotclock).abs() > 1,
            "FDI dotclock and encoder dotclock mismatch, fdi: %i, encoder: %i\n",
            &[fdi_dotclock as i64, dotclock as i64],
        );
    }
}

// upstream: intel_modeset_verify.c verify_encoder_state()
fn verify_encoder_state<I: ModesetVerifyIo>(io: &mut I, state: &IntelAtomicState) {
    let display = &state.display;

    for encoder in &state.encoders {
        let mut enabled = false;
        let mut found = false;

        io.drm_dbg_kms(
            display.drm_id,
            "[ENCODER:%d:%s]\n",
            &[encoder.id as i64],
            encoder.name,
        );

        for change in &state.connectors {
            let old_conn_state = &change.old_state;
            let new_conn_state = &change.new_state;

            if old_conn_state.best_encoder == Some(encoder.id) {
                found = true;
            }

            if new_conn_state.best_encoder != Some(encoder.id) {
                continue;
            }

            found = true;
            enabled = true;

            io.display_state_warn(
                display,
                new_conn_state.crtc != encoder.crtc,
                "connector's crtc doesn't match encoder crtc\n",
                &[],
            );
        }

        if !found {
            continue;
        }

        io.display_state_warn(
            display,
            encoder.crtc.is_some() != enabled,
            "encoder's enabled state mismatch (expected %i, found %i)\n",
            &[encoder.crtc.is_some() as i64, enabled as i64],
        );

        if encoder.crtc.is_none() {
            let (active, pipe) = io.encoder_get_hw_state(encoder);
            io.display_state_warn(
                display,
                active,
                "encoder detached but still enabled on pipe %c.\n",
                &[pipe.name() as i64],
            );
        }
    }
}

// upstream: intel_modeset_verify.c verify_crtc_state()
fn verify_crtc_state<I: ModesetVerifyIo>(io: &mut I, state: &IntelAtomicState, crtc: &Crtc) {
    let display = &state.display;
    let sw_crtc_state = new_crtc_state(state, crtc);
    let Some(mut hw_crtc_state) = io.alloc_crtc_state(crtc) else {
        return;
    };

    io.drm_dbg_kms(
        display.drm_id,
        "[CRTC:%d:%s]\n",
        &[crtc.id as i64],
        crtc.name,
    );

    hw_crtc_state.hw.enable = sw_crtc_state.hw.enable;
    io.get_pipe_config(&mut hw_crtc_state);

    // The 830 keeps both pipes enabled.
    if display.i830 && hw_crtc_state.hw.active {
        hw_crtc_state.hw.active = sw_crtc_state.hw.active;
    }

    io.display_state_warn(
        display,
        sw_crtc_state.hw.active != hw_crtc_state.hw.active,
        "crtc active state doesn't match with hw state (expected %i, found %i)\n",
        &[
            sw_crtc_state.hw.active as i64,
            hw_crtc_state.hw.active as i64,
        ],
    );

    io.display_state_warn(
        display,
        crtc.active != sw_crtc_state.hw.active,
        "transitional active state does not match atomic hw state (expected %i, found %i)\n",
        &[sw_crtc_state.hw.active as i64, crtc.active as i64],
    );

    let primary_crtc_id = sw_crtc_state.primary_crtc_id;
    let primary_crtc = state
        .crtcs
        .iter()
        .find(|change| change.crtc.id == primary_crtc_id)
        .map(|change| &change.crtc)
        .expect("primary CRTC must be present in the atomic state");

    for encoder in state
        .encoders
        .iter()
        .filter(|encoder| encoder.crtc == Some(primary_crtc.id))
    {
        let (active, pipe) = io.encoder_get_hw_state(encoder);
        io.display_state_warn(
            display,
            active != sw_crtc_state.hw.active,
            "[ENCODER:%i] active %i with crtc active %i\n",
            &[
                encoder.id as i64,
                active as i64,
                sw_crtc_state.hw.active as i64,
            ],
        );

        io.display_state_warn(
            display,
            active && primary_crtc.pipe != pipe,
            "Encoder connected to wrong pipe %c\n",
            &[pipe.name() as i64],
        );

        if active {
            io.encoder_get_config(encoder, &mut hw_crtc_state);
        }
    }

    if sw_crtc_state.hw.active {
        intel_pipe_config_sanity_check(io, display, &hw_crtc_state);

        if !io.pipe_config_compare(sw_crtc_state, &hw_crtc_state, false) {
            io.display_state_warn(display, true, "pipe state doesn't match!\n", &[]);
            io.dump_crtc_state(&hw_crtc_state, None, "hw state");
            io.dump_crtc_state(sw_crtc_state, None, "sw state");
        }
    }

    io.destroy_crtc_state(crtc, hw_crtc_state);
}

// upstream: intel_modeset_verify.c intel_modeset_verify_crtc()
pub fn intel_modeset_verify_crtc<I: ModesetVerifyIo>(
    io: &mut I,
    state: &IntelAtomicState,
    crtc: &Crtc,
) {
    let new_crtc_state = new_crtc_state(state, crtc);

    if !new_crtc_state.needs_modeset && !new_crtc_state.needs_fastset {
        return;
    }

    io.wm_state_verify(state, crtc);
    verify_connector_state(io, state, Some(crtc));
    verify_crtc_state(io, state, crtc);
    io.dpll_state_verify(state, crtc);
    io.mpllb_state_verify(state, crtc);
}

// upstream: intel_modeset_verify.c intel_modeset_verify_disabled()
pub fn intel_modeset_verify_disabled<I: ModesetVerifyIo>(io: &mut I, state: &IntelAtomicState) {
    verify_encoder_state(io, state);
    verify_connector_state(io, state, None);
    io.dpll_verify_disabled(state);
}
