//! Station SA Query management receive paths from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

const MAC_HEADER_LEN: usize = 24;
const SA_QUERY_BODY_LEN: usize = 4;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_ACTION: u8 = 0xd0;
const CATEGORY_SA_QUERY: u8 = 8;
const ACTION_SA_QUERY_REQ: u8 = 0;
const ACTION_SA_QUERY_RESP: u8 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaQueryState {
    pub station_mode: bool,
    pub management_frame_protection: bool,
    pub engaged: bool,
    pub transaction_id: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaQueryError {
    ShortFrame,
    NotActionFrame,
    NotSaQuery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaQueryOutcome {
    IgnoreUnexpected,
    SendResponse { transaction_id: u16 },
    MatchedResponse { transaction_id: u16 },
    IgnoreTransactionMismatch { expected: u16, received: u16 },
}

fn transaction_id(frame: &[u8], action: u8) -> Result<u16, SaQueryError> {
    if frame.len() < MAC_HEADER_LEN + SA_QUERY_BODY_LEN {
        return Err(SaQueryError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT || frame[0] & FC0_SUBTYPE_MASK != FC0_SUBTYPE_ACTION
    {
        return Err(SaQueryError::NotActionFrame);
    }
    if frame[MAC_HEADER_LEN] != CATEGORY_SA_QUERY || frame[MAC_HEADER_LEN + 1] != action {
        return Err(SaQueryError::NotSaQuery);
    }
    Ok(u16::from_le_bytes([
        frame[MAC_HEADER_LEN + 2],
        frame[MAC_HEADER_LEN + 3],
    ]))
}

/// Process an incoming station SA Query Request and return its response ID.
// upstream: ieee80211_input.c ieee80211_recv_sa_query_req()
pub fn receive_sa_query_request(
    frame: &[u8],
    state: &mut SaQueryState,
) -> Result<SaQueryOutcome, SaQueryError> {
    if !state.station_mode || !state.management_frame_protection {
        return Ok(SaQueryOutcome::IgnoreUnexpected);
    }
    let transaction_id = transaction_id(frame, ACTION_SA_QUERY_REQ)?;
    state.transaction_id = transaction_id;
    Ok(SaQueryOutcome::SendResponse { transaction_id })
}

/// Process an SA Query Response only when engaged and its transaction ID matches.
// upstream: ieee80211_input.c ieee80211_recv_sa_query_resp()
pub fn receive_sa_query_response(
    frame: &[u8],
    state: &mut SaQueryState,
) -> Result<SaQueryOutcome, SaQueryError> {
    if !state.engaged {
        return Ok(SaQueryOutcome::IgnoreUnexpected);
    }
    let received = transaction_id(frame, ACTION_SA_QUERY_RESP)?;
    if received != state.transaction_id {
        return Ok(SaQueryOutcome::IgnoreTransactionMismatch {
            expected: state.transaction_id,
            received,
        });
    }
    state.engaged = false;
    Ok(SaQueryOutcome::MatchedResponse {
        transaction_id: received,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(action: u8, id: u16) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0; MAC_HEADER_LEN + SA_QUERY_BODY_LEN];
        frame[0] = FC0_SUBTYPE_ACTION;
        frame[MAC_HEADER_LEN] = CATEGORY_SA_QUERY;
        frame[MAC_HEADER_LEN + 1] = action;
        frame[MAC_HEADER_LEN + 2..].copy_from_slice(&id.to_le_bytes());
        frame
    }

    #[test]
    fn station_mfp_request_saves_id_and_requests_response() {
        let mut state = SaQueryState {
            station_mode: true,
            management_frame_protection: true,
            ..Default::default()
        };
        assert_eq!(
            receive_sa_query_request(&frame(ACTION_SA_QUERY_REQ, 0x1234), &mut state),
            Ok(SaQueryOutcome::SendResponse {
                transaction_id: 0x1234
            })
        );
        assert_eq!(state.transaction_id, 0x1234);
    }

    #[test]
    fn response_requires_matching_active_transaction() {
        let mut state = SaQueryState {
            engaged: true,
            transaction_id: 7,
            ..Default::default()
        };
        assert_eq!(
            receive_sa_query_response(&frame(ACTION_SA_QUERY_RESP, 8), &mut state),
            Ok(SaQueryOutcome::IgnoreTransactionMismatch {
                expected: 7,
                received: 8
            })
        );
        assert!(state.engaged);
        assert_eq!(
            receive_sa_query_response(&frame(ACTION_SA_QUERY_RESP, 7), &mut state),
            Ok(SaQueryOutcome::MatchedResponse { transaction_id: 7 })
        );
        assert!(!state.engaged);
    }

    #[test]
    fn ignored_unprotected_requests_are_not_parsed() {
        assert_eq!(
            receive_sa_query_request(&[], &mut SaQueryState::default()),
            Ok(SaQueryOutcome::IgnoreUnexpected)
        );
        assert_eq!(
            receive_sa_query_response(&[], &mut SaQueryState::default()),
            Ok(SaQueryOutcome::IgnoreUnexpected)
        );
    }
}
