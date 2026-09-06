use crate::{FilterChain, FilterDecision, SeccompData};

/// Linux seccomp mode attached to one task.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum SeccompMode {
    /// No syscall filtering.
    #[default]
    Disabled = 0,
    /// Fixed read/write/exit/rt_sigreturn allowlist.
    Strict = 1,
    /// One or more immutable classic-BPF filters.
    Filter = 2,
}

/// Complete inheritable seccomp state for one Linux task.
#[derive(Clone, Default)]
pub struct SeccompState {
    mode: SeccompMode,
    filters: FilterChain,
}

impl SeccompState {
    /// Returns an unfiltered task state.
    pub const fn disabled() -> Self {
        Self {
            mode: SeccompMode::Disabled,
            filters: FilterChain::empty(),
        }
    }

    /// Returns the current Linux-visible mode.
    pub const fn mode(&self) -> SeccompMode {
        self.mode
    }

    /// Returns an immutable snapshot of the filter ancestry.
    pub fn filters(&self) -> FilterChain {
        self.filters.clone()
    }

    /// Returns the number of immutable programs in the current ancestry
    /// without cloning its leaf reference.
    pub fn filter_count(&self) -> usize {
        self.filters.filter_count()
    }

    /// Evaluates the current immutable ancestry without cloning its leaf.
    ///
    /// Kernel adapters first copy the complete task state at their single
    /// publication point, then call this method after releasing that lock.
    pub fn evaluate(&self, data: &SeccompData) -> FilterDecision {
        self.filters.evaluate(data)
    }

    /// Enters strict mode. No later mode transition is permitted.
    pub fn try_enter_strict(&mut self) -> Result<(), StateTransitionError> {
        if self.mode != SeccompMode::Disabled || !self.filters.is_empty() {
            return Err(StateTransitionError::ModeConflict);
        }
        self.mode = SeccompMode::Strict;
        Ok(())
    }

    /// Commits a preallocated filter leaf after exact-state revalidation.
    ///
    /// `expected` is the caller's snapshot from before allocation. `prepared`
    /// must directly extend it. A stale writer or malformed plan leaves this
    /// task unchanged.
    pub fn try_publish_filter(
        &mut self,
        expected: &FilterChain,
        prepared: &FilterChain,
    ) -> Result<(), StateTransitionError> {
        if self.mode == SeccompMode::Strict {
            return Err(StateTransitionError::ModeConflict);
        }
        if !self.filters.same_identity(expected) {
            return Err(StateTransitionError::Stale);
        }
        if !prepared.directly_extends(expected) {
            return Err(StateTransitionError::InvalidPreparedState);
        }
        self.filters = prepared.clone();
        self.mode = SeccompMode::Filter;
        Ok(())
    }

    /// Classifies whether this live sibling may receive the caller's TSYNC
    /// state.
    pub fn sync_eligibility(&self, caller: &Self) -> SyncEligibility {
        if caller.mode != SeccompMode::Filter || caller.filters.is_empty() {
            return SyncEligibility::CallerNotFiltered;
        }
        match self.mode {
            SeccompMode::Disabled => SyncEligibility::Eligible,
            SeccompMode::Filter if self.filters.is_ancestor_of(&caller.filters) => {
                SyncEligibility::Eligible
            }
            SeccompMode::Filter => SyncEligibility::DivergentFilter,
            SeccompMode::Strict => SyncEligibility::ModeConflict,
        }
    }

    /// Prepares the caller state for a later group-wide synchronized commit.
    ///
    /// This method never mutates the sibling. Task-group locking, fallible
    /// storage preparation, `no_new_privs` propagation, and the final
    /// all-or-nothing publication remain adapter-owned.
    pub fn prepare_synchronized_from(&self, caller: &Self) -> Result<Self, StateTransitionError> {
        match self.sync_eligibility(caller) {
            SyncEligibility::Eligible => Ok(caller.clone()),
            SyncEligibility::CallerNotFiltered => Err(StateTransitionError::InvalidPreparedState),
            SyncEligibility::ModeConflict | SyncEligibility::DivergentFilter => {
                Err(StateTransitionError::ModeConflict)
            }
        }
    }
}

