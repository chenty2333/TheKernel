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
