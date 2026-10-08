//! Firmware receive-packet framing from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{command_group_id, command_opcode};

pub const RX_PACKET_HEADER_BYTES: usize = 8;
pub const FH_FRAME_SIZE_MASK: u32 = 0x0000_3fff;
pub const FH_FRAME_INVALID: u32 = 0x5555_0000;
pub const FH_FRAME_ALIGNMENT: usize = 0x40;
pub const NOTIFICATION_ORIGIN: u8 = 0x80;
pub const COMMAND_FAILED_MASK: u8 = 0x40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxPacketError {
    TooShort,
    InvalidMarker,
    InvalidLength,
}

/// One checked packet from an FH RX transfer buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxPacket<'a> {
    pub length_flags: u32,
    pub opcode: u8,
    pub group_id: u8,
    pub command_failed: bool,
    pub index: u8,
    pub queue_id: u8,
    pub payload: &'a [u8],
    pub total_bytes: usize,
}

impl RxPacket<'_> {
    pub const fn command_id(&self) -> u32 {
        ((self.group_id as u32) << 8) | self.opcode as u32
    }

    pub const fn is_notification(&self) -> bool {
        self.queue_id & NOTIFICATION_ORIGIN != 0
    }

    pub const fn command_queue_id(&self) -> u8 {
        self.queue_id & !NOTIFICATION_ORIGIN
    }

    pub const fn aligned_next_offset(&self) -> usize {
        (self.total_bytes + FH_FRAME_ALIGNMENT - 1) & !(FH_FRAME_ALIGNMENT - 1)
    }
}

/// Validate FH length/invalid marker and decode the OpenBSD command header.
// upstream: if_iwx.c iwx_rx_pkt_valid() and initial iwx_rx_pkt() framing
pub fn parse_rx_packet(
    buffer: &[u8],
    narrow_compatibility: bool,
) -> Result<RxPacket<'_>, RxPacketError> {
    if buffer.len() < RX_PACKET_HEADER_BYTES {
        return Err(RxPacketError::TooShort);
    }
    let length_flags = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
    if length_flags == FH_FRAME_INVALID {
        return Err(RxPacketError::InvalidMarker);
    }
    let body_len = (length_flags & FH_FRAME_SIZE_MASK) as usize;
    if body_len < 4 {
        return Err(RxPacketError::InvalidLength);
    }
    let total_bytes = 4usize
        .checked_add(body_len)
        .ok_or(RxPacketError::InvalidLength)?;
    if total_bytes > buffer.len() {
        return Err(RxPacketError::InvalidLength);
    }
    let opcode = buffer[4];
    let raw_group = buffer[5];
    let command_failed = raw_group & COMMAND_FAILED_MASK != 0;
    let mut group_id = raw_group & !COMMAND_FAILED_MASK;
    let index = buffer[6];
    let queue_id = buffer[7];
    let raw_code = (u32::from(group_id) << 8) | u32::from(opcode);
    if (queue_id & !NOTIFICATION_ORIGIN == 0)
        && index == 0
        && command_opcode(raw_code) == 0
        && command_group_id(raw_code) == 0
    {
        return Err(RxPacketError::InvalidMarker);
    }
    if narrow_compatibility && group_id == 1 {
        group_id = 0;
    }
    Ok(RxPacket {
        length_flags,
        opcode,
        group_id,
        command_failed,
        index,
        queue_id,
        payload: &buffer[RX_PACKET_HEADER_BYTES..total_bytes],
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn packet(opcode: u8, group: u8, index: u8, queue: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let body_len = 4 + payload.len();
        bytes.extend_from_slice(&(body_len as u32).to_le_bytes());
        bytes.extend_from_slice(&[opcode, group, index, queue]);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn packet_length_payload_and_alignment_follow_fh_framing() {
        let bytes = packet(0x17, 0x05, 7, 0x80 | 9, &[1, 2, 3]);
        let parsed = parse_rx_packet(&bytes, false).unwrap();
        assert_eq!(parsed.command_id(), 0x0517);
        assert_eq!(parsed.index, 7);
        assert_eq!(parsed.command_queue_id(), 9);
        assert!(parsed.is_notification());
        assert_eq!(parsed.payload, [1, 2, 3]);
        assert_eq!(parsed.total_bytes, 11);
        assert_eq!(parsed.aligned_next_offset(), 64);
    }

    #[test]
    fn narrow_compatibility_reverses_long_group_reply_and_validates_lengths() {
        let bytes = packet(0x98, 1, 2, 0, &[0xaa]);
        assert_eq!(parse_rx_packet(&bytes, false).unwrap().command_id(), 0x0198);
        assert_eq!(parse_rx_packet(&bytes, true).unwrap().command_id(), 0x0098);
        assert_eq!(
            parse_rx_packet(&bytes[..7], false),
            Err(RxPacketError::TooShort)
        );
        assert_eq!(
            parse_rx_packet(&FH_FRAME_INVALID.to_le_bytes(), false),
            Err(RxPacketError::TooShort)
        );
        let mut invalid = packet(1, 0, 0, 0, &[]);
        invalid[..4].copy_from_slice(&FH_FRAME_INVALID.to_le_bytes());
        assert_eq!(
            parse_rx_packet(&invalid, false),
            Err(RxPacketError::InvalidMarker)
        );
        let mut truncated = packet(1, 2, 3, 4, &[]);
        truncated[..4].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(
            parse_rx_packet(&truncated, false),
            Err(RxPacketError::InvalidLength)
        );
    }
}
