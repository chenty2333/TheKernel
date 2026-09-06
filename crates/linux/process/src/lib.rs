//! Explicit-domain process, thread-group, session, and zombie lifecycle state.
//!
//! This crate never selects a process registry or init process globally. A
//! kernel owns a [`ProcessDomain`] and passes its [`ProcessRegistry`] to
//! topology queries. The durable zombie payload is a caller-chosen type, so
//! Linux wait status, credentials, and accounting remain adapter policy.

#![no_std]
#![feature(allocator_api)]
#![warn(missing_docs)]

extern crate alloc;

mod linux_abi;
mod process;
mod process_group;
mod session;

/// A process ID, also used as session ID, process group ID, and thread ID.
pub type Pid = u32;

pub use linux_abi::{
    Clone3Args, Clone3Plan, ClonePlan, PidfdPlan, ProcessAbiError, Rusage, RusageSelector,
    SetTidPlan, TimeVal, UsageSnapshot,
};
pub use process::{
    CommittedProcessExit, CreatedSession, ExitOutcome, InitialProcessAdmission,
    PROCESS_MEMBERSHIP_LIMIT, Process, ProcessAdmission, ProcessDomain, ProcessError,
    ProcessExitAdmission, ProcessRegistry, ProcessReparentBatch, Processes, ReaperScope,
    ReparentedProcess, ScopedInitProcessAdmission, ScopedInitialProcessAdmission, ThreadAdmission,
    ThreadExitOutcome, ThreadExitTransition, ThreadIds, ThreadPublicationOutcome,
};
pub use process_group::ProcessGroup;
pub use session::Session;
