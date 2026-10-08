//! TX scheduler queue commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand, RingError, TxRing};

pub const SCD_QUEUE_CONFIG_CMD: u8 = 0x17;
pub const DATA_PATH_GROUP: u8 = 0x05;
pub const DQA_QUEUE_ADD: u32 = 0;
pub const DQA_QUEUE_REMOVE: u32 = 1;
pub const TX_QUEUE_CFG_ENABLE_QUEUE: u16 = 1;
pub const DEFAULT_QUEUE_SIZE: usize = 256;
const CMD_VERSION_UNKNOWN: u8 = 99;
const COMMAND_QUEUE_ID: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub station_id: u8,
    pub queue_id: u8,
    pub tid: u8,
    pub ring_size: usize,
    pub byte_count_address: u64,
    pub tfd_address: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    UnsupportedCommandVersion(u8),
    InvalidQueueSize,
    InvalidQueueId,
    InvalidStation,
    InvalidResponse,
    Command(CommandError),
    Ring(RingError),
}

impl From<CommandError> for QueueError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}
impl From<RingError> for QueueError {
    fn from(error: RingError) -> Self {
        Self::Ring(error)
    }
}

/// Convert a power-of-two ring size into the firmware's exponent-minus-three code.
pub fn queue_cb_size(ring_size: usize) -> Result<u32, QueueError> {
    if ring_size < 8 || !ring_size.is_power_of_two() {
        return Err(QueueError::InvalidQueueSize);
    }
    Ok(ring_size.trailing_zeros().saturating_sub(3))
}

/// Serialize a legacy TX_QUEUE_CFG v0 enable/disable command.
// upstream: if_iwx.c iwx_enable_txq()
pub fn legacy_queue_command(
    config: QueueConfig,
    enabled: bool,
    slot: u8,
) -> Result<EncodedCommand, QueueError> {
    let mut payload = [0; 24];
    payload[0] = config.station_id;
    payload[1] = config.tid;
    let (flags, cb_size, bc_address, tfd_address) = if enabled {
        (
            TX_QUEUE_CFG_ENABLE_QUEUE,
            queue_cb_size(config.ring_size)?,
            config.byte_count_address,
            config.tfd_address,
        )
    } else {
        (0, 0, 0, 0)
    };
    payload[2..4].copy_from_slice(&flags.to_le_bytes());
    payload[4..8].copy_from_slice(&cb_size.to_le_bytes());
    payload[8..16].copy_from_slice(&bc_address.to_le_bytes());
    payload[16..24].copy_from_slice(&tfd_address.to_le_bytes());
    let command = HostCommand {
        id: u32::from(SCD_QUEUE_CONFIG_CMD),
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, COMMAND_QUEUE_ID)?)
}

/// Select the legacy or v3 scheduler command version exactly as upstream does.
pub fn scheduler_queue_command(
    version: u8,
    config: QueueConfig,
    enabled: bool,
    slot: u8,
) -> Result<EncodedCommand, QueueError> {
    if version == 0 || version == CMD_VERSION_UNKNOWN {
        legacy_queue_command(config, enabled, slot)
    } else {
        dqa_queue_command(version, config, enabled, slot)
    }
}

/// Serialize an API v3 DQA queue add/remove command.
// upstream: if_iwx.c iwx_enable_txq()
pub fn dqa_queue_command(
    version: u8,
    config: QueueConfig,
    enabled: bool,
    slot: u8,
) -> Result<EncodedCommand, QueueError> {
    if version != 3 {
        return Err(QueueError::UnsupportedCommandVersion(version));
    }
    if config.station_id >= 32 {
        return Err(QueueError::InvalidStation);
    }
    let bytes = if enabled {
        let cb_size = queue_cb_size(config.ring_size)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(36)
            .map_err(|_| QueueError::Command(CommandError::PayloadTooLarge))?;
        bytes.extend_from_slice(&DQA_QUEUE_ADD.to_le_bytes());
        bytes.extend_from_slice(&(1u32 << config.station_id).to_le_bytes());
        bytes.push(config.tid);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&cb_size.to_le_bytes());
        bytes.extend_from_slice(&config.byte_count_address.to_le_bytes());
        bytes.extend_from_slice(&config.tfd_address.to_le_bytes());
        bytes
    } else {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(12)
            .map_err(|_| QueueError::Command(CommandError::PayloadTooLarge))?;
        bytes.extend_from_slice(&DQA_QUEUE_REMOVE.to_le_bytes());
        bytes.extend_from_slice(&(1u32 << config.station_id).to_le_bytes());
        bytes.extend_from_slice(&u32::from(config.tid).to_le_bytes());
        bytes
    };
    let command = HostCommand {
        id: (u32::from(DATA_PATH_GROUP) << 8) | u32::from(SCD_QUEUE_CONFIG_CMD),
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&bytes],
    };
    Ok(EncodedCommand::encode(&command, slot, COMMAND_QUEUE_ID)?)
}

/// Queue enable/aggregation bookkeeping retained beside the TX rings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxQueueState {
    pub enabled_mask: u32,
    pub full_mask: u32,
    pub tid: [u8; 32],
}

