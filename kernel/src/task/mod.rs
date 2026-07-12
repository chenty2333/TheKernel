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
        check_current_ptrace_access, check_current_signal_access, may_begin_gid_map_write,
        may_begin_uid_map_write, may_update_setgroups_policy, may_write_gid_map, may_write_uid_map,
    },
    creds::{CapabilityState, Cred, CredentialSlot, Credentials, DacCredentialView},
    idmap::{ID_MAP_MAX_EXTENTS, IdMapInputExtent, Kgid, Kuid, validate_id_map_input},
    jobctl::{ContinueResult, StopReport},
    process::{
        CgroupNamespace, CommittedProcessExit, InitialProcessThreadAdmission,
        PendingThreadPublication, PidNamespace, Process, ProcessGroup, ProcessThreadAdmission,
        Session, ThreadExitTransition, TimeNamespace, UTS_FIELD_LEN, UserNamespace, UtsNamespace,
        ZombieSnapshot, init_process_domain, process_domain, process_error,
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