/// Result of Linux TSYNC ancestry validation for one sibling.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SyncEligibility {
    /// The sibling is disabled or points to an ancestor of the caller.
    Eligible,
    /// The caller has no valid filter state to synchronize.
    CallerNotFiltered,
    /// The sibling is in strict mode.
    ModeConflict,
    /// The sibling's immutable leaf is on another branch.
    DivergentFilter,
}

/// Failure while publishing a prepared task-state transition.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StateTransitionError {
    /// The current mode cannot enter the requested mode.
    ModeConflict,
    /// Another writer changed the exact leaf after preparation.
    Stale,
    /// The proposed state is not the prepared child of the expected leaf.
    InvalidPreparedState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClassicBpfInstruction, FilterBudget, FilterMetadata, SECCOMP_RET_ALLOW, VerifiedProgram,
        classic_bpf_opcode as opcode,
    };
    use alloc::vec;

    fn append(chain: &FilterChain, budget: &FilterBudget) -> FilterChain {
        chain
            .try_append(
                VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(
                    opcode::RET_K,
                    0,
                    0,
                    SECCOMP_RET_ALLOW,
                )])
                .unwrap(),
                FilterMetadata::default(),
                budget,
            )
            .unwrap()
    }

    #[test]
    fn prepared_publication_detects_stale_and_wrong_parent() {
        let mut state = SeccompState::disabled();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let root = state.filters();
        let first = append(&root, &budget);
        state.try_publish_filter(&root, &first).unwrap();

        let divergent = append(&root, &budget);
        assert_eq!(
            state.try_publish_filter(&root, &divergent),
            Err(StateTransitionError::Stale)
        );
        let current = state.filters();
        assert_eq!(
            state.try_publish_filter(&current, &divergent),
            Err(StateTransitionError::InvalidPreparedState)
        );
        assert!(state.filters().same_identity(&first));
    }

    #[test]
    fn strict_mode_is_irreversible() {
        let mut state = SeccompState::disabled();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        state.try_enter_strict().unwrap();
        assert_eq!(
            state.try_enter_strict(),
            Err(StateTransitionError::ModeConflict)
        );
        let root = FilterChain::empty();
        let filter = append(&root, &budget);
        assert_eq!(
            state.try_publish_filter(&root, &filter),
            Err(StateTransitionError::ModeConflict)
        );
    }

    #[test]
    fn tsync_accepts_disabled_and_ancestor_but_rejects_divergence() {
        let root = FilterChain::empty();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let first = append(&root, &budget);
        let second = append(&first, &budget);

        let mut caller = SeccompState::disabled();
        caller.try_publish_filter(&root, &first).unwrap();
        caller.try_publish_filter(&first, &second).unwrap();
        let disabled = SeccompState::disabled();
        assert_eq!(
            disabled.sync_eligibility(&caller),
            SyncEligibility::Eligible
        );

        let mut ancestor = SeccompState::disabled();
        ancestor.try_publish_filter(&root, &first).unwrap();
        assert_eq!(
            ancestor.sync_eligibility(&caller),
            SyncEligibility::Eligible
        );

        let divergent_leaf = append(&root, &budget);
        let mut divergent = SeccompState::disabled();
        divergent
            .try_publish_filter(&root, &divergent_leaf)
            .unwrap();
        assert_eq!(
            divergent.sync_eligibility(&caller),
            SyncEligibility::DivergentFilter
        );
    }

    #[test]
    fn sync_preparation_is_non_mutating_and_shares_exact_leaf() {
        let root = FilterChain::empty();
        let budget = FilterBudget::try_new(usize::MAX).unwrap();
        let leaf = append(&root, &budget);
        let mut caller = SeccompState::disabled();
        caller.try_publish_filter(&root, &leaf).unwrap();
        let sibling = SeccompState::disabled();
        let prepared = sibling.prepare_synchronized_from(&caller).unwrap();
        assert_eq!(sibling.mode(), SeccompMode::Disabled);
        assert!(prepared.filters().same_identity(&caller.filters()));
        assert_eq!(prepared.filter_count(), 1);
        assert_eq!(
            prepared
                .evaluate(&SeccompData {
                    number: 0,
                    architecture: crate::AUDIT_ARCH_X86_64,
                    instruction_pointer: 0,
                    arguments: [0; 6],
                })
                .action
                .raw(),
            SECCOMP_RET_ALLOW
        );
    }
}
