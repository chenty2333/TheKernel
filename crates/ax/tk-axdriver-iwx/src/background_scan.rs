//! Background-scan roaming completion from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxvar.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

pub const BGSCAN_MAX_TIDS: usize = 16;
pub const BGSCAN_FIRST_AGG_TX_QUEUE: u8 = 2;
pub const BGSCAN_STATION_ID: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BgscanState {
    pub shutdown: bool,
    pub background_scan_enabled: bool,
    pub run_state: bool,
    pub rsn_enabled: bool,
    pub mfp: bool,
    pub aggregation_queues: [u8; BGSCAN_MAX_TIDS],
    pub have_pairwise_key: bool,
    pub have_group_key: bool,
    pub have_integrity_group_key: bool,
    pub default_tx_key: u8,
    pub igtk_key_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgscanAction {
    FlushStation,
    DisableTxQueue {
        station_id: u8,
        queue_id: u8,
        tid: u8,
    },
    ClearTxBa {
        tid: u8,
    },
    MfpLeave,
    DeletePairwiseKey,
    DeleteGroupKey {
        key_id: u8,
    },
    DeleteIntegrityGroupKey {
        key_id: u8,
    },
    ClearProtectedPort,
    SwitchBss,
    TxStopped,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BgscanTaskFailure<E> {
    NotRunning,
    Operation(E),
}

#[derive(Debug, PartialEq, Eq)]
pub struct BgscanTaskOutcome<E> {
    pub failure: Option<BgscanTaskFailure<E>>,
    pub roam_released: bool,
    pub init_scheduled: bool,
}

/// Replace the deferred node-switch argument and enqueue background completion.
// upstream: if_iwx.c iwx_bgscan_done()
pub fn replace_bgscan_unref_argument(
    current: &mut Option<Vec<u8>>,
    argument: Vec<u8>,
    mut enqueue_task: impl FnMut(),
) -> Option<Vec<u8>> {
    let old = current.replace(argument);
    enqueue_task();
    old
}

/// Flush roaming TX, tear down aggregation/keys, transfer node args, and resume roaming.
// upstream: if_iwx.c iwx_bgscan_done_task()
pub fn run_bgscan_done_task<E>(
    state: &mut BgscanState,
    unref_argument: &mut Option<Vec<u8>>,
    mut operation: impl FnMut(BgscanAction) -> Result<(), E>,
    mut side_effect: impl FnMut(BgscanAction),
    mut transfer_argument: impl FnMut(Option<Vec<u8>>),
    mut schedule_init: impl FnMut(),
    mut release_task_reference: impl FnMut(),
) -> BgscanTaskOutcome<E> {
    let mut failure = if state.shutdown || !state.background_scan_enabled || !state.run_state {
        Some(BgscanTaskFailure::NotRunning)
    } else {
        None
    };
    if failure.is_none() {
        if let Err(error) = operation(BgscanAction::FlushStation) {
            failure = Some(BgscanTaskFailure::Operation(error));
        }
    }
    if failure.is_none() {
        for tid in 0..BGSCAN_MAX_TIDS {
            if state.shutdown {
                break;
            }
            if state.aggregation_queues[tid] == 0 {
                continue;
            }
            let queue_id = BGSCAN_FIRST_AGG_TX_QUEUE + tid as u8;
            match operation(BgscanAction::DisableTxQueue {
                station_id: BGSCAN_STATION_ID,
                queue_id,
                tid: tid as u8,
            }) {
                Ok(()) => {
                    side_effect(BgscanAction::ClearTxBa { tid: tid as u8 });
                    state.aggregation_queues[tid] = 0;
                }
                Err(error) => {
                    failure = Some(BgscanTaskFailure::Operation(error));
                    break;
                }
            }
        }
    }
    if failure.is_none() && state.rsn_enabled {
        if state.mfp {
            side_effect(BgscanAction::MfpLeave);
        }
        if state.have_pairwise_key {
            side_effect(BgscanAction::DeletePairwiseKey);
        }
        if state.have_group_key && matches!(state.default_tx_key, 1 | 2) {
            side_effect(BgscanAction::DeleteGroupKey {
                key_id: state.default_tx_key,
            });
        }
        if state.have_integrity_group_key && matches!(state.igtk_key_id, 4 | 5) {
            side_effect(BgscanAction::DeleteIntegrityGroupKey {
                key_id: state.igtk_key_id,
            });
        }
        side_effect(BgscanAction::ClearProtectedPort);
    }

    let roam_released = failure.is_none();
    if roam_released {
        transfer_argument(unref_argument.take());
        side_effect(if state.mfp {
            BgscanAction::SwitchBss
        } else {
            BgscanAction::TxStopped
        });
    } else {
        unref_argument.take();
        if !state.shutdown {
            schedule_init();
        }
    }
    release_task_reference();
    BgscanTaskOutcome {
        failure,
        roam_released,
        init_scheduled: !roam_released && !state.shutdown,
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::cell::RefCell;

    use super::*;

    fn state() -> BgscanState {
        BgscanState {
            shutdown: false,
            background_scan_enabled: true,
            run_state: true,
            rsn_enabled: true,
            mfp: true,
            aggregation_queues: [0; BGSCAN_MAX_TIDS],
            have_pairwise_key: true,
            have_group_key: true,
            have_integrity_group_key: true,
            default_tx_key: 1,
            igtk_key_id: 4,
        }
    }

    #[test]
    fn bgscan_completion_disables_aggregation_and_transfers_roaming_argument() {
        let mut state = state();
        state.aggregation_queues[3] = 5;
        let mut argument = Some(vec![1, 2]);
        let actions = RefCell::new(Vec::new());
        let transferred = RefCell::new(None);
        let mut released = false;
        let outcome = run_bgscan_done_task(
            &mut state,
            &mut argument,
            |action| {
                actions.borrow_mut().push(action);
                Ok::<_, ()>(())
            },
            |action| actions.borrow_mut().push(action),
            |arg| *transferred.borrow_mut() = arg,
            || panic!("successful roam does not schedule init"),
            || released = true,
        );
        assert_eq!(outcome.failure, None);
        assert!(outcome.roam_released && released);
        assert!(argument.is_none());
        assert_eq!(*transferred.borrow(), Some(vec![1, 2]));
        assert_eq!(state.aggregation_queues[3], 0);
        assert_eq!(
            *actions.borrow(),
            [
                BgscanAction::FlushStation,
                BgscanAction::DisableTxQueue {
                    station_id: 0,
                    queue_id: 5,
                    tid: 3,
                },
                BgscanAction::ClearTxBa { tid: 3 },
                BgscanAction::MfpLeave,
                BgscanAction::DeletePairwiseKey,
                BgscanAction::DeleteGroupKey { key_id: 1 },
                BgscanAction::DeleteIntegrityGroupKey { key_id: 4 },
                BgscanAction::ClearProtectedPort,
                BgscanAction::SwitchBss,
            ]
        );
    }

    #[test]
    fn bgscan_disable_failure_frees_argument_and_schedules_reinitialization() {
        let mut state = state();
        state.aggregation_queues[0] = 4;
        let mut argument = Some(vec![9]);
        let mut scheduled = false;
        let mut released = false;
        let mut step = 0;
        let outcome = run_bgscan_done_task(
            &mut state,
            &mut argument,
            |_| {
                step += 1;
                if step == 2 { Err("disable") } else { Ok(()) }
            },
            |_| {},
            |_| panic!("failed roam discards the unref arg"),
            || scheduled = true,
            || released = true,
        );
        assert_eq!(
            outcome.failure,
            Some(BgscanTaskFailure::Operation("disable"))
        );
        assert!(!outcome.roam_released && outcome.init_scheduled);
        assert!(argument.is_none() && scheduled && released);
        assert_eq!(state.aggregation_queues[0], 4);
    }
}
