//! Deauthentication and disassociation receive paths from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263,
//! `ieee80211_proto.c` rev 1.176, and `ieee80211.h` rev 1.137 (BSD-3-Clause).
//! Copyright (c) 2001 Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler,
//! Errno Consulting; Copyright (c) 2007-2009 Damien Bergamini.

use crate::ProtocolState;

const MAC_HEADER_LEN: usize = 24;
const REASON_LEN: usize = 2;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_DEAUTH: u8 = 0xc0;
const FC0_SUBTYPE_DISASSOC: u8 = 0xa0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RxOperatingMode {
    #[default]
    Station,
    HostAp,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisconnectPolicy {
    pub mode: RxOperatingMode,
    pub state: ProtocolState,
    pub background_scan: bool,
    pub stay_authenticated: bool,
    pub peer_is_bss: bool,
    pub peer_authenticated: bool,
    pub peer_associated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectKind {
    Deauthentication,
    Disassociation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectRxError {
    ShortFrame,
    NotDisconnectManagementFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisconnectRxResult {
    pub kind: DisconnectKind,
    pub reason: u16,
    pub state_transition: Option<ProtocolState>,
    pub leave_peer: bool,
    pub ignored_during_background_scan: bool,
}

fn parse_disconnect(frame: &[u8]) -> Result<(u8, u16), DisconnectRxError> {
    if frame.len() < MAC_HEADER_LEN + REASON_LEN {
        return Err(DisconnectRxError::ShortFrame);
    }
    let subtype = frame[0] & FC0_SUBTYPE_MASK;
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT
        || !matches!(subtype, FC0_SUBTYPE_DEAUTH | FC0_SUBTYPE_DISASSOC)
    {
        return Err(DisconnectRxError::NotDisconnectManagementFrame);
    }
    let reason = u16::from_le_bytes([frame[MAC_HEADER_LEN], frame[MAC_HEADER_LEN + 1]]);
    Ok((subtype, reason))
}

/// Receive a Deauthentication frame and plan source station/AP state changes.
// upstream: ieee80211_input.c ieee80211_recv_deauth()
pub fn receive_deauthentication(
    frame: &[u8],
    policy: DisconnectPolicy,
) -> Result<DisconnectRxResult, DisconnectRxError> {
    let (subtype, reason) = parse_disconnect(frame)?;
    if subtype != FC0_SUBTYPE_DEAUTH {
        return Err(DisconnectRxError::NotDisconnectManagementFrame);
    }
    let bgscan = policy.background_scan && policy.state == ProtocolState::Run;
    let stay_auth = policy.stay_authenticated
        && matches!(
            policy.state,
            ProtocolState::Auth | ProtocolState::Assoc | ProtocolState::Run
        );
    let (state_transition, leave_peer) = match policy.mode {
        RxOperatingMode::Station if !(bgscan || stay_auth) => (Some(ProtocolState::Auth), false),
        RxOperatingMode::HostAp if !policy.peer_is_bss => (
            None,
            !(policy.stay_authenticated && (policy.peer_authenticated || policy.peer_associated)),
        ),
        _ => (None, false),
    };
    Ok(DisconnectRxResult {
        kind: DisconnectKind::Deauthentication,
        reason,
        state_transition,
        leave_peer,
        ignored_during_background_scan: bgscan && policy.mode == RxOperatingMode::Station,
    })
}

/// Receive a Disassociation frame and plan source station/AP state changes.
// upstream: ieee80211_input.c ieee80211_recv_disassoc()
pub fn receive_disassociation(
    frame: &[u8],
    policy: DisconnectPolicy,
) -> Result<DisconnectRxResult, DisconnectRxError> {
    let (subtype, reason) = parse_disconnect(frame)?;
    if subtype != FC0_SUBTYPE_DISASSOC {
        return Err(DisconnectRxError::NotDisconnectManagementFrame);
    }
    let bgscan = policy.background_scan && policy.state == ProtocolState::Run;
    let (state_transition, leave_peer) = match policy.mode {
        RxOperatingMode::Station if !bgscan => (Some(ProtocolState::Assoc), false),
        RxOperatingMode::HostAp if !policy.peer_is_bss => (None, true),
        _ => (None, false),
    };
    Ok(DisconnectRxResult {
        kind: DisconnectKind::Disassociation,
        reason,
        state_transition,
        leave_peer,
        ignored_during_background_scan: bgscan && policy.mode == RxOperatingMode::Station,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(subtype: u8, reason: u16) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0; MAC_HEADER_LEN + REASON_LEN];
        frame[0] = subtype;
        frame[MAC_HEADER_LEN..].copy_from_slice(&reason.to_le_bytes());
        frame
    }

    fn policy(mode: RxOperatingMode, state: ProtocolState) -> DisconnectPolicy {
        DisconnectPolicy {
            mode,
            state,
            background_scan: false,
            stay_authenticated: false,
            peer_is_bss: false,
            peer_authenticated: false,
            peer_associated: false,
        }
    }

    #[test]
    fn station_deauth_and_disassoc_choose_source_states() {
        let deauth = receive_deauthentication(
            &frame(FC0_SUBTYPE_DEAUTH, 3),
            policy(RxOperatingMode::Station, ProtocolState::Run),
        )
        .unwrap();
        assert_eq!(deauth.reason, 3);
        assert_eq!(deauth.state_transition, Some(ProtocolState::Auth));
        let disassoc = receive_disassociation(
            &frame(FC0_SUBTYPE_DISASSOC, 8),
            policy(RxOperatingMode::Station, ProtocolState::Run),
        )
        .unwrap();
        assert_eq!(disassoc.state_transition, Some(ProtocolState::Assoc));
    }

    #[test]
    fn background_scan_and_stay_auth_suppress_deauth_state_change() {
        let mut bgscan = policy(RxOperatingMode::Station, ProtocolState::Run);
        bgscan.background_scan = true;
        let result = receive_deauthentication(&frame(FC0_SUBTYPE_DEAUTH, 1), bgscan).unwrap();
        assert_eq!(result.state_transition, None);
        assert!(result.ignored_during_background_scan);
        let mut stay_auth = policy(RxOperatingMode::Station, ProtocolState::Assoc);
        stay_auth.stay_authenticated = true;
        assert_eq!(
            receive_deauthentication(&frame(FC0_SUBTYPE_DEAUTH, 1), stay_auth)
                .unwrap()
                .state_transition,
            None
        );
    }

    #[test]
    fn hostap_peer_leave_and_wrong_frame_are_explicit() {
        let result = receive_disassociation(
            &frame(FC0_SUBTYPE_DISASSOC, 4),
            policy(RxOperatingMode::HostAp, ProtocolState::Run),
        )
        .unwrap();
        assert!(result.leave_peer);
        assert_eq!(
            receive_disassociation(
                &frame(FC0_SUBTYPE_DEAUTH, 4),
                policy(RxOperatingMode::Station, ProtocolState::Run)
            ),
            Err(DisconnectRxError::NotDisconnectManagementFrame)
        );
    }

    #[test]
    fn rejects_truncated_reason_field() {
        assert_eq!(
            receive_deauthentication(
                &[0; MAC_HEADER_LEN + 1],
                policy(RxOperatingMode::Station, ProtocolState::Auth)
            ),
            Err(DisconnectRxError::ShortFrame)
        );
    }
}
