use alloc::sync::Arc;
use core::mem::size_of;

use crate::{
    Action, ClassicBpfInstruction, FILTER_PATH_PENALTY, FilterBudget, MAX_INSNS_PER_PATH,
    SeccompData, SeccompExecutor, VerifiedProgram, budget::FilterCharge,
};

/// Per-filter installation metadata retained with an immutable program.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct FilterMetadata {
    /// Request audit logging when this filter supplies the winning action.
    pub log: bool,
    /// The installation requested `SECCOMP_FILTER_FLAG_SPEC_ALLOW`.
    ///
    /// Architecture mitigation is adapter-owned; retaining the bit makes the
    /// publication decision auditable without putting architecture work here.
    pub speculative_execution_allowed: bool,
    /// Opaque adapter-owned user-notification listener identity.
    ///
    /// Zero means that this filter has no listener.  The policy crate keeps
    /// no FD or task ownership; the embedding kernel resolves this identity
    /// at the action boundary.
    pub listener_id: u64,
}

/// Selected action and the metadata of the filter that supplied it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct FilterDecision {
    /// Most restrictive raw action across the complete chain.
    pub action: Action,
    /// Winning filter metadata. It is `None` when every filter returned
    /// `ALLOW`, matching Linux's lack of a selected allow filter.
    pub matched_filter: Option<FilterMetadata>,
}

struct FilterNode {
    executor: Option<Arc<dyn SeccompExecutor>>,
    metadata: FilterMetadata,
    previous: Option<Arc<FilterNode>>,
    path_cost: usize,
    filter_count: usize,
    _charge: FilterCharge,
}

/// Aligned native input storage for the x86_64 translator.
///
/// `InputProfile::NativeAlignedWords` checks logical word offsets, while its
/// native loads also require the snapshot base to be four-byte aligned. A
/// plain `[u8; SECCOMP_DATA_SIZE]` only has byte alignment, so keep that
/// precondition explicit at the one boundary where the JIT receives bytes.
#[repr(align(4))]
struct NativeInput([u8; crate::SECCOMP_DATA_SIZE]);

