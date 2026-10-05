// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_display.c:
// bdw_get_pipe_misc_output_format and selected hsw_get_pipe_config fields.
// Copyright © 2006-2007 Intel Corporation.
// intel_vrr.c: intel_vrr_get_config (display13 branches).
// Copyright © 2020 Intel Corporation.
// intel_{display,vrr,vdsc}_regs.h selected fields:
// Copyright © 2025 / 2024 / 2023 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. Non-DSI ADL-P/N only.
// Active DSC/joining is discovered, not decoded into a fabricated complete PPS.
use crate::{
    Error,
    display::{Pipe, ReadoutIo, Timings, intel_get_pipe_src_size, intel_get_transcoder_timings},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Rgb,
    Ycbcr444,
    Ycbcr420,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipeMisc {
    pub raw: u32,
    pub output: OutputFormat,
    pub bpc: Option<u32>,
}
impl PipeMisc {
    pub fn full_blend(self) -> bool {
        self.output != OutputFormat::Ycbcr420 || self.raw & (1 << 26) != 0
    }
}
pub fn decode_pipe_misc(raw: u32) -> PipeMisc {
    PipeMisc {
        raw,
        output: if raw & (1 << 27) != 0 {
            OutputFormat::Ycbcr420
        } else if raw & (1 << 11) != 0 {
            OutputFormat::Ycbcr444
        } else {
            OutputFormat::Rgb
        },
        bpc: match (raw >> 5) & 7 {
            0 => Some(8),
            1 => Some(10),
            2 => Some(6),
            4 => Some(12),
            _ => None,
        },
    }
}
pub fn bdw_get_pipe_misc_output_format(io: &impl ReadoutIo, pipe: Pipe) -> Result<PipeMisc, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    Ok(decode_pipe_misc(
        io.read32(0x70030 + pipe.index() * 0x1000)?,
    ))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VrrConfig {
    pub raw_control: u32,
    pub enabled: bool,
    pub guardband: u32,
    /// Absent unless FLIP_LINE_EN. Hardware values are stored minus one.
    pub flipline: Option<u32>,
    pub vmax: Option<u32>,
    pub vmin: Option<u32>,
    pub vsync: Option<(u32, u32)>,
}
pub fn intel_vrr_get_config(io: &impl ReadoutIo, pipe: Pipe) -> Result<VrrConfig, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let ctl = io.read32(pipe.transcoder_register(0x60420))?;
    let mut out = VrrConfig {
        raw_control: ctl,
        enabled: ctl & (1 << 31) != 0,
        guardband: ctl & 0xffff,
        flipline: None,
        vmax: None,
        vmin: None,
        vsync: None,
    };
    if ctl & (1 << 29) != 0 {
        let read = |r| {
            io.read32(pipe.transcoder_register(r))?
                .checked_add(1)
                .ok_or(Error::Refused)
        };
        out.flipline = Some(read(0x60438)?);
        out.vmax = Some(read(0x60424)?);
        out.vmin = Some(read(0x60434)?);
        let vsync = io.read32(pipe.transcoder_register(0x60078))?;
        out.vsync = Some((vsync & 0x1fff, (vsync >> 16) & 0x1fff));
    }
    // ADL-P has no CMRR, VRR DC balance or always-VRR timing generator.
    // No pre-display13 set-context-latency adjustment is applied here.
    Ok(out)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DssConfig {
    pub control1: u32,
    pub control2: u32,
}
impl DssConfig {
    pub fn compression_enabled(self) -> bool {
        self.control2 & (1 << 31) != 0
    }
    pub fn joining_enabled(self) -> bool {
        self.control1 & ((1 << 30) | (1 << 29) | (1 << 28)) != 0
    }
    pub fn uncompressed(self) -> bool {
        !self.compression_enabled() && !self.joining_enabled() && self.control2 & (1 << 15) == 0
    }
}
/// Display13 DSC/joining shares the pipe power domain. This is the two-control
/// prefix of intel_dsc_get_config plus raw bigjoiner evidence, NOT PPS readout.
pub fn read_dss_controls(io: &impl ReadoutIo, pipe: Pipe) -> Result<DssConfig, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let base = 0x78000 + pipe.index() * 0x200;
    Ok(DssConfig {
        control1: io.read32(base)?,
        control2: io.read32(base + 4)?,
    })
}
pub const fn chicken_trans_register(pipe: Pipe) -> u32 {
    match pipe {
        Pipe::A => 0x420c0,
        Pipe::B => 0x420c4,
        Pipe::C => 0x420c8,
        Pipe::D => 0x420d8,
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipeConfig {
    pub pipe: Pipe,
    pub transconf: u32,
    pub dss: DssConfig,
    pub chicken: u32,
    pub frame_start_delay: u32,
    pub timings: Timings,
    pub vrr: VrrConfig,
    pub source: (u32, u32),
    pub misc: PipeMisc,
    pub linetime_raw: u32,
    pub linetime: u32,
    pub multiplier_raw: u32,
    pub pixel_multiplier: u32,
}
/// An already pinned pipe and matching non-panel transcoder are required.
/// The outer topology capture must separately reject panel/DSI transcoders,
/// joined pipes, unsupported PPS, color/scaler state and asynchronous changes.
/// No writes, no enabling a dark pipe; this is not whole firmware admission.
pub fn read_pipe_config(io: &impl ReadoutIo, pipe: Pipe) -> Result<Option<PipeConfig>, Error> {
    if !io.pipe_powered(pipe) {
        return Ok(None);
    }
    let transconf = io.read32(pipe.transcoder_register(0x70008))?;
    if transconf & (1 << 31) == 0 {
        return Ok(None);
    }
    let dss = read_dss_controls(io, pipe)?;
    let chicken = io.read32(chicken_trans_register(pipe))?;
    let frame_start_delay = ((chicken >> 27) & 3) + 1;
    let interlaced = transconf & (3 << 21) == (3 << 21);
    let timings = intel_get_transcoder_timings(io, pipe, interlaced)?;
    let vrr = intel_vrr_get_config(io, pipe)?;
    let source = intel_get_pipe_src_size(io, pipe)?;
    let misc = bdw_get_pipe_misc_output_format(io, pipe)?;
    // Color and scaler readout belong between these source steps; the outer
    // snapshot captures their independent domains and performs stability checks.
    let linetime_raw = io.read32(0x45270 + pipe.index() * 4)?;
    let multiplier_raw = io.read32(pipe.transcoder_register(0x6002c))?;
    let pixel_multiplier = multiplier_raw.checked_add(1).ok_or(Error::Refused)?;
    Ok(Some(PipeConfig {
        pipe,
        transconf,
        dss,
        chicken,
        frame_start_delay,
        timings,
        vrr,
        source,
        misc,
        linetime_raw,
        linetime: linetime_raw & 0x1ff,
        multiplier_raw,
        pixel_multiplier,
    }))
}
