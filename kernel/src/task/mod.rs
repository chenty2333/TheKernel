//! User task management.

mod access;
mod accounting;
pub(crate) mod coredump;
mod creds;
mod futex;
mod idmap;
mod jobctl;
mod ops;
mod process;
mod resources;
mod restart;
mod signal;
mod stat;
mod thread;
mod thread_cred;
mod timer;
mod user;

// Re-exports from split sub-modules — keep the old `crate::task::*` paths unchanged.
pub(crate) use self::{
    access::{
        PtraceCredentialMode, check_current_process_prlimit_access,
        check_current_process_ptrace_access, check_current_process_signal_access,
        check_current_ptrace_access, check_current_signal_access,
    },
    creds::{CapabilityState, Cred, CredentialSlot, Credentials, DacCredentialView},
    jobctl::{ContinueResult, StopReport},
    process::{
        CgroupNamespace, PidNamespace, TimeNamespace, UTS_FIELD_LEN, UserNamespace, UtsNamespace,
    },
    restart::*,
    thread::ProcStateHint,
};
pub use self::{
    accounting::*,
    futex::*,
    ops::*,
    process::{Mempolicy, ProcessData},
    resources::*,
    signal::*,
    stat::*,
    thread::{AsThread, Thread},
    timer::*,
    user::*,
};
