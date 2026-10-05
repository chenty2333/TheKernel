// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/skl_scaler.c:
// skl_pipe_scaler_get_hw_state, skl_scaler_get_config (display13, no CASF).
// Copyright © 2020 Intel Corporation.
// intel_display_regs.h: scaler fields, Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No scaler programming or trace import.
use crate::{
    Error,
    display::{Pipe, ReadoutIo},
};

/// Caller pins PANEL_FITTER(pipe) only if already enabled, independently of
/// PIPE(pipe); a dark scaler domain must never be read just to discover state.
pub trait ScalerIo: ReadoutIo {
    fn scalers_powered(&self, pipe: Pipe) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scaler {
    pub ctl: u32,
    pub position: Option<u32>,
    pub size: Option<u32>,
}
impl Scaler {
    pub fn enabled(self) -> bool {
        self.ctl & (1 << 31) != 0
    }
    /// Zero binds the pipe, 1..5 bind PLANE_1..5. Reserved bindings cannot grant
    /// ownership and are retained, not silently collapsed to pipe scaling.
    pub fn binding(self) -> u32 {
        (self.ctl >> 25) & 7
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipeScalerConfig {
    pub id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
fn register(pipe: Pipe, id: u32, offset: u32) -> u32 {
    offset + pipe.index() * 0x800 + id * 0x100
}
pub fn skl_pipe_scaler_get_hw_state(io: &impl ScalerIo, pipe: Pipe) -> Result<Option<u32>, Error> {
    if !io.pipe_powered(pipe) || !io.scalers_powered(pipe) {
        return Ok(None);
    }
    for id in 0..2 {
        let ctl = io.read32(register(pipe, id, 0x68180))?;
        if ctl & ((1 << 31) | (7 << 25)) == 1 << 31 {
            return Ok(Some(id));
        }
    }
    Ok(None)
}
pub fn skl_scaler_get_config(
    io: &impl ScalerIo,
    pipe: Pipe,
) -> Result<Option<PipeScalerConfig>, Error> {
    let Some(id) = skl_pipe_scaler_get_hw_state(io, pipe)? else {
        return Ok(None);
    };
    // scaler_has_casf is false on display13; no sharpness registers are read.
    let pos = io.read32(register(pipe, id, 0x68170))?;
    let size = io.read32(register(pipe, id, 0x68174))?;
    Ok(Some(PipeScalerConfig {
        id,
        x: pos >> 16,
        y: pos & 0xffff,
        width: size >> 16,
        height: size & 0xffff,
    }))
}
/// Additional TheKernel ownership evidence: inspect BOTH controls, including
/// plane-bound and reserved bindings which upstream's pipe getter skips. An
/// active scaler is not admitted for plane-only fastboot; complete geometry /
/// filter programming belongs to the later modeset path, not this helper.
/// `None` means the independently pinned scaler power domain is off.
pub fn read_all_scalers(io: &impl ScalerIo, pipe: Pipe) -> Result<Option<[Scaler; 2]>, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    if !io.scalers_powered(pipe) {
        return Ok(None);
    }
    let mut out = [Scaler {
        ctl: 0,
        position: None,
        size: None,
    }; 2];
    for (id, s) in out.iter_mut().enumerate() {
        s.ctl = io.read32(register(pipe, id as u32, 0x68180))?;
        if s.enabled() {
            s.position = Some(io.read32(register(pipe, id as u32, 0x68170))?);
            s.size = Some(io.read32(register(pipe, id as u32, 0x68174))?);
        }
    }
    Ok(Some(out))
}
