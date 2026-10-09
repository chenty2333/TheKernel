//! Management subtype dispatch from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

const MAC_HEADER_LEN: usize = 24;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const SUBTYPE_ASSOC_REQ: u8 = 0x00;
const SUBTYPE_ASSOC_RESP: u8 = 0x10;
const SUBTYPE_REASSOC_REQ: u8 = 0x20;
const SUBTYPE_REASSOC_RESP: u8 = 0x30;
const SUBTYPE_PROBE_REQ: u8 = 0x40;
const SUBTYPE_PROBE_RESP: u8 = 0x50;
const SUBTYPE_BEACON: u8 = 0x80;
const SUBTYPE_DISASSOC: u8 = 0xa0;
const SUBTYPE_AUTH: u8 = 0xb0;
const SUBTYPE_DEAUTH: u8 = 0xc0;
const SUBTYPE_ACTION: u8 = 0xd0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagementRxKind {
    Beacon,
    ProbeResponse,
    ProbeRequest,
    Authentication,
    AssociationRequest,
    ReassociationRequest,
    AssociationResponse,
    ReassociationResponse,
    Deauthentication,
    Disassociation,
    Action,
    Unhandled { subtype: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagementRxError {
    ShortFrame,
    NotManagementFrame,
}

/// Select the OpenBSD management receive path by frame-control subtype.
// upstream: ieee80211_input.c ieee80211_recv_mgmt()
pub fn receive_management_kind(frame: &[u8]) -> Result<ManagementRxKind, ManagementRxError> {
    if frame.len() < MAC_HEADER_LEN {
        return Err(ManagementRxError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT {
        return Err(ManagementRxError::NotManagementFrame);
    }
    let subtype = frame[0] & FC0_SUBTYPE_MASK;
    Ok(match subtype {
        SUBTYPE_BEACON => ManagementRxKind::Beacon,
        SUBTYPE_PROBE_RESP => ManagementRxKind::ProbeResponse,
        SUBTYPE_PROBE_REQ => ManagementRxKind::ProbeRequest,
        SUBTYPE_AUTH => ManagementRxKind::Authentication,
        SUBTYPE_ASSOC_REQ => ManagementRxKind::AssociationRequest,
        SUBTYPE_REASSOC_REQ => ManagementRxKind::ReassociationRequest,
        SUBTYPE_ASSOC_RESP => ManagementRxKind::AssociationResponse,
        SUBTYPE_REASSOC_RESP => ManagementRxKind::ReassociationResponse,
        SUBTYPE_DEAUTH => ManagementRxKind::Deauthentication,
        SUBTYPE_DISASSOC => ManagementRxKind::Disassociation,
        SUBTYPE_ACTION => ManagementRxKind::Action,
        _ => ManagementRxKind::Unhandled { subtype },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_station_management_frames_to_typed_handlers() {
        for (subtype, expected) in [
            (SUBTYPE_BEACON, ManagementRxKind::Beacon),
            (SUBTYPE_PROBE_RESP, ManagementRxKind::ProbeResponse),
            (SUBTYPE_AUTH, ManagementRxKind::Authentication),
            (SUBTYPE_ASSOC_RESP, ManagementRxKind::AssociationResponse),
            (
                SUBTYPE_REASSOC_RESP,
                ManagementRxKind::ReassociationResponse,
            ),
            (SUBTYPE_DEAUTH, ManagementRxKind::Deauthentication),
            (SUBTYPE_DISASSOC, ManagementRxKind::Disassociation),
            (SUBTYPE_ACTION, ManagementRxKind::Action),
        ] {
            let mut frame = alloc::vec![0; MAC_HEADER_LEN];
            frame[0] = subtype;
            assert_eq!(receive_management_kind(&frame), Ok(expected));
        }
    }

    #[test]
    fn hostap_request_subtypes_and_unhandled_are_explicit() {
        for (subtype, expected) in [
            (SUBTYPE_PROBE_REQ, ManagementRxKind::ProbeRequest),
            (SUBTYPE_ASSOC_REQ, ManagementRxKind::AssociationRequest),
            (SUBTYPE_REASSOC_REQ, ManagementRxKind::ReassociationRequest),
        ] {
            let mut frame = alloc::vec![0; MAC_HEADER_LEN];
            frame[0] = subtype;
            assert_eq!(receive_management_kind(&frame), Ok(expected));
        }
        let mut frame = alloc::vec![0; MAC_HEADER_LEN];
        frame[0] = 0xe0;
        assert_eq!(
            receive_management_kind(&frame),
            Ok(ManagementRxKind::Unhandled { subtype: 0xe0 })
        );
    }

    #[test]
    fn rejects_short_or_wrong_frame_type() {
        assert_eq!(
            receive_management_kind(&[0; MAC_HEADER_LEN - 1]),
            Err(ManagementRxError::ShortFrame)
        );
        let mut data = alloc::vec![0; MAC_HEADER_LEN];
        data[0] = 0x08;
        assert_eq!(
            receive_management_kind(&data),
            Err(ManagementRxError::NotManagementFrame)
        );
    }
}
