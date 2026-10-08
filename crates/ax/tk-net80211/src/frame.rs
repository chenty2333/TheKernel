//! Station data-frame conversion from OpenBSD net80211.
//!
//! Upstream: OpenBSD `sys/net80211/ieee80211_output.c` revision 1.148,
//! `ieee80211_encap()` station-mode path, and `ieee80211_input.c` revision
//! 1.263, `ieee80211_decap()`. Both files use the BSD-3-Clause license.
//! Copyright (c) 2001 Atsushi Onoe
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting
//! Copyright (c) 2007-2009 Damien Bergamini

use alloc::vec::Vec;

/// IEEE 802.11 / Ethernet MAC address.
pub type MacAddress = [u8; 6];

/// Ethernet frame without FCS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetFrame {
    pub destination: MacAddress,
    pub source: MacAddress,
    pub ether_type: u16,
    pub payload: Vec<u8>,
}

/// Why an 802.11 data MPDU could not be converted to Ethernet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecapError {
    Truncated,
    NotData,
    UnsupportedVersion,
    ProtectedFrame,
    NoDistributionSystem,
    InvalidSnap,
}

const FC_TYPE_MASK: u8 = 0x0c;
const FC_TYPE_DATA: u8 = 0x08;
const FC_SUBTYPE_QOS: u8 = 0x80;
const FC1_DIR_MASK: u8 = 0x03;
const FC1_DIR_NODS: u8 = 0x00;
const FC1_DIR_TODS: u8 = 0x01;
const FC1_DIR_FROMDS: u8 = 0x02;
const FC1_DIR_DSTODS: u8 = 0x03;
const FC1_PROTECTED: u8 = 0x40;
const FC1_ORDER: u8 = 0x80;
const IEEE80211_HEADER_LEN: usize = 24;
const LLC_SNAP_LEN: usize = 8;
const ETHERNET_HEADER_LEN: usize = 14;
const LLC_SNAP: [u8; 6] = [0xaa, 0xaa, 0x03, 0, 0, 0];

/// Decapsulate an OpenBSD net80211 input data frame into an Ethernet frame.
// upstream: ieee80211_input.c ieee80211_decap()
pub fn decap_data(frame: &[u8]) -> Result<EthernetFrame, DecapError> {
    if frame.len() < IEEE80211_HEADER_LEN {
        return Err(DecapError::Truncated);
    }
    let fc0 = frame[0];
    let fc1 = frame[1];
    if fc0 & 0x03 != 0 {
        return Err(DecapError::UnsupportedVersion);
    }
    if fc0 & FC_TYPE_MASK != FC_TYPE_DATA {
        return Err(DecapError::NotData);
    }
    if fc1 & FC1_PROTECTED != 0 {
        return Err(DecapError::ProtectedFrame);
    }

    let direction = fc1 & FC1_DIR_MASK;
    let mut header_len = IEEE80211_HEADER_LEN;
    if direction == FC1_DIR_DSTODS {
        header_len += 6;
    }
    let qos = fc0 & FC_SUBTYPE_QOS != 0;
    if qos {
        header_len += 2;
        if fc1 & FC1_ORDER != 0 {
            header_len += 4;
        }
    }
    let header = frame.get(..header_len).ok_or(DecapError::Truncated)?;
    let payload = frame.get(header_len..).ok_or(DecapError::Truncated)?;

    let addr1 = address(header, 4)?;
    let addr2 = address(header, 10)?;
    let addr3 = address(header, 16)?;
    let (destination, source) = match direction {
        FC1_DIR_NODS => (addr1, addr2),
        FC1_DIR_TODS => (addr3, addr2),
        FC1_DIR_FROMDS => (addr1, addr3),
        FC1_DIR_DSTODS => (addr3, address(header, 24)?),
        _ => return Err(DecapError::NoDistributionSystem),
    };

    let (ether_type, body) = if payload.starts_with(&LLC_SNAP) {
        if payload.len() < LLC_SNAP_LEN {
            return Err(DecapError::Truncated);
        }
        (
            u16::from_be_bytes([payload[6], payload[7]]),
            &payload[LLC_SNAP_LEN..],
        )
    } else {
        let length = u16::try_from(payload.len()).map_err(|_| DecapError::InvalidSnap)?;
        (length, payload)
    };
    Ok(EthernetFrame {
        destination,
        source,
        ether_type,
        payload: body.to_vec(),
    })
}

