//! TX command serialization and descriptor submission from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{DeviceFamily, DmaError, RingError, TxRing, TxSegment};

const TX_COMMAND_OPCODE: u8 = 0x1c;
const TX_COMMAND_HEADER_BYTES: usize = 4;
const TX_OFFLOAD_PAD: u32 = 1 << 13;
const TX_FLAGS_ENCRYPTION_DISABLED: u16 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxFrame<'a> {
    pub queue_id: u8,
    pub slot: u8,
    pub header: &'a [u8],
    pub payload_segments: &'a [TxSegment],
    pub flags: u16,
    pub rate_n_flags: u32,
    pub encryption_offloaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    Ring(RingError),
    Dma(DmaError),
    InvalidHeader,
    PayloadTooLarge,
    TooManySegments,
}

impl From<RingError> for TxError {
    fn from(error: RingError) -> Self {
        Self::Ring(error)
    }
}

impl From<DmaError> for TxError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedTxFrame {
    /// Command-header and generation-specific TX command, followed by MAC header.
    pub command: Vec<u8>,
    /// Total on-air byte count used in the SCD byte-count table.
    pub byte_count: u16,
    /// Hardware offload header-padding state.
    pub offload_assist: u32,
    /// Number of bytes the MAC-header offload must skip before payload.
    pub pad_bytes: u8,
}

/// Serialize `iwx_tx()`'s Gen2/Gen3 TX command and append command/payload TFDs.
// upstream: if_iwx.c iwx_tx()
pub fn encode_tx_frame(
    family: DeviceFamily,
    frame: &TxFrame<'_>,
) -> Result<EncodedTxFrame, TxError> {
    if frame.header.len() < 10 || frame.header.len() > 62 {
        return Err(TxError::InvalidHeader);
    }
    if frame.payload_segments.len() + 2 > crate::rings::TX_BUFFER_COUNT {
        return Err(TxError::TooManySegments);
    }
    let payload_len = frame
        .payload_segments
        .iter()
        .try_fold(0usize, |sum, segment| {
            sum.checked_add(usize::from(segment.length))
        })
        .ok_or(TxError::PayloadTooLarge)?;
    let total_len = frame
        .header
        .len()
        .checked_add(payload_len)
        .ok_or(TxError::PayloadTooLarge)?;
    let byte_count = u16::try_from(total_len).map_err(|_| TxError::PayloadTooLarge)?;

    let mut flags = frame.flags;
    if !frame.encryption_offloaded {
        flags |= TX_FLAGS_ENCRYPTION_DISABLED;
    }
    let pad_bytes = ((4 - (frame.header.len() & 3)) & 3) as u8;
    let offload_assist = (((frame.header.len() / 2) as u32) & 0x1f) << 8
        | if pad_bytes != 0 { TX_OFFLOAD_PAD } else { 0 };

    let mut command = Vec::new();
    let tx_command_bytes = if family >= DeviceFamily::Ax210 {
        28
    } else {
        20
    };
    command
        .try_reserve_exact(
            TX_COMMAND_HEADER_BYTES
                + tx_command_bytes
                + frame.header.len()
                + usize::from(pad_bytes),
        )
        .map_err(|_| TxError::PayloadTooLarge)?;
    command.extend_from_slice(&[TX_COMMAND_OPCODE, 0, frame.slot, frame.queue_id]);
    command.extend_from_slice(&byte_count.to_le_bytes());
    if family >= DeviceFamily::Ax210 {
        command.extend_from_slice(&flags.to_le_bytes());
        command.extend_from_slice(&offload_assist.to_le_bytes());
        command.extend_from_slice(&[0; 8]); // DRAM security info
        command.extend_from_slice(&frame.rate_n_flags.to_le_bytes());
        command.extend_from_slice(&[0; 8]); // Gen3 reserved bytes
    } else {
        command.extend_from_slice(&(offload_assist as u16).to_le_bytes());
        command.extend_from_slice(&u32::from(flags).to_le_bytes());
        command.extend_from_slice(&[0; 8]); // DRAM security info
        command.extend_from_slice(&frame.rate_n_flags.to_le_bytes());
    }
    command.extend_from_slice(frame.header);
    command.resize(command.len() + usize::from(pad_bytes), 0);

    Ok(EncodedTxFrame {
        command,
        byte_count,
        offload_assist,
        pad_bytes,
    })
}

