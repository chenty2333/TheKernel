#![no_std]
#![forbid(unsafe_code)]

//! Pure Linux-visible memory-management policy contracts.
//!
//! This crate owns checked address ranges, mapping generations, bounded pin
//! and fault lifecycles, and arithmetic-only mapping planners. It deliberately
//! does not own page tables, frames, files, tasks, locks, raw user memory, or
//! architecture-specific addresses. Consumers freeze those mechanism facts,
//! call this crate, and execute the resulting plan in their own transaction.

mod brk;
mod charge;
mod error;
mod fault;
mod identity;
mod madvise;
mod mapping;
mod memfd;
mod mempolicy;
mod mincore;
mod mmap;
mod msync;
mod pin;
mod plan;
mod range;
mod release;
mod uapi;
mod userfaultfd;

pub use brk::{
    BrkAdmission, STACK_GUARD_GAP_DEFAULT, StartGap, align_up, check_data_rlimit, classify_brk,
    data_segment_size, growth_crosses_next_guard_gap, growth_fits_task_size,
    growth_start_in_range, vm_start_gap,
};
pub use charge::{
    ReservePlan, ReserveRequest, derive_accountable, derive_noreserve, mapping_kind_is_shared,
    plan_reserve,
};
pub use error::MmError;
pub use fault::{
    FaultAccess, FaultAdmission, FaultAdmissionContext, FaultAdmissionKind, FaultAdmissionPermit,
    FaultCapacity, FaultCompletionPermit, FaultDisposition, FaultFailure, FaultHandlerId, FaultKey,
    FaultLifecycleState, FaultLoad, FaultPageAddress, FaultPort, FaultRequest, FaultRequestId,
    FaultType, validate_fault_completion,
};
pub use identity::{AddressSpaceId, MappingGeneration, MappingId, PinOwner};
pub use madvise::{
    Advice, AdviceAvailability, MADV_COLD, MADV_COLLAPSE, MADV_DODUMP, MADV_DOFORK, MADV_DONTDUMP,
    MADV_DONTFORK, MADV_DONTNEED, MADV_DONTNEED_LOCKED, MADV_FREE, MADV_GUARD_INSTALL,
    MADV_GUARD_REMOVE, MADV_HUGEPAGE, MADV_HWPOISON, MADV_KEEPONFORK, MADV_MERGEABLE,
    MADV_NOHUGEPAGE, MADV_NORMAL, MADV_PAGEOUT, MADV_POPULATE_READ, MADV_POPULATE_WRITE,
    MADV_RANDOM, MADV_REMOVE, MADV_SEQUENTIAL, MADV_SOFT_OFFLINE, MADV_UNMERGEABLE, MADV_WILLNEED,
    MADV_WIPEONFORK, advice_refused_on_droppable, advice_valid, process_madvise_remote_valid,
};
pub use mapping::{
    ExpectedMapping, InvalidationRange, InvalidationReason, MappingAccess, MappingKind,
    MappingSnapshot,
};
pub use memfd::{
    AddSeals, F_ALL_SEALS, F_SEAL_EXEC, F_SEAL_FUTURE_WRITE, F_SEAL_GROW, F_SEAL_SEAL,
    F_SEAL_SHRINK, F_SEAL_WRITE, F_WRITE_SEALS, MEMFD_NAME_MAX, MEMFD_NOEXEC_SCOPE_EXEC,
    MEMFD_NOEXEC_SCOPE_NOEXEC_ENFORCED, MEMFD_NOEXEC_SCOPE_NOEXEC_SEAL, MFD_ALL_FLAGS,
    MFD_ALLOW_SEALING, MFD_CLOEXEC, MFD_EXEC, MFD_HUGE_MASK, MFD_HUGE_SHIFT, MFD_HUGETLB,
    MFD_NAME_COPY_LEN, MFD_NAME_MAX_LEN, MFD_NAME_PREFIX_LEN, MFD_NOEXEC_SEAL, MemfdPlan,
    close_on_exec, inode_mode, name_within_limit, plan_add_seals, sanitize_flags,
    write_seal_needs_quiescence,
};
pub use mempolicy::{
    ALLOWED_NODEMASK, GetMempolicyError, MAX_NODEMASK_BITS, MPOL_BIND, MPOL_DEFAULT, MPOL_F_ADDR,
    MPOL_F_MEMS_ALLOWED, MPOL_F_NODE, MPOL_F_NUMA_BALANCING, MPOL_F_RELATIVE_NODES,
    MPOL_F_STATIC_NODES, MPOL_INTERLEAVE, MPOL_LOCAL, MPOL_MAX, MPOL_MF_MOVE, MPOL_MF_MOVE_ALL,
    MPOL_MF_STRICT, MPOL_MF_VALID, MPOL_MODE_FLAGS, MPOL_PREFERRED, MPOL_PREFERRED_MANY,
    MPOL_USER_NODEMASK_FLAGS, MPOL_WEIGHTED_INTERLEAVE, EffectiveMempolicy, MbindPlan,
    MempolicyError, MempolicyRequest, NR_NODE_IDS, effective_policy, next_interleave_node,
    parse_node_mask, plan_mbind, rejects_scanned_word, reported_policy, sanitize_mode_flags,
    scanned_words, stores_user_nodemask, validate, validate_get,
};
pub use mincore::MincorePlan;
pub use mmap::{
    LEGACY_MAP_MASK, MAP_SYNC, PAGE_SIZE, data_limit_admits_growth, is_data_mapping,
    map_shared_validate_errno,
};
pub use msync::{
    MS_ASYNC, MS_INVALIDATE, MS_SYNC, MsyncResult, MsyncStep, msync_flags_valid, msync_result,
    msync_step,
};
pub use pin::{
    LifecycleProgress, MutationBlocker, PinAccess, PinAccounting, PinBudget, PinBudgetCharge,
    PinDuration, PinLeaseView, PinQuota, PinRegistry, PinRegistryState, PinRequest, PinReservation,
    PinSnapshot, PinToken, PinUse, TeardownReport,
};
pub use plan::{
    AffineRelocation, MemlockLimit, MemlockPlan, PageCoveringPlan, RemapGeometry,
    RemapSegmentGeometry, relocate_affine_origin,
};
pub use range::{PageRange, PageSize, UserRange};
pub use release::{FreeMemFacts, MreleaseEligible, eligibility, task_dying, task_will_free_mem};
pub use uapi::IoVec;
pub use userfaultfd::{
    UFFD_API, UFFD_O_CLOEXEC, UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY, UffdAdmissionFacts,
    UffdApiLifecycle, UffdApiNegotiation, UffdApiRequest, UffdApiResponse, UffdApiState,
    UffdCopyMode, UffdCopyRequest, UffdCreateFlags, UffdFaultPolicy, UffdFeatures, UffdIoctls,
    UffdRegisterMode, UffdRegistration, UffdRegistrationCommit, UffdRegistrationDeltaPlan,
    UffdRegistrationId, UffdRegistrationIntent, UffdRegistrationPlan, UffdRegistrationReplacement,
    UffdRegistrationRequest, UffdRegistrationTable, UffdResolverOutcome, UffdResolverResult,
    UffdZeroPageMode, UffdZeroPageRequest, admit_creation, admit_raw_creation,
    uffd_can_register_droppable_vma,
};

#[cfg(test)]
mod tests;
