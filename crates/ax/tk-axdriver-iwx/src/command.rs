//! Firmware host-command wire encoding and submission from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230,
//! `iwx_send_cmd()`, `iwx_send_cmd_pdu()`, and status framing. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use alloc::vec::Vec;

use crate::{DmaError, DmaRegion, RingError, TxRing, TxSegment};

pub const CMD_ASYNC: u32 = 1 << 0;
pub const CMD_WANT_RESPONSE: u32 = 1 << 1;
pub const CMD_SEND_DURING_RFKILL: u32 = 1 << 2;
pub const CMD_FAILED_MASK: u8 = 0x40;
pub const LONG_GROUP: u8 = 1;
pub const FIRST_TB_BYTES: usize = 20;
pub const INLINE_COMMAND_BYTES: usize = 324;
pub const MAX_COMMAND_PAYLOAD: usize = 4096 - 8;
pub const MAX_RESPONSE_BYTES: usize = 4096;
const MAX_HOST_COMMAND_PARTS: usize = 2;

/// Command encoding/queue failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    TooManyParts,
    PayloadTooLarge,
    InvalidResponse,
    Ring(RingError),
    Dma(DmaError),
}

impl From<RingError> for CommandError {
    fn from(error: RingError) -> Self {
        Self::Ring(error)
    }
}
impl From<DmaError> for CommandError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

/// Host command parts correspond to the two upstream `iwx_host_cmd.data[]` TBS.
#[derive(Debug)]
pub struct HostCommand<'a> {
    pub id: u32,
    pub flags: u32,
    pub response_capacity: usize,
    pub parts: &'a [&'a [u8]],
}

/// Encoded wide firmware command and its original ID/compatibility state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedCommand {
    pub original_id: u32,
    pub wire_id: u32,
    pub narrow_compatibility: bool,
    pub flags: u32,
    pub response_capacity: usize,
    pub bytes: Vec<u8>,
}

impl EncodedCommand {
    /// Format the command header, concatenate payload parts, and apply the
    /// group-0-to-LONG_GROUP compatibility conversion.
    // upstream: if_iwx.c iwx_send_cmd() payload and wide-header preparation
    pub fn encode(command: &HostCommand<'_>, slot: u8, queue: u8) -> Result<Self, CommandError> {
        if command.parts.len() > MAX_HOST_COMMAND_PARTS {
            return Err(CommandError::TooManyParts);
        }
        let payload_len = command
            .parts
            .iter()
            .try_fold(0usize, |sum, part| sum.checked_add(part.len()))
            .ok_or(CommandError::PayloadTooLarge)?;
        if payload_len > MAX_COMMAND_PAYLOAD {
            return Err(CommandError::PayloadTooLarge);
        }
        let asynchronous = command.flags & CMD_ASYNC != 0;
        let want_response = command.flags & CMD_WANT_RESPONSE != 0;
        if want_response
            && (asynchronous
                || command.response_capacity < 8
                || command.response_capacity > MAX_RESPONSE_BYTES)
        {
            return Err(CommandError::InvalidResponse);
        }

        let mut wire_id = command.id;
        let narrow_compatibility = command_group_id(wire_id) == 0;
        if narrow_compatibility {
            wire_id = (u32::from(LONG_GROUP) << 8) | u32::from(command_opcode(wire_id));
        }
        let group_id = command_group_id(wire_id);
        let opcode = command_opcode(wire_id);
        let version = command_version(wire_id);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(8 + payload_len)
            .map_err(|_| CommandError::PayloadTooLarge)?;
        bytes.extend_from_slice(&[opcode, group_id, slot, queue]);
        bytes.extend_from_slice(&(payload_len as u16).to_le_bytes());
        bytes.extend_from_slice(&[0, version]);
        for part in command.parts {
            bytes.extend_from_slice(part);
        }
        Ok(Self {
            original_id: command.id,
            wire_id,
            narrow_compatibility,
            flags: command.flags,
            response_capacity: command.response_capacity,
            bytes,
        })
    }

    /// The TFD's first and optional second buffer address/length pair.
    pub fn tx_segments(&self, address: u64) -> Result<Vec<TxSegment>, CommandError> {
        let mut segments = Vec::new();
        let first_len = self.bytes.len().min(FIRST_TB_BYTES);
        segments
            .try_reserve(2)
            .map_err(|_| CommandError::PayloadTooLarge)?;
        segments.push(TxSegment {
            address,
            length: first_len as u16,
        });
        if self.bytes.len() > first_len {
            segments.push(TxSegment {
                address: address + first_len as u64,
                length: (self.bytes.len() - first_len) as u16,
            });
        }
        Ok(segments)
    }
}

