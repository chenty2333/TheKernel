//! Task-local seccomp publication and aggregate filter accounting.

use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult, LinuxError};
use axrcu::{ClearError, PublishError, RcuError};
use axsync::Mutex;
use spin::Once;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_seccomp::{
    FilterBudget, FilterChain, SeccompMode, SeccompState, StateTransitionError,
};

use super::Thread;
use crate::rcu::{SECCOMP_RETIRE_CAPACITY, SeccompRcuSlot, SeccompRetireReservation};

pub(crate) type ThreadSeccompSlot = SeccompRcuSlot<SeccompState>;

// Every mutation of a thread seccomp slot passes this gate.  TSYNC takes it
// while it validates and prepares all sibling replacements, making a stale
// sibling impossible between validation and its group publication.
static SECCOMP_PUBLICATION_GATE: Mutex<()> = Mutex::new(());

/// A terminal seccomp clear prepared before the task's irreversible exit
/// publication. The reservation makes the later clear allocation-free and
/// bounded; the old slot owner is returned only after the clear has queued its
/// independent grace-period owner.
pub(crate) struct SeccompExitRetirement {
    expected: Arc<SeccompState>,
    reservation: SeccompRetireReservation,
}

/// One exact live member participating in a TSYNC transaction.  The adapter
/// pins task membership before constructing these entries, so a recycled TID
/// cannot redirect a synchronized policy update.
pub(crate) struct SeccompTsyncTarget<'a> {
    pub(crate) tid: Pid,
    pub(crate) thread: &'a Thread,
}

/// Linux returns the offending thread ID for a normal TSYNC failure.  Keeping
/// the reason alongside it lets `TSYNC_ESRCH` convert only disappearance into
/// ESRCH while preserving ordinary ancestry/mode failure reporting.
#[derive(Debug)]
pub(crate) struct SeccompTsyncFailure {
    pub(crate) tid: Pid,
    pub(crate) error: SeccompPublicationError,
}

/// Builds the per-thread publication slot and its terminal disabled owner.
/// Disabled tasks use an empty slot, so syscall entry only performs the
/// pointer fast-bit load and does not enter the RCU domain. The disabled owner
/// is retained by the thread for cold snapshots and terminal teardown; exit
/// can therefore clear an active slot without allocating a replacement.
pub(crate) fn new_thread_seccomp(
    initial: Arc<SeccompState>,
) -> AxResult<(ThreadSeccompSlot, Arc<SeccompState>)> {
    let terminal_disabled =
        Arc::try_new(SeccompState::disabled()).map_err(|_| AxError::NoMemory)?;
    let slot = if initial.mode() == SeccompMode::Disabled {
        drop(initial);
        crate::rcu::seccomp_slot(None)?
    } else {
        crate::rcu::seccomp_slot(Some(initial))?
    };
    Ok((slot, terminal_disabled))
}

/// Errors from an all-or-nothing seccomp state publication.
#[derive(Debug)]
pub(crate) enum SeccompPublicationError {
    /// The prepared state violates the Linux seccomp transition rules.
    Transition(StateTransitionError),
    /// The independent bounded seccomp retire queue is full.
    RetireCapacity,
    /// The replacement state could not be allocated before publication.
    NoMemory,
    /// The exact state used for preparation was replaced by another writer.
    Stale,
    /// The monotonic RCU epoch cannot represent another publication.
    EpochExhausted,
    /// The boot CPU registration or task-context contract was violated.
    BadState,
}

impl SeccompPublicationError {
    pub(crate) fn into_ax_error(self) -> AxError {
        match self {
            Self::Transition(StateTransitionError::ModeConflict) => AxError::InvalidInput,
            Self::Transition(StateTransitionError::Stale) | Self::Stale => {
                LinuxError::EAGAIN.into()
            }
            Self::Transition(StateTransitionError::InvalidPreparedState)
            | Self::EpochExhausted
            | Self::BadState => AxError::BadState,
            Self::NoMemory => AxError::NoMemory,
            Self::RetireCapacity => AxError::ResourceBusy,
        }
    }
}

/// Aggregate logical bytes available to all live seccomp programs and nodes.
///
/// The limit is architecture-independent and always active. Fork and clone
/// share immutable nodes and therefore do not duplicate this charge.
pub(crate) const SECCOMP_FILTER_BUDGET_BYTES: usize = 16 * 1024 * 1024;

