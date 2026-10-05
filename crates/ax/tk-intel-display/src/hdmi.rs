// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_hdmi.c:
// hsw_infoframes_enabled, hsw_infoframe_enable, hsw_dip_data_reg,
// hsw_read_infoframe, intel_read_infoframe, intel_hdmi_read_gcp_infoframe
// (display13 HDMI packet branches).
// Copyright 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2009 Intel Corporation.
// intel_display_regs.h selected fields: Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. No packet writes or sink configuration.
use crate::{
    Error,
    display::{Pipe, ReadoutIo},
    hdmi_packet::{Infoframe, hdmi_infoframe_unpack},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameType {
    Avi,
    Spd,
    Vendor,
    Drm,
}
impl FrameType {
    pub const fn packet_type(self) -> u8 {
        match self {
            Self::Avi => 0x82,
            Self::Spd => 0x83,
            Self::Vendor => 0x81,
            Self::Drm => 0x87,
        }
    }
    pub const fn enable_mask(self) -> u32 {
        match self {
            Self::Avi => 1 << 12,
            Self::Spd => 1,
            Self::Vendor => 1 << 8,
            Self::Drm => 1 << 28,
        }
    }
    pub const fn data_base(self) -> u32 {
        match self {
            Self::Avi => 0x60220,
            Self::Spd => 0x602a0,
            Self::Vendor => 0x60260,
            Self::Drm => 0x60440,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawInfoframe {
    pub raw: [u8; 32],
    pub kind: FrameType,
}
impl RawInfoframe {
    /// HSW DIP has an ECC/reserved hole at byte3. Remove that hole, NOT the
    /// checksum at byte4; preserve the source memmove+offset result exactly.
    pub fn packet(self) -> [u8; 31] {
        let mut packet = [0; 31];
        packet[..3].copy_from_slice(&self.raw[..3]);
        packet[3..].copy_from_slice(&self.raw[4..]);
        packet
    }
    pub fn unpack(self) -> Result<Infoframe, Error> {
        let packet = self.packet();
        if packet[0] != self.kind.packet_type() {
            return Err(Error::InvalidHeader);
        }
        hdmi_infoframe_unpack(&packet)
    }
}
pub fn hsw_infoframes_enabled(raw: u32) -> u32 {
    // HAS_AS_SDP is true on display13. PPS is not in this source helper's mask;
    // full raw control is retained separately so admission can still reject it.
    raw & ((1 << 20) | (1 << 16) | (1 << 12) | (1 << 8) | (1 << 4) | 1 | (1 << 28) | (1 << 23))
}
/// Convert source hardware enable bits to the i915 software DIP indices.
pub fn intel_hdmi_infoframes_enabled(raw: u32) -> u32 {
    let enabled = hsw_infoframes_enabled(raw);
    let mut out = 0;
    for (index, bit) in [16, 4, 20, 23, 12, 0, 8, 28].into_iter().enumerate() {
        if enabled & (1 << bit) != 0 {
            out |= 1 << index;
        }
    }
    out
}
pub fn hsw_read_infoframe(
    io: &impl ReadoutIo,
    pipe: Pipe,
    kind: FrameType,
) -> Result<RawInfoframe, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let mut raw = [0; 32];
    for (n, word) in raw.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        word.copy_from_slice(
            &io.read32(pipe.transcoder_register(kind.data_base() + n as u32 * 4))?
                .to_le_bytes(),
        );
    }
    Ok(RawInfoframe { raw, kind })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdmiReadout {
    pub control: u32,
    /// Filtered hardware bits; unlike the software DIP-index bitmap.
    pub enable: u32,
    pub enabled_packets: u32,
    pub gcp: Option<u32>,
    pub frames: [Option<RawInfoframe>; 4],
}
pub fn read_hdmi_state(io: &impl ReadoutIo, pipe: Pipe) -> Result<HdmiReadout, Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let control = io.read32(pipe.transcoder_register(0x60200))?;
    let enable = hsw_infoframes_enabled(control);
    let gcp = if enable & (1 << 16) != 0 {
        Some(io.read32(pipe.transcoder_register(0x60210))?)
    } else {
        None
    };
    // Same HDMI get_config order as intel_ddi_get_config: AVI/SPD/VS/DRM.
    let mut frames = [None; 4];
    for (kind, slot) in [
        FrameType::Avi,
        FrameType::Spd,
        FrameType::Vendor,
        FrameType::Drm,
    ]
    .into_iter()
    .zip(frames.iter_mut())
    {
        if enable & kind.enable_mask() != 0 {
            *slot = Some(hsw_read_infoframe(io, pipe, kind)?);
        }
    }
    Ok(HdmiReadout {
        control,
        enable,
        enabled_packets: intel_hdmi_infoframes_enabled(control),
        gcp,
        frames,
    })
}
