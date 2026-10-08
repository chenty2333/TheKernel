//! Transmit BlockAck negotiation state from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_proto.c` rev 1.176 and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

pub const TX_BA_INIT: u8 = 0;
pub const TX_BA_REQUESTED: u8 = 1;
pub const TX_BA_AGREED: u8 = 2;
pub const ADD_BA_AMSDU: u16 = 1 << 0;
pub const ADD_BA_POLICY: u16 = 1 << 1;
pub const ADD_BA_TID_SHIFT: u8 = 2;
pub const ADD_BA_WINDOW_SHIFT: u8 = 6;
pub const ADD_BA_MAX_WINDOW: u16 = 64;
pub const ADD_BA_RESPONSE_TIMEOUT_MICROS: u64 = 1_000_000;
pub const ADD_BA_STATUS_UNSPECIFIED: u16 = 1;
pub const ADD_BA_REQUEST_INTERVAL_MAX: u8 = 30;
pub const DELBA_REASON_SETUP_REQUIRED: u16 = 38;
pub const DELBA_REASON_TIMEOUT: u16 = 39;
pub const ERR_BUSY: i32 = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxBaAgreement {
    pub state: u8,
    pub token: u8,
    pub timeout_value: u16,
    pub window_start: u16,
    pub window_end: u16,
    pub window_size: u16,
    pub parameters: u16,
    pub request_interval: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AddbaTxPolicy {
    pub delayed_ba: bool,
    pub tx_start_offload: bool,
    /// Callback result: zero succeeds, EBUSY leaves the request pending, other values refuse.
    pub offload_result: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddbaTxOutcome {
    Busy,
    Error(i32),
    OffloadStarted,
    OffloadRefused { status: u16 },
    SendRequest { timeout_micros: u64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DelbaRequestEffects {
    pub transmit_delba: bool,
    pub stop_transmit: bool,
    pub stop_receive: bool,
    pub cancel_inactivity_timeout: bool,
    pub cancel_gap_timeout: bool,
    pub retire_reorder_buffers: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxBaTimeoutEffects {
    pub tx_timeout_statistic: bool,
    pub retry_interval_incremented: bool,
    pub delba_reason: Option<u16>,
    pub delba: DelbaRequestEffects,
}

/// Apply the transmit BA timeout callback for a request or established agreement.
// upstream: ieee80211_proto.c ieee80211_tx_ba_timeout()
pub fn tx_ba_timeout(tx: &mut TxBaAgreement, rx: &mut crate::BaAgreement) -> TxBaTimeoutEffects {
    if tx.state == TX_BA_REQUESTED {
        tx.state = TX_BA_INIT;
        let retry_interval_incremented = tx.request_interval < ADD_BA_REQUEST_INTERVAL_MAX;
        if retry_interval_incremented {
            tx.request_interval += 1;
        }
        return TxBaTimeoutEffects {
            retry_interval_incremented,
            delba_reason: Some(DELBA_REASON_SETUP_REQUIRED),
            delba: DelbaRequestEffects {
                transmit_delba: true,
                ..Default::default()
            },
            ..Default::default()
        };
    }
    if tx.state == TX_BA_AGREED {
        let delba = request_delba(tx, rx, DELBA_REASON_TIMEOUT, true);
        return TxBaTimeoutEffects {
            tx_timeout_statistic: true,
            delba_reason: Some(DELBA_REASON_TIMEOUT),
            delba,
            ..Default::default()
        };
    }
    TxBaTimeoutEffects::default()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxBaTimeoutEffects {
    pub rx_timeout_statistic: bool,
    pub delba: DelbaRequestEffects,
}

/// Apply the receive BA inactivity timeout callback.
// upstream: ieee80211_proto.c ieee80211_rx_ba_timeout()
pub fn rx_ba_timeout(tx: &mut TxBaAgreement, rx: &mut crate::BaAgreement) -> RxBaTimeoutEffects {
    RxBaTimeoutEffects {
        rx_timeout_statistic: true,
        delba: request_delba(tx, rx, DELBA_REASON_TIMEOUT, false),
    }
}

/// Apply local originator/recipient agreement teardown and return driver effects.
// upstream: ieee80211_proto.c ieee80211_delba_request()
pub fn request_delba(
    tx: &mut TxBaAgreement,
    rx: &mut crate::BaAgreement,
    reason: u16,
    originator: bool,
) -> DelbaRequestEffects {
    let mut effects = DelbaRequestEffects {
        transmit_delba: reason != 0,
        ..Default::default()
    };
    if originator {
        effects.stop_transmit = true;
        *tx = TxBaAgreement::default();
    } else {
        effects.stop_receive = true;
        effects.cancel_inactivity_timeout = true;
        effects.cancel_gap_timeout = true;
        effects.retire_reorder_buffers = true;
        rx.state = crate::ba_rx::BA_STATE_INIT;
        rx.token = 0;
        rx.window_start = 0;
        rx.window_end = 0;
        rx.window_size = 0;
        rx.timeout_micros = 0;
        rx.request_interval = 0;
    }
    effects
}

/// Initiate transmit BlockAck negotiation for one traffic identifier.
// upstream: ieee80211_proto.c ieee80211_addba_request()
pub fn start_addba_request(
    agreement: &mut TxBaAgreement,
    dialog_token: &mut u8,
    ssn: u16,
    tid: u8,
    policy: AddbaTxPolicy,
) -> AddbaTxOutcome {
    if agreement.state != TX_BA_INIT {
        return AddbaTxOutcome::Busy;
    }

    agreement.state = TX_BA_REQUESTED;
    agreement.token = *dialog_token;
    *dialog_token = dialog_token.wrapping_add(1);
    agreement.timeout_value = 0;
    agreement.window_size = ADD_BA_MAX_WINDOW;
    agreement.window_start = ssn;
    agreement.window_end = agreement
        .window_start
        .wrapping_add(agreement.window_size - 1)
        & 0x0fff;
    agreement.parameters = (agreement.window_size << ADD_BA_WINDOW_SHIFT)
        | ((tid as u16) << ADD_BA_TID_SHIFT)
        | ADD_BA_AMSDU;
    if !policy.delayed_ba {
        agreement.parameters |= ADD_BA_POLICY;
    }

    if policy.tx_start_offload {
        match policy.offload_result.unwrap_or(ERR_BUSY) {
            0 => {
                agreement.state = TX_BA_AGREED;
                AddbaTxOutcome::OffloadStarted
            }
            ERR_BUSY => AddbaTxOutcome::Error(ERR_BUSY),
            _ => {
                agreement.state = TX_BA_INIT;
                AddbaTxOutcome::OffloadRefused {
                    status: ADD_BA_STATUS_UNSPECIFIED,
                }
            }
        }
    } else {
        AddbaTxOutcome::SendRequest {
            timeout_micros: ADD_BA_RESPONSE_TIMEOUT_MICROS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addba_request_sets_sequence_token_window_and_immediate_policy() {
        let mut ba = TxBaAgreement::default();
        let mut token = 255;
        assert_eq!(
            start_addba_request(&mut ba, &mut token, 0x0ffe, 5, AddbaTxPolicy::default()),
            AddbaTxOutcome::SendRequest {
                timeout_micros: ADD_BA_RESPONSE_TIMEOUT_MICROS
            }
        );
        assert_eq!(token, 0);
        assert_eq!(ba.state, TX_BA_REQUESTED);
        assert_eq!(ba.token, 255);
        assert_eq!(ba.window_start, 0x0ffe);
        assert_eq!(ba.window_end, 61);
        assert_eq!(
            ba.parameters,
            (64 << 6) | (5 << 2) | ADD_BA_AMSDU | ADD_BA_POLICY
        );
    }

    #[test]
    fn addba_request_busy_and_driver_offload_results_follow_source() {
        let mut ba = TxBaAgreement {
            state: TX_BA_REQUESTED,
            ..Default::default()
        };
        let mut token = 1;
        assert_eq!(
            start_addba_request(&mut ba, &mut token, 0, 0, AddbaTxPolicy::default()),
            AddbaTxOutcome::Busy
        );

        ba.state = TX_BA_INIT;
        let result = start_addba_request(
            &mut ba,
            &mut token,
            10,
            3,
            AddbaTxPolicy {
                delayed_ba: true,
                tx_start_offload: true,
                offload_result: Some(0),
            },
        );
        assert_eq!(result, AddbaTxOutcome::OffloadStarted);
        assert_eq!(ba.state, TX_BA_AGREED);
        assert_eq!(ba.parameters & ADD_BA_POLICY, 0);

        ba.state = TX_BA_INIT;
        assert_eq!(
            start_addba_request(
                &mut ba,
                &mut token,
                0,
                3,
                AddbaTxPolicy {
                    tx_start_offload: true,
                    offload_result: Some(-5),
                    ..Default::default()
                },
            ),
            AddbaTxOutcome::OffloadRefused {
                status: ADD_BA_STATUS_UNSPECIFIED
            }
        );
        assert_eq!(ba.state, TX_BA_INIT);
    }

    #[test]
    fn delba_request_sends_only_with_reason_and_clears_selected_direction() {
        let mut tx = TxBaAgreement {
            state: TX_BA_AGREED,
            token: 2,
            window_size: 64,
            ..Default::default()
        };
        let mut rx = crate::BaAgreement {
            state: crate::ba_rx::BA_STATE_AGREED,
            token: 3,
            timeout_micros: 1000,
            ..Default::default()
        };
        let effects = request_delba(&mut tx, &mut rx, 39, true);
        assert_eq!(
            effects,
            DelbaRequestEffects {
                transmit_delba: true,
                stop_transmit: true,
                ..Default::default()
            }
        );
        assert_eq!(tx, TxBaAgreement::default());
        assert_eq!(rx.state, crate::ba_rx::BA_STATE_AGREED);

        let effects = request_delba(&mut tx, &mut rx, 0, false);
        assert_eq!(
            effects,
            DelbaRequestEffects {
                stop_receive: true,
                cancel_inactivity_timeout: true,
                cancel_gap_timeout: true,
                retire_reorder_buffers: true,
                ..Default::default()
            }
        );
        assert_eq!(rx, crate::BaAgreement::default());
    }

    #[test]
    fn ba_timeout_callbacks_follow_requested_agreed_and_receive_paths() {
        let mut tx = TxBaAgreement {
            state: TX_BA_REQUESTED,
            request_interval: ADD_BA_REQUEST_INTERVAL_MAX - 1,
            ..Default::default()
        };
        let mut rx = crate::BaAgreement::default();
        let requested = tx_ba_timeout(&mut tx, &mut rx);
        assert_eq!(tx.state, TX_BA_INIT);
        assert_eq!(tx.request_interval, ADD_BA_REQUEST_INTERVAL_MAX);
        assert!(requested.retry_interval_incremented);
        assert_eq!(requested.delba_reason, Some(DELBA_REASON_SETUP_REQUIRED));
        assert!(!requested.delba.stop_transmit);

        tx.state = TX_BA_AGREED;
        let agreed = tx_ba_timeout(&mut tx, &mut rx);
        assert!(agreed.tx_timeout_statistic);
        assert_eq!(agreed.delba_reason, Some(DELBA_REASON_TIMEOUT));
        assert_eq!(tx, TxBaAgreement::default());

        rx.state = crate::ba_rx::BA_STATE_AGREED;
        let receive = rx_ba_timeout(&mut tx, &mut rx);
        assert!(receive.rx_timeout_statistic);
        assert!(receive.delba.stop_receive);
        assert_eq!(rx, crate::BaAgreement::default());
    }
}