/// Encapsulate an Ethernet frame for an associated station (ToDS).
///
/// `sequence` is the 12-bit non-QoS sequence number; a caller that negotiated
/// QoS can maintain the per-TID counters and use the corresponding upstream
/// QoS data path instead. The `protected` flag selects hardware encryption.
// upstream: ieee80211_output.c ieee80211_encap() station path
pub fn encap_station(
    ethernet: &[u8],
    bssid: MacAddress,
    sequence: u16,
    protected: bool,
) -> Result<Vec<u8>, DecapError> {
    if ethernet.len() < ETHERNET_HEADER_LEN {
        return Err(DecapError::Truncated);
    }
    let mut frame = Vec::with_capacity(IEEE80211_HEADER_LEN + LLC_SNAP_LEN + ethernet.len());
    let destination: MacAddress = ethernet[..6]
        .try_into()
        .map_err(|_| DecapError::Truncated)?;
    let source: MacAddress = ethernet[6..12]
        .try_into()
        .map_err(|_| DecapError::Truncated)?;
    let ether_type = u16::from_be_bytes([ethernet[12], ethernet[13]]);
    frame.extend_from_slice(&[
        FC_TYPE_DATA,
        FC1_DIR_TODS | if protected { FC1_PROTECTED } else { 0 },
    ]);
    frame.extend_from_slice(&[0; 2]); // Duration.
    frame.extend_from_slice(&bssid); // Address 1: receiver/AP.
    frame.extend_from_slice(&source); // Address 2: transmitter/station.
    frame.extend_from_slice(&destination); // Address 3: final destination.
    frame.extend_from_slice(&((sequence & 0x0fff) << 4).to_le_bytes());
    frame.extend_from_slice(&LLC_SNAP);
    frame.extend_from_slice(&ether_type.to_be_bytes());
    frame.extend_from_slice(&ethernet[ETHERNET_HEADER_LEN..]);
    Ok(frame)
}

fn address(frame: &[u8], offset: usize) -> Result<MacAddress, DecapError> {
    frame
        .get(offset..offset + 6)
        .ok_or(DecapError::Truncated)?
        .try_into()
        .map_err(|_| DecapError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ethernet() -> Vec<u8> {
        [
            &[0x10, 0x11, 0x12, 0x13, 0x14, 0x15][..],
            &[0x20, 0x21, 0x22, 0x23, 0x24, 0x25],
            &[0x08, 0x00],
            &[1, 2, 3, 4],
        ]
        .concat()
    }

    #[test]
    fn station_encapsulation_roundtrips_snap_payload() {
        let eth = ethernet();
        let bssid = [0x30, 0x31, 0x32, 0x33, 0x34, 0x35];
        let wifi = encap_station(&eth, bssid, 0x1003, false).unwrap();
        assert_eq!(wifi.len(), 24 + 8 + 4);
        assert_eq!(&wifi[4..10], &bssid);
        assert_eq!(&wifi[10..16], &eth[6..12]);
        assert_eq!(&wifi[16..22], &eth[..6]);
        assert_eq!(
            decap_data(&wifi).unwrap(),
            EthernetFrame {
                destination: eth[..6].try_into().unwrap(),
                source: eth[6..12].try_into().unwrap(),
                ether_type: 0x0800,
                payload: vec![1, 2, 3, 4],
            }
        );
    }

    #[test]
    fn decapsulation_covers_each_address_direction_and_qos_header() {
        let da = [1, 2, 3, 4, 5, 6];
        let sa = [7, 8, 9, 10, 11, 12];
        let bssid = [13, 14, 15, 16, 17, 18];
        let addr4 = [19, 20, 21, 22, 23, 24];
        for (direction, expected_da, expected_sa, four) in [
            (FC1_DIR_NODS, da, sa, false),
            (FC1_DIR_TODS, bssid, sa, false),
            (FC1_DIR_FROMDS, da, bssid, false),
            (FC1_DIR_DSTODS, bssid, addr4, true),
        ] {
            let mut frame = Vec::new();
            frame.extend_from_slice(&[FC_TYPE_DATA | FC_SUBTYPE_QOS, direction]);
            frame.extend_from_slice(&[0; 2]);
            frame.extend_from_slice(&da);
            frame.extend_from_slice(&sa);
            frame.extend_from_slice(&bssid);
            frame.extend_from_slice(&[0; 2]);
            if four {
                frame.extend_from_slice(&addr4);
            }
            frame.extend_from_slice(&[0; 2]);
            frame.extend_from_slice(&LLC_SNAP);
            frame.extend_from_slice(&[0x08, 0x06, 42]);
            let result = decap_data(&frame).unwrap();
            assert_eq!(result.destination, expected_da);
            assert_eq!(result.source, expected_sa);
            assert_eq!(result.ether_type, 0x0806);
            assert_eq!(result.payload, [42]);
        }
    }

    #[test]
    fn rejects_non_data_protected_and_short_frames() {
        assert_eq!(decap_data(&[0; 23]), Err(DecapError::Truncated));
        let mut frame = encap_station(&ethernet(), [0; 6], 0, false).unwrap();
        frame[0] = 0x00;
        assert_eq!(decap_data(&frame), Err(DecapError::NotData));
        frame[0] = FC_TYPE_DATA;
        frame[1] |= FC1_PROTECTED;
        assert_eq!(decap_data(&frame), Err(DecapError::ProtectedFrame));
    }
}
