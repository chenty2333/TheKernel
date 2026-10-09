// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/skl_universal_plane.c:
// skl_format_to_fourcc, skl_get_initial_plane_config, skl_plane_stride_mult.
// Copyright © 2020 Intel Corporation.
// intel_fb.c: intel_tile_{size,width_bytes,height}, intel_fb_align_height
// (main-plane ADL-P branches), Copyright © 2021 Intel Corporation.
// skl_universal_plane_regs.h: selected fields, Copyright © 2024 Intel Corporation.
// include/uapi/drm/drm_fourcc.h: selected encodings, Copyright 2011 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No allocation or hardware mutation.
use crate::{
    Error,
    display::{Pipe, ReadoutIo},
};

pub const XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const RGB565: u32 = u32::from_le_bytes(*b"RG16");

/// ADL-P exposes five universal planes per pipe. Numbering is zero-based internally
/// like i915 PLANE_1; never confuse it with KMS object IDs or CURSOR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Plane(u8);
impl Plane {
    pub const PRIMARY: Self = Self(0);
    pub const SECOND: Self = Self(1);
    pub const THIRD: Self = Self(2);
    pub const FOURTH: Self = Self(3);
    pub const FIFTH: Self = Self(4);
    pub fn new(index: u8) -> Result<Self, Error> {
        if index >= 5 {
            return Err(Error::Refused);
        }
        Ok(Self(index))
    }
    pub const fn register(self, pipe: Pipe, primary_a: u32) -> u32 {
        primary_a + pipe.index() * 0x1000 + self.0 as u32 * 0x100
    }
}

