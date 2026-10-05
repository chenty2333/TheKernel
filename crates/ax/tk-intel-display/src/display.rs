// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_display.c:
// intel_get_transcoder_timings and intel_get_pipe_src_size, non-DSI display 13.
// Copyright © 2006-2007 Intel Corporation.
// intel_display_regs.h: timing/source/TRANSCONF fields (Copyright © 2025 Intel Corporation).
// MIT permission text: ../LICENSE-MIT.
// Other generations, DSI, joiner/scaler/color/PLL/WM readout omitted. This is
// a timing-readout slice, not full hsw_get_pipe_config or fastboot admission.
use crate::{Error, RegisterIo};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pipe {
    A,
    B,
    C,
    D,
}
impl Pipe {
    pub const fn index(self) -> u32 {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::D => 3,
        }
    }
    pub const fn transcoder_register(self, a_offset: u32) -> u32 {
        a_offset + self.index() * 0x1000
    }
}
/// The caller holds stable powered-domain ownership through the complete read.
/// Implementations may only report true when the domain is already enabled;
/// readout must never wake a disabled domain merely to discover its state.
pub trait ReadoutIo: RegisterIo {
    fn pipe_powered(&self, pipe: Pipe) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timings {
    pub hdisplay: u32,
    pub htotal: u32,
    pub hblank_start: u32,
    pub hblank_end: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub vdisplay: u32,
    pub vtotal: u32,
    pub vblank_start: u32,
    pub vblank_end: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub set_context_latency: u32,
    pub interlaced: bool,
}
impl Timings {
    /// Validation belongs to TheKernel admission, not the upstream decode.
    /// Blank/source/clock/scaling equivalence must still be checked separately.
    pub fn validate(&self) -> Result<(), Error> {
        if self.hdisplay <= self.hblank_start
            && self.hblank_start <= self.hsync_start
            && self.hsync_start < self.hsync_end
            && self.hsync_end <= self.hblank_end
            && self.hblank_end <= self.htotal
            && self.vdisplay <= self.vblank_start
            && self.vblank_start <= self.vsync_start
            && self.vsync_start < self.vsync_end
            && self.vsync_end <= self.vblank_end
            && self.vblank_end <= self.vtotal
        {
            Ok(())
        } else {
            Err(Error::Refused)
        }
    }
}
fn pair(value: u32) -> (u32, u32) {
    ((value & 0xffff) + 1, (value >> 16) + 1)
}

/// Reads precisely in intel_get_transcoder_timings order. The caller-derived
/// interlace state is applied before display-13 SCL overrides vblank_start.
pub fn intel_get_transcoder_timings(
    io: &impl ReadoutIo,
    pipe: Pipe,
    interlaced: bool,
) -> Result<Timings, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let read = |offset| io.read32(pipe.transcoder_register(offset));
    let (hdisplay, htotal) = pair(read(0x60000)?);
    let (hblank_start, hblank_end) = pair(read(0x60004)?);
    let (hsync_start, hsync_end) = pair(read(0x60008)?);
    let (vdisplay, mut vtotal) = pair(read(0x6000c)?);
    let (_, mut vblank_end) = pair(read(0x60010)?);
    let (vsync_start, vsync_end) = pair(read(0x60014)?);
    if interlaced {
        vtotal += 1;
        vblank_end += 1;
    }
    let set_context_latency = read(0x6007c)?;
    let vblank_start = vdisplay
        .checked_add(set_context_latency)
        .ok_or(Error::Refused)?;
    Ok(Timings {
        hdisplay,
        htotal,
        hblank_start,
        hblank_end,
        hsync_start,
        hsync_end,
        vdisplay,
        vtotal,
        vblank_start,
        vblank_end,
        vsync_start,
        vsync_end,
        set_context_latency,
        interlaced,
    })
}
/// PIPESRC has width in the HIGH half (unlike TRANS_HTOTAL active). Scaler /
/// joiner adjustment is intentionally not implemented or implicitly accepted.
pub fn intel_get_pipe_src_size(io: &impl ReadoutIo, pipe: Pipe) -> Result<(u32, u32), Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let raw = io.read32(0x6001c + pipe.index() * 0x1000)?;
    Ok(((raw >> 16) + 1, (raw & 0xffff) + 1))
}
