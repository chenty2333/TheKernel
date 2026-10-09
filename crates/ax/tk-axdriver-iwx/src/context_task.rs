//! Deferred MAC/PHY context work from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTaskKind {
    Mac,
    Phy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextTaskEligibility {
    pub run_state: bool,
    pub newstate_pending: bool,
    pub shutdown: bool,
    pub has_phy_context: bool,
}

fn schedule_context_update(
    state: ContextTaskEligibility,
    kind: ContextTaskKind,
    mut enqueue: impl FnMut(ContextTaskKind),
) -> bool {
    if state.run_state && !state.newstate_pending && !state.shutdown {
        enqueue(kind);
        true
    } else {
        false
    }
}

/// Queue PHY context refresh on RUN channel changes.
// upstream: if_iwx.c iwx_updatechan()
pub fn updatechan(state: ContextTaskEligibility, enqueue: impl FnMut(ContextTaskKind)) -> bool {
    schedule_context_update(state, ContextTaskKind::Phy, enqueue)
}

/// Queue MAC context refresh after protection changes.
// upstream: if_iwx.c iwx_updateprot()
pub fn updateprot(state: ContextTaskEligibility, enqueue: impl FnMut(ContextTaskKind)) -> bool {
    schedule_context_update(state, ContextTaskKind::Mac, enqueue)
}

/// Queue MAC context refresh after short-slot changes.
// upstream: if_iwx.c iwx_updateslot()
pub fn updateslot(state: ContextTaskEligibility, enqueue: impl FnMut(ContextTaskKind)) -> bool {
    schedule_context_update(state, ContextTaskKind::Mac, enqueue)
}

/// Queue MAC context refresh after EDCA updates.
// upstream: if_iwx.c iwx_updateedca()
pub fn updateedca(state: ContextTaskEligibility, enqueue: impl FnMut(ContextTaskKind)) -> bool {
    schedule_context_update(state, ContextTaskKind::Mac, enqueue)
}

