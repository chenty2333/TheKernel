//! Interface TX dequeue and MFP leave ordering from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

pub const ETHERNET_HEADER_BYTES: usize = 14;
pub const MFP_LEAVE_TIMEOUT_NS: u64 = 500_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxStartState {
    pub interface_running: bool,
    pub output_active: bool,
    pub queue_full_mask: u32,
    pub tx_flush: bool,
    pub ieee80211_run: bool,
    pub tx_management_only: bool,
    pub interface_up: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxStartReport {
    pub sent: usize,
    pub output_errors: usize,
    pub stopped_on_queue_full: bool,
}

/// Signal an MFP leave waiter only for a running RUN-state protected node.
// upstream: if_iwx.c iwx_mfp_leave_done()
pub const fn mfp_leave_done(interface_running: bool, run_state: bool, node_mfp: bool) -> bool {
    interface_running && run_state && node_mfp
}

/// Drain management frames first, then eligible data frames, retaining iwx_start() stop rules.
// upstream: if_iwx.c iwx_start()
pub fn start_transmit<M, N>(
    state: &mut TxStartState,
    mut dequeue_management: impl FnMut() -> Option<(M, N)>,
    mut dequeue_data: impl FnMut() -> Option<M>,
    mut ethernet_header_complete: impl FnMut(&M) -> bool,
    mut pullup_ethernet_header: impl FnMut(M) -> Option<M>,
    mut encapsulate: impl FnMut(M) -> Option<(M, N)>,
    mut transmit: impl FnMut(M, N) -> Result<(), (M, N)>,
    mut release_node: impl FnMut(N),
    mut output_error: impl FnMut(),
    mut set_tx_timer: impl FnMut(),
) -> TxStartReport {
    let mut report = TxStartReport::default();
    if !state.interface_running || state.output_active {
        return report;
    }
    loop {
        if state.queue_full_mask != 0 {
            state.output_active = true;
            report.stopped_on_queue_full = true;
            break;
        }
        if state.tx_flush {
            break;
        }
        let (frame, node, management) = if let Some((frame, node)) = dequeue_management() {
            (frame, node, true)
        } else {
            if !state.ieee80211_run || state.tx_management_only {
                break;
            }
            let Some(frame) = dequeue_data() else {
                break;
            };
            let frame = if ethernet_header_complete(&frame) {
                frame
            } else if let Some(frame) = pullup_ethernet_header(frame) {
                frame
            } else {
                report.output_errors += 1;
                output_error();
                continue;
            };
            let Some((frame, node)) = encapsulate(frame) else {
                report.output_errors += 1;
                output_error();
                continue;
            };
            (frame, node, false)
        };
        if let Err((_frame, node)) = transmit(frame, node) {
            // The TX path owns and releases packet storage on failure; the
            // 802.11 node reference remains the caller's obligation.
            // Since transmit consumes both values, the adapter returns the node
            // only through its release callback.
            report.output_errors += 1;
            output_error();
            release_node(node);
            continue;
        }
        let _ = management;
        report.sent += 1;
        if state.interface_up {
            set_tx_timer();
        }
    }
    report
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfpLeaveAction {
    EnableManagementOnly,
    ClearDeauthSent,
    InstallUnrefCallback,
    SendDeauth,
    WaitForTransmit { timeout_ns: u64 },
    ClearUnrefCallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfpLeaveResult<E> {
    Complete,
    SendFailed(E),
    WaitInterrupted(E),
}

/// Send authenticated deauthentication and wait up to 500ms for net80211 node release.
// upstream: if_iwx.c iwx_mfp_leave()
pub fn mfp_leave<E>(mut action: impl FnMut(MfpLeaveAction) -> Result<(), E>) -> MfpLeaveResult<E> {
    for step in [
        MfpLeaveAction::EnableManagementOnly,
        MfpLeaveAction::ClearDeauthSent,
        MfpLeaveAction::InstallUnrefCallback,
    ] {
        if let Err(error) = action(step) {
            return MfpLeaveResult::WaitInterrupted(error);
        }
    }
    if let Err(error) = action(MfpLeaveAction::SendDeauth) {
        let _ = action(MfpLeaveAction::ClearUnrefCallback);
        return MfpLeaveResult::SendFailed(error);
    }
    if let Err(error) = action(MfpLeaveAction::WaitForTransmit {
        timeout_ns: MFP_LEAVE_TIMEOUT_NS,
    }) {
        let _ = action(MfpLeaveAction::ClearUnrefCallback);
        return MfpLeaveResult::WaitInterrupted(error);
    }
    MfpLeaveResult::Complete
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn management_frames_bypass_run_and_data_gates_and_queue_full_sets_oactive() {
        let mut state = TxStartState {
            interface_running: true,
            output_active: false,
            queue_full_mask: 0,
            tx_flush: false,
            ieee80211_run: false,
            tx_management_only: true,
            interface_up: true,
        };
        let mut management = vec![(1u8, 9u8)];
        let mut data_dequeued = false;
        let mut transmitted = Vec::new();
        let report = start_transmit(
            &mut state,
            || management.pop(),
            || {
                data_dequeued = true;
                None
            },
            |_| true,
            |frame| Some(frame),
            |frame| Some((frame, 1)),
            |frame, node| {
                transmitted.push((frame, node));
                Ok(())
            },
            |_| {},
            || {},
            || {},
        );
        assert_eq!(report.sent, 1);
        assert_eq!(transmitted, [(1, 9)]);
        assert!(!data_dequeued);
        state.queue_full_mask = 1;
        let report = start_transmit::<u8, u8>(
            &mut state,
            || None,
            || None,
            |_| true,
            Some,
            |frame| Some((frame, 0)),
            |_, _| Ok(()),
            |_| {},
            || {},
            || {},
        );
        assert!(report.stopped_on_queue_full && state.output_active);
    }

    #[test]
    fn mfp_leave_clears_callback_on_send_error_and_waits_at_source_timeout() {
        let mut steps = Vec::new();
        let result = mfp_leave(|step| {
            steps.push(step);
            if step == MfpLeaveAction::SendDeauth {
                Err("send")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, MfpLeaveResult::SendFailed("send"));
        assert_eq!(steps.last(), Some(&MfpLeaveAction::ClearUnrefCallback));
        steps.clear();
        assert_eq!(
            mfp_leave(|step| {
                steps.push(step);
                Ok::<_, ()>(())
            }),
            MfpLeaveResult::Complete
        );
        assert!(steps.contains(&MfpLeaveAction::WaitForTransmit {
            timeout_ns: MFP_LEAVE_TIMEOUT_NS
        }));
        assert!(mfp_leave_done(true, true, true));
        assert!(!mfp_leave_done(false, true, true));
        assert!(!mfp_leave_done(true, false, true));
        assert!(!mfp_leave_done(true, true, false));
    }
}
