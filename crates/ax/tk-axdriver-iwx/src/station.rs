//! Station drain and TX-path flush commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const ADD_STA_COMMAND: u32 = 0x18;
pub const TX_PATH_FLUSH_COMMAND: u32 = 0x1e;
pub const STA_MODE_MODIFY: u8 = 1;
pub const STA_FLG_DRAIN_FLOW: u32 = 1 << 12;
pub const TX_FLUSH_QUEUE_LIMIT: usize = 16;
pub const TX_FLUSH_QUEUE_INFO_BYTES: usize = 8;
pub const TX_FLUSH_RESPONSE_BYTES: usize = 4 + TX_FLUSH_QUEUE_LIMIT * TX_FLUSH_QUEUE_INFO_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationError {
    InvalidResponse,
    TooManyQueues,
    Command(CommandError),
}

impl From<CommandError> for StationError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Serialize ADD_STA drain-flow state and request its command status.
// upstream: if_iwx.c iwx_drain_sta()
pub fn drain_station_command(
    mac_id_color: u32,
    station_id: u8,
    drain: bool,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; 48];
    payload[0] = STA_MODE_MODIFY;
    payload[4..8].copy_from_slice(&mac_id_color.to_le_bytes());
    payload[16] = station_id;
    let flags = if drain { STA_FLG_DRAIN_FLOW } else { 0 };
    payload[20..24].copy_from_slice(&flags.to_le_bytes());
    payload[24..28].copy_from_slice(&STA_FLG_DRAIN_FLOW.to_le_bytes());
    let command = HostCommand {
        id: ADD_STA_COMMAND,
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Serialize TXPATH_FLUSH for one station and a TID mask.
// upstream: if_iwx.c iwx_flush_sta_tids()
pub fn tx_path_flush_command(
    station_id: u32,
    tids: u16,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&station_id.to_le_bytes());
    payload[4..6].copy_from_slice(&tids.to_le_bytes());
    let command = HostCommand {
        id: TX_PATH_FLUSH_COMMAND,
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8 + TX_FLUSH_RESPONSE_BYTES,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushedQueue {
    pub tid: u16,
    pub queue_id: u16,
    pub read_before_flush: u16,
    pub read_after_flush: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxFlushResponse {
    pub station_id: u16,
    pub queues: Vec<FlushedQueue>,
}

/// Validate the fixed TXPATH_FLUSH response size/count and decode queue cursors.
// upstream: if_iwx.c iwx_flush_sta_tids()
pub fn parse_tx_flush_response(
    payload: &[u8],
    expected_station: u16,
) -> Result<TxFlushResponse, StationError> {
    if payload.len() != TX_FLUSH_RESPONSE_BYTES {
        return Err(StationError::InvalidResponse);
    }
    let station_id = le_u16(payload, 0);
    let count = usize::from(le_u16(payload, 2));
    if station_id != expected_station {
        return Err(StationError::InvalidResponse);
    }
    if count > TX_FLUSH_QUEUE_LIMIT {
        return Err(StationError::TooManyQueues);
    }
    let mut queues = Vec::new();
    queues
        .try_reserve_exact(count)
        .map_err(|_| StationError::InvalidResponse)?;
    for index in 0..count {
        let offset = 4 + index * TX_FLUSH_QUEUE_INFO_BYTES;
        queues.push(FlushedQueue {
            tid: le_u16(payload, offset),
            queue_id: le_u16(payload, offset + 2),
            read_before_flush: le_u16(payload, offset + 4),
            read_after_flush: le_u16(payload, offset + 6),
        });
    }
    Ok(TxFlushResponse { station_id, queues })
}

/// Preserve the source's drain-on, flush, drain-off order and always clear the
/// driver TXFLUSH state on success or failure.
// upstream: if_iwx.c iwx_flush_sta()
pub fn flush_station<E>(
    mut set_txflush: impl FnMut(bool),
    mut set_drain: impl FnMut(bool) -> Result<(), E>,
    mut flush_tids: impl FnMut(u16) -> Result<(), E>,
) -> Result<(), E> {
    set_txflush(true);
    let result = (|| {
        set_drain(true)?;
        flush_tids(u16::MAX)?;
        set_drain(false)
    })();
    set_txflush(false);
    result
}

const fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::cell::RefCell;

    use super::*;

    #[test]
    fn station_drain_and_txpath_flush_commands_keep_source_fields() {
        let drain = drain_station_command(0x1234, 2, true, 3, 0).unwrap();
        assert_eq!(drain.flags, CMD_WANT_RESPONSE);
        assert_eq!(drain.bytes.len(), 56);
        assert_eq!(&drain.bytes[12..16], &0x1234u32.to_le_bytes());
        assert_eq!(drain.bytes[24], 2);
        assert_eq!(&drain.bytes[28..32], &STA_FLG_DRAIN_FLOW.to_le_bytes());
        assert_eq!(&drain.bytes[32..36], &STA_FLG_DRAIN_FLOW.to_le_bytes());

        let flush = tx_path_flush_command(0, 0x1234, 4, 0).unwrap();
        assert_eq!(flush.flags, CMD_WANT_RESPONSE);
        assert_eq!(flush.response_capacity, 8 + TX_FLUSH_RESPONSE_BYTES);
        assert_eq!(&flush.bytes[8..16], &[0, 0, 0, 0, 0x34, 0x12, 0, 0]);
    }

    #[test]
    fn flush_response_decodes_queue_cursors_and_checks_count() {
        let mut payload = vec![0; TX_FLUSH_RESPONSE_BYTES];
        payload[0..2].copy_from_slice(&3u16.to_le_bytes());
        payload[2..4].copy_from_slice(&1u16.to_le_bytes());
        payload[4..12].copy_from_slice(&[5, 0, 9, 0, 2, 0, 7, 0]);
        let response = parse_tx_flush_response(&payload, 3).unwrap();
        assert_eq!(
            response.queues,
            [FlushedQueue {
                tid: 5,
                queue_id: 9,
                read_before_flush: 2,
                read_after_flush: 7,
            }]
        );
        assert_eq!(
            parse_tx_flush_response(&payload, 4),
            Err(StationError::InvalidResponse)
        );
        payload[2..4].copy_from_slice(&17u16.to_le_bytes());
        assert_eq!(
            parse_tx_flush_response(&payload, 3),
            Err(StationError::TooManyQueues)
        );
    }

    #[test]
    fn flush_station_clears_guard_and_preserves_drain_order_on_error() {
        let events = RefCell::new(Vec::new());
        let result = flush_station(
            |state| events.borrow_mut().push(if state { 1 } else { 0 }),
            |state| {
                events.borrow_mut().push(if state { 2 } else { 3 });
                Ok::<_, ()>(())
            },
            |mask| {
                assert_eq!(mask, u16::MAX);
                events.borrow_mut().push(4);
                Err(())
            },
        );
        assert_eq!(result, Err(()));
        assert_eq!(*events.borrow(), [1, 2, 4, 0]);
    }
}
