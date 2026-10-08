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
    )
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
}
