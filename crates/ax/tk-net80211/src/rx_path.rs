//! Station data receive pipeline from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263,
//! `ieee80211_input.h` rev 1.59, and `ieee80211.h` rev 1.137 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

use crate::{
    DecapError, EthernetFrame, NodeTable, ProtocolState, decap_amsdu, decap_data, has_qos_control,
    has_sequence_control, header_length, qos_control,
};

const FRAME_MIN_LEN: usize = 16;
const FC0_VERSION_MASK: u8 = 0x03;
const FC0_VERSION_0: u8 = 0;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_DATA: u8 = 0x08;
const FC0_TYPE_CTL: u8 = 0x04;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_NODATA: u8 = 0x40;
const FC1_DIR_MASK: u8 = 0x03;
const FC1_DIR_FROMDS: u8 = 0x02;
const FC1_RETRY: u8 = 0x08;
const FC1_PROTECTED: u8 = 0x40;
const QOS_TID_MASK: u16 = 0x000f;
const QOS_ACK_POLICY_MASK: u16 = 0x0060;
const QOS_ACK_POLICY_BA: u16 = 0x0060;
const QOS_ACK_POLICY_NORMAL: u16 = 0;
const QOS_AMSDU: u16 = 0x0080;
const TID_COUNT: usize = 16;
const RXI_HWDEC: u32 = 1 << 0;
const RXI_AMPDU_DONE: u32 = 1 << 1;
const RXI_SAME_SEQ: u32 = 1 << 3;
const BA_STATE_REQUESTED: u8 = 1;
const BA_STATE_AGREED: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxDropReason {
    MonitorHandled,
    ShortFrame,
    BadVersion,
    HeaderLength,
    ExplicitBaWithoutAgreement { tid: u8 },
    Fragment,
    Duplicate,
    WrongDirection,
    WrongBss,
    MulticastEcho,
    NoData,
    Unencrypted,
    EncryptedWithoutProtection,
    NeedsSoftwareDecrypt,
    Decapsulation(DecapError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxDataPolicy {
    pub state: ProtocolState,
    pub monitor_mode: bool,
    pub current_bssid: [u8; 6],
    pub interface_address: [u8; 6],
    pub simplex: bool,
    pub wep_enabled: bool,
    pub rsn_rx_protected: bool,
    pub peer_ht: bool,
    pub ampdu_done: bool,
    pub hardware_decrypted: bool,
    pub same_sequence: bool,
    pub rx_ba_states: [u8; TID_COUNT],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RxDataResult {
    Drop(RxDropReason),
    Reorder {
        tid: u8,
    },
    Ethernet {
        frames: alloc::vec::Vec<EthernetFrame>,
        tid: u8,
        amsdu: bool,
        check_rssi: bool,
    },
}

/// Process one station-mode data MPDU through duplicate, protection and decap stages.
// upstream: ieee80211_input.c ieee80211_inputm()
pub fn receive_station_data(
    table: &mut NodeTable,
    frame: &[u8],
    rssi: u8,
    receive_timestamp: u64,
    rx_flags: u32,
    policy: RxDataPolicy,
) -> RxDataResult {
    if policy.monitor_mode {
        return RxDataResult::Drop(RxDropReason::MonitorHandled);
    }
    if frame.len() < FRAME_MIN_LEN {
        return RxDataResult::Drop(RxDropReason::ShortFrame);
    }
    let fc0 = frame[0];
    let fc1 = frame[1];
    if fc0 & FC0_VERSION_MASK != FC0_VERSION_0 {
        return RxDataResult::Drop(RxDropReason::BadVersion);
    }
    let kind = fc0 & FC0_TYPE_MASK;
    let subtype = fc0 & FC0_SUBTYPE_MASK;
    let is_data = kind == FC0_TYPE_DATA;
    let has_qos = is_data && has_qos_control(fc0);
    let qos = if has_qos {
        match qos_control(frame) {
            Ok(value) => value,
            Err(_) => return RxDataResult::Drop(RxDropReason::HeaderLength),
        }
    } else {
        0
    };
    let tid = (qos & QOS_TID_MASK) as u8;
    let header_len = if kind == FC0_TYPE_CTL {
        0
    } else {
        match header_length(frame) {
            Ok(length) => length,
            Err(_) => return RxDataResult::Drop(RxDropReason::HeaderLength),
        }
    };
    let rx_flags = rx_flags
        | if policy.hardware_decrypted {
            RXI_HWDEC
        } else {
            0
        }
        | if policy.ampdu_done { RXI_AMPDU_DONE } else { 0 }
        | if policy.same_sequence {
            RXI_SAME_SEQ
        } else {
            0
        };

    if policy.state == ProtocolState::Run
        && is_data
        && has_qos
        && subtype & FC0_SUBTYPE_NODATA == 0
        && rx_flags & RXI_AMPDU_DONE == 0
    {
        let ba_state = policy.rx_ba_states[usize::from(tid)];
        let ack_policy = qos & QOS_ACK_POLICY_MASK;
        if ack_policy == QOS_ACK_POLICY_BA && ba_state != BA_STATE_AGREED {
            return RxDataResult::Drop(RxDropReason::ExplicitBaWithoutAgreement { tid });
        }
        if ba_state == BA_STATE_AGREED
            && matches!(ack_policy, QOS_ACK_POLICY_BA | QOS_ACK_POLICY_NORMAL)
        {
            return RxDataResult::Reorder { tid };
        }
        if ba_state == BA_STATE_REQUESTED && ack_policy == QOS_ACK_POLICY_NORMAL {
            return RxDataResult::Reorder { tid };
        }
    }

    if has_sequence_control(fc0) {
        let sequence_control = u16::from_le_bytes([frame[22], frame[23]]);
        if fc1 & 0x04 != 0 || sequence_control & 0x000f != 0 {
            return RxDataResult::Drop(RxDropReason::Fragment);
        }
    }
    if has_sequence_control(fc0) && policy.state != ProtocolState::Scan {
        let sequence = u16::from_le_bytes([frame[22], frame[23]]) >> 4;
        let previous = if has_qos {
            &mut table.bss_node.qos_rx_sequences[usize::from(tid)]
        } else {
            &mut table.bss_node.rx_sequence
        };
        if rx_flags & RXI_SAME_SEQ != 0 {
            if sequence != *previous {
                return RxDataResult::Drop(RxDropReason::Duplicate);
            }
        } else if fc1 & FC1_RETRY != 0 && sequence == *previous {
            return RxDataResult::Drop(RxDropReason::Duplicate);
        } else {
            *previous = sequence;
        }
    }
    let check_rssi = !matches!(policy.state, ProtocolState::Init | ProtocolState::Scan);
    if check_rssi {
        if rssi != 0 {
            table.bss_node.access_point.rssi = rssi;
        }
        table.bss_node.receive_timestamp = receive_timestamp;
    }
    if !is_data {
        return RxDataResult::Drop(RxDropReason::NoData);
    }
    if fc1 & FC1_DIR_MASK != FC1_DIR_FROMDS {
        return RxDataResult::Drop(RxDropReason::WrongDirection);
    }
    if policy.state != ProtocolState::Scan {
        if frame[10..16] != policy.current_bssid {
            return RxDataResult::Drop(RxDropReason::WrongBss);
        }
    }
    let address1_multicast = frame[4] & 1 != 0;
    let address3 = &frame[16..22];
    if policy.simplex && address1_multicast && address3 == policy.interface_address {
        return RxDataResult::Drop(RxDropReason::MulticastEcho);
    }
    if subtype & FC0_SUBTYPE_NODATA != 0 {
        return RxDataResult::Drop(RxDropReason::NoData);
    }
    let protected = fc1 & FC1_PROTECTED != 0;
    let rx_protection = policy.wep_enabled || policy.rsn_rx_protected;
    if rx_protection {
        if rx_flags & RXI_HWDEC == 0 {
            if !protected {
                return RxDataResult::Drop(RxDropReason::Unencrypted);
            }
            return RxDataResult::Drop(RxDropReason::NeedsSoftwareDecrypt);
        }
    } else if protected || rx_flags & RXI_HWDEC != 0 {
        return RxDataResult::Drop(RxDropReason::EncryptedWithoutProtection);
    }
    let amsdu = policy.peer_ht && has_qos && qos & QOS_AMSDU != 0;
    let frames = if amsdu {
        match decap_amsdu(&frame[header_len..], policy.interface_address) {
            Ok(frames) => frames,
            Err(error) => return RxDataResult::Drop(RxDropReason::Decapsulation(error)),
        }
    } else {
        match decap_data(frame) {
            Ok(frame) => alloc::vec![frame],
            Err(error) => return RxDataResult::Drop(RxDropReason::Decapsulation(error)),
        }
    };
    RxDataResult::Ethernet {
        frames,
        tid,
        amsdu,
        check_rssi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RxDataPolicy {
        RxDataPolicy {
            state: ProtocolState::Run,
            monitor_mode: false,
            current_bssid: [2, 0, 0, 0, 0, 1],
            interface_address: [2, 0, 0, 0, 0, 2],
            simplex: false,
            wep_enabled: false,
            rsn_rx_protected: false,
            peer_ht: false,
            ampdu_done: true,
            hardware_decrypted: false,
            same_sequence: false,
            rx_ba_states: [0; TID_COUNT],
        }
    }

    fn data_frame(qos: bool) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0x08 | if qos { 0x80 } else { 0 }, 0x02];
        frame.extend_from_slice(&[0; 2]);
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 2]);
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
        frame.extend_from_slice(&[2, 0, 0, 0, 0, 3]);
        frame.extend_from_slice(&[0x10, 0]);
        if qos {
            frame.extend_from_slice(&[0, 0]);
        }
        frame.extend_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 0x08, 0]);
        frame.extend_from_slice(b"rx");
        frame
    }

    #[test]
    fn station_from_ds_data_is_deduplicated_and_decapsulated() {
        let mut table = NodeTable::default();
        let frame = data_frame(false);
        let first = receive_station_data(&mut table, &frame, 40, 11, 0, policy());
        assert!(matches!(first, RxDataResult::Ethernet { .. }));
        let mut retry = frame.clone();
        retry[1] |= FC1_RETRY;
        let duplicate = receive_station_data(&mut table, &retry, 40, 12, 0, policy());
        assert_eq!(duplicate, RxDataResult::Drop(RxDropReason::Duplicate));
        assert_eq!(table.bss_node.receive_timestamp, 11);
    }

    #[test]
    fn qos_ba_policy_routes_to_reorder_or_rejects_unknown_agreement() {
        let mut policy = policy();
        policy.ampdu_done = false;
        let mut frame = data_frame(true);
        frame[24] = QOS_ACK_POLICY_BA as u8;
        assert_eq!(
            receive_station_data(&mut NodeTable::default(), &frame, 0, 0, 0, policy),
            RxDataResult::Drop(RxDropReason::ExplicitBaWithoutAgreement { tid: 0 })
        );
        policy.rx_ba_states[0] = BA_STATE_AGREED;
        assert_eq!(
            receive_station_data(&mut NodeTable::default(), &frame, 0, 0, 0, policy),
            RxDataResult::Reorder { tid: 0 }
        );
    }

    #[test]
    fn rejects_fragments_wrong_bss_and_privacy_mismatch() {
        let mut table = NodeTable::default();
        let mut frame = data_frame(false);
        frame[1] |= 0x04;
        assert_eq!(
            receive_station_data(&mut table, &frame, 0, 0, 0, policy()),
            RxDataResult::Drop(RxDropReason::Fragment)
        );
        frame[1] &= !0x04;
        frame[10] = 4;
        assert_eq!(
            receive_station_data(&mut table, &frame, 0, 0, 0, policy()),
            RxDataResult::Drop(RxDropReason::WrongBss)
        );
        frame[10..16].copy_from_slice(&policy().current_bssid);
        frame[1] |= FC1_PROTECTED;
        assert_eq!(
            receive_station_data(&mut table, &frame, 0, 0, 0, policy()),
            RxDataResult::Drop(RxDropReason::EncryptedWithoutProtection)
        );
    }

    #[test]
    fn negotiated_ht_amsdu_splits_subframes_and_validates_station_da() {
        let mut policy = policy();
        policy.peer_ht = true;
        let mut frame = data_frame(true);
        frame[24] = QOS_AMSDU as u8;
        frame.truncate(26);
        let mut msdu = alloc::vec![0xaa, 0xaa, 3, 0, 0, 0, 0x08, 0x00, 9, 8];
        let mut aggregate = alloc::vec![2, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 3];
        aggregate.extend_from_slice(&(msdu.len() as u16).to_be_bytes());
        aggregate.append(&mut msdu);
        frame.extend_from_slice(&aggregate);
        let result = receive_station_data(&mut NodeTable::default(), &frame, 0, 0, 0, policy);
        assert!(matches!(
            result,
            RxDataResult::Ethernet { ref frames, amsdu: true, .. }
                if frames.len() == 1 && frames[0].ether_type == 0x0800 && frames[0].payload == [9, 8]
        ));
    }
}