impl NativeInput {
    fn from_data(data: &SeccompData) -> Self {
        Self(data.native_bytes())
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for FilterNode {
    fn drop(&mut self) {
        let mut cursor = self.previous.take();
        while let Some(node) = cursor {
            match Arc::try_unwrap(node) {
                Ok(mut unique) => cursor = unique.previous.take(),
                Err(shared) => {
                    drop(shared);
                    break;
                }
            }
        }
    }
}

/// An immutable, reference-counted seccomp filter ancestry.
#[derive(Clone, Default)]
pub struct FilterChain {
    leaf: Option<Arc<FilterNode>>,
}

impl FilterChain {
    /// Returns an empty chain for a task with no filters.
    pub const fn empty() -> Self {
        Self { leaf: None }
    }

    /// Returns whether no filter is installed.
    pub fn is_empty(&self) -> bool {
        self.leaf.is_none()
    }

    /// Returns the number of programs in this ancestry.
    pub fn filter_count(&self) -> usize {
        self.leaf.as_ref().map_or(0, |leaf| leaf.filter_count)
    }

    /// Returns whether this immutable ancestry already owns `listener_id`.
    /// The adapter uses this to preserve Linux's one-listener-per-filter-tree
    /// admission rule without placing file ownership in the ABI crate.
    pub fn contains_listener_id(&self, listener_id: u64) -> bool {
        listener_id != 0
            && core::iter::successors(self.leaf.as_deref(), |node| node.previous.as_deref())
                .any(|node| node.metadata.listener_id == listener_id)
    }

    /// Returns whether any ancestor contains a user-notification listener.
    pub fn has_listener(&self) -> bool {
        core::iter::successors(self.leaf.as_deref(), |node| node.previous.as_deref())
            .any(|node| node.metadata.listener_id != 0)
    }

    /// Returns the Linux v6.12 unblinded migration path-accounting value.
    pub fn path_cost(&self) -> usize {
        self.leaf.as_ref().map_or(0, |leaf| leaf.path_cost)
    }

    /// Returns whether two task states point to the exact same immutable leaf.
    pub fn same_identity(&self, other: &Self) -> bool {
        match (&self.leaf, &other.leaf) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    /// Returns whether `self` is an ancestor of `descendant`.
    ///
    /// The empty chain is the root ancestor. This is the identity-based rule
    /// Linux uses for TSYNC eligibility; equivalent bytecode on a divergent
    /// branch is not ancestry.
    pub fn is_ancestor_of(&self, descendant: &Self) -> bool {
        if self.leaf.is_none() {
            return true;
        }
        let mut cursor = descendant.leaf.as_deref();
        while let Some(node) = cursor {
            if self
                .leaf
                .as_ref()
                .is_some_and(|ancestor| core::ptr::eq(ancestor.as_ref(), node))
            {
                return true;
            }
            cursor = node.previous.as_deref();
        }
        false
    }

    /// Test-only policy-chain construction without a platform executor.
    ///
    /// Product adapters must use [`Self::try_append_with_executor`] so the
    /// AX classic-BPF mechanism is retained with the Linux policy plan.
    #[cfg(test)]
    pub fn try_append(
        &self,
        program: VerifiedProgram,
        metadata: FilterMetadata,
        budget: &FilterBudget,
    ) -> Result<Self, FilterInstallError> {
        let action = program
            .instructions()
            .last()
            .filter(|instruction| instruction.code == crate::classic_bpf_opcode::RET_K)
            .map_or(crate::SECCOMP_RET_KILL_PROCESS, |instruction| instruction.k);
        self.try_append_with_executor(
            program,
            metadata,
            budget,
            Some(Arc::new(TestPolicyExecutor(action))),
        )
    }

    /// Allocates an immutable Linux policy leaf with its AX mechanism bridge.
    ///
    /// The executor owns the generic verified interpreter and any optional
    /// x86_64 native image. A missing executor is rejected at the adapter
    /// boundary; it is represented here only for legacy in-memory states,
    /// which fail closed on evaluation.
    pub fn try_append_with_executor(
        &self,
        program: VerifiedProgram,
        metadata: FilterMetadata,
        budget: &FilterBudget,
        executor: Option<Arc<dyn SeccompExecutor>>,
    ) -> Result<Self, FilterInstallError> {
        let path_cost = if self.is_empty() {
            program.path_charge()
        } else {
            program
                .path_charge()
                .checked_add(self.path_cost())
                .and_then(|cost| cost.checked_add(FILTER_PATH_PENALTY))
                .ok_or(FilterInstallError::PathTooLong)?
        };
        if path_cost > MAX_INSNS_PER_PATH {
            return Err(FilterInstallError::PathTooLong);
        }
        let filter_count = self
            .filter_count()
            .checked_add(1)
            .ok_or(FilterInstallError::PathTooLong)?;
        if self
            .leaf
            .as_ref()
            .is_some_and(|leaf| !leaf._charge.belongs_to(budget))
        {
            return Err(FilterInstallError::BudgetMismatch);
        }
        let charge_bytes = program
            .len()
            .checked_mul(size_of::<ClassicBpfInstruction>())
            .and_then(|bytes| bytes.checked_add(size_of::<FilterNode>()))
            .ok_or(FilterInstallError::BudgetExceeded)?;
        let charge = budget
            .try_reserve(charge_bytes)
            .map_err(|_| FilterInstallError::BudgetExceeded)?;
        let node = Arc::try_new(FilterNode {
            executor,
            metadata,
            previous: self.leaf.clone(),
            path_cost,
            filter_count,
            _charge: charge,
        })
        .map_err(|_| FilterInstallError::NoMemory)?;
        Ok(Self { leaf: Some(node) })
    }

    /// Returns whether this chain is exactly one newly linked child of
    /// `previous`.
    pub fn directly_extends(&self, previous: &Self) -> bool {
        self.leaf
            .as_ref()
            .is_some_and(|leaf| match (&leaf.previous, &previous.leaf) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            })
    }

    /// Runs every filter newest-to-oldest without allocation or mutation.
    pub fn evaluate(&self, data: &SeccompData) -> FilterDecision {
        let mut selected = Action::from_raw(crate::SECCOMP_RET_ALLOW);
        let mut matched_filter = None;
        let mut native_data = None;
        let mut cursor = self.leaf.as_ref();
        while let Some(node) = cursor {
            let raw = if let Some(executor) = node.executor.as_ref() {
                let native = native_data.get_or_insert_with(|| NativeInput::from_data(data));
                executor.execute(native.as_bytes())
            } else {
                // Production adapters always publish an AX mechanism executor
                // with each admitted Linux program.  A missing executor is
                // retained only by the policy-only construction API, whose
                // result deliberately fails closed rather than embedding a
                // second generic cBPF interpreter in the ABI crate.
                crate::SECCOMP_RET_KILL_PROCESS
            };
            let current = Action::from_raw(raw);
            // Strictly-less preserves the newest filter's DATA for ties.
            if current.precedence() < selected.precedence() {
                selected = current;
                matched_filter = Some(node.metadata);
            }
            cursor = node.previous.as_ref();
        }
        FilterDecision {
            action: selected,
            matched_filter,
        }
    }
}

#[cfg(test)]
struct TestPolicyExecutor(u32);

#[cfg(test)]
impl SeccompExecutor for TestPolicyExecutor {
    fn execute(&self, _data: &[u8]) -> u32 {
        self.0
    }
}

/// Failure while preparing an immutable filter leaf.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FilterInstallError {
    /// Allocation for the shared leaf failed.
    NoMemory,
    /// Linux v6.12's 32768 converted-instruction ancestry budget would be
    /// exceeded.
    PathTooLong,
    /// The aggregate live-program byte budget cannot cover the new node.
    BudgetExceeded,
    /// An append attempted to splice nodes from different budget domains.
    BudgetMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClassicBpfInstruction, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS,
        SECCOMP_RET_LOG, SECCOMP_RET_TRAP, classic_bpf_opcode as opcode,
    };
    use alloc::{vec, vec::Vec};

