//! Block Ack negotiation receive helpers from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use alloc::vec::Vec;

const MAC_HEADER_LEN: usize = 24;
const BAR_HEADER_LEN: usize = 16;
const ACTION_FIXED_LEN: usize = 9;
const ACTION_CATEGORY_BA: u8 = 3;
const ACTION_ADDBA_REQ: u8 = 0;
const ACTION_ADDBA_RESP: u8 = 1;
const ACTION_DELBA: u8 = 2;
const CATEGORY_SA_QUERY: u8 = 8;
const ACTION_SA_QUERY_REQ: u8 = 0;
const ACTION_SA_QUERY_RESP: u8 = 1;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_ACTION: u8 = 0xd0;
const BA_STATE_INIT: u8 = 0;
const BA_STATE_REQUESTED: u8 = 1;
const BA_STATE_AGREED: u8 = 2;
const BA_POLICY: u16 = 0x0002;
const BA_AMSDU: u16 = 0x0001;
const DELBA_INITIATOR: u16 = 0x0800;
const STATUS_SUCCESS: u16 = 0;
const STATUS_REFUSED: u16 = 37;
const REASON_SETUP_REQUIRED: u16 = 38;
const DUR_TU_MICROS: u64 = 1024;
const BA_TID_COUNT: usize = 16;
const BA_MAX_WINDOW: u16 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaRxError {
    ShortFrame,
    NotBlockAckAction,
    UnsupportedAction,
    InvalidTid(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionDispatch {
    AddbaRequest,
    AddbaResponse,
    Delba,
    SaQueryRequest,
    SaQueryResponse,
    Ignore { category: u8, action: u8 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaAgreement {
    pub state: u8,
    pub token: u8,
    pub window_start: u16,
    pub window_end: u16,
    pub window_size: u16,
    pub timeout_micros: u64,
    pub request_interval: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddbaRequestPolicy {
    pub run_state: bool,
    pub port_valid: bool,
    pub rsn_enabled: bool,
    pub peer_ht: bool,
    pub background_scan_tx_mgmt_only: bool,
    pub protected_block_ack_required: bool,
    pub peer_mfp: bool,
    pub peer_pbac: bool,
    pub no_ack_tid_mask: u16,
    pub local_delayed_ba: bool,
    pub max_window_size: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddbaRequestOutcome {
    Ignore,
    Refuse {
        tid: u8,
        token: u8,
        status: u16,
    },
    ExistingAgreement {
        tid: u8,
        refresh_timeout_micros: u64,
        move_window_to: Option<u16>,
    },
    PrepareAgreement {
        tid: u8,
        token: u8,
        params: u16,
        timeout_micros: u64,
        window_start: u16,
        window_size: u16,
        window_end: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddbaResponseOutcome {
    pub tid: u8,
    pub status: u16,
    pub send_delba: bool,
    pub delba_reason: Option<u16>,
    pub driver_start: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelbaOutcome {
    pub tid: u8,
    pub reason: u16,
    pub stops_rx: bool,
    pub stops_tx: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarTidOutcome {
    NoAgreement {
        delba_reason: u16,
    },
    PbacViolation,
    PbacNoMove,
    Refresh {
        timeout_micros: u64,
        move_window_to: Option<u16>,
    },
}

fn action_body(frame: &[u8]) -> Result<&[u8], BaRxError> {
    if frame.len() < MAC_HEADER_LEN + 2 {
        return Err(BaRxError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT || frame[0] & FC0_SUBTYPE_MASK != FC0_SUBTYPE_ACTION
    {
        return Err(BaRxError::NotBlockAckAction);
    }
    let body = &frame[MAC_HEADER_LEN..];
    if body[0] != ACTION_CATEGORY_BA {
        return Err(BaRxError::NotBlockAckAction);
    }
    Ok(body)
}

/// Dispatch the supported BA and SA Query management action subtypes.
// upstream: ieee80211_input.c ieee80211_recv_action()
pub fn dispatch_action(frame: &[u8]) -> Result<ActionDispatch, BaRxError> {
    if frame.len() < MAC_HEADER_LEN + 2 {
        return Err(BaRxError::ShortFrame);
    }
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT || frame[0] & FC0_SUBTYPE_MASK != FC0_SUBTYPE_ACTION
    {
        return Err(BaRxError::NotBlockAckAction);
    }
    let category = frame[MAC_HEADER_LEN];
    let action = frame[MAC_HEADER_LEN + 1];
    Ok(match (category, action) {
        (ACTION_CATEGORY_BA, ACTION_ADDBA_REQ) => ActionDispatch::AddbaRequest,
        (ACTION_CATEGORY_BA, ACTION_ADDBA_RESP) => ActionDispatch::AddbaResponse,
        (ACTION_CATEGORY_BA, ACTION_DELBA) => ActionDispatch::Delba,
        (CATEGORY_SA_QUERY, ACTION_SA_QUERY_REQ) => ActionDispatch::SaQueryRequest,
        (CATEGORY_SA_QUERY, ACTION_SA_QUERY_RESP) => ActionDispatch::SaQueryResponse,
        _ => ActionDispatch::Ignore { category, action },
    })
}

/// Process an incoming ADDBA Request into source accept/refuse/ignore decisions.
// upstream: ieee80211_input.c ieee80211_recv_addba_req()
pub fn receive_addba_request(
    frame: &[u8],
    agreement: &mut BaAgreement,
    policy: AddbaRequestPolicy,
) -> Result<AddbaRequestOutcome, BaRxError> {
    let body = action_body(frame)?;
    if body.len() < ACTION_FIXED_LEN || body[1] != ACTION_ADDBA_REQ {
        return Err(if body.len() < ACTION_FIXED_LEN {
            BaRxError::ShortFrame
        } else {
            BaRxError::UnsupportedAction
        });
    }
    let token = body[2];
    let params = u16::from_le_bytes([body[3], body[4]]);
    let tid = ((params >> 2) & 0x0f) as u8;
    let buffer_size = (params >> 6) & 0x03ff;
    let timeout_tu = u16::from_le_bytes([body[5], body[6]]);
    let sequence_control = u16::from_le_bytes([body[7], body[8]]);
    let ssn = sequence_control >> 4;
    if usize::from(tid) >= BA_TID_COUNT {
        return Err(BaRxError::InvalidTid(tid));
    }
    if !policy.run_state || (policy.rsn_enabled && !policy.port_valid) || !policy.peer_ht {
        return Ok(AddbaRequestOutcome::Ignore);
    }
    if agreement.state == BA_STATE_REQUESTED || policy.background_scan_tx_mgmt_only {
        return Ok(AddbaRequestOutcome::Ignore);
    }
    let timeout_micros = u64::from(timeout_tu) * DUR_TU_MICROS;
    if agreement.state == BA_STATE_AGREED {
        let pbac = policy.peer_mfp && policy.peer_pbac;
        let move_window_to = if pbac && seq_lt(agreement.window_start, ssn) {
            Some(ssn)
        } else {
            None
        };
        if !pbac {
            return Ok(AddbaRequestOutcome::Ignore);
        }
        return Ok(AddbaRequestOutcome::ExistingAgreement {
            tid,
            refresh_timeout_micros: if agreement.timeout_micros != 0 {
                agreement.timeout_micros
            } else {
                0
            },
            move_window_to,
        });
    }
    if (policy.protected_block_ack_required && !(policy.peer_mfp && policy.peer_pbac))
        || policy.no_ack_tid_mask & (1 << tid) != 0
        || (!policy.local_delayed_ba && params & BA_POLICY == 0)
    {
        return Ok(AddbaRequestOutcome::Refuse {
            tid,
            token,
            status: STATUS_REFUSED,
        });
    }
    let max_window = policy.max_window_size.clamp(1, BA_MAX_WINDOW);
    let window_size = if buffer_size == 0 || buffer_size > max_window {
        max_window
    } else {
        buffer_size
    };
    agreement.state = BA_STATE_REQUESTED;
    agreement.timeout_micros = timeout_micros;
    agreement.token = token;
    agreement.window_size = window_size;
    agreement.window_start = ssn;
    agreement.window_end = ssn.wrapping_add(window_size - 1) & 0x0fff;
    agreement.request_interval = 0;
    let params =
        (params & BA_POLICY) | ((window_size & 0x03ff) << 6) | (u16::from(tid) << 2) | BA_AMSDU;
    Ok(AddbaRequestOutcome::PrepareAgreement {
        tid,
        token,
        params,
        timeout_micros,
        window_start: ssn,
        window_size,
        window_end: ssn.wrapping_add(window_size - 1) & 0x0fff,
    })
}

/// Accept the local driver's completed RX Block Ack setup.
// upstream: ieee80211_input.c ieee80211_addba_req_accept()
pub fn accept_addba_request(agreement: &mut BaAgreement, tid: u8) -> u32 {
    agreement.state = BA_STATE_AGREED;
    u32::from(agreement.token) << 8 | u32::from(tid) | (u32::from(STATUS_SUCCESS) << 16)
}

/// Roll back and reject the local driver's RX Block Ack setup.
// upstream: ieee80211_input.c ieee80211_addba_req_refuse()
pub fn refuse_addba_request(agreement: &mut BaAgreement, tid: u8) -> u32 {
    agreement.state = BA_STATE_INIT;
    (u32::from(STATUS_REFUSED) << 16) | (u32::from(agreement.token) << 8) | u32::from(tid)
}

/// Process a peer ADDBA response matching a locally requested TX agreement.
// upstream: ieee80211_input.c ieee80211_recv_addba_resp()
pub fn receive_addba_response(
    frame: &[u8],
    agreement: &mut BaAgreement,
) -> Result<AddbaResponseOutcome, BaRxError> {
    let body = action_body(frame)?;
    if body.len() < ACTION_FIXED_LEN || body[1] != ACTION_ADDBA_RESP {
        return Err(if body.len() < ACTION_FIXED_LEN {
            BaRxError::ShortFrame
        } else {
            BaRxError::UnsupportedAction
        });
    }
    let token = body[2];
    let status = u16::from_le_bytes([body[3], body[4]]);
    let params = u16::from_le_bytes([body[5], body[6]]);
    let tid = ((params >> 2) & 0x0f) as u8;
    let buffer_size = (params >> 6) & 0x03ff;
    let timeout_tu = u16::from_le_bytes([body[7], body[8]]);
    if agreement.state != BA_STATE_REQUESTED || token != agreement.token {
        return Ok(AddbaResponseOutcome {
            tid,
            status,
            send_delba: false,
            delba_reason: None,
            driver_start: false,
        });
    }
    if status != STATUS_SUCCESS {
        agreement.request_interval = agreement.request_interval.saturating_add(1);
        agreement.state = BA_STATE_INIT;
        return Ok(AddbaResponseOutcome {
            tid,
            status,
            send_delba: true,
            delba_reason: Some(REASON_SETUP_REQUIRED),
            driver_start: false,
        });
    }
    agreement.window_size = buffer_size.min(BA_MAX_WINDOW).max(1);
    agreement.timeout_micros = u64::from(timeout_tu) * DUR_TU_MICROS;
    Ok(AddbaResponseOutcome {
        tid,
        status,
        send_delba: false,
        delba_reason: None,
        driver_start: true,
    })
}

/// Apply the successful driver callback for a TX Block Ack agreement.
// upstream: ieee80211_input.c ieee80211_addba_resp_accept()
pub fn accept_addba_response(agreement: &mut BaAgreement) {
    agreement.state = BA_STATE_AGREED;
    agreement.request_interval = 1;
}

/// Apply a failed driver callback for a TX Block Ack agreement.
// upstream: ieee80211_input.c ieee80211_addba_resp_refuse()
pub fn refuse_addba_response(agreement: &mut BaAgreement, _status: u16) {
    agreement.state = BA_STATE_INIT;
}

/// Process a peer DELBA and report which direction's agreement to tear down.
// upstream: ieee80211_input.c ieee80211_recv_delba()
pub fn receive_delba(
    frame: &[u8],
    rx_agreement: &mut BaAgreement,
    tx_agreement: &mut BaAgreement,
) -> Result<Option<DelbaOutcome>, BaRxError> {
    let body = action_body(frame)?;
    if body.len() < 6 || body[1] != ACTION_DELBA {
        return Err(if body.len() < 6 {
            BaRxError::ShortFrame
        } else {
            BaRxError::UnsupportedAction
        });
    }
    let params = u16::from_le_bytes([body[2], body[3]]);
    let reason = u16::from_le_bytes([body[4], body[5]]);
    let tid = (params >> 12) as u8;
    let initiator = params & DELBA_INITIATOR != 0;
    let agreement = if initiator {
        rx_agreement
    } else {
        tx_agreement
    };
    if agreement.state != BA_STATE_AGREED {
        return Ok(None);
    }
    agreement.state = BA_STATE_INIT;
    Ok(Some(DelbaOutcome {
        tid,
        reason,
        stops_rx: initiator,
        stops_tx: !initiator,
    }))
}

/// Decode BlockAckReq basic/compressed or Multi-TID sequence-window advances.
// upstream: ieee80211_input.c ieee80211_recv_bar()
pub fn receive_bar(frame: &[u8], peer_ht: bool) -> Result<Vec<(u8, u16)>, BaRxError> {
    if !peer_ht {
        return Ok(Vec::new());
    }
    if frame.len() < BAR_HEADER_LEN + 4 {
        return Err(BaRxError::ShortFrame);
    }
    let control = u16::from_le_bytes([frame[BAR_HEADER_LEN], frame[BAR_HEADER_LEN + 1]]);
    let tid = ((control >> 12) & 0x0f) as u8;
    if control & 0x0002 != 0 {
        let tid_count = usize::from(tid) + 1;
        if frame.len() < BAR_HEADER_LEN + 2 + 4 * tid_count {
            return Err(BaRxError::ShortFrame);
        }
        let mut windows = Vec::with_capacity(tid_count);
        for index in 0..tid_count {
            let offset = BAR_HEADER_LEN + 2 + index * 4;
            let info = u16::from_le_bytes([frame[offset], frame[offset + 1]]);
            let sequence = u16::from_le_bytes([frame[offset + 2], frame[offset + 3]]) >> 4;
            windows.push((((info >> 12) & 0x0f) as u8, sequence));
        }
        Ok(windows)
    } else {
        let sequence =
            u16::from_le_bytes([frame[BAR_HEADER_LEN + 2], frame[BAR_HEADER_LEN + 3]]) >> 4;
        Ok(alloc::vec![(tid, sequence)])
    }
}

/// Apply a BlockAckReq sequence number to its per-TID receive window.
// upstream: ieee80211_input.c ieee80211_bar_tid()
pub fn apply_bar_tid(
    agreement: &BaAgreement,
    ssn: u16,
    peer_mfp: bool,
    peer_pbac: bool,
) -> BarTidOutcome {
    if agreement.state != BA_STATE_AGREED {
        return BarTidOutcome::NoAgreement {
            delba_reason: REASON_SETUP_REQUIRED,
        };
    }
    if peer_mfp && peer_pbac {
        if seq_lt(ssn, agreement.window_start) || seq_lt(agreement.window_end, ssn) {
            return BarTidOutcome::PbacViolation;
        }
        return BarTidOutcome::PbacNoMove;
    }
    BarTidOutcome::Refresh {
        timeout_micros: agreement.timeout_micros,
        move_window_to: seq_lt(agreement.window_start, ssn).then_some(ssn),
    }
}

fn seq_lt(a: u16, b: u16) -> bool {
    ((a.wrapping_sub(b)) & 0x0fff) > 0x0800
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(action: u8, body: &[u8]) -> Vec<u8> {
        let mut frame = alloc::vec![0; MAC_HEADER_LEN];
        frame[0] = FC0_SUBTYPE_ACTION;
        frame.push(ACTION_CATEGORY_BA);
        frame.push(action);
        frame.extend_from_slice(body);
        frame
    }

    fn agreement(state: u8, token: u8) -> BaAgreement {
        BaAgreement {
            state,
            token,
            window_start: 100,
            window_end: 163,
            window_size: 64,
            timeout_micros: 10_000,
            request_interval: 1,
        }
    }

    #[test]
    fn addba_request_builds_bounded_rx_window_and_refusal_policy() {
        let body = alloc::vec![4, 0x02, 0x02, 2, 0, 0x30, 0x06];
        let frame = action(ACTION_ADDBA_REQ, &body);
        let mut ba = agreement(BA_STATE_INIT, 0);
        let outcome = receive_addba_request(
            &frame,
            &mut ba,
            AddbaRequestPolicy {
                run_state: true,
                port_valid: true,
                peer_ht: true,
                local_delayed_ba: true,
                max_window_size: 64,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            outcome,
            AddbaRequestOutcome::PrepareAgreement {
                tid: 0,
                window_size: 8,
                ..
            }
        ));
        assert_eq!(ba.state, BA_STATE_REQUESTED);
        let rejected = receive_addba_request(
            &frame,
            &mut agreement(BA_STATE_INIT, 0),
            AddbaRequestPolicy {
                run_state: true,
                peer_ht: true,
                protected_block_ack_required: true,
                max_window_size: 64,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            rejected,
            AddbaRequestOutcome::Refuse {
                status: STATUS_REFUSED,
                ..
            }
        ));
    }

    #[test]
    fn addba_response_checks_request_token_then_start_or_retry() {
        let body = alloc::vec![7, 0, 0, 0, 0, 5, 0];
        let frame = action(ACTION_ADDBA_RESP, &body);
        let mut ba = agreement(BA_STATE_REQUESTED, 7);
        let response = receive_addba_response(&frame, &mut ba).unwrap();
        assert!(response.driver_start);
        accept_addba_response(&mut ba);
        assert_eq!(ba.state, BA_STATE_AGREED);
        let failure = action(ACTION_ADDBA_RESP, &[7, 37, 0, 5, 0, 0, 0]);
        let failed =
            receive_addba_response(&failure, &mut agreement(BA_STATE_REQUESTED, 7)).unwrap();
        assert!(failed.send_delba);
    }

    #[test]
    fn delba_direction_and_multi_tid_bar_are_decoded() {
        let mut rx = agreement(BA_STATE_AGREED, 1);
        let mut tx = agreement(BA_STATE_AGREED, 1);
        let delba = action(ACTION_DELBA, &[0x00, 0x08, 3, 0]);
        let result = receive_delba(&delba, &mut rx, &mut tx).unwrap().unwrap();
        assert!(result.stops_rx);
        assert_eq!(rx.state, BA_STATE_INIT);
        let mut bar = alloc::vec![0; BAR_HEADER_LEN + 10];
        bar[BAR_HEADER_LEN..BAR_HEADER_LEN + 2].copy_from_slice(&0x1002u16.to_le_bytes());
        bar[BAR_HEADER_LEN + 2..BAR_HEADER_LEN + 6]
            .copy_from_slice(&[0x1000u16.to_le_bytes(), 0x0040u16.to_le_bytes()].concat());
        bar[BAR_HEADER_LEN + 6..BAR_HEADER_LEN + 10]
            .copy_from_slice(&[0x2000u16.to_le_bytes(), 0x0080u16.to_le_bytes()].concat());
        assert_eq!(receive_bar(&bar, true).unwrap(), vec![(1, 4), (2, 8)]);
    }

    #[test]
    fn action_dispatch_routes_supported_management_actions() {
        let frame = action(ACTION_ADDBA_REQ, &[]);
        assert_eq!(dispatch_action(&frame), Ok(ActionDispatch::AddbaRequest));
        let mut sa_query = action(ACTION_SA_QUERY_RESP, &[0, 0]);
        sa_query[MAC_HEADER_LEN] = CATEGORY_SA_QUERY;
        assert_eq!(
            dispatch_action(&sa_query),
            Ok(ActionDispatch::SaQueryResponse)
        );
    }

    #[test]
    fn bar_tid_checks_agreement_pbac_window_and_inactivity_refresh() {
        let agreed = agreement(BA_STATE_AGREED, 1);
        assert_eq!(
            apply_bar_tid(&agreement(BA_STATE_INIT, 1), 101, false, false),
            BarTidOutcome::NoAgreement {
                delba_reason: REASON_SETUP_REQUIRED
            }
        );
        assert_eq!(
            apply_bar_tid(&agreed, 101, true, true),
            BarTidOutcome::PbacNoMove
        );
        assert_eq!(
            apply_bar_tid(&agreed, 99, true, true),
            BarTidOutcome::PbacViolation
        );
        assert_eq!(
            apply_bar_tid(&agreed, 120, false, false),
            BarTidOutcome::Refresh {
                timeout_micros: 10_000,
                move_window_to: Some(120)
            }
        );
    }
}