/// Submit a prepared command on the queue and copy its wire bytes into either
/// the inline command array or the caller-provided external DMA buffer.
// upstream: if_iwx.c iwx_send_cmd() descriptor write, sync and queue kick preparation
pub fn submit_command<R: DmaRegion>(
    ring: &mut TxRing<R>,
    command: &EncodedCommand,
    external: Option<&mut R>,
) -> Result<usize, CommandError> {
    let slot = ring.current;
    let (address, bytes) = if command.bytes.len() <= INLINE_COMMAND_BYTES {
        ring.commands
            .write_at(slot * INLINE_COMMAND_BYTES, &command.bytes)?;
        (ring.command_address(slot)?, command.bytes.len())
    } else {
        let external = external.ok_or(CommandError::Dma(DmaError::RegionTooSmall))?;
        if external.capacity() < command.bytes.len() {
            return Err(CommandError::Dma(DmaError::RegionTooSmall));
        }
        external.write_at(0, &command.bytes)?;
        (external.device_address(), command.bytes.len())
    };
    let segments = command.tx_segments(address)?;
    let _ = bytes;
    Ok(ring.submit_without_byte_count(&segments)?)
}

pub const fn command_opcode(id: u32) -> u8 {
    id as u8
}
pub const fn command_group_id(id: u32) -> u8 {
    (id >> 8) as u8
}
pub const fn command_version(id: u32) -> u8 {
    (id >> 16) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_group_command_uses_wide_long_group_header() {
        let command = HostCommand {
            id: 0x002a,
            flags: 0,
            response_capacity: 0,
            parts: &[&[1, 2], &[3]],
        };
        let encoded = EncodedCommand::encode(&command, 7, 0).unwrap();
        assert!(encoded.narrow_compatibility);
        assert_eq!(encoded.wire_id, 0x012a);
        assert_eq!(&encoded.bytes[..8], &[0x2a, LONG_GROUP, 7, 0, 3, 0, 0, 0]);
        assert_eq!(&encoded.bytes[8..], &[1, 2, 3]);
        assert_eq!(
            encoded.tx_segments(0x1000).unwrap(),
            [TxSegment {
                address: 0x1000,
                length: 11
            }]
        );
    }

    #[test]
    fn wide_group_preserves_group_version_and_splits_at_first_tfd_buffer() {
        let payload = [0x55; 40];
        let command = HostCommand {
            id: 0x0007_0210,
            flags: 0,
            response_capacity: 0,
            parts: &[&payload],
        };
        let encoded = EncodedCommand::encode(&command, 3, 1).unwrap();
        assert!(!encoded.narrow_compatibility);
        assert_eq!(&encoded.bytes[..8], &[0x10, 2, 3, 1, 40, 0, 0, 7]);
        assert_eq!(
            encoded.tx_segments(0x8000).unwrap(),
            [
                TxSegment {
                    address: 0x8000,
                    length: 20
                },
                TxSegment {
                    address: 0x8014,
                    length: 28
                },
            ]
        );
    }

    #[test]
    fn rejects_unsupported_part_counts_payload_size_and_async_response_pair() {
        let parts = [&[][..], &[][..], &[][..]];
        assert_eq!(
            EncodedCommand::encode(
                &HostCommand {
                    id: 1,
                    flags: 0,
                    response_capacity: 0,
                    parts: &parts
                },
                0,
                0
            ),
            Err(CommandError::TooManyParts)
        );
        let large = [0; MAX_COMMAND_PAYLOAD + 1];
        assert_eq!(
            EncodedCommand::encode(
                &HostCommand {
                    id: 1,
                    flags: 0,
                    response_capacity: 0,
                    parts: &[&large]
                },
                0,
                0
            ),
            Err(CommandError::PayloadTooLarge)
        );
        assert_eq!(
            EncodedCommand::encode(
                &HostCommand {
                    id: 1,
                    flags: CMD_ASYNC | CMD_WANT_RESPONSE,
                    response_capacity: 16,
                    parts: &[]
                },
                0,
                0
            ),
            Err(CommandError::InvalidResponse)
        );
    }
}
