#![no_std]
#![forbid(unsafe_code)]

//! Pure Linux-visible memory-management policy contracts.
//!
//! This crate owns checked address ranges, mapping generations, bounded pin
//! and fault lifecycles, and arithmetic-only mapping planners. It deliberately
//! does not own page tables, frames, files, tasks, locks, raw user memory, or
//! architecture-specific addresses. Consumers freeze those mechanism facts,
//! call this crate, and execute the resulting plan in their own transaction.

mod error;
mod fault;
mod identity;
mod mapping;
mod mempolicy;
mod mincore;
mod pin;
mod plan;
mod range;
mod uapi;
mod userfaultfd;

pub use error::MmError;
pub use fault::{
    FaultAccess, FaultAdmission, FaultAdmissionContext, FaultAdmissionKind, FaultAdmissionPermit,
    FaultCapacity, FaultCompletionPermit, FaultDisposition, FaultFailure, FaultHandlerId, FaultKey,
    FaultLifecycleState, FaultLoad, FaultPageAddress, FaultPort, FaultRequest, FaultRequestId,
    FaultType, validate_fault_completion,
};
pub use identity::{AddressSpaceId, MappingGeneration, MappingId, PinOwner};
pub use mapping::{
    ExpectedMapping, InvalidationRange, InvalidationReason, MappingAccess, MappingKind,
    MappingSnapshot,
};
pub use mempolicy::{
    ALLOWED_NODEMASK, GetMempolicyError, MAX_NODEMASK_BITS, MPOL_BIND, MPOL_DEFAULT, MPOL_F_ADDR,
    MPOL_F_MEMS_ALLOWED, MPOL_F_NODE, MPOL_F_NUMA_BALANCING, MPOL_F_RELATIVE_NODES,
    MPOL_F_STATIC_NODES, MPOL_INTERLEAVE, MPOL_LOCAL, MPOL_MAX, MPOL_MF_MOVE, MPOL_MF_MOVE_ALL,
    MPOL_MF_STRICT, MPOL_MF_VALID, MPOL_MODE_FLAGS, MPOL_PREFERRED, MPOL_PREFERRED_MANY,
    MPOL_WEIGHTED_INTERLEAVE, MbindPlan, MempolicyError, MempolicyRequest, NR_NODE_IDS,
    effective_policy, next_interleave_node, parse_node_mask, plan_mbind, sanitize_mode_flags,
    scanned_words, validate, validate_get,
};
pub use mincore::MincorePlan;
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
pub use uapi::IoVec;
pub use userfaultfd::{
    UFFD_API, UFFD_O_CLOEXEC, UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY, UffdApiLifecycle,
    UffdApiNegotiation, UffdApiRequest, UffdApiResponse, UffdApiState, UffdCopyMode,
    UffdCopyRequest, UffdCreateFlags, UffdFaultPolicy, UffdFeatures, UffdIoctls, UffdRegisterMode,
    UffdRegistration, UffdRegistrationCommit, UffdRegistrationDeltaPlan, UffdRegistrationId,
    UffdRegistrationIntent, UffdRegistrationPlan, UffdRegistrationReplacement,
    UffdRegistrationRequest, UffdRegistrationTable, UffdResolverOutcome, UffdResolverResult,
    UffdZeroPageMode, UffdZeroPageRequest,
};

#[cfg(test)]
mod tests;
