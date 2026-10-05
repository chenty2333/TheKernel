// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_color.c: skl_get_config,
// icl_read_csc, ilk_read_pipe_csc, icl_read_output_csc, icl_read_luts,
// glk_read_degamma_lut, ilk_read_lut_8, bdw_read_lut_10,
// icl_read_lut_multi_segment and their LUT packing helpers (display13 only).
// Copyright © 2016 Intel Corporation.
// intel_color_regs.h: selected fields, Copyright © 2023 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No color programming, only discovery.
use crate::{
    Error,
    display::{Pipe, ReadoutIo},
};

/// Serializes palette selectors and color commits for the entire capture;
/// caller holds an already-enabled pipe power reference. A failed restore
/// quarantines discovery. No concurrent DSB/IRQ color programming is allowed.
pub trait ColorIo: ReadoutIo {
    fn with_color_lock<T>(&self, operation: impl FnOnce() -> Result<T, Error>) -> Result<T, Error>;
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LutEntry {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
    pub raw: [u32; 2],
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CscMatrix {
    pub preoff: [u16; 3],
    pub coeff: [u16; 9],
    pub postoff: [u16; 3],
    /// Preoffsets, six coefficient dwords, then postoffsets in source read order.
    pub raw: [u32; 12],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostLut {
    Disabled,
    Legacy8,
    Precision10,
    MultiSegmentSuperFineOnly,
}
impl PostLut {
    pub const fn entries(self) -> usize {
        match self {
            Self::Disabled => 0,
            Self::Legacy8 => 256,
            Self::Precision10 => 1024,
            Self::MultiSegmentSuperFineOnly => 9,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorConfig {
    pub gamma_mode: u32,
    pub csc_mode: u32,
    pub bottom_color: u32,
    pub pipe_csc: Option<CscMatrix>,
    pub output_csc: Option<CscMatrix>,
    pub degamma_entries: usize,
    pub post_lut: PostLut,
}
impl ColorConfig {
    /// Disabled tables do not affect equality; enabled multi-segment readout
    /// is incomplete in upstream too and must never be treated as a full LUT.
    pub fn bypassed(self) -> bool {
        self.gamma_mode & ((1 << 31) | (1 << 30)) == 0
            && self.csc_mode & ((1 << 31) | (1 << 30)) == 0
            && self.bottom_color & ((1 << 31) | (1 << 30)) == 0
            && self.degamma_entries == 0
            && self.post_lut == PostLut::Disabled
    }
}
fn pack(value: u32, bits: u32) -> u16 {
    let max = (1 << bits) - 1;
    ((value * 65535 + max / 2) / max) as u16
}
pub fn i9xx_lut_8_pack(raw: u32) -> LutEntry {
    LutEntry {
        red: pack((raw >> 16) & 255, 8),
        green: pack((raw >> 8) & 255, 8),
        blue: pack(raw & 255, 8),
        raw: [raw, 0],
    }
}
pub fn ilk_lut_10_pack(raw: u32) -> LutEntry {
    LutEntry {
        red: pack((raw >> 20) & 1023, 10),
        green: pack((raw >> 10) & 1023, 10),
        blue: pack(raw & 1023, 10),
        raw: [raw, 0],
    }
}
pub fn ilk_lut_12p4_pack(low: u32, high: u32) -> LutEntry {
    LutEntry {
        red: ((((high >> 20) & 1023) << 6) | ((low >> 24) & 63)) as u16,
        green: ((((high >> 10) & 1023) << 6) | ((low >> 14) & 63)) as u16,
        blue: (((high & 1023) << 6) | ((low >> 4) & 63)) as u16,
        raw: [low, high],
    }
}
pub fn glk_degamma_lut_pack(raw: u32) -> LutEntry {
    let v = raw.min(65535) as u16;
    LutEntry {
        red: v,
        green: v,
        blue: v,
        raw: [raw, 0],
    }
}
fn read_csc(io: &impl ReadoutIo, pipe: Pipe, output: bool) -> Result<CscMatrix, Error> {
    let shift = pipe.index() * 0x100;
    let (pre, coeff, post) = if output {
        (0x49068, 0x49050, 0x49074)
    } else {
        (0x49030, 0x49010, 0x49040)
    };
    let mut csc = CscMatrix::default();
    for (n, v) in csc.preoff.iter_mut().enumerate() {
        let raw = io.read32(pre + shift + n as u32 * 4)?;
        csc.raw[n] = raw;
        *v = raw as u16;
    }
    for row in 0..3 {
        let pair = io.read32(coeff + shift + row * 8)?;
        let third = io.read32(coeff + shift + row * 8 + 4)?;
        csc.raw[3 + row as usize * 2] = pair;
        csc.raw[4 + row as usize * 2] = third;
        let n = row as usize * 3;
        csc.coeff[n] = (pair >> 16) as u16;
        csc.coeff[n + 1] = pair as u16;
        csc.coeff[n + 2] = (third >> 16) as u16;
    }
    for (n, v) in csc.postoff.iter_mut().enumerate() {
        let raw = io.read32(post + shift + n as u32 * 4)?;
        csc.raw[9 + n] = raw;
        *v = raw as u16;
    }
    Ok(csc)
}
fn indexed<I: ColorIo, T>(
    io: &I,
    index: u32,
    auto: u32,
    operation: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    // Firmware safety divergence: preserve the complete selector, not just its
    // index field. Even a failed selector store may have reached hardware.
    let before = io.read32(index)?;
    let result = (|| {
        io.write32(index, 0)?;
        io.write32(index, auto)?;
        let v = operation()?;
        io.write32(index, 0)?;
        Ok(v)
    })();
    if io.write32(index, before).is_err() || io.read32(index).ok() != Some(before) {
        return Err(Error::RestoreFailed(index));
    }
    result
}

/// Buffers are caller-owned (typically heap allocated), avoiding a large kernel
/// stack frame. Only the returned valid counts may be used after success;
/// partially filled buffers on ANY error are untrusted and must not be published.
/// `c8_planes` is the caller's verified indexed-plane mask, not inferred from
/// i915's initial-plane format fallback (upstream has a c8 readout FIXME).
pub fn intel_color_get_config(
    io: &impl ColorIo,
    pipe: Pipe,
    c8_planes: bool,
    degamma: &mut [LutEntry],
    gamma: &mut [LutEntry],
) -> Result<ColorConfig, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    io.with_color_lock(|| {
        let shift = pipe.index() * 0x800;
        let gamma_mode = io.read32(0x4a480 + shift)?;
        let csc_mode = io.read32(0x49028 + pipe.index() * 0x100)?;
        let bottom_color = io.read32(0x70034 + pipe.index() * 0x1000)?;
        let degamma_entries = if gamma_mode & (1 << 31) != 0 { 129 } else { 0 };
        let post_lut = if gamma_mode & (1 << 30) == 0 && !c8_planes {
            PostLut::Disabled
        } else {
            match gamma_mode & 3 {
                0 => PostLut::Legacy8,
                1 => PostLut::Precision10,
                3 => PostLut::MultiSegmentSuperFineOnly,
                _ => return Err(Error::Refused),
            }
        };
        if degamma.len() < degamma_entries || gamma.len() < post_lut.entries() {
            return Err(Error::Truncated);
        }
        if degamma_entries != 0 {
            indexed(io, 0x4a484 + shift, 1 << 10, || {
                for v in degamma.iter_mut().take(degamma_entries) {
                    *v = glk_degamma_lut_pack(io.read32(0x4a488 + shift)?);
                }
                Ok(())
            })?;
        }
        match post_lut {
            PostLut::Disabled => {}
            PostLut::Legacy8 => {
                for (n, v) in gamma.iter_mut().take(256).enumerate() {
                    *v = i9xx_lut_8_pack(io.read32(0x4a000 + shift + n as u32 * 4)?);
                }
            }
            PostLut::Precision10 => {
                indexed(io, 0x4a400 + shift, 1 << 15, || {
                    for v in gamma.iter_mut().take(1024) {
                        *v = ilk_lut_10_pack(io.read32(0x4a404 + shift)?);
                    }
                    Ok(())
                })?;
            }
            PostLut::MultiSegmentSuperFineOnly => {
                indexed(io, 0x4a408 + shift, 1 << 15, || {
                    // Upstream FIXME: fine/coarse PAL_PREC_DATA reads are wrong;
                    // only these nine entries are trustworthy. Never zero-fill the
                    // remaining 1015 entries and call the resulting table complete.
                    for v in gamma.iter_mut().take(9) {
                        let low = io.read32(0x4a40c + shift)?;
                        let high = io.read32(0x4a40c + shift)?;
                        *v = ilk_lut_12p4_pack(low, high);
                    }
                    Ok(())
                })?;
            }
        }
        // Wa_1406463849 (CSC reads disarm an update) is ICL-only. TGL/ADL-P
        // reads do not disarm CSC. No removed ICL workaround is applied here.
        let pipe_csc = if csc_mode & (1 << 31) != 0 {
            Some(read_csc(io, pipe, false)?)
        } else {
            None
        };
        let output_csc = if csc_mode & (1 << 30) != 0 {
            Some(read_csc(io, pipe, true)?)
        } else {
            None
        };
        Ok(ColorConfig {
            gamma_mode,
            csc_mode,
            bottom_color,
            pipe_csc,
            output_csc,
            degamma_entries,
            post_lut,
        })
    })
}