/// Publish the serialized TX command and its payload segments in a hardware ring.
// upstream: if_iwx.c iwx_tx() DMA descriptor construction and ring kick preparation
pub fn submit_tx_frame<R: crate::DmaRegion>(
    ring: &mut TxRing<R>,
    family: DeviceFamily,
    frame: &TxFrame<'_>,
) -> Result<usize, TxError> {
    let encoded = encode_tx_frame(family, frame)?;
    let command_address = ring.command_address(ring.current)?;
    ring.commands.write_at(
        ring.current * crate::rings::TX_COMMAND_BYTES,
        &encoded.command,
    )?;
    let first_bytes = encoded.command.len().min(crate::command::FIRST_TB_BYTES);
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(frame.payload_segments.len() + 2)
        .map_err(|_| TxError::PayloadTooLarge)?;
    segments.push(TxSegment {
        address: command_address,
        length: first_bytes as u16,
    });
    if encoded.command.len() > first_bytes {
        segments.push(TxSegment {
            address: command_address + first_bytes as u64,
            length: (encoded.command.len() - first_bytes) as u16,
        });
    } else {
        segments.push(TxSegment {
            address: command_address + first_bytes as u64,
            length: 0,
        });
    }
    segments.extend_from_slice(frame.payload_segments);
    Ok(ring.submit(&segments, encoded.byte_count)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen2_and_gen3_tx_command_fields_follow_packed_layouts() {
        let header = [0x08, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let frame = TxFrame {
            queue_id: 4,
            slot: 3,
            header: &header,
            payload_segments: &[TxSegment {
                address: 0x4000,
                length: 31,
            }],
            flags: 1,
            rate_n_flags: 0x12345678,
            encryption_offloaded: false,
        };
        let gen2 = encode_tx_frame(DeviceFamily::Family22000, &frame).unwrap();
        assert_eq!(gen2.byte_count, 45);
        assert_eq!(gen2.pad_bytes, 2);
        assert_eq!(gen2.offload_assist, (7 << 8) | TX_OFFLOAD_PAD);
        assert_eq!(&gen2.command[..4], &[TX_COMMAND_OPCODE, 0, 3, 4]);
        assert_eq!(&gen2.command[4..8], &[45, 0, 0, 0x27]);
        assert_eq!(&gen2.command[6..8], &[0, 0x27]); // Gen2 offload is 16-bit.
        assert_eq!(&gen2.command[8..12], &((1u32 << 1) | 1).to_le_bytes());
        assert_eq!(&gen2.command[20..24], &0x12345678u32.to_le_bytes());
        assert_eq!(&gen2.command[24..38], &header);
        assert_eq!(&gen2.command[38..], &[0, 0]);

        let gen3 = encode_tx_frame(DeviceFamily::Ax210, &frame).unwrap();
        assert_eq!(&gen3.command[4..8], &[45, 0, 3, 0]);
        assert_eq!(
            &gen3.command[8..12],
            &((7u32 << 8) | TX_OFFLOAD_PAD).to_le_bytes()
        );
        assert_eq!(&gen3.command[20..24], &0x12345678u32.to_le_bytes());
        assert_eq!(&gen3.command[32..46], &header);
    }

    #[test]
    fn validates_mac_header_and_aggregate_payload_length() {
        let frame = TxFrame {
            queue_id: 1,
            slot: 0,
            header: &[0; 9],
            payload_segments: &[],
            flags: 0,
            rate_n_flags: 0,
            encryption_offloaded: false,
        };
        assert_eq!(
            encode_tx_frame(DeviceFamily::Ax210, &frame),
            Err(TxError::InvalidHeader)
        );
    }
}