static SECCOMP_FILTER_BUDGET: Once<FilterBudget> = Once::new();

/// Allocates the seccomp accounting domain during kernel initialization.
///
/// Syscall entry only reads the already-published budget and never performs
/// this allocation lazily.
pub(crate) fn init_seccomp_filter_budget() -> AxResult<()> {
    SECCOMP_FILTER_BUDGET
        .try_call_once(|| {
            FilterBudget::try_new(SECCOMP_FILTER_BUDGET_BYTES).map_err(|_| AxError::NoMemory)
        })
        .map(|_| ())
}

/// Returns the boot-initialized aggregate seccomp filter budget.
pub(crate) fn seccomp_filter_budget() -> &'static FilterBudget {
    SECCOMP_FILTER_BUDGET
        .get()
        .expect("seccomp filter budget must be initialized before userspace")
}

impl Thread {
    /// Takes one atomically consistent task-local seccomp snapshot for a
    /// cold-path owner (clone, procfs, or filter preparation).
    pub(crate) fn seccomp_snapshot(&self) -> Arc<SeccompState> {
        // Do not split this into `is_empty` followed by `load`: terminal exit
        // may clear the slot between those operations. `load_if_present`
        // linearizes pointer presence with its Arc strong-count increment.
        self.seccomp
            .load_if_present()
            .unwrap_or_else(|| self.seccomp_terminal_disabled.clone())
    }

    /// Returns whether this task has an active seccomp policy. The disabled
    /// path is a single atomic load and does not enter the RCU domain.
    pub(crate) fn seccomp_active(&self) -> bool {
        !self.seccomp.is_empty()
    }