/// Reset a queue, send the generation-selected configuration, and validate
/// the assigned queue and write pointer before exposing it as enabled.
// upstream: if_iwx.c iwx_enable_txq()
pub fn enable_tx_queue<E, R: crate::DmaRegion>(
    state: &mut TxQueueState,
    ring: &mut TxRing<R>,
    config: QueueConfig,
    command_version: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<Vec<u8>, E>,
) -> Result<(), TxQueueError<E>> {
    let qid = usize::from(config.queue_id);
    if qid >= 32 {
        return Err(TxQueueError::Queue(QueueError::InvalidQueueId));
    }
    if ring.queue_id != u16::from(config.queue_id) {
        return Err(TxQueueError::Queue(QueueError::InvalidQueueId));
    }
    ring.reset()
        .map_err(|e| TxQueueError::Queue(QueueError::Ring(e)))?;
    let command = scheduler_queue_command(command_version, config, true, ring.current as u8)
        .map_err(TxQueueError::Queue)?;
    let response = send(&command).map_err(TxQueueError::Send)?;
    validate_enable_response(&response, config.queue_id, ring.current_hardware as u16)
        .map_err(TxQueueError::Queue)?;
    state.enabled_mask |= 1 << qid;
    state.tid[qid] = config.tid;
    Ok(())
}

/// Remove a queue and reset its descriptor state only after command success.
// upstream: if_iwx.c iwx_disable_txq()
pub fn disable_tx_queue<E, R: crate::DmaRegion>(
    state: &mut TxQueueState,
    ring: &mut TxRing<R>,
    config: QueueConfig,
    command_version: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<Vec<u8>, E>,
) -> Result<(), TxQueueError<E>> {
    let qid = usize::from(config.queue_id);
    if qid >= 32 {
        return Err(TxQueueError::Queue(QueueError::InvalidQueueId));
    }
    if ring.queue_id != u16::from(config.queue_id) {
        return Err(TxQueueError::Queue(QueueError::InvalidQueueId));
    }
    let command = scheduler_queue_command(command_version, config, false, ring.current as u8)
        .map_err(TxQueueError::Queue)?;
    let response = send(&command).map_err(TxQueueError::Send)?;
    if response.len() < 8 {
        return Err(TxQueueError::Queue(QueueError::InvalidResponse));
    }
    state.enabled_mask &= !(1 << qid);
    ring.reset()
        .map_err(|e| TxQueueError::Queue(QueueError::Ring(e)))?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum TxQueueError<E> {
    Queue(QueueError),
    Send(E),
}

/// Check the firmware's response queue and initial hardware write pointer.
// upstream: if_iwx.c iwx_enable_txq() response validation
pub fn validate_enable_response(
    response: &[u8],
    expected_queue: u8,
    expected_write_pointer: u16,
) -> Result<(), QueueError> {
    if response.len() != 8 {
        return Err(QueueError::InvalidResponse);
    }
    let queue = u16::from_le_bytes([response[0], response[1]]);
    let write_pointer = u16::from_le_bytes([response[4], response[5]]);
    if queue != u16::from(expected_queue) || write_pointer != expected_write_pointer {
        return Err(QueueError::InvalidResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> QueueConfig {
        QueueConfig {
            station_id: 7,
            queue_id: 9,
            tid: 5,
            ring_size: DEFAULT_QUEUE_SIZE,
            byte_count_address: 0x1122_3344_5566_7788,
            tfd_address: 0x8877_6655_4433_2211,
        }
    }

    #[test]
    fn queue_size_and_legacy_payload_match_exponent_minus_three_layout() {
        assert_eq!(queue_cb_size(256), Ok(5));
        assert_eq!(queue_cb_size(7), Err(QueueError::InvalidQueueSize));
        let enabled = legacy_queue_command(queue(), true, 3).unwrap();
        assert_eq!(enabled.flags, CMD_WANT_RESPONSE);
        assert_eq!(enabled.response_capacity, 8);
        assert_eq!(enabled.bytes.len(), 32);
        assert_eq!(enabled.bytes[3], COMMAND_QUEUE_ID);
        assert_eq!(
            &enabled.bytes[8..],
            &[
                7, 5, 1, 0, 5, 0, 0, 0, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x11, 0x22,
                0x33, 0x44, 0x55, 0x66, 0x77, 0x88
            ]
        );

        let disabled = legacy_queue_command(queue(), false, 3).unwrap();
        assert_eq!(
            &disabled.bytes[8..],
            &[
                7, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn dqa_queue_add_remove_and_response_checks_match_source_layout() {
        let add = dqa_queue_command(3, queue(), true, 4).unwrap();
        assert_eq!(add.flags, CMD_WANT_RESPONSE);
        assert_eq!(add.response_capacity, 8);
        assert_eq!(&add.bytes[8..12], &DQA_QUEUE_ADD.to_le_bytes());
        assert_eq!(&add.bytes[12..16], &(1u32 << 7).to_le_bytes());
        assert_eq!(add.bytes.len(), 44);
        assert_eq!(&add.bytes[28..36], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&add.bytes[36..44], &0x8877_6655_4433_2211u64.to_le_bytes());

        let remove = dqa_queue_command(3, queue(), false, 4).unwrap();
        assert_eq!(
            &remove.bytes[8..20],
            &[1, 0, 0, 0, 0x80, 0, 0, 0, 5, 0, 0, 0]
        );

        let mut response = [0; 8];
        response[0..2].copy_from_slice(&9u16.to_le_bytes());
        response[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(validate_enable_response(&response, 9, 3), Ok(()));
        assert_eq!(
            validate_enable_response(&response, 8, 3),
            Err(QueueError::InvalidResponse)
        );
        assert!(scheduler_queue_command(99, queue(), true, 1).is_ok());
        assert_eq!(
            scheduler_queue_command(2, queue(), true, 1),
            Err(QueueError::UnsupportedCommandVersion(2))
        );
    }
}
