#![no_std]
#![forbid(unsafe_code)]

//! Pure Linux v6.6 restartable-sequence policy contracts.
//!
//! This crate owns the fixed `rseq`/`rseq_cs` value layouts, explicit
//! exclusive-user-limit descriptor proofs, CPU sentinel checks, restart-gate
//! classification, and one-registration lifecycle.  It does not perform the
//! `rseq(2)` syscall, access user memory, inspect instruction bytes, manipulate
//! instruction pointers, disable IRQs, invoke a scheduler, or own task/thread
//! objects.  An embedding kernel supplies those mechanisms and maps
//! [`RseqError`] to its concrete errno type.
//!
//! Side-effecting adapters use a reservation protocol.  Fallible epoch and
//! revision transitions happen during preparation; after usercopy or an IP
//! update succeeds, the matching `commit_*`/`on_exec_success` finalize is
//! infallible.  Adapter failures use `cancel_*`, so an event revision cannot
//! strand a successful external side effect behind `EAGAIN`.

mod abi;
mod descriptor;
mod error;
mod layout;
mod registration;
mod restart;
mod thread;

pub use abi::{
    CpuCheck, CpuIdState, RSEQ_ABI_ALIGN, RSEQ_ABI_SIZE, RSEQ_AREA_ALIGN, RSEQ_AREA_SIZE,
    RSEQ_AREA_SUPPORTED_FLAGS, RSEQ_CPU_ID_REGISTRATION_FAILED, RSEQ_CPU_ID_UNINITIALIZED,
    RSEQ_CS_ALIGN, RSEQ_CS_SIGNATURE_BYTES, RSEQ_CS_SIZE, RSEQ_CS_SUPPORTED_FLAGS,
    RSEQ_FLAG_UNREGISTER, RSEQ_FLAG_SLICE_EXT_DEFAULT_ON, RseqArea, RseqCriticalSection, UserAddressLimit,
};
pub use descriptor::{
    RseqDescriptor, ValidatedRseqCriticalSection, ValidatedRseqDescriptor, validate_descriptor,
};
pub use error::{ErrnoClass, RseqErrno, RseqError};
pub use layout::{DecodePlan, DecodedRseq, RseqLayout, decode_area, decode_critical_section};
pub use registration::{
    RegisterPlan, RegistrationLifecycle, RseqEpoch, RseqRegisterPlan, RseqRegistration,
    RseqRegistrationOperation, RseqRegistrationRequest, RseqRegistrationState, RseqUnregisterPlan,
    UnregisterPlan, ValidatedRegistrationRequest,
};
pub use restart::{RestartDecision, decide_restart, restart_gate};
pub use thread::{
    ExecPlan, ForkMode, ForkPlan, ResumePlan, RseqEventMask, RseqRevision, ThreadRegisterPlan,
    ThreadRseq, ThreadUnregisterPlan,
};

/// Alias retained as the concise state name used by syscall adapters.
pub type RseqState = RseqRegistrationState;

/// Alias for a register request used by adapters that spell the operation.
pub type RseqRegisterRequest = RseqRegistrationRequest;

/// Alias for the same fixed-width request used during unregister.
pub type RseqUnregisterRequest = RseqRegistrationRequest;

/// Alias for the successful one-registration record.
pub type Registration = RseqRegistration;

/// Alias for the typed register/unregister operation selector.
pub type RegistrationOperation = RseqRegistrationOperation;
