//! Station authentication response receive path from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263 and
//! `ieee80211_proto.c` rev 1.176 (BSD-3-Clause). Copyright (c) 2001 Atsushi
//! Onoe; Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright
//! (c) 2007-2009 Damien Bergamini.

use crate::{OpenAuthEffects, OpenAuthState, auth_open_station};

const MAC_HEADER_LEN: usize = 24;
const AUTH_FIXED_LEN: usize = 6;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_AUTH: u8 = 0xb0;
const AUTH_ALG_OPEN: u16 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRxError {
    ShortFrame,
    NotAuthentication,
    UnsupportedAlgorithm(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthRxResult {
    pub algorithm: u16,
    pub sequence: u16,
    pub status: u16,
    pub effects: OpenAuthEffects,
}

/// Decode Open System authentication management frames and apply station state.
// upstream: ieee80211_input.c ieee80211_recv_auth()
pub fn receive_auth_response(
    frame: &[u8],
    state: &mut OpenAuthState,
) -> Result<AuthRxResult, AuthRxError> {
    if frame.len() < MAC_HEADER_LEN + AUTH_FIXED_LEN {
        return Err(AuthRxError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT || frame[0] & FC0_SUBTYPE_MASK != FC0_SUBTYPE_AUTH {
        return Err(AuthRxError::NotAuthentication);
    }
    let body = &frame[MAC_HEADER_LEN..MAC_HEADER_LEN + AUTH_FIXED_LEN];
    let algorithm = u16::from_le_bytes([body[0], body[1]]);
    let sequence = u16::from_le_bytes([body[2], body[3]]);
    let status = u16::from_le_bytes([body[4], body[5]]);
    if algorithm != AUTH_ALG_OPEN {
        return Err(AuthRxError::UnsupportedAlgorithm(algorithm));
    }
    state.sequence = sequence;
    state.status = status;
    state.auth_subtype = frame[0] & FC0_SUBTYPE_MASK;
    let effects = auth_open_station(state);
    Ok(AuthRxResult {
        algorithm,
        sequence,
        status,
        effects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtocolState;

    fn frame(algorithm: u16, sequence: u16, status: u16) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0; MAC_HEADER_LEN + AUTH_FIXED_LEN];
        frame[0] = FC0_SUBTYPE_AUTH;
        frame[MAC_HEADER_LEN..MAC_HEADER_LEN + 2].copy_from_slice(&algorithm.to_le_bytes());
        frame[MAC_HEADER_LEN + 2..MAC_HEADER_LEN + 4].copy_from_slice(&sequence.to_le_bytes());
        frame[MAC_HEADER_LEN + 4..MAC_HEADER_LEN + 6].copy_from_slice(&status.to_le_bytes());
        frame
    }

    fn state() -> OpenAuthState {
        OpenAuthState {
            authentication_state: true,
            rsn_enabled: true,
            is_bss_node: true,
            sequence: 0,
            status: 0,
            auth_subtype: 0,
            node_failures: 0,
            bad_auth_count: 0,
            auth_fail_count: 0,
        }
    }

    #[test]
    fn parses_open_auth_and_returns_station_transition_effects() {
        let mut state = state();
        let response = receive_auth_response(&frame(AUTH_ALG_OPEN, 2, 0), &mut state).unwrap();
        assert_eq!(response.sequence, 2);
        assert_eq!(state.sequence, 2);
        assert_eq!(response.effects.new_state, Some(ProtocolState::Assoc));
        assert!(response.effects.delete_pairwise_key);
    }

    #[test]
    fn unsupported_algorithm_is_not_misrouted_as_open_auth() {
        let mut state = state();
        assert_eq!(
            receive_auth_response(&frame(1, 2, 0), &mut state),
            Err(AuthRxError::UnsupportedAlgorithm(1))
        );
        assert_eq!(state.bad_auth_count, 0);
    }

    #[test]
    fn rejects_short_and_non_authentication_frames() {
        let mut state = state();
        assert_eq!(
            receive_auth_response(&[0; MAC_HEADER_LEN + 5], &mut state),
            Err(AuthRxError::ShortFrame)
        );
        assert_eq!(
            receive_auth_response(&frame(0, 2, 0)[..24], &mut state),
            Err(AuthRxError::ShortFrame)
        );
        let mut not_auth = frame(0, 2, 0);
        not_auth[0] = 0x80;
        assert_eq!(
            receive_auth_response(&not_auth, &mut state),
            Err(AuthRxError::NotAuthentication)
        );
    }
}
