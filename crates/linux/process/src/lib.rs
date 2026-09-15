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
mod wait;

/// A process ID, also used as session ID, process group ID, and thread ID.
pub type Pid = u32;

pub use linux_abi::{
    AT_VECTOR_SIZE, AT_VECTOR_SIZE_ARCH, AT_VECTOR_SIZE_BASE, CLONE3_ONLY_FLAGS, Clone3Args,
    Clone3Plan, ClonePlan, PidfdPlan, ProcessAbiError, Rusage, RusageSelector, SAVED_AUXV_BYTES,
    SetTidPlan, TASK_COMM_LEN, TaskComm, TimeVal, UsageSnapshot, clone_flag_admission,
    prctl_set_name_read_bound, proc_stat_comm_field, ptrace_options, saved_auxv_image,
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
pub use wait::{
    WAIT4_OPTIONS_ALLOWED, WAITID_EVENT_FLAGS, WAITID_OPTIONS_ALLOWED, WaitEventKind,
    WaitEventSelection, WaitEventState, WaitIdError, WaitIdType, select_child_event,
    validate_id_type, zombie_is_delayed,
};
