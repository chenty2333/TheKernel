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
pub const STA_MODIFY_ADD_BA_TID: u8 = 1 << 3;
pub const STA_MODIFY_UAPSD_ACS: u8 = 1 << 2;
pub const STA_ID_LINK: u8 = 0;
pub const STA_ID_MONITOR: u8 = 2;
pub const STA_TYPE_LINK: u8 = 0;
pub const STA_TYPE_GENERAL_PURPOSE: u8 = 1;
pub const REMOVE_STA_COMMAND: u32 = 0x19;
pub const FW_COMMAND_VERSION_UNKNOWN: u8 = 99;
pub const STA_FLAG_MAX_AGG_SIZE_SHIFT: u32 = 19;
pub const STA_FLAG_MAX_AGG_SIZE_MASK: u32 = 0xf << STA_FLAG_MAX_AGG_SIZE_SHIFT;
pub const STA_FLAG_AGG_DENSITY_SHIFT: u32 = 23;
pub const STA_FLAG_AGG_DENSITY_MASK: u32 = 7 << STA_FLAG_AGG_DENSITY_SHIFT;
pub const STA_FLAG_FAT_SHIFT: u32 = 26;
pub const STA_FLAG_FAT_MASK: u32 = 3 << STA_FLAG_FAT_SHIFT;
pub const STA_FLAG_MIMO_SHIFT: u32 = 28;
pub const STA_FLAG_MIMO_MASK: u32 = 3 << STA_FLAG_MIMO_SHIFT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StationAddConfig {
    pub use_mld_api: bool,
    pub monitor_mode: bool,
    pub update: bool,
    pub mac_id_color: u32,
    pub address: [u8; 6],
    pub mimo_enabled: bool,
    pub ht: bool,
    pub vht: bool,
    pub ht_stream2: bool,
    pub ht_stream3: bool,
    pub vht_stream2: bool,
    pub channel_allows_40mhz: bool,
    pub peer_supports_ht40: bool,
    pub channel_allows_80mhz: bool,
    pub peer_supports_vht80: bool,
    pub channel_allows_160mhz: bool,
    pub peer_supports_vht160: bool,
    pub ht_ampdu_exponent: u8,
    pub vht_ampdu_exponent: u8,
    pub ampdu_density: u8,
    pub uapsd_node: bool,
    pub uapsd_supported: bool,
    pub uapsd_access_categories: u8,
    pub uapsd_max_service_period: u8,
}

