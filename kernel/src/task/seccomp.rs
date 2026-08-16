//! Task-local seccomp publication and aggregate filter accounting.

use core::{mem, sync::atomic::Ordering};

use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;
use spin::Once;
use thekernel_linux_seccomp::{
    FilterBudget, FilterChain, SeccompMode, SeccompState, StateTransitionError,
};

use super::Thread;

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
    /// Takes one atomically consistent task-local seccomp snapshot.
    ///
    /// The spin critical section performs only the immutable state clone;
    /// filter evaluation and program preparation happen after it is released.
    pub(crate) fn seccomp_snapshot(&self) -> SeccompState {
        if !self
            .seccomp_active
            .load(core::sync::atomic::Ordering::Acquire)
        {
            return SeccompState::disabled();
        }
        self.seccomp.lock().clone()
    }

    /// Returns the current Linux-visible seccomp mode.
    pub(crate) fn seccomp_mode(&self) -> SeccompMode {
        self.seccomp_snapshot().mode()
    }

    /// Enters irreversible strict mode at the task-local publication point.
    pub(crate) fn try_enter_seccomp_strict(&self) -> Result<(), StateTransitionError> {
        let mut state = self.seccomp.lock();
        state.try_enter_strict()?;
        self.seccomp_active
            .store(true, core::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Publishes one already-allocated filter leaf after exact ancestry
    /// revalidation. This is task-local and is not a TSYNC transaction.
    pub(crate) fn try_publish_seccomp_filter(
        &self,
        expected: &FilterChain,
        prepared: &FilterChain,
    ) -> Result<(), StateTransitionError> {
        let mut state = self.seccomp.lock();
        state.try_publish_filter(expected, prepared)?;
        self.seccomp_active
            .store(true, core::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Detaches this task's seccomp ownership after irreversible exit commit.
    ///
    /// The old immutable ancestry is returned so its potentially long chain
    /// destruction and aggregate-budget release happen after the publication
    /// lock and every process-lifecycle lock have been released.  A stale
    /// external `Arc<TaskInner>` may keep the exited [`Thread`] alive, but it
    /// must not keep task-local filter ownership or its global charge alive.
    pub(crate) fn retire_seccomp_after_exit(&self) -> SeccompState {
        retire_seccomp_state(&self.seccomp, &self.seccomp_active)
    }
}

fn retire_seccomp_state(
    publication: &SpinNoIrq<SeccompState>,
    active: &core::sync::atomic::AtomicBool,
) -> SeccompState {
    let mut state = publication.lock();
    let retired = mem::take(&mut *state);
    // Publish the terminal disabled fast path only after the authoritative
    // slot is empty. Readers that observed the old `true` value serialize on
    // the slot and clone Disabled; readers that observe `false` can skip it.
    active.store(false, Ordering::Release);
    retired
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::sync::atomic::AtomicBool;

    use thekernel_linux_seccomp::{
        ClassicBpfInstruction, FilterMetadata, SECCOMP_RET_ALLOW, VerifiedProgram,
    };

    use super::*;

    #[test]
    fn terminal_retirement_detaches_charge_before_task_object_drop() {
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = FilterChain::empty();
        let leaf = root
            .try_append(
                VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(
                    0x06, // Linux classic BPF_RET | BPF_K.
                    0,
                    0,
                    SECCOMP_RET_ALLOW,
                )])
                .unwrap(),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let mut filtered = SeccompState::disabled();
        filtered.try_publish_filter(&root, &leaf).unwrap();
        drop(leaf);
        assert!(budget.used_bytes() > 0);

        let publication = SpinNoIrq::new(filtered);
        let active = AtomicBool::new(true);
        let retired = retire_seccomp_state(&publication, &active);

        assert_eq!(publication.lock().mode(), SeccompMode::Disabled);
        assert!(!active.load(Ordering::Acquire));
        assert_eq!(retired.filter_count(), 1);
        assert!(budget.used_bytes() > 0);
        let retired_again = retire_seccomp_state(&publication, &active);
        assert_eq!(retired_again.mode(), SeccompMode::Disabled);
        assert_eq!(retired_again.filter_count(), 0);
        drop(retired_again);
        assert!(budget.used_bytes() > 0);
        drop(retired);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn retiring_one_inherited_slot_preserves_the_other_owner() {
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = FilterChain::empty();
        let leaf = root
            .try_append(
                VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(
                    0x06, // Linux classic BPF_RET | BPF_K.
                    0,
                    0,
                    SECCOMP_RET_ALLOW,
                )])
                .unwrap(),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let mut parent_state = SeccompState::disabled();
        parent_state.try_publish_filter(&root, &leaf).unwrap();
        let child_state = parent_state.clone();
        drop(leaf);

        let parent = SpinNoIrq::new(parent_state);
        let parent_active = AtomicBool::new(true);
        let child = SpinNoIrq::new(child_state);
        let child_active = AtomicBool::new(true);
        let retired_parent = retire_seccomp_state(&parent, &parent_active);
        drop(retired_parent);

        assert!(budget.used_bytes() > 0);
        assert_eq!(child.lock().filter_count(), 1);
        assert_eq!(
            child
                .lock()
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

        let retired_child = retire_seccomp_state(&child, &child_active);
        drop(retired_child);
        assert_eq!(budget.used_bytes(), 0);
    }
}