/// This function preserves i915's fallback to XRGB8888 for unknown/indexed
/// format fields. Such a fallback is NOT evidence permitting native scanout;
/// admission below checks the original format field as well.
pub fn skl_format_to_fourcc(format: u32, rgb_order: bool, alpha: bool) -> u32 {
    let raw = match format {
        0x0e000000 => *b"RG16",
        0x01000000 => *b"NV12",
        0x08000000 => *b"XYUV",
        0x03000000 => *b"P010",
        0x05000000 => *b"P012",
        0x07000000 => *b"P016",
        0x00800000 => *b"Y210",
        0x01800000 => *b"Y212",
        0x02800000 => *b"Y216",
        0x03800000 => *b"XV30",
        0x04800000 => *b"XV36",
        0x05800000 => *b"XV48",
        0x02000000 => match (rgb_order, alpha) {
            (false, false) => *b"XR30",
            (false, true) => *b"AR30",
            (true, false) => *b"XB30",
            (true, true) => *b"AB30",
        },
        0x06000000 => match (rgb_order, alpha) {
            (false, false) => *b"XR4H",
            (false, true) => *b"AR4H",
            (true, false) => *b"XB4H",
            (true, true) => *b"AB4H",
        },
        _ => match (rgb_order, alpha) {
            (false, false) => *b"XR24",
            (false, true) => *b"AR24",
            (true, false) => *b"XB24",
            (true, true) => *b"AB24",
        },
    };
    u32::from_le_bytes(raw)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Modifier {
    Linear,
    X,
    Y,
    Yf,
    YGen12RcCcs,
    YGen12McCcs,
    YfCcs,
}
impl Modifier {
    pub const fn drm(self) -> u64 {
        match self {
            Self::Linear => 0,
            Self::X => (1 << 56) | 1,
            Self::Y => (1 << 56) | 2,
            Self::Yf => (1 << 56) | 3,
            Self::YfCcs => (1 << 56) | 5,
            Self::YGen12RcCcs => (1 << 56) | 6,
            Self::YGen12McCcs => (1 << 56) | 7,
        }
    }
    pub const fn has_auxiliary(self) -> bool {
        matches!(self, Self::YGen12RcCcs | Self::YGen12McCcs | Self::YfCcs)
    }
}
pub fn initial_modifier(ctl: u32) -> Result<Modifier, Error> {
    // ADL-P has no HAS_4TILE (that is DG2/display14+). In particular tiling
    // field 5 is Yf, not 4-tile, even though this display is XE_LPD.
    match (ctl >> 10) & 7 {
        0 => Ok(Modifier::Linear),
        1 => Ok(Modifier::X),
        4 => Ok(if ctl & (1 << 15) != 0 {
            Modifier::YGen12RcCcs
        } else if ctl & (1 << 4) != 0 {
            Modifier::YGen12McCcs
        } else {
            Modifier::Y
        }),
        5 => Ok(if ctl & (1 << 15) != 0 {
            Modifier::YfCcs
        } else {
            Modifier::Yf
        }),
        _ => Err(Error::Refused),
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialPlaneConfig {
    pub ctl: u32,
    pub color_ctl: u32,
    pub surface_raw: u32,
    pub offset: u32,
    pub size_raw: u32,
    pub stride_raw: u32,
    pub fourcc: u32,
    pub modifier: Modifier,
    pub cpp: u32,
    pub format_planes: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    /// Main plane only. Compression and multi-plane formats require additional
    /// addresses/ownership proof before use; this must not stand in for them.
    pub main_size: u64,
    pub rotation_degrees_ccw: u32,
    pub reflect_x: bool,
}
impl InitialPlaneConfig {
    pub const fn surface(self) -> u32 {
        self.surface_raw & 0xfffff000
    }
    /// A necessary plane-local condition, not full fastboot equivalence. DPT
    /// (all ADL-P tiled scanout), aux planes, color, scalers, WM, GGTT ownership,
    /// link and latch stability must be proven by the outer state transaction.
    pub fn native_linear_xrgb(self) -> bool {
        self.ctl & (0x1f << 23) == 4 << 24
            && self.fourcc == XRGB8888
            && self.modifier == Modifier::Linear
            && self.rotation_degrees_ccw == 0
            && !self.reflect_x
            && self.offset == 0
            && self.surface_raw & 0xfff == 0
            && self.ctl & ((3 << 21) | (1 << 19) | (1 << 15) | (1 << 9) | (1 << 4)) == 0
            && self.color_ctl
                & ((1 << 21) | (1 << 20) | (7 << 17) | (1 << 15) | (1 << 14) | (3 << 4))
                == 0
            && self.color_ctl & (1 << 13) != 0
            && self.pitch >= self.width.saturating_mul(4)
    }

    /// A narrow plane-local proof for the supported linear RGB565 primary
    /// surface. All outer GGTT, link, vblank, watermark and DDB conditions are
    /// still checked by the Native transaction.
    pub fn native_linear_rgb565(self) -> bool {
        self.ctl & (0x1f << 23) == 14 << 24
            && self.fourcc == RGB565
            && self.modifier == Modifier::Linear
            && self.rotation_degrees_ccw == 0
            && !self.reflect_x
            && self.offset == 0
            && self.surface_raw & 0xfff == 0
            && self.ctl & ((3 << 21) | (1 << 19) | (1 << 15) | (1 << 9) | (1 << 4)) == 0
            && self.color_ctl
                & ((1 << 21) | (1 << 20) | (7 << 17) | (1 << 15) | (1 << 14) | (3 << 4))
                == 0
            && self.color_ctl & (1 << 13) != 0
            && self.cpp == 2
            && self.format_planes == 1
            && self.pitch >= self.width.saturating_mul(2)
    }

    pub fn native_linear_rgb(self) -> bool {
        self.native_linear_xrgb() || self.native_linear_rgb565()
    }
}

pub fn skl_get_initial_plane_config(
    io: &impl ReadoutIo,
    pipe: Pipe,
    plane: Plane,
) -> Result<Option<InitialPlaneConfig>, Error> {
    if !io.pipe_powered(pipe) {
        return Ok(None);
    }
    let read = |r| io.read32(plane.register(pipe, r));
    if read(0x70180)? & (1 << 31) == 0 {
        return Ok(None);
    }
    // Match the separate plane->get_hw_state and get_initial_plane_config reads.
    let ctl = read(0x70180)?;
    if ctl & (1 << 31) == 0 {
        return Err(Error::Refused);
    }
    let color_ctl = read(0x701cc)?;
    let pixel_format = ctl & (0x1f << 23);
    let fourcc = skl_format_to_fourcc(
        pixel_format,
        ctl & (1 << 20) != 0,
        (color_ctl >> 4) & 3 != 0,
    );
    let modifier = initial_modifier(ctl)?;
    let rotation_degrees_ccw = match ctl & 3 {
        0 => 0,
        1 => 270,
        2 => 180,
        _ => 90,
    };
    if rotation_degrees_ccw == 90 || rotation_degrees_ccw == 270 {
        return Err(Error::Refused);
    }
    let reflect_x = ctl & (1 << 8) != 0;
    let surface_raw = read(0x7019c)?;
    let offset = read(0x701a4)?;
    let size_raw = read(0x70190)?;
    let width = (size_raw & 0xffff) + 1;
    let height = (size_raw >> 16) + 1;
    let stride_raw = read(0x70188)?;
    let (cpp, format_planes) = match pixel_format {
        0x0e000000 => (2, 1),
        0x01000000 => (1, 2),
        0x03000000 | 0x05000000 | 0x07000000 => (2, 2),
        0x06000000 | 0x04800000 | 0x05800000 => (8, 1),
        _ => (4, 1),
    };
    let tile_width = match modifier {
        Modifier::Linear => 4096,
        Modifier::X => 512,
        Modifier::Y | Modifier::YGen12RcCcs | Modifier::YGen12McCcs => 128,
        Modifier::Yf | Modifier::YfCcs => match cpp {
            1 => 64,
            2 | 4 => 128,
            _ => 256,
        },
    };
    let stride_mult = if modifier == Modifier::Linear {
        64
    } else {
        tile_width
    };
    let pitch = (stride_raw & 0xfff) * stride_mult;
    let tile_height = 4096 / tile_width;
    let aligned_height = height.div_ceil(tile_height) * tile_height;
    Ok(Some(InitialPlaneConfig {
        ctl,
        color_ctl,
        surface_raw,
        offset,
        size_raw,
        stride_raw,
        fourcc,
        modifier,
        cpp,
        format_planes,
        width,
        height,
        pitch,
        main_size: u64::from(pitch) * u64::from(aligned_height),
        rotation_degrees_ccw,
        reflect_x,
    }))
}

#[cfg(test)]
mod tests {
    use super::{InitialPlaneConfig, Modifier, RGB565};

    #[test]
    fn native_linear_rgb565_readout_matches_source_plane_format() {
        let plane = InitialPlaneConfig {
            ctl: (1 << 31) | (14 << 24),
            color_ctl: 1 << 13,
            surface_raw: 0x1000,
            offset: 0,
            size_raw: 0,
            stride_raw: 60,
            fourcc: RGB565,
            modifier: Modifier::Linear,
            cpp: 2,
            format_planes: 1,
            width: 1920,
            height: 1080,
            pitch: 3840,
            main_size: 3840 * 1080,
            rotation_degrees_ccw: 0,
            reflect_x: false,
        };
        assert!(plane.native_linear_rgb565());
        assert!(plane.native_linear_rgb());
        assert!(!plane.native_linear_xrgb());
    }
}
