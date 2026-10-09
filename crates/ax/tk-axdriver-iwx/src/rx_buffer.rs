//! RX transfer-buffer packet walk from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{RxPacket, RxPacketError, parse_rx_packet};

pub const RX_BUFFER_SIZE: usize = 4096;
pub const RX_PACKET_MINIMUM_BYTES: usize = 8;
pub const RX_MPDU_COMMAND: u32 = 0x00c1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxMbufPlan {
    CopyCurrentPacket,
    TransferBuffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxBufferReport {
    pub packets_seen: usize,
    pub mpdu_packets: usize,
    pub replaced_ring_buffer: bool,
    pub input_errors: u32,
    pub stopped_at_offset: usize,
    pub transferred_buffer: bool,
    pub handled_latch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxBufferError<E> {
    Dispatch(E),
    CommandDone(E),
}

/// Walk one FH RX buffer, replace the first MPDU's ring buffer, dispatch packets and ACK replies.
// upstream: if_iwx.c iwx_rx_pkt()
pub fn process_rx_buffer<'a, E>(
    buffer: &'a [u8],
    ax210_or_newer: bool,
    rx_ring_index: u16,
    mut narrow_command: impl FnMut(u8, u8) -> bool,
    mut replace_rx_buffer: impl FnMut(u16) -> bool,
    mut dispatch: impl FnMut(&RxPacket<'a>, RxMbufPlan) -> Result<bool, E>,
    mut command_done: impl FnMut(u8, u8, u32) -> Result<(), E>,
) -> Result<RxBufferReport, RxBufferError<E>> {
    let mut report = RxBufferReport {
        packets_seen: 0,
        mpdu_packets: 0,
        replaced_ring_buffer: false,
        input_errors: 0,
        stopped_at_offset: 0,
        transferred_buffer: false,
        handled_latch: true,
    };
    let mut offset = 0usize;
    while offset + RX_PACKET_MINIMUM_BYTES < buffer.len() {
        let slice = &buffer[offset..];
        let preliminary = parse_rx_packet(slice, false);
        let Some(raw) = preliminary.as_ref().ok() else {
            break;
        };
        let narrow = narrow_command(raw.command_queue_id(), raw.index);
        let packet = match parse_rx_packet(slice, narrow) {
            Ok(packet) => packet,
            Err(_) => break,
        };
        let code = packet.command_id();
        let length = packet.total_bytes;
        if length < RX_PACKET_MINIMUM_BYTES || length > buffer.len() - offset {
            break;
        }
        report.packets_seen += 1;

        if code == RX_MPDU_COMMAND {
            report.mpdu_packets += 1;
            if report.mpdu_packets == 1 {
                if !replace_rx_buffer(rx_ring_index) {
                    report.input_errors += 1;
                    report.stopped_at_offset = offset;
                    break;
                }
                report.replaced_ring_buffer = true;
            }
            let next_offset = offset + packet.aligned_next_offset();
            let next_valid = next_offset + RX_PACKET_MINIMUM_BYTES < buffer.len()
                && parse_rx_packet(&buffer[next_offset..], false).is_ok();
            let plan = if ax210_or_newer || !next_valid {
                report.transferred_buffer = true;
                RxMbufPlan::TransferBuffer
            } else {
                RxMbufPlan::CopyCurrentPacket
            };
            let handled = dispatch(&packet, plan).map_err(RxBufferError::Dispatch)?;
            if !handled {
                report.handled_latch = false;
            }
            if report.handled_latch && !packet.is_notification() {
                command_done(packet.command_queue_id(), packet.index, code)
                    .map_err(RxBufferError::CommandDone)?;
            }
            report.stopped_at_offset = offset;
            if plan == RxMbufPlan::TransferBuffer {
                break;
            }
        } else {
            let handled = dispatch(&packet, RxMbufPlan::CopyCurrentPacket)
                .map_err(RxBufferError::Dispatch)?;
            if !handled {
                report.handled_latch = false;
            }
            if report.handled_latch && !packet.is_notification() {
                command_done(packet.command_queue_id(), packet.index, code)
                    .map_err(RxBufferError::CommandDone)?;
            }
        }
        offset += packet.aligned_next_offset();
        report.stopped_at_offset = offset;
        if ax210_or_newer {
            break;
        }
    }
    Ok(report)
}

/// Return the packet decoder's framing error without altering queue state.
pub fn rx_buffer_packet_error(buffer: &[u8]) -> Option<RxPacketError> {
    parse_rx_packet(buffer, false).err()
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell;

    use super::*;

    fn packet(code: u32, index: u8, queue: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let body = 4 + payload.len();
        bytes.extend_from_slice(&(body as u32).to_le_bytes());
        bytes.extend_from_slice(&[
            (code & 0xff) as u8,
            ((code >> 8) & 0xff) as u8,
            index,
            queue,
        ]);
        bytes.extend_from_slice(payload);
        bytes.resize((bytes.len() + 63) & !63, 0);
        bytes
    }

    #[test]
    fn ax210_replaces_ring_buffer_before_delivering_mpdu_and_transfers_ownership() {
        let bytes = packet(RX_MPDU_COMMAND, 0, 0x80, &[1, 2, 3]);
        let replaced = Cell::new(false);
        let delivered = Cell::new(false);
        let report = process_rx_buffer(
            &bytes,
            true,
            7,
            |_, _| false,
            |index| {
                assert_eq!(index, 7);
                replaced.set(true);
                true
            },
            |packet, plan| {
                assert!(replaced.get());
                assert_eq!(packet.payload, [1, 2, 3]);
                assert_eq!(plan, RxMbufPlan::TransferBuffer);
                delivered.set(true);
                Ok(true)
            },
            |_, _, _| Ok::<_, ()>(()),
        )
        .unwrap();
        assert!(delivered.get() && report.replaced_ring_buffer && report.transferred_buffer);
        assert_eq!(report.mpdu_packets, 1);
    }

    #[test]
    fn pre_ax_multi_packet_buffer_copies_then_transfers_last_mpdu() {
        let mut bytes = packet(RX_MPDU_COMMAND, 0, 0x80, &[1]);
        bytes.extend(packet(RX_MPDU_COMMAND, 1, 0x80, &[2]));
        let mut plans = Vec::new();
        let report = process_rx_buffer(
            &bytes,
            false,
            0,
            |_, _| false,
            |_| true,
            |packet, plan| {
                plans.push((packet.index, plan));
                Ok(true)
            },
            |_, _, _| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(
            plans,
            [
                (0, RxMbufPlan::CopyCurrentPacket),
                (1, RxMbufPlan::TransferBuffer)
            ]
        );
        assert_eq!(report.mpdu_packets, 2);
        assert!(report.transferred_buffer);
    }

    #[test]
    fn direct_command_ack_and_notification_origin_follow_handled_latch() {
        let mut bytes = packet(0x0117, 3, 0, &[1, 0, 0, 0]);
        bytes.extend(packet(0x0017, 4, 0x80, &[]));
        let mut done = Vec::new();
        let report = process_rx_buffer(
            &bytes,
            false,
            0,
            |_, _| false,
            |_| true,
            |_, _| Ok(true),
            |qid, index, id| {
                done.push((qid, index, id));
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert_eq!(done, [(0, 3, 0x0117)]);
        assert_eq!(report.packets_seen, 2);
    }
}
