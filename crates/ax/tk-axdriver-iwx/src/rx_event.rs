//! Firmware packet classification and command-response retirement from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, CommandSlots, RxPacket};

const RX_PHY: u32 = 0x00c0;
const RX_MPDU: u32 = 0x00c1;
const BAR_FRAME_RELEASE: u32 = 0x00c2;
const FRAME_RELEASE: u32 = 0x00c3;
const TX_CMD: u32 = 0x001c;
const BA_NOTIF: u32 = 0x00c5;
const MISSED_BEACONS: u32 = 0x00a2;
const MISSED_BEACONS_WIDE: u32 = (3 << 8) | 0xf6;
const MFUART_LOAD: u32 = 0x00b1;
const STATISTICS: u32 = 0x009c;
const DTS_MEASUREMENT: u32 = 0x00dd;
const CT_KILL: u32 = (4 << 8) | 0xfe;
const TIME_EVENT_NOTIFICATION: u32 = 0x002a;
const UAPSD_MISBEHAVING: u32 = 0x0078;
const MCC_CHUB_UPDATE: u32 = 0x00c9;
const FIRMWARE_ERROR: u32 = 0x0002;
const CHANNEL_SWITCH: u32 = (3 << 8) | 0xff;
const SESSION_PROTECTION_NOTIF: u32 = (3 << 8) | 0xfb;
const SYSTEM_STATS_END: u32 = (2 << 8) | 0xfd;
const SYSTEM_STATS_OPER: u32 = (0x10 << 8) | 0x00;
const SYSTEM_STATS_PART1: u32 = (0x10 << 8) | 0x01;
const FSEQ_MISMATCH: u32 = (2 << 8) | 0xff;
const DEBUG_LOG: u32 = 0x00f7;
const MCAST_FILTER: u32 = 0x00d0;
const DATA_PATH_DQA: u32 = (5 << 8) | 0x00;
const DATA_PATH_TLC: u32 = (5 << 8) | 0x0f;
const DATA_PATH_RLC: u32 = (5 << 8) | 0x08;
const DATA_PATH_NO_DATA: u32 = (5 << 8) | 0xf5;
const DATA_PATH_DUAL_CHAIN: u32 = (5 << 8) | 0xf6;
const DATA_PATH_TLC_UPDATE: u32 = (5 << 8) | 0xf7;
const REGULATORY_GROUP: u8 = 0x0c;
const REGULATORY_NVM_ACCESS_COMPLETE: u32 = (REGULATORY_GROUP as u32) << 8;
const UMAC_SCAN_ITERATION_COMPLETE: u32 = (LONG_GROUP as u32) << 8 | 0xb5;
const SYSTEM_SOC_CONFIG: u32 = (2 << 8) | 0x01;
const SYSTEM_STATS_CMD: u32 = (2 << 8) | 0x0f;
const PHY_DTS_WIDE: u32 = (4 << 8) | 0xff;
const PHY_TEMP_THRESH: u32 = (4 << 8) | 0x04;
const BT_PROFILE: u32 = (9 << 8) | 0xff;
const ALIVE: u32 = 0x0001;
const INIT_COMPLETE: u32 = 0x0004;
const LONG_GROUP: u8 = 1;
const MAC_CONF_GROUP: u8 = 3;
const DATA_PATH_GROUP: u8 = 5;
const REGULATORY_NVM_GROUP: u8 = 0x0c;
const UMAC_SCAN_COMPLETE: u32 = ((LONG_GROUP as u32) << 8) | 0x0f;
const PNVM_COMPLETE: u32 = ((REGULATORY_NVM_GROUP as u32) << 8) | 0xfe;
const RESP_BAID: u32 = ((DATA_PATH_GROUP as u32) << 8) | 0x16;
const RESP_QUEUE: u32 = ((DATA_PATH_GROUP as u32) << 8) | 0x17;
const RESP_SEC_KEY: u32 = ((DATA_PATH_GROUP as u32) << 8) | 0x18;
const RESP_SESSION: u32 = ((MAC_CONF_GROUP as u32) << 8) | 0x05;
const RESP_MAC: u32 = ((MAC_CONF_GROUP as u32) << 8) | 0x08;
const RESP_LINK: u32 = ((MAC_CONF_GROUP as u32) << 8) | 0x09;
const RESP_STA: u32 = ((MAC_CONF_GROUP as u32) << 8) | 0x0a;
const RESP_STA_REMOVE: u32 = ((MAC_CONF_GROUP as u32) << 8) | 0x0c;
const RESP_SCAN_CFG: u32 = ((LONG_GROUP as u32) << 8) | 0x0c;
const RESP_SCAN_REQ: u32 = ((LONG_GROUP as u32) << 8) | 0x0d;
const RESP_SCAN_ABORT: u32 = ((LONG_GROUP as u32) << 8) | 0x0e;
const RESP_NVM_INFO: u32 = ((REGULATORY_NVM_GROUP as u32) << 8) | 0x02;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareEvent<'a> {
    RxPhy(&'a [u8]),
    RxMpdu(&'a [u8]),
    TxStatus(&'a [u8]),
    BlockAck(&'a [u8]),
    BarRelease(&'a [u8]),
    FrameRelease(&'a [u8]),
    Alive(&'a [u8]),
    InitComplete,
    ScanComplete(&'a [u8]),
    PnvmComplete,
    CommandResponse(&'a RxPacket<'a>),
    Unknown { command_id: u32 },
}

/// Known direct command replies from the source switch that are ACKed by iwx_rx_pkt().
fn is_command_response(id: u32) -> bool {
    matches!(
        id,
        RESP_BAID
            | RESP_QUEUE
            | RESP_SEC_KEY
            | RESP_SESSION
            | RESP_MAC
            | RESP_LINK
            | RESP_STA
            | RESP_STA_REMOVE
            | RESP_NVM_INFO
            | 0x0017
            | 0x0018
            | 0x001f
            | 0x006a
            | 0x0098
            | 0x0028
            | 0x00d1
            | 0x0077
            | 0x00ee
            | 0x00c9
            | 0x00c8
            | 0x00a9
            | 0x002c
            | 0x0019
            | 0x001e
            | 0x009b
            | 0x0029
            | 0x009c
            | 0x001d
            | RESP_SCAN_CFG
            | RESP_SCAN_REQ
            | RESP_SCAN_ABORT
            | 0x01d2
            | MCAST_FILTER
            | DATA_PATH_DQA
            | DATA_PATH_TLC
            | DATA_PATH_RLC
            | DATA_PATH_NO_DATA
            | DATA_PATH_DUAL_CHAIN
            | DATA_PATH_TLC_UPDATE
            | REGULATORY_NVM_ACCESS_COMPLETE
            | SYSTEM_SOC_CONFIG
            | SYSTEM_STATS_CMD
            | SYSTEM_STATS_END
            | SYSTEM_STATS_OPER
            | SYSTEM_STATS_PART1
            | FSEQ_MISMATCH
            | PHY_DTS_WIDE
            | PHY_TEMP_THRESH
            | CT_KILL
            | CHANNEL_SWITCH
            | SESSION_PROTECTION_NOTIF
            | UAPSD_MISBEHAVING
            | TIME_EVENT_NOTIFICATION
            | MISSED_BEACONS
            | MFUART_LOAD
            | DTS_MEASUREMENT
            | DEBUG_LOG
            | BT_PROFILE
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverFirmwareEvent<'a> {
    Core(FirmwareEvent<'a>),
    MissedBeacons(&'a [u8]),
    MfuartLoad,
    Statistics(&'a [u8]),
    DtsMeasurement(&'a [u8]),
    CriticalTemperature(&'a [u8]),
    DirectCommandResponse(&'a RxPacket<'a>),
    MccChubUpdate(&'a [u8]),
    FirmwareError(&'a [u8]),
    TimeEvent(&'a [u8]),
    UapsdMisbehaving(&'a [u8]),
    SessionProtection(&'a [u8]),
    ChannelSwitch,
    SystemStatisticsEnd(&'a [u8]),
    SystemStatistics(&'a [u8]),
    FirmwareSequenceMismatch,
    DebugLog,
    MulticastFilter,
    DataPathIgnored(u32),
    Unknown { command_id: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPolicyError {
    TruncatedPayload,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPolicyAction {
    ScheduleInit,
    EndTimeEvent,
    DisableUapsd {
        station_id: u32,
        power_level: Option<u8>,
    },
    SystemStatisticsCleared,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwarePolicyState {
    pub hardware_error: bool,
    pub shutdown: bool,
    pub station_mode: bool,
    pub run_state: bool,
    pub time_event_active: bool,
    pub time_event_uid: u32,
    pub uapsd_enabled: bool,
    pub power_management_enabled: bool,
    pub system_statistics_cleared: bool,
}

/// Apply the state changes and deferred work caused by non-packet firmware notifications.
// upstream: if_iwx.c iwx_rx_pkt() notification handlers
pub fn apply_event_policy(
    event: DriverFirmwareEvent<'_>,
    state: &mut FirmwarePolicyState,
) -> Result<Option<EventPolicyAction>, EventPolicyError> {
    match event {
        DriverFirmwareEvent::CriticalTemperature(payload) => {
            if payload.len() < 2 {
                return Err(EventPolicyError::TruncatedPayload);
            }
            state.hardware_error = true;
            Ok(Some(EventPolicyAction::ScheduleInit))
        }
        DriverFirmwareEvent::TimeEvent(payload) => {
            if payload.len() < 20 {
                return Err(EventPolicyError::TruncatedPayload);
            }
            let uid = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            let action = u32::from_le_bytes(payload[16..20].try_into().unwrap());
            if uid == state.time_event_uid && action & (1 << 1) != 0 {
                state.time_event_active = false;
                Ok(Some(EventPolicyAction::EndTimeEvent))
            } else {
                Ok(None)
            }
        }
        DriverFirmwareEvent::UapsdMisbehaving(payload) => {
            if payload.len() < 4 {
                return Err(EventPolicyError::TruncatedPayload);
            }
            if !state.uapsd_enabled {
                return Ok(None);
            }
            state.uapsd_enabled = false;
            Ok(Some(EventPolicyAction::DisableUapsd {
                station_id: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                power_level: state.power_management_enabled.then_some(3),
            }))
        }
        DriverFirmwareEvent::SessionProtection(payload) => {
            if payload.len() < 16 {
                return Err(EventPolicyError::TruncatedPayload);
            }
            let status = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            let start = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            let conf_id = u32::from_le_bytes(payload[12..16].try_into().unwrap());
            if status == 1 && start == 0 && conf_id == 0 {
                state.time_event_active = false;
                Ok(Some(EventPolicyAction::EndTimeEvent))
            } else {
                Ok(None)
            }
        }
        DriverFirmwareEvent::ChannelSwitch
            if state.station_mode && state.run_state && !state.shutdown =>
        {
            Ok(Some(EventPolicyAction::ScheduleInit))
        }
        DriverFirmwareEvent::SystemStatisticsEnd(_) => {
            state.system_statistics_cleared = true;
            Ok(Some(EventPolicyAction::SystemStatisticsCleared))
        }
        _ => Ok(None),
    }
}

/// Classify every event branch in iwx_rx_pkt(), including lifecycle notifications.
// upstream: if_iwx.c iwx_rx_pkt() notification switch
pub fn decode_driver_event<'a>(packet: &'a RxPacket<'a>) -> DriverFirmwareEvent<'a> {
    let id = packet.command_id();
    match id {
        MISSED_BEACONS | MISSED_BEACONS_WIDE => DriverFirmwareEvent::MissedBeacons(packet.payload),
        MFUART_LOAD => DriverFirmwareEvent::MfuartLoad,
        STATISTICS => DriverFirmwareEvent::Statistics(packet.payload),
        DTS_MEASUREMENT | PHY_DTS_WIDE | PHY_TEMP_THRESH => {
            DriverFirmwareEvent::DtsMeasurement(packet.payload)
        }
        CT_KILL => DriverFirmwareEvent::CriticalTemperature(packet.payload),
        MCC_CHUB_UPDATE => DriverFirmwareEvent::MccChubUpdate(packet.payload),
        FIRMWARE_ERROR => DriverFirmwareEvent::FirmwareError(packet.payload),
        TIME_EVENT_NOTIFICATION => DriverFirmwareEvent::TimeEvent(packet.payload),
        UAPSD_MISBEHAVING => DriverFirmwareEvent::UapsdMisbehaving(packet.payload),
        SESSION_PROTECTION_NOTIF => DriverFirmwareEvent::SessionProtection(packet.payload),
        CHANNEL_SWITCH => DriverFirmwareEvent::ChannelSwitch,
        SYSTEM_STATS_END => DriverFirmwareEvent::SystemStatisticsEnd(packet.payload),
        SYSTEM_STATS_OPER | SYSTEM_STATS_PART1 => {
            DriverFirmwareEvent::SystemStatistics(packet.payload)
        }
        FSEQ_MISMATCH => DriverFirmwareEvent::FirmwareSequenceMismatch,
        DEBUG_LOG => DriverFirmwareEvent::DebugLog,
        MCAST_FILTER => DriverFirmwareEvent::MulticastFilter,
        DATA_PATH_DQA | DATA_PATH_TLC | DATA_PATH_RLC | DATA_PATH_NO_DATA
        | DATA_PATH_DUAL_CHAIN | DATA_PATH_TLC_UPDATE => DriverFirmwareEvent::DataPathIgnored(id),
        id if is_command_response(id) => DriverFirmwareEvent::DirectCommandResponse(packet),
        _ => match decode_firmware_event(packet) {
            FirmwareEvent::Unknown { command_id } => DriverFirmwareEvent::Unknown { command_id },
            core => DriverFirmwareEvent::Core(core),
        },
    }
}

/// Classify the command switch entries that affect RX, firmware, scans or command waiters.
// upstream: if_iwx.c iwx_rx_pkt() notification switch
pub fn decode_firmware_event<'a>(packet: &'a RxPacket<'a>) -> FirmwareEvent<'a> {
    match packet.command_id() {
        RX_PHY => FirmwareEvent::RxPhy(packet.payload),
        RX_MPDU => FirmwareEvent::RxMpdu(packet.payload),
        TX_CMD => FirmwareEvent::TxStatus(packet.payload),
        BA_NOTIF => FirmwareEvent::BlockAck(packet.payload),
        BAR_FRAME_RELEASE => FirmwareEvent::BarRelease(packet.payload),
        FRAME_RELEASE => FirmwareEvent::FrameRelease(packet.payload),
        ALIVE => FirmwareEvent::Alive(packet.payload),
        INIT_COMPLETE => FirmwareEvent::InitComplete,
        UMAC_SCAN_COMPLETE => FirmwareEvent::ScanComplete(packet.payload),
        UMAC_SCAN_ITERATION_COMPLETE => FirmwareEvent::ScanComplete(packet.payload),
        PNVM_COMPLETE => FirmwareEvent::PnvmComplete,
        id if is_command_response(id) => FirmwareEvent::CommandResponse(packet),
        id => FirmwareEvent::Unknown { command_id: id },
    }
}

/// Store a known command reply and retire its descriptor if the event is a response.
// upstream: if_iwx.c iwx_rx_pkt() response copy and iwx_cmd_done() dispatch
pub fn process_command_response(
    packet: &RxPacket<'_>,
    generation: u32,
    slots: &mut CommandSlots,
) -> Result<bool, CommandError> {
    if !is_command_response(packet.command_id()) {
        return Ok(false);
    }
    if packet.total_bytes < crate::RX_PACKET_HEADER_BYTES {
        return Err(CommandError::InvalidResponse);
    }
    let queue = packet.command_queue_id();
    if packet.is_notification() {
        return Ok(false);
    }
    match slots.receive_response(
        queue,
        usize::from(packet.index),
        generation,
        packet.payload,
        packet.command_failed,
    ) {
        Ok(()) | Err(CommandError::NoResponseSlot) | Err(CommandError::InvalidResponse) => {}
        Err(error) => return Err(error),
    }
    slots.command_done(queue, usize::from(packet.index), generation)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::{CMD_WANT_RESPONSE, parse_rx_packet};

    fn raw(opcode: u8, group: u8, index: u8, queue: u8, payload: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&((4 + payload.len()) as u32).to_le_bytes());
        data.extend_from_slice(&[opcode, group, index, queue]);
        data.extend_from_slice(payload);
        data
    }

    #[test]
    fn receive_switch_classifies_core_device_and_station_events() {
        let bytes = raw(0xc0, 0, 0, 0x80, &[1, 2]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert_eq!(
            decode_firmware_event(&packet),
            FirmwareEvent::RxPhy(&[1, 2])
        );
        let bytes = raw(0x0f, LONG_GROUP, 0, 0x80, &[]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_firmware_event(&packet),
            FirmwareEvent::ScanComplete(_)
        ));
        let bytes = raw(0xfe, REGULATORY_NVM_GROUP, 0, 0x80, &[]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert_eq!(decode_firmware_event(&packet), FirmwareEvent::PnvmComplete);
        let bytes = raw(0x44, 0x0f, 0, 0x80, &[]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert_eq!(
            decode_firmware_event(&packet),
            FirmwareEvent::Unknown { command_id: 0x0f44 }
        );
    }

    #[test]
    fn direct_command_response_is_saved_before_ack_and_notification_is_not_retired() {
        let mut slots = CommandSlots::new(0, 9);
        slots.reserve(3, 9, CMD_WANT_RESPONSE, 16, false).unwrap();
        let payload = [8, 7, 6, 5];
        let bytes = raw(0x17, DATA_PATH_GROUP, 3, 0, &payload);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(process_command_response(&packet, 9, &mut slots).unwrap());
        let complete = slots.take_completed(3, 9).unwrap();
        assert_eq!(complete.response, Some(payload.to_vec()));

        slots.reserve(4, 9, 0, 0, false).unwrap();
        let bytes = raw(0x17, DATA_PATH_GROUP, 4, 0x80, &[]);
        let notification = parse_rx_packet(&bytes, false).unwrap();
        assert!(!process_command_response(&notification, 9, &mut slots).unwrap());
        assert_eq!(slots.queued(), 1);
    }

    #[test]
    fn full_iwx_switch_classifies_regulatory_temperature_roam_and_system_notifications() {
        let bytes = raw(0x78, 0, 0, 0x80, &[1]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_driver_event(&packet),
            DriverFirmwareEvent::UapsdMisbehaving(_)
        ));
        let bytes = raw(0xfe, 4, 0, 0x80, &[25, 0]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_driver_event(&packet),
            DriverFirmwareEvent::CriticalTemperature(_)
        ));
        let bytes = raw(0xfb, 3, 0, 0x80, &[0; 12]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_driver_event(&packet),
            DriverFirmwareEvent::SessionProtection(_)
        ));
        let bytes = raw(0xfd, 2, 0, 0x80, &[]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_driver_event(&packet),
            DriverFirmwareEvent::SystemStatisticsEnd(_)
        ));
        let bytes = raw(0x01, 0x10, 0, 0x80, &[]);
        let packet = parse_rx_packet(&bytes, false).unwrap();
        assert!(matches!(
            decode_driver_event(&packet),
            DriverFirmwareEvent::SystemStatistics(_)
        ));
    }

    #[test]
    fn notification_policy_updates_kill_uapsd_session_and_statistics_state() {
        let mut state = FirmwarePolicyState {
            hardware_error: false,
            shutdown: false,
            station_mode: true,
            run_state: true,
            time_event_active: true,
            time_event_uid: 7,
            uapsd_enabled: true,
            power_management_enabled: true,
            system_statistics_cleared: false,
        };
        assert_eq!(
            apply_event_policy(
                DriverFirmwareEvent::CriticalTemperature(&[90, 0]),
                &mut state
            ),
            Ok(Some(EventPolicyAction::ScheduleInit))
        );
        assert!(state.hardware_error);
        let mut time_event = [0u8; 24];
        time_event[8..12].copy_from_slice(&7u32.to_le_bytes());
        time_event[16..20].copy_from_slice(&(1u32 << 1).to_le_bytes());
        assert_eq!(
            apply_event_policy(DriverFirmwareEvent::TimeEvent(&time_event), &mut state),
            Ok(Some(EventPolicyAction::EndTimeEvent))
        );
        let mut uapsd = [0u8; 8];
        uapsd[..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(
            apply_event_policy(DriverFirmwareEvent::UapsdMisbehaving(&uapsd), &mut state),
            Ok(Some(EventPolicyAction::DisableUapsd {
                station_id: 3,
                power_level: Some(3)
            }))
        );
        assert!(!state.uapsd_enabled);
        assert_eq!(
            apply_event_policy(DriverFirmwareEvent::ChannelSwitch, &mut state),
            Ok(Some(EventPolicyAction::ScheduleInit))
        );
        assert_eq!(
            apply_event_policy(DriverFirmwareEvent::SystemStatisticsEnd(&[]), &mut state),
            Ok(Some(EventPolicyAction::SystemStatisticsCleared))
        );
        assert!(state.system_statistics_cleared);
    }
}