/// Queue MAC context refresh after DTIM timing changes.
// upstream: if_iwx.c iwx_updatedtim()
pub fn updatedtim(state: ContextTaskEligibility, enqueue: impl FnMut(ContextTaskKind)) -> bool {
    schedule_context_update(state, ContextTaskKind::Mac, enqueue)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacContextTaskOutcome {
    Skipped,
    Updated,
    SendFailed,
}

/// Run MAC MODIFY, session unprotection, and ref release in source order.
// upstream: if_iwx.c iwx_mac_ctxt_task()
pub fn run_mac_context_task<E>(
    state: ContextTaskEligibility,
    mut send_modify: impl FnMut() -> Result<(), E>,
    mut unprotect_session: impl FnMut(),
    mut release_reference: impl FnMut(),
) -> MacContextTaskOutcome {
    if state.shutdown || !state.run_state {
        release_reference();
        return MacContextTaskOutcome::Skipped;
    }
    let result = send_modify();
    unprotect_session();
    release_reference();
    if result.is_ok() {
        MacContextTaskOutcome::Updated
    } else {
        MacContextTaskOutcome::SendFailed
    }
}

pub const HT_SECONDARY_NONE: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhyContextTaskConfig {
    pub eligibility: ContextTaskEligibility,
    pub mimo_enabled: bool,
    pub old_sco: u8,
    pub new_sco: u8,
    pub old_vht_width: u8,
    pub new_vht_width: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhyContextTaskOutcome {
    pub skipped: bool,
    pub chains: u8,
    pub going_narrow: bool,
    pub rate_before_failed: bool,
    pub phy_update_failed: bool,
    pub rate_after_failed: bool,
}

/// Update rates before narrowing PHY width and after widening it, then release the task ref.
// upstream: if_iwx.c iwx_phy_ctxt_task()
pub fn run_phy_context_task(
    config: PhyContextTaskConfig,
    mut init_rate_scaling: impl FnMut() -> bool,
    mut update_phy: impl FnMut(u8, u8, u8) -> bool,
    mut release_reference: impl FnMut(),
) -> PhyContextTaskOutcome {
    if config.eligibility.shutdown
        || !config.eligibility.run_state
        || !config.eligibility.has_phy_context
    {
        release_reference();
        return PhyContextTaskOutcome {
            skipped: true,
            ..PhyContextTaskOutcome::default()
        };
    }
    let chains = if config.mimo_enabled { 2 } else { 1 };
    let changed = config.old_sco != config.new_sco || config.old_vht_width != config.new_vht_width;
    let going_narrow = changed
        && ((config.new_sco == HT_SECONDARY_NONE && config.old_sco != HT_SECONDARY_NONE)
            || config.old_vht_width > config.new_vht_width);
    let mut outcome = PhyContextTaskOutcome {
        chains,
        going_narrow,
        ..PhyContextTaskOutcome::default()
    };
    if changed {
        if going_narrow {
            outcome.rate_before_failed = !init_rate_scaling();
        }
        outcome.phy_update_failed = !update_phy(chains, config.new_sco, config.new_vht_width);
        if !going_narrow {
            outcome.rate_after_failed = !init_rate_scaling();
        }
    }
    release_reference();
    outcome
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;

    fn eligibility() -> ContextTaskEligibility {
        ContextTaskEligibility {
            run_state: true,
            newstate_pending: false,
            shutdown: false,
            has_phy_context: true,
        }
    }

    #[test]
    fn ieee80211_update_callbacks_schedule_the_source_context_task() {
        let mut queued = Vec::new();
        assert!(updatechan(eligibility(), |kind| queued.push(kind)));
        assert!(updateprot(eligibility(), |kind| queued.push(kind)));
        assert!(updateslot(eligibility(), |kind| queued.push(kind)));
        assert!(updateedca(eligibility(), |kind| queued.push(kind)));
        assert!(updatedtim(eligibility(), |kind| queued.push(kind)));
        assert_eq!(
            queued,
            [
                ContextTaskKind::Phy,
                ContextTaskKind::Mac,
                ContextTaskKind::Mac,
                ContextTaskKind::Mac,
                ContextTaskKind::Mac,
            ]
        );
        assert!(!updatechan(
            ContextTaskEligibility {
                newstate_pending: true,
                ..eligibility()
            },
            |_| panic!("pending state task suppresses context work"),
        ));
    }

    #[test]
    fn mac_task_unprotects_after_modify_and_always_releases_reference() {
        let steps = RefCell::new(Vec::new());
        assert_eq!(
            run_mac_context_task(
                eligibility(),
                || {
                    steps.borrow_mut().push(1);
                    Err(())
                },
                || steps.borrow_mut().push(2),
                || steps.borrow_mut().push(3),
            ),
            MacContextTaskOutcome::SendFailed
        );
        assert_eq!(*steps.borrow(), [1, 2, 3]);
    }

    #[test]
    fn phy_task_orders_rate_rescale_around_narrowing_and_widening() {
        let steps = RefCell::new(Vec::new());
        let narrow = run_phy_context_task(
            PhyContextTaskConfig {
                eligibility: eligibility(),
                mimo_enabled: true,
                old_sco: 1,
                new_sco: HT_SECONDARY_NONE,
                old_vht_width: 2,
                new_vht_width: 1,
            },
            || {
                steps.borrow_mut().push(1);
                true
            },
            |chains, sco, width| {
                steps.borrow_mut().push(2);
                assert_eq!((chains, sco, width), (2, 0, 1));
                true
            },
            || steps.borrow_mut().push(3),
        );
        assert!(narrow.going_narrow);
        assert_eq!(*steps.borrow(), [1, 2, 3]);

        steps.borrow_mut().clear();
        let wide = run_phy_context_task(
            PhyContextTaskConfig {
                old_sco: 0,
                new_sco: 1,
                old_vht_width: 1,
                new_vht_width: 2,
                ..PhyContextTaskConfig {
                    eligibility: eligibility(),
                    mimo_enabled: false,
                    old_sco: 0,
                    new_sco: 0,
                    old_vht_width: 0,
                    new_vht_width: 0,
                }
            },
            || {
                steps.borrow_mut().push(2);
                true
            },
            |chains, _, _| {
                steps.borrow_mut().push(1);
                assert_eq!(chains, 1);
                true
            },
            || steps.borrow_mut().push(3),
        );
        assert!(!wide.going_narrow);
        assert_eq!(*steps.borrow(), [1, 2, 3]);
    }
}