/// Build ADD_STA initialization/update fields for HT/VHT, aggregation and U-APSD.
// upstream: if_iwx.c iwx_add_sta_cmd()
pub fn station_add_command(
    config: StationAddConfig,
    slot: u8,
    queue: u8,
) -> Result<Option<EncodedCommand>, CommandError> {
    if config.use_mld_api {
        return Ok(None);
    }
    let mut payload = [0u8; 48];
    payload[0] = if config.update { 1 } else { 0 };
    payload[4..8].copy_from_slice(&config.mac_id_color.to_le_bytes());
    let station_id = if config.monitor_mode {
        STA_ID_MONITOR
    } else {
        STA_ID_LINK
    };
    payload[16] = station_id;
    payload[35] = if config.monitor_mode {
        STA_TYPE_GENERAL_PURPOSE
    } else {
        STA_TYPE_LINK
    };
    if !config.update {
        let address = if config.monitor_mode {
            [0; 6]
        } else {
            config.address
        };
        payload[8..14].copy_from_slice(&address);
    }
    let mut station_flags = 0u32;
    let mut station_flags_mask = STA_FLAG_FAT_MASK | STA_FLAG_MIMO_MASK;
    if config.ht {
        station_flags_mask |= STA_FLAG_MAX_AGG_SIZE_MASK | STA_FLAG_AGG_DENSITY_MASK;
        if config.mimo_enabled {
            if config.vht {
                if config.vht_stream2 {
                    station_flags |= 1 << STA_FLAG_MIMO_SHIFT;
                }
            } else {
                if config.ht_stream2 {
                    station_flags |= 1 << STA_FLAG_MIMO_SHIFT;
                }
                if config.ht_stream3 {
                    station_flags |= 2 << STA_FLAG_MIMO_SHIFT;
                }
            }
        }
        if config.channel_allows_40mhz && config.peer_supports_ht40 {
            station_flags |= 1 << STA_FLAG_FAT_SHIFT;
        }
        if config.vht {
            if config.channel_allows_160mhz && config.peer_supports_vht160 {
                station_flags = (station_flags & !STA_FLAG_FAT_MASK) | (3 << STA_FLAG_FAT_SHIFT);
            } else if config.channel_allows_80mhz && config.peer_supports_vht80 {
                station_flags = (station_flags & !STA_FLAG_FAT_MASK) | (2 << STA_FLAG_FAT_SHIFT);
            }
        }
        let aggregate_exponent = if config.vht {
            config.vht_ampdu_exponent
        } else {
            config.ht_ampdu_exponent
        }
        .min(7);
        station_flags |= u32::from(aggregate_exponent) << STA_FLAG_MAX_AGG_SIZE_SHIFT;
        let density = match config.ampdu_density {
            2 => 4,
            4 => 5,
            8 => 6,
            16 => 7,
            _ => 0,
        };
        station_flags |= density << STA_FLAG_AGG_DENSITY_SHIFT;
    }
    payload[20..24].copy_from_slice(&station_flags.to_le_bytes());
    payload[24..28].copy_from_slice(&station_flags_mask.to_le_bytes());
    if config.uapsd_node && config.uapsd_supported {
        payload[17] = STA_MODIFY_UAPSD_ACS;
        payload[46] = crate::uapsd_service_period(config.uapsd_max_service_period);
        payload[47] = crate::uapsd_ac_mask(config.uapsd_access_categories);
    }
    let command = HostCommand {
        id: ADD_STA_COMMAND,
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(Some(EncodedCommand::encode(&command, slot, queue)?))
}

/// Validate the low-byte ADD_STA success status.
// upstream: if_iwx.c iwx_add_sta_cmd()
pub fn validate_station_add_status(response: &[u8]) -> Result<(), StationError> {
    let status = response.get(..4).ok_or(StationError::InvalidResponse)?;
    if u32::from_le_bytes(status.try_into().unwrap()) & 0xff == 1 {
        Ok(())
    } else {
        Err(StationError::InvalidResponse)
    }
}

/// Serialize the four-byte legacy REMOVE_STA command.
// upstream: if_iwx.c iwx_rm_sta_cmd()
pub fn remove_station_command(
    monitor_mode: bool,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; 4];
    payload[0] = if monitor_mode {
        STA_ID_MONITOR
    } else {
        STA_ID_LINK
    };
    let command = HostCommand {
        id: REMOVE_STA_COMMAND,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StationQueue {
    pub queue_id: u16,
    pub tid: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StationRemoveState {
    pub active: bool,
    pub mld: bool,
    pub qenable_mask: u32,
    pub rx_ba_sessions: u8,
    pub rx_ba_start_mask: u16,
    pub rx_ba_stop_mask: u16,
    pub tx_ba_start_mask: u16,
    pub tx_ba_stop_mask: u16,
    pub agg_queue_by_tid: [u8; 8],
    pub agreed_tx_ba_mask: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StationRemoveError<E> {
    Inactive,
    Flush(E),
    DisableManagement(E),
    DisableAggregation(E),
    Remove(E),
}

impl StationRemoveState {
    /// Flush traffic, remove DQA queues when required, remove station/link, then
    /// reset BA and queue state and issue DELBA callbacks.
    // upstream: if_iwx.c iwx_rm_sta()
    pub fn remove<E>(
        &mut self,
        command_version: u8,
        first_aggregate_queue: u16,
        queues: &[StationQueue],
        mut flush: impl FnMut() -> Result<(), E>,
        mut disable_management: impl FnMut() -> Result<(), E>,
        mut disable_queue: impl FnMut(u16, u8) -> Result<(), E>,
        mut remove_firmware_station: impl FnMut(bool) -> Result<(), E>,
        mut send_delba: impl FnMut(u8),
    ) -> Result<(), StationRemoveError<E>> {
        if !self.active {
            return Err(StationRemoveError::Inactive);
        }
        flush().map_err(StationRemoveError::Flush)?;
        if !self.mld && command_version != 0 && command_version != FW_COMMAND_VERSION_UNKNOWN {
            disable_management().map_err(StationRemoveError::DisableManagement)?;
            for queue in queues {
                let qid = usize::from(queue.queue_id);
                if queue.queue_id < first_aggregate_queue || qid >= 32 {
                    continue;
                }
                if self.qenable_mask & (1 << qid) != 0 {
                    disable_queue(queue.queue_id, queue.tid)
                        .map_err(StationRemoveError::DisableAggregation)?;
                }
            }
        }
        remove_firmware_station(self.mld).map_err(StationRemoveError::Remove)?;

        self.active = false;
        self.rx_ba_sessions = 0;
        self.rx_ba_start_mask = 0;
        self.rx_ba_stop_mask = 0;
        self.tx_ba_start_mask = 0;
        self.tx_ba_stop_mask = 0;
        self.agg_queue_by_tid = [0; 8];
        for queue in queues {
            if queue.queue_id < 32 {
                self.qenable_mask &= !(1 << queue.queue_id);
            }
        }
        for tid in 0..8 {
            if self.agreed_tx_ba_mask & (1 << tid) != 0 {
                send_delba(tid as u8);
            }
        }
        self.agreed_tx_ba_mask = 0;
        Ok(())
    }
}

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

    #[test]
    fn add_station_encodes_peer_capabilities_aggregation_and_uapsd() {
        let config = StationAddConfig {
            use_mld_api: false,
            monitor_mode: false,
            update: false,
            mac_id_color: 0x1122_3344,
            address: [0, 1, 2, 3, 4, 5],
            mimo_enabled: true,
            ht: true,
            vht: true,
            ht_stream2: false,
            ht_stream3: false,
            vht_stream2: true,
            channel_allows_40mhz: true,
            peer_supports_ht40: true,
            channel_allows_80mhz: true,
            peer_supports_vht80: true,
            channel_allows_160mhz: false,
            peer_supports_vht160: false,
            ht_ampdu_exponent: 2,
            vht_ampdu_exponent: 9,
            ampdu_density: 8,
            uapsd_node: true,
            uapsd_supported: true,
            uapsd_access_categories: crate::WMM_AC_VO | crate::WMM_AC_BE,
            uapsd_max_service_period: crate::WMM_SP_4,
        };
        let command = station_add_command(config, 3, 0).unwrap().unwrap();
        assert_eq!(command.flags, CMD_WANT_RESPONSE);
        assert_eq!(command.bytes.len(), 56);
        assert_eq!(&command.bytes[12..16], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&command.bytes[16..22], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(command.bytes[24], STA_ID_LINK);
        assert_eq!(command.bytes[25], STA_MODIFY_UAPSD_ACS);
        let flags = u32::from_le_bytes(command.bytes[28..32].try_into().unwrap());
        assert_eq!(flags & STA_FLAG_MIMO_MASK, 1 << STA_FLAG_MIMO_SHIFT);
        assert_eq!((flags & STA_FLAG_FAT_MASK) >> STA_FLAG_FAT_SHIFT, 2);
        assert_eq!(
            (flags & STA_FLAG_MAX_AGG_SIZE_MASK) >> STA_FLAG_MAX_AGG_SIZE_SHIFT,
            7
        );
        assert_eq!(
            (flags & STA_FLAG_AGG_DENSITY_MASK) >> STA_FLAG_AGG_DENSITY_SHIFT,
            6
        );
        assert_eq!(
            &command.bytes[32..36],
            &(STA_FLAG_FAT_MASK
                | STA_FLAG_MIMO_MASK
                | STA_FLAG_MAX_AGG_SIZE_MASK
                | STA_FLAG_AGG_DENSITY_MASK)
                .to_le_bytes()
        );
        assert_eq!(command.bytes[54], 4);
        assert_eq!(
            command.bytes[55],
            crate::uapsd_ac_mask(config.uapsd_access_categories)
        );
        assert_eq!(
            station_add_command(
                StationAddConfig {
                    use_mld_api: true,
                    ..config
                },
                0,
                0
            )
            .unwrap(),
            None
        );
        assert_eq!(validate_station_add_status(&1u32.to_le_bytes()), Ok(()));
        assert_eq!(
            validate_station_add_status(&2u32.to_le_bytes()),
            Err(StationError::InvalidResponse)
        );
    }

    #[test]
    fn remove_station_orders_flush_queue_removal_cleanup_and_delba() {
        use core::cell::RefCell;
        let mut state = StationRemoveState {
            active: true,
            mld: false,
            qenable_mask: (1 << 1) | (1 << 2) | (1 << 3),
            rx_ba_sessions: 2,
            rx_ba_start_mask: 1,
            rx_ba_stop_mask: 2,
            tx_ba_start_mask: 4,
            tx_ba_stop_mask: 8,
            agg_queue_by_tid: [2, 3, 0, 0, 0, 0, 0, 0],
            agreed_tx_ba_mask: 1 | (1 << 3),
        };
        let queues = [
            StationQueue {
                queue_id: 1,
                tid: 0,
            },
            StationQueue {
                queue_id: 2,
                tid: 0,
            },
            StationQueue {
                queue_id: 3,
                tid: 1,
            },
        ];
        let events = RefCell::new(Vec::new());
        state
            .remove(
                3,
                2,
                &queues,
                || {
                    events.borrow_mut().push(0);
                    Ok::<_, ()>(())
                },
                || {
                    events.borrow_mut().push(1);
                    Ok(())
                },
                |qid, tid| {
                    events.borrow_mut().push(10 + qid as u8 + tid);
                    Ok(())
                },
                |mld| {
                    assert!(!mld);
                    events.borrow_mut().push(2);
                    Ok(())
                },
                |tid| events.borrow_mut().push(20 + tid),
            )
            .unwrap();
        assert_eq!(*events.borrow(), [0, 1, 12, 14, 2, 20, 23]);
        assert!(!state.active);
        assert_eq!(state.qenable_mask, 0);
        assert_eq!(state.rx_ba_sessions, 0);
        assert_eq!(state.agg_queue_by_tid, [0; 8]);
        assert_eq!(state.agreed_tx_ba_mask, 0);
    }
}
