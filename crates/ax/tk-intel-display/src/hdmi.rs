// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_hdmi.c:
// hsw_infoframes_enabled, hsw_infoframe_enable, hsw_dip_data_reg,
// hsw_read_infoframe, intel_read_infoframe, intel_hdmi_read_gcp_infoframe
// (display13 HDMI packet branches).
// Copyright 2006 Dave Airlie <airlied@linux.ie>
// Copyright © 2006-2009 Intel Corporation.
// intel_display_regs.h selected fields: Copyright © 2025 Intel Corporation.
// MIT permission text: ../LICENSE-MIT. HDMI packet writes are typed and caller-owned.
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
/// Pack the generic infoframe bytes into the HSW DIP buffer's ECC hole layout.
/// `hdmi_infoframe_pack_only()` supplies the checksum/header packet; this
/// reproduces `intel_write_infoframe()`'s memmove and zero byte at DW0 byte 3.
// upstream: intel_hdmi.c intel_write_infoframe()
pub fn infoframe_packet_to_dip_data(packet: &[u8]) -> Result<[u8; 32], Error> {
    if !(3..=31).contains(&packet.len()) {
        return Err(Error::Refused);
    }
    let mut raw = [0; 32];
    raw[..3].copy_from_slice(&packet[..3]);
    raw[4..packet.len() + 1].copy_from_slice(&packet[3..]);
    raw[3] = 0;
    Ok(raw)
}

/// Write one prepacked HDMI packet through the HSW transcoder DIP aperture.
/// All 32 bytes are written so the hardware ECC state is deterministic, just
/// as the source writes the remaining data words as zero. The caller owns the
/// surrounding modeset/port state and decides whether this packet is enabled.
// upstream: intel_hdmi.c hsw_write_infoframe()
pub fn hsw_write_infoframe(
    io: &impl ReadoutIo,
    pipe: Pipe,
    kind: FrameType,
    raw_dip_data: &[u8; 32],
) -> Result<(), Error> {
    if !io.pipe_powered(pipe) {
        return Err(Error::Refused);
    }
    let ctl = pipe.transcoder_register(0x60200);
    let mut value = io.read32(ctl)? & !kind.enable_mask();
    io.write32(ctl, value)?;
    for (index, bytes) in raw_dip_data.chunks_exact(4).enumerate() {
        let offset = kind.data_base() + index as u32 * 4;
        let word = u32::from_le_bytes(bytes.try_into().map_err(|_| Error::Truncated)?);
        io.write32(pipe.transcoder_register(offset), word)?;
    }
    value |= kind.enable_mask();
    io.write32(ctl, value)?;
    let _ = io.read32(ctl)?; // intel_de_posting_read()
    Ok(())
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

#[cfg(test)]
mod write_tests {
    extern crate std;
    use std::{cell::RefCell, collections::BTreeMap, vec::Vec};

    use super::*;
    use crate::RegisterIo;

    #[derive(Default)]
    struct Mock {
        registers: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        powered: bool,
    }
    impl RegisterIo for Mock {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            Ok(*self.registers.borrow().get(&offset).unwrap_or(&0))
        }
        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            self.registers.borrow_mut().insert(offset, value);
            self.writes.borrow_mut().push((offset, value));
            Ok(())
        }
    }
    impl ReadoutIo for Mock {
        fn pipe_powered(&self, _pipe: Pipe) -> bool {
            self.powered
        }
    }

    #[test]
    fn packet_hole_and_hsw_write_order_match_source() {
        let packet = [0x82, 2, 13, 0x7a, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let raw = infoframe_packet_to_dip_data(&packet).unwrap();
        assert_eq!(&raw[..4], &[0x82, 2, 13, 0]);
        assert_eq!(&raw[4..4 + packet.len() - 3], &packet[3..]);
        assert!(raw[4 + packet.len() - 3..].iter().all(|b| *b == 0));
        let mock = Mock {
            powered: true,
            ..Mock::default()
        };
        let pipe = Pipe::A;
        mock.registers
            .borrow_mut()
            .insert(pipe.transcoder_register(0x60200), 0xabc0_0123);
        hsw_write_infoframe(&mock, pipe, FrameType::Avi, &raw).unwrap();
        let writes = mock.writes.borrow();
        assert_eq!(writes.len(), 10); // disable, eight data writes, enable
        assert_eq!(
            writes[0],
            (pipe.transcoder_register(0x60200), 0xabc0_0123 & !(1 << 12))
        );
        assert_eq!(
            writes[1],
            (
                pipe.transcoder_register(0x60220),
                u32::from_le_bytes([0x82, 2, 13, 0])
            )
        );
        assert_eq!(writes[8], (pipe.transcoder_register(0x6023c), 0));
        assert_eq!(
            writes[9],
            (pipe.transcoder_register(0x60200), 0xabc0_0123 | (1 << 12))
        );
    }

    #[test]
    fn packet_write_refuses_when_pipe_power_is_not_owned() {
        let mock = Mock::default();
        assert_eq!(
            hsw_write_infoframe(&mock, Pipe::A, FrameType::Avi, &[0; 32]),
            Err(Error::Refused)
        );
        assert!(mock.writes.borrow().is_empty());
    }
}