    /// Runs a read-only seccomp query from an owned immutable snapshot.
    ///
    /// Only `load_if_present` runs in the preemption-stable RCU section. The
    /// filter evaluation itself runs after that section has released its pin,
    /// while the returned Arc keeps the exact published state alive across
    /// the whole closure. This is important for syscall paths: a filter may
    /// walk an inherited chain and account execution without extending a
    /// non-preemptible section. Publication and clear still retain their
    /// pointer/epoch linearization rules, and the disabled path remains the
    /// single `is_empty` fast-bit check in the caller.
    pub(crate) fn with_seccomp_current<R>(
        &self,
        operation: impl for<'a> FnOnce(&'a SeccompState) -> R,
    ) -> Option<R> {
        let state = self.seccomp.load_if_present()?;
        Some(operation(&state))
    }

    /// Reserves the bounded retirement entry for terminal teardown before
    /// exit is committed. A full queue is reported while the task is still
    /// reversible; no lifecycle lock is held while a grace period is waited
    /// on, because terminal retirement is completed by the normal policy
    /// worker after this method has returned.
    pub(crate) fn prepare_seccomp_exit_retirement(
        &self,
    ) -> AxResult<Option<SeccompExitRetirement>> {
        let Some(expected) = self.seccomp.load_if_present() else {
            return Ok(None);
        };
        let reservation = self.seccomp.reserve_retire().map_err(|error| match error {
            RcuError::RetireCapacity => AxError::ResourceBusy,
            RcuError::UnregisteredCpu
            | RcuError::NotTaskContext
            | RcuError::CpuAlreadyRegistered
            | RcuError::CpuBusy
            | RcuError::CpuNotRegistered
            | RcuError::ReaderNestingOverflow
            | RcuError::EpochExhausted
            | RcuError::EmptySlot => AxError::BadState,
        })?;
        Ok(Some(SeccompExitRetirement {
            expected,
            reservation,
        }))
    }

    /// Completes a previously prepared terminal clear without waiting. The
    /// returned slot owner remains in the caller's custody until all exit
    /// lifecycle locks have been released.
    pub(crate) fn complete_seccomp_exit_retirement(
        &self,
        retirement: Option<SeccompExitRetirement>,
    ) -> AxResult<Arc<SeccompState>> {
        let _publication = SECCOMP_PUBLICATION_GATE.lock();
        let Some(retirement) = retirement else {
            return Ok(self.seccomp_terminal_disabled.clone());
        };
        let result = self
            .seccomp
            .clear(&retirement.expected, retirement.reservation)
            .map_err(|error| match error {
                ClearError::Stale | ClearError::EpochExhausted => AxError::BadState,
                ClearError::NotTaskContext | ClearError::BadContext => AxError::BadState,
            });
        if result.is_ok() {
            crate::rcu::wake_seccomp_retire_worker();
        }
        result
    }

    /// Returns the current Linux-visible seccomp mode.
    pub(crate) fn seccomp_mode(&self) -> SeccompMode {
        if !self.seccomp_active() {
            return SeccompMode::Disabled;
        }
        self.with_seccomp_current(SeccompState::mode)
            .unwrap_or(SeccompMode::Disabled)
    }

    /// Enters irreversible strict mode at the task-local publication point.
    pub(crate) fn try_enter_seccomp_strict(&self) -> Result<(), SeccompPublicationError> {
        let _publication = SECCOMP_PUBLICATION_GATE.lock();
        self.try_enter_seccomp_strict_locked()
    }

    fn try_enter_seccomp_strict_locked(&self) -> Result<(), SeccompPublicationError> {
        if self.seccomp.is_empty() {
            let mut replacement = SeccompState::disabled();
            replacement
                .try_enter_strict()
                .map_err(SeccompPublicationError::Transition)?;
            let replacement =
                Arc::try_new(replacement).map_err(|_| SeccompPublicationError::NoMemory)?;
            return self.publish_initial_seccomp_state(replacement);
        }
        let expected = self.seccomp.load();
        let mut replacement = (*expected).clone();
        replacement
            .try_enter_strict()
            .map_err(SeccompPublicationError::Transition)?;
        let retire = self.reserve_seccomp_retire()?;
        let replacement =
            Arc::try_new(replacement).map_err(|_| SeccompPublicationError::NoMemory)?;
        self.publish_seccomp_state(expected, replacement, retire)?;
        Ok(())
    }

    /// Publishes one already-allocated filter leaf after exact ancestry
    /// revalidation. This is task-local and is not a TSYNC transaction.
    pub(crate) fn try_publish_seccomp_filter(
        &self,
        expected: &Arc<SeccompState>,
        prepared: &FilterChain,
    ) -> Result<(), SeccompPublicationError> {
        let _publication = SECCOMP_PUBLICATION_GATE.lock();
        self.try_publish_seccomp_filter_locked(expected, prepared)
    }

    fn try_publish_seccomp_filter_locked(
        &self,
        expected: &Arc<SeccompState>,
        prepared: &FilterChain,
    ) -> Result<(), SeccompPublicationError> {
        let expected_filters = expected.filters();
        let mut replacement = (**expected).clone();
        replacement
            .try_publish_filter(&expected_filters, prepared)
            .map_err(SeccompPublicationError::Transition)?;
        let replacement =
            Arc::try_new(replacement).map_err(|_| SeccompPublicationError::NoMemory)?;
        if self.seccomp.is_empty() {
            if !Arc::ptr_eq(expected, &self.seccomp_terminal_disabled) {
                return Err(SeccompPublicationError::Stale);
            }
            return self.publish_initial_seccomp_state(replacement);
        }
        let retire = self.reserve_seccomp_retire()?;
        self.publish_seccomp_state(expected.clone(), replacement, retire)?;
        Ok(())
    }

    /// Atomically synchronize one already-verified new filter leaf to every
    /// pinned member of a thread group.  All fallible allocations, ancestry
    /// checks, and retire reservations complete before the first slot is
    /// published.  The publication gate also serializes ordinary seccomp
    /// writers, so the commit phase has no stale-writer path.
    pub(crate) fn try_publish_seccomp_tsync(
        &self,
        expected: &Arc<SeccompState>,
        prepared: &FilterChain,
        targets: &[SeccompTsyncTarget<'_>],
    ) -> Result<(), SeccompTsyncFailure> {
        let _publication = SECCOMP_PUBLICATION_GATE.lock();
        let expected_filters = expected.filters();
        let mut caller_replacement = (**expected).clone();
        caller_replacement
            .try_publish_filter(&expected_filters, prepared)
            .map_err(|error| SeccompTsyncFailure {
                tid: self.kernel_tid(),
                error: SeccompPublicationError::Transition(error),
            })?;
        let replacement = Arc::try_new(caller_replacement).map_err(|_| SeccompTsyncFailure {
            tid: self.kernel_tid(),
            error: SeccompPublicationError::NoMemory,
        })?;

        struct PreparedTarget<'a> {
            thread: &'a Thread,
            expected: Arc<SeccompState>,
            retire: Option<SeccompRetireReservation>,
        }
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(targets.len())
            .map_err(|_| SeccompTsyncFailure {
                tid: self.kernel_tid(),
                error: SeccompPublicationError::NoMemory,
            })?;
        for target in targets {
            let current = target.thread.seccomp_snapshot();
            current
                .prepare_synchronized_from(&replacement)
                .map_err(|error| SeccompTsyncFailure {
                    tid: target.tid,
                    error: SeccompPublicationError::Transition(error),
                })?;
            let retire = if target.thread.seccomp.is_empty() {
                if !Arc::ptr_eq(&current, &target.thread.seccomp_terminal_disabled) {
                    return Err(SeccompTsyncFailure {
                        tid: target.tid,
                        error: SeccompPublicationError::Stale,
                    });
                }
                None
            } else {
                Some(target.thread.reserve_seccomp_retire().map_err(|error| {
                    SeccompTsyncFailure {
                        tid: target.tid,
                        error,
                    }
                })?)
            };
            plans.push(PreparedTarget {
                thread: target.thread,
                expected: current,
                retire,
            });
        }

        // The gate makes every expected pointer stable. A reserved RCU
        // publication cannot fail from queue pressure, therefore any failure
        // here is an internal invariant breach rather than a partial Linux
        // syscall result.
        for plan in plans {
            let result = match plan.retire {
                Some(retire) => {
                    plan.thread
                        .publish_seccomp_state(plan.expected, replacement.clone(), retire)
                }
                None => plan
                    .thread
                    .publish_initial_seccomp_state(replacement.clone())
                    .map(|()| plan.expected),
            };
            if result.is_err() {
                panic!("seccomp TSYNC publication changed despite publication gate");
            }
        }
        Ok(())
    }

    fn publish_initial_seccomp_state(
        &self,
        replacement: Arc<SeccompState>,
    ) -> Result<(), SeccompPublicationError> {
        self.seccomp
            .publish_if_empty(replacement)
            .map_err(|error| match error {
                PublishError::Stale(_) => SeccompPublicationError::Stale,
                PublishError::EpochExhausted(_) => SeccompPublicationError::EpochExhausted,
            })?;
        crate::rcu::wake_seccomp_retire_worker();
        Ok(())
    }

    fn reserve_seccomp_retire(&self) -> Result<SeccompRetireReservation, SeccompPublicationError> {
        match self.seccomp.reserve_retire() {
            Ok(reservation) => Ok(reservation),
            Err(RcuError::RetireCapacity) => {
                // Exit and installation are task-context operations. Reclaim
                // a finite batch before reporting bounded pressure to Linux.
                crate::rcu::drain_seccomp_retire(SECCOMP_RETIRE_CAPACITY);
                self.seccomp.reserve_retire().map_err(|error| match error {
                    RcuError::RetireCapacity => SeccompPublicationError::RetireCapacity,
                    RcuError::UnregisteredCpu
                    | RcuError::NotTaskContext
                    | RcuError::CpuAlreadyRegistered
                    | RcuError::CpuBusy
                    | RcuError::CpuNotRegistered
                    | RcuError::ReaderNestingOverflow
                    | RcuError::EpochExhausted
                    | RcuError::EmptySlot => SeccompPublicationError::BadState,
                })
            }
            Err(
                RcuError::UnregisteredCpu
                | RcuError::NotTaskContext
                | RcuError::CpuAlreadyRegistered
                | RcuError::CpuBusy
                | RcuError::CpuNotRegistered
                | RcuError::ReaderNestingOverflow
                | RcuError::EpochExhausted
                | RcuError::EmptySlot,
            ) => Err(SeccompPublicationError::BadState),
        }
    }

    fn publish_seccomp_state(
        &self,
        expected: Arc<SeccompState>,
        replacement: Arc<SeccompState>,
        retire: SeccompRetireReservation,
    ) -> Result<Arc<SeccompState>, SeccompPublicationError> {
        let result = self
            .seccomp
            .publish(replacement, &expected, retire)
            .map_err(|error| match error {
                PublishError::Stale(_) => SeccompPublicationError::Stale,
                PublishError::EpochExhausted(_) => SeccompPublicationError::EpochExhausted,
            });
        if result.is_ok() {
            crate::rcu::wake_seccomp_retire_worker();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};

    use spin::Mutex;
    use thekernel_linux_seccomp::{
        ClassicBpfInstruction, FilterMetadata, SECCOMP_RET_ALLOW, VerifiedProgram,
    };

    use super::*;

    // These fixtures intentionally exercise the production SECCOMP_DOMAIN.
    // Serialize the tests so one test cannot drain another test's retire
    // entry while the domain remains shared across the test process.
    static SECCOMP_TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn allow_filter() -> VerifiedProgram {
        VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(
            0x06, // Linux classic BPF_RET | BPF_K.
            0,
            0,
            SECCOMP_RET_ALLOW,
        )])
        .unwrap()
    }

    fn append_allow_filter(root: &FilterChain, budget: &FilterBudget) -> FilterChain {
        let program = allow_filter();
        let executor = crate::seccomp_jit::try_compile(&program).unwrap();
        root.try_append_with_executor(program, FilterMetadata::default(), budget, Some(executor))
            .unwrap()
    }

    #[test]
    fn filtered_state_can_be_retired_through_its_independent_rcu_slot() {
        let _serial = SECCOMP_TEST_SERIAL.lock();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = FilterChain::empty();
        let leaf = append_allow_filter(&root, &budget);
        let mut filtered = SeccompState::disabled();
        filtered.try_publish_filter(&root, &leaf).unwrap();
        drop(leaf);
        assert!(budget.used_bytes() > 0);

        let initial = Arc::new(filtered);
        let slot = crate::rcu::seccomp_slot(Some(initial.clone())).unwrap();
        assert_eq!(Arc::strong_count(&initial), 2);
        slot.with_current(|state| {
            assert_eq!(state.mode(), SeccompMode::Filter);
            assert_eq!(state.filter_count(), 1);
        });
        assert_eq!(Arc::strong_count(&initial), 2);
        let expected = slot.load();
        let retire = slot.reserve_retire().unwrap();
        let retired = match slot.publish(Arc::new(SeccompState::disabled()), &expected, retire) {
            Ok(retired) => retired,
            Err(_) => panic!("fresh seccomp slot publication unexpectedly failed"),
        };
        drop(expected);
        assert_eq!(retired.filter_count(), 1);
        assert!(budget.used_bytes() > 0);
        drop(retired);
        drop(initial);
        crate::rcu::drain_seccomp_retire(1);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn stale_seccomp_publication_keeps_the_new_state() {
        let _serial = SECCOMP_TEST_SERIAL.lock();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = FilterChain::empty();
        let leaf = append_allow_filter(&root, &budget);
        let mut filtered = SeccompState::disabled();
        filtered.try_publish_filter(&root, &leaf).unwrap();
        drop(leaf);

        let initial = Arc::new(filtered);
        let slot = crate::rcu::seccomp_slot(Some(initial.clone())).unwrap();
        let stale = slot.load();
        let first = slot.reserve_retire().unwrap();
        let current = match slot.publish(Arc::new(SeccompState::disabled()), &stale, first) {
            Ok(current) => current,
            Err(_) => panic!("fresh seccomp slot publication unexpectedly failed"),
        };
        drop(current);
        let second = slot.reserve_retire().unwrap();
        let result = slot.publish(Arc::new(SeccompState::disabled()), &stale, second);
        assert!(matches!(result, Err(PublishError::Stale(_))));
        assert_eq!(slot.load().mode(), SeccompMode::Disabled);
        drop(stale);
        drop(initial);
        crate::rcu::drain_seccomp_retire(2);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn inherited_filter_chain_keeps_budget_owner_alive_independently() {
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = FilterChain::empty();
        let leaf = append_allow_filter(&root, &budget);
        let mut parent = SeccompState::disabled();
        parent.try_publish_filter(&root, &leaf).unwrap();
        drop(leaf);
        let child = parent.clone();
        drop(parent);

        assert_eq!(child.filter_count(), 1);
        assert_eq!(
            child
                .evaluate(&thekernel_linux_seccomp::SeccompData {
                    number: 0,
                    architecture: 0xc000_003e,
                    instruction_pointer: 0,
                    arguments: [0; 6],
                })
                .action
                .raw(),
            SECCOMP_RET_ALLOW
        );
        assert!(budget.used_bytes() > 0);
        drop(child);
        assert_eq!(budget.used_bytes(), 0);
    }
}
