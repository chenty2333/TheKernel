//! TX-response and compressed block-ack notification decoding from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

pub const TX_RESPONSE_HEADER_BYTES: usize = 40;
pub const TX_STATUS_BYTES: usize = 4;
pub const TX_STATUS_SUCCESS: u16 = 0x01;
pub const TX_STATUS_DIRECT_DONE: u16 = 0x02;
pub const TX_STATUS_MASK: u16 = 0xff;
pub const COMPRESSED_BA_HEADER_BYTES: usize = 32;
pub const COMPRESSED_BA_RATID_BYTES: usize = 4;
pub const COMPRESSED_BA_TFD_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxCompletionError {
    InvalidLength,
    AggregateOnNonAggregateQueue,
    SequenceOutsideQueue,
    AllocationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxStatusNotification {
    pub frame_count: u8,
    pub status: u16,
    pub failed: bool,
    pub scd_ssn: Option<u32>,
}

/// Validate a TX response and extract its status/queue consumer SSN.
// upstream: if_iwx.c iwx_rx_tx_cmd()
pub fn parse_tx_status(
    payload: &[u8],
    queue_id: u16,
    first_aggregate_queue: u16,
    max_tfd_queue_size: usize,
) -> Result<TxStatusNotification, TxCompletionError> {
    if payload.len() < TX_RESPONSE_HEADER_BYTES + TX_STATUS_BYTES + 4 {
        return Err(TxCompletionError::InvalidLength);
    }
    let frame_count = payload[0];
    if frame_count == 0 {
        return Err(TxCompletionError::InvalidLength);
    }
    if queue_id < first_aggregate_queue && frame_count > 1 {
        return Err(TxCompletionError::AggregateOnNonAggregateQueue);
    }
    let status_end = TX_RESPONSE_HEADER_BYTES
        .checked_add(usize::from(frame_count) * TX_STATUS_BYTES)
        .ok_or(TxCompletionError::InvalidLength)?;
    if payload.len() < status_end + 4 {
        return Err(TxCompletionError::InvalidLength);
    }
    let status = u16::from_le_bytes([
        payload[TX_RESPONSE_HEADER_BYTES],
        payload[TX_RESPONSE_HEADER_BYTES + 1],
    ]) & TX_STATUS_MASK;
    let scd_ssn = if frame_count > 1 {
        None // iwx_rx_tx_cmd() defers aggregate completions to COMPRESSED_BA_NOTIF.
    } else {
        let ssn = u32::from_le_bytes(payload[status_end..status_end + 4].try_into().unwrap());
        if ssn as usize >= max_tfd_queue_size {
            return Err(TxCompletionError::SequenceOutsideQueue);
        }
        Some(ssn)
    };
    Ok(TxStatusNotification {
        frame_count,
        status,
        failed: status != TX_STATUS_SUCCESS && status != TX_STATUS_DIRECT_DONE,
        scd_ssn,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressedBaTfd {
    pub queue_id: u16,
    pub tfd_index: u16,
    pub tid: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedBaNotification {
    pub flags: u32,
    pub station_id: u8,
    pub transmitted: u16,
    pub acknowledged: u16,
    pub tfd_progress: Vec<CompressedBaTfd>,
}

/// Decode the variable RATID/TFD arrays in a compressed BA notification.
// upstream: if_iwx.c iwx_rx_compressed_ba()
pub fn parse_compressed_ba(payload: &[u8]) -> Result<CompressedBaNotification, TxCompletionError> {
    if payload.len() < COMPRESSED_BA_HEADER_BYTES {
        return Err(TxCompletionError::InvalidLength);
    }
    let flags = le_u32(payload, 0);
    let station_id = payload[4];
    let transmitted = le_u16(payload, 14);
    let acknowledged = le_u16(payload, 16);
    let tfd_count = usize::from(le_u16(payload, 28));
    let ratid_count = usize::from(le_u16(payload, 30));
    if tfd_count == 0 {
        return Err(TxCompletionError::InvalidLength);
    }
    let tfd_start = COMPRESSED_BA_HEADER_BYTES
        .checked_add(
            ratid_count
                .checked_mul(COMPRESSED_BA_RATID_BYTES)
                .ok_or(TxCompletionError::InvalidLength)?,
        )
        .ok_or(TxCompletionError::InvalidLength)?;
    let end = tfd_start
        .checked_add(
            tfd_count
                .checked_mul(COMPRESSED_BA_TFD_BYTES)
                .ok_or(TxCompletionError::InvalidLength)?,
        )
        .ok_or(TxCompletionError::InvalidLength)?;
    if payload.len() < end {
        return Err(TxCompletionError::InvalidLength);
    }
    let mut tfd_progress = Vec::new();
    tfd_progress
        .try_reserve_exact(tfd_count)
        .map_err(|_| TxCompletionError::AllocationFailed)?;
    for index in 0..tfd_count {
        let offset = tfd_start + index * COMPRESSED_BA_TFD_BYTES;
        tfd_progress.push(CompressedBaTfd {
            queue_id: le_u16(payload, offset),
            tfd_index: le_u16(payload, offset + 2),
            tid: payload[offset + 5],
        });
    }
    Ok(CompressedBaNotification {
        flags,
        station_id,
        transmitted,
        acknowledged,
        tfd_progress,
    })
}

const fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

const fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_response_checks_queue_kind_and_extracts_nonaggregate_ssn() {
        let mut payload = [0; TX_RESPONSE_HEADER_BYTES + TX_STATUS_BYTES + 4];
        payload[0] = 1;
        payload[40..42].copy_from_slice(&TX_STATUS_SUCCESS.to_le_bytes());
        payload[44..48].copy_from_slice(&0x1234u32.to_le_bytes());
        assert_eq!(
            parse_tx_status(&payload, 0, 2, 65_536),
            Ok(TxStatusNotification {
                frame_count: 1,
                status: TX_STATUS_SUCCESS,
                failed: false,
                scd_ssn: Some(0x1234),
            })
        );
        payload[0] = 2;
        assert_eq!(
            parse_tx_status(&payload, 1, 2, 65_536),
            Err(TxCompletionError::AggregateOnNonAggregateQueue)
        );
    }

    #[test]
    fn compressed_ba_skips_ratid_array_before_tfd_progress() {
        let mut payload =
            [0; COMPRESSED_BA_HEADER_BYTES + COMPRESSED_BA_RATID_BYTES + COMPRESSED_BA_TFD_BYTES];
        payload[4] = 0;
        payload[14..16].copy_from_slice(&10u16.to_le_bytes());
        payload[16..18].copy_from_slice(&8u16.to_le_bytes());
        payload[28..30].copy_from_slice(&1u16.to_le_bytes());
        payload[30..32].copy_from_slice(&1u16.to_le_bytes());
        payload[32..36].copy_from_slice(&[1, 2, 3, 4]);
        payload[36..38].copy_from_slice(&7u16.to_le_bytes());
        payload[38..40].copy_from_slice(&0x123u16.to_le_bytes());
        payload[41] = 5;
        let parsed = parse_compressed_ba(&payload).unwrap();
        assert_eq!(parsed.transmitted, 10);
        assert_eq!(parsed.acknowledged, 8);
        assert_eq!(
            parsed.tfd_progress[0],
            CompressedBaTfd {
                queue_id: 7,
                tfd_index: 0x123,
                tid: 5
            }
        );
        assert_eq!(
            parse_compressed_ba(&payload[..payload.len() - 1]),
            Err(TxCompletionError::InvalidLength)
        );
    }
}