    const RET_K_PATH_CHARGE: usize = 5;
    const STACKED_RET_K_INCREMENT: usize = RET_K_PATH_CHARGE + FILTER_PATH_PENALTY;
    const MAX_RET_K_CHAIN_DEPTH: usize =
        1 + (MAX_INSNS_PER_PATH - RET_K_PATH_CHARGE) / STACKED_RET_K_INCREMENT;

    fn returning(value: u32) -> VerifiedProgram {
        VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(opcode::RET_K, 0, 0, value)])
            .unwrap()
    }

    fn data() -> SeccompData {
        SeccompData {
            number: 0,
            architecture: crate::AUDIT_ARCH_X86_64,
            instruction_pointer: 0,
            arguments: [0; 6],
        }
    }

    fn budget() -> FilterBudget {
        FilterBudget::try_new(usize::MAX).unwrap()
    }

    struct TestExecutor;

    impl SeccompExecutor for TestExecutor {
        fn execute(&self, data: &[u8]) -> u32 {
            assert_eq!(data.len(), crate::SECCOMP_DATA_SIZE);
            assert_eq!((data.as_ptr() as usize) & 3, 0);
            u32::from_ne_bytes(data[0..4].try_into().unwrap())
        }
    }

    #[test]
    fn external_executor_is_shared_with_cloned_chain() {
        let budget = budget();
        let root = FilterChain::empty();
        let program = returning(SECCOMP_RET_ALLOW);
        let executor: Arc<dyn SeccompExecutor> = Arc::new(TestExecutor);
        let chain = root
            .try_append_with_executor(program, FilterMetadata::default(), &budget, Some(executor))
            .unwrap();
        let cloned = chain.clone();
        let input = SeccompData {
            number: 17,
            architecture: crate::AUDIT_ARCH_X86_64,
            instruction_pointer: 0,
            arguments: [0; 6],
        };
        assert_eq!(chain.evaluate(&input).action.raw(), 17);
        assert_eq!(cloned.evaluate(&input).action.raw(), 17);
    }

    #[test]
    fn absent_external_executor_fails_closed() {
        let budget = budget();
        let chain = FilterChain::empty()
            .try_append_with_executor(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
                None,
            )
            .unwrap();
        let decision = chain.evaluate(&data());
        assert_eq!(decision.action.raw(), SECCOMP_RET_KILL_PROCESS);
    }

    fn returning_with_len(length: usize, value: u32) -> VerifiedProgram {
        let mut instructions = Vec::new();
        instructions.try_reserve_exact(length).unwrap();
        for _ in 1..length {
            instructions.push(ClassicBpfInstruction::new(opcode::LD_IMM, 0, 0, 0));
        }
        instructions.push(ClassicBpfInstruction::new(opcode::RET_K, 0, 0, value));
        VerifiedProgram::try_from_vec(instructions).unwrap()
    }

    #[test]
    fn immutable_identity_distinguishes_divergent_equal_programs() {
        let root = FilterChain::empty();
        let budget = budget();
        let left = root
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let right = root
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let child = left
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        assert!(root.is_ancestor_of(&left));
        assert!(left.is_ancestor_of(&child));
        assert!(!right.is_ancestor_of(&child));
        assert!(!left.same_identity(&right));
    }

    #[test]
    fn signed_precedence_is_order_independent() {
        let root = FilterChain::empty();
        let budget = budget();
        let chain = root
            .try_append(
                returning(SECCOMP_RET_LOG),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap()
            .try_append(
                returning(SECCOMP_RET_ERRNO | 9),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap()
            .try_append(
                returning(SECCOMP_RET_KILL_PROCESS),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap()
            .try_append(
                returning(SECCOMP_RET_TRAP | 7),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        assert_eq!(
            chain.evaluate(&data()).action.raw(),
            SECCOMP_RET_KILL_PROCESS
        );
    }

    #[test]
    fn equal_precedence_keeps_newest_data_and_metadata() {
        let budget = budget();
        let older = FilterChain::empty()
            .try_append(
                returning(SECCOMP_RET_ERRNO | 3),
                FilterMetadata {
                    log: false,
                    speculative_execution_allowed: false,
                    listener_id: 0,
                },
                &budget,
            )
            .unwrap();
        let newest_metadata = FilterMetadata {
            log: true,
            speculative_execution_allowed: true,
            listener_id: 0,
        };
        let chain = older
            .try_append(returning(SECCOMP_RET_ERRNO | 11), newest_metadata, &budget)
            .unwrap();
        let decision = chain.evaluate(&data());
        assert_eq!(decision.action.raw(), SECCOMP_RET_ERRNO | 11);
        assert_eq!(decision.matched_filter, Some(newest_metadata));
    }

    #[test]
    fn path_cost_includes_four_instruction_ancestor_penalty() {
        let budget = budget();
        let first = FilterChain::empty()
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let second = first
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        assert_eq!(first.path_cost(), 5);
        assert_eq!(second.path_cost(), 14);
        assert_eq!(second.filter_count(), 2);
    }

    #[test]
    fn exact_path_limit_is_accepted_and_next_append_is_atomic() {
        let budget = budget();
        let first = returning_with_len(4, SECCOMP_RET_ALLOW);
        let first_charge = first.path_charge();
        assert_eq!(first_charge, 8);
        assert_eq!(
            (MAX_INSNS_PER_PATH - first_charge) % STACKED_RET_K_INCREMENT,
            0
        );
        let mut chain = FilterChain::empty()
            .try_append(first, FilterMetadata::default(), &budget)
            .unwrap();
        for _ in 0..(MAX_INSNS_PER_PATH - first_charge) / STACKED_RET_K_INCREMENT {
            chain = chain
                .try_append(
                    returning(SECCOMP_RET_ALLOW),
                    FilterMetadata::default(),
                    &budget,
                )
                .unwrap();
        }
        assert_eq!(chain.path_cost(), MAX_INSNS_PER_PATH);
        let identity = chain.clone();
        assert!(matches!(
            chain.try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            ),
            Err(FilterInstallError::PathTooLong)
        ));
        assert!(chain.same_identity(&identity));
    }

    #[test]
    fn maximum_depth_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(|| {
                let budget = budget();
                let mut chain = FilterChain::empty();
                for _ in 0..MAX_RET_K_CHAIN_DEPTH {
                    chain = chain
                        .try_append(
                            returning(SECCOMP_RET_ALLOW),
                            FilterMetadata::default(),
                            &budget,
                        )
                        .unwrap();
                }
                drop(chain);
                assert_eq!(budget.used_bytes(), 0);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn concurrent_last_owners_release_a_deep_chain_iteratively() {
        use std::sync::{Arc as StdArc, Barrier};

        const WORKERS: usize = 8;
        let budget = budget();
        let mut chain = FilterChain::empty();
        for _ in 0..MAX_RET_K_CHAIN_DEPTH {
            chain = chain
                .try_append(
                    returning(SECCOMP_RET_ALLOW),
                    FilterMetadata::default(),
                    &budget,
                )
                .unwrap();
        }
        let barrier = StdArc::new(Barrier::new(WORKERS + 1));
        let mut handles = Vec::new();
        handles.try_reserve_exact(WORKERS).unwrap();
        for _ in 0..WORKERS {
            let owned = chain.clone();
            let barrier = barrier.clone();
            handles.push(
                std::thread::Builder::new()
                    .stack_size(64 * 1024)
                    .spawn(move || {
                        barrier.wait();
                        drop(owned);
                    })
                    .unwrap(),
            );
        }
        drop(chain);
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn aggregate_budget_rolls_back_and_refunds_on_final_drop() {
        let probe_budget = budget();
        let probe = FilterChain::empty()
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &probe_budget,
            )
            .unwrap();
        let one_node_bytes = probe_budget.used_bytes();
        drop(probe);
        assert_eq!(probe_budget.used_bytes(), 0);

        let budget = FilterBudget::try_new(one_node_bytes).unwrap();
        let first = FilterChain::empty()
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            )
            .unwrap();
        let shared = first.clone();
        assert_eq!(budget.used_bytes(), one_node_bytes);
        assert!(matches!(
            first.try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &budget,
            ),
            Err(FilterInstallError::BudgetExceeded)
        ));
        assert_eq!(budget.used_bytes(), one_node_bytes);
        drop(first);
        assert_eq!(budget.used_bytes(), one_node_bytes);
        drop(shared);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn chain_rejects_cross_budget_splicing() {
        let first_budget = budget();
        let second_budget = budget();
        let chain = FilterChain::empty()
            .try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &first_budget,
            )
            .unwrap();
        assert!(matches!(
            chain.try_append(
                returning(SECCOMP_RET_ALLOW),
                FilterMetadata::default(),
                &second_budget,
            ),
            Err(FilterInstallError::BudgetMismatch)
        ));
        assert_eq!(second_budget.used_bytes(), 0);
    }
}
