// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/skl_watermark.c:
// skl_wm_level_from_reg_val, skl_pipe_wm_get_hw_state, skl_ddb_entry_init,
// skl_ddb_entry_init_from_hw, skl_ddb_get_hw_plane_state,
// skl_pipe_ddb_get_hw_state, intel_enabled_dbuf_slices_mask (display13 only).
// Copyright © 2022 Intel Corporation.
// skl_{universal_plane,watermark}_regs.h / intel_cursor_regs.h fields:
// Copyright © 2024 / 2023 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// No WM computation, allocation, SAGV/PCODE or hardware programming implied.
use crate::{
    Error, RegisterIo,
    display::{Pipe, ReadoutIo},
    universal_plane::Plane,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatermarkPlane {
    Universal(Plane),
    Cursor,
}
impl WatermarkPlane {
    fn reg(self, pipe: Pipe, primary: u32, cursor: u32) -> u32 {
        match self {
            Self::Universal(p) => p.register(pipe, primary),
            Self::Cursor => cursor + pipe.index() * 0x1000,
        }
    }
}
const PLANES: [WatermarkPlane; 6] = [
    WatermarkPlane::Universal(Plane::PRIMARY),
    WatermarkPlane::Universal(Plane::SECOND),
    WatermarkPlane::Universal(Plane::THIRD),
    WatermarkPlane::Universal(Plane::FOURTH),
    WatermarkPlane::Universal(Plane::FIFTH),
    WatermarkPlane::Cursor,
];
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WatermarkLevel {
    pub raw: u32,
    pub enable: bool,
    pub ignore_lines: bool,
    pub blocks: u32,
    pub lines: u32,
}
pub fn skl_wm_level_from_reg_val(raw: u32) -> WatermarkLevel {
    WatermarkLevel {
        raw,
        enable: raw & (1 << 31) != 0,
        ignore_lines: raw & (1 << 30) != 0,
        blocks: raw & 0x1fff,
        lines: (raw >> 14) & 0x1fff,
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaneWatermarks {
    /// Display13 integrated parts have six levels plus dedicated SAGV WMs;
    /// offsets for SAGV must not be misinterpreted as latency levels6/7.
    pub levels: [WatermarkLevel; 6],
    pub transition: WatermarkLevel,
    pub sagv: WatermarkLevel,
    pub sagv_transition: WatermarkLevel,
}
pub fn skl_pipe_wm_get_hw_state(
    io: &impl ReadoutIo,
    pipe: Pipe,
) -> Result<[PlaneWatermarks; 6], Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let mut out = [PlaneWatermarks::default(); 6];
    for (id, wm) in PLANES.into_iter().zip(out.iter_mut()) {
        for (level, v) in wm.levels.iter_mut().enumerate() {
            let r = id.reg(pipe, 0x70240, 0x70140) + level as u32 * 4;
            *v = skl_wm_level_from_reg_val(io.read32(r)?);
        }
        wm.transition = skl_wm_level_from_reg_val(io.read32(id.reg(pipe, 0x70268, 0x70168))?);
        wm.sagv = skl_wm_level_from_reg_val(io.read32(id.reg(pipe, 0x70258, 0x70158))?);
        wm.sagv_transition = skl_wm_level_from_reg_val(io.read32(id.reg(pipe, 0x7025c, 0x7015c))?);
    }
    Ok(out)
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DdbEntry {
    pub raw: u32,
    pub start: u16,
    pub end: u16,
}
pub fn skl_ddb_entry_init_from_hw(raw: u32) -> DdbEntry {
    let start = (raw & 0x1fff) as u16;
    let end = ((raw >> 16) & 0x1fff) as u16;
    // Zero end denotes no allocation; do not turn disabled DDB into block0.
    DdbEntry {
        raw,
        start,
        end: if end == 0 { 0 } else { end + 1 },
    }
}
impl DdbEntry {
    /// Additional safety check, not an alternative to source decoding. Caller
    /// resolves MBUS-relative coordinates and the enabled DBUF slices first.
    pub fn validate(self, ceiling: u16) -> Result<(), Error> {
        if self.start == 0 && self.end == 0 || self.start < self.end && self.end <= ceiling {
            Ok(())
        } else {
            Err(Error::Refused)
        }
    }
    pub fn blocks(self) -> u16 {
        self.end.saturating_sub(self.start)
    }
}
pub fn skl_pipe_ddb_get_hw_state(io: &impl ReadoutIo, pipe: Pipe) -> Result<[DdbEntry; 6], Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let mut out = [DdbEntry::default(); 6];
    for (id, v) in PLANES.into_iter().zip(out.iter_mut()) {
        // No NV12_BUF_CFG on display11+, no MIN_BUF_CFG until display30.
        *v = skl_ddb_entry_init_from_hw(io.read32(id.reg(pipe, 0x7027c, 0x7017c))?);
    }
    Ok(out)
}

/// Whether the one display13 HDMI timing reduction has no greater linear
/// XRGB WM demand under Linux 7.2.3's `skl_compute_plane_wm()` semantics.
///
/// This intentionally recognizes only the captured 4K30 firmware profile and
/// its 1080p60 target (plus exact identity). It is not a general clock/pitch
/// bandwidth theorem. The caller must separately prove the framebuffer is
/// linear XRGB, unscaled, and that it preserves the captured WM/DDB words.
/// `tests/upstream_watermark.rs` compiles the corresponding upstream C path and
/// checks its level-0 block, line, and DDB requirements against this profile.
pub fn adlp_linear_xrgb_4k30_watermark_profile_no_worse(
    baseline_pixel_rate_khz: u32,
    baseline_width: u32,
    baseline_htotal: u32,
    target_pixel_rate_khz: u32,
    target_width: u32,
    target_htotal: u32,
) -> bool {
    const BASELINE: (u32, u32, u32) = (297_000, 3_840, 4_400);
    const TARGET: (u32, u32, u32) = (148_500, 1_920, 2_200);

    if (baseline_pixel_rate_khz, baseline_width, baseline_htotal) != BASELINE {
        return false;
    }
    if (target_pixel_rate_khz, target_width, target_htotal) == BASELINE {
        return true;
    }
    if (target_pixel_rate_khz, target_width, target_htotal) != TARGET {
        return false;
    }

    // The exact source profile facts that make the upstream method choice and
    // method-2 line term comparable. Method-1's remaining fixed-point cases
    // (latencies 1..14) are exhaustively checked against upstream C.
    let ceil_div = |n: u32, d: u32| n.checked_add(d - 1).map(|n| n / d);
    let baseline_linetime = match ceil_div(
        baseline_htotal.checked_mul(1_000).unwrap_or(0),
        baseline_pixel_rate_khz,
    ) {
        Some(value) if value != 0 => value,
        _ => return false,
    };
    let target_linetime = match ceil_div(
        target_htotal.checked_mul(1_000).unwrap_or(0),
        target_pixel_rate_khz,
    ) {
        Some(value) if value != 0 => value,
        _ => return false,
    };
    let baseline_bytes_per_line = match baseline_width.checked_mul(4) {
        Some(value) => value,
        None => return false,
    };
    let target_bytes_per_line = match target_width.checked_mul(4) {
        Some(value) => value,
        None => return false,
    };
    let baseline_blocks_per_line =
        match ceil_div(baseline_bytes_per_line, 512).and_then(|v| v.checked_add(1)) {
            Some(value) => value,
            None => return false,
        };
    let target_blocks_per_line =
        match ceil_div(target_bytes_per_line, 512).and_then(|v| v.checked_add(1)) {
            Some(value) => value,
            None => return false,
        };

    baseline_linetime == 15
        && target_linetime == baseline_linetime
        && baseline_blocks_per_line == 31
        && target_blocks_per_line == 16
        && target_pixel_rate_khz.checked_mul(2) == Some(baseline_pixel_rate_khz)
        && target_pixel_rate_khz.checked_mul(baseline_htotal)
            == baseline_pixel_rate_khz.checked_mul(target_htotal)
}

#[cfg(test)]
mod retained_budget_tests {
    use super::adlp_linear_xrgb_4k30_watermark_profile_no_worse as profile;

    #[test]
    fn only_the_source_checked_profile_and_identity_are_admitted() {
        assert!(profile(297_000, 3_840, 4_400, 297_000, 3_840, 4_400));
        assert!(profile(297_000, 3_840, 4_400, 148_500, 1_920, 2_200));
        assert!(!profile(148_500, 1_920, 2_200, 297_000, 3_840, 4_400));
        assert!(!profile(297_000, 3_840, 4_401, 148_500, 1_920, 2_200));
        assert!(!profile(297_000, 3_840, 4_400, 148_500, 1_921, 2_200));
        assert!(!profile(297_000, 3_840, 4_400, 148_500, 1_920, 2_201));
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DbufState {
    pub ctl: [u32; 4],
    pub enabled_slices: u8,
    pub mbus_ctl: u32,
}
/// Caller must pin the already-enabled display-core power domain and serialize
/// global DBUF/CDCLK/MBUS writes throughout readout. This helper does not wake
/// power, change MBUS, or claim to reconstruct the full global bandwidth state.
pub fn read_dbuf_state(io: &impl RegisterIo) -> Result<DbufState, Error> {
    let mut ctl = [0; 4];
    let mut enabled_slices = 0;
    for (id, (r, v)) in [0x45008, 0x44fe8, 0x44300, 0x44304]
        .into_iter()
        .zip(ctl.iter_mut())
        .enumerate()
    {
        *v = io.read32(r)?;
        if *v & (1 << 30) != 0 {
            enabled_slices |= 1 << id;
        }
    }
    let mbus_ctl = io.read32(0x4438c)?;
    Ok(DbufState {
        ctl,
        enabled_slices,
        mbus_ctl,
    })
}
