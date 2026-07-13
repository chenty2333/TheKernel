//! User task management.

mod access;
mod accounting;
pub(crate) mod coredump;
mod creds;
mod exec_cred;
mod futex;
mod jobctl;
mod ops;
mod process;
mod resources;
mod restart;
pub(crate) mod security;
mod signal;
mod stat;
mod thread;
mod thread_cred;
mod timer;
mod user;

// Re-exports from split sub-modules — keep the old `crate::task::*` paths unchanged.
pub(crate) use thekernel_linux_cred::{
    CredError, ID_MAP_MAX_EXTENTS, IdMap, IdMapInputExtent, Kgid, Kuid, UserGid, UserUid,
    validate_id_map_input,
};

pub(crate) use self::{
    access::{
        PtraceAccessMode, check_current_process_prlimit_access,
        check_current_process_ptrace_access, check_current_process_signal_access,
        check_current_ptrace_image_snapshot, check_current_signal_access,
        check_current_thread_ptrace_image_access, may_begin_gid_map_write, may_begin_uid_map_write,
        may_update_setgroups_policy, may_write_gid_map, may_write_uid_map, ns_capable,
        signal_credential_allows,
    },
    creds::{CapabilityState, Cred, CredentialSlot, Credentials, DacCredentialView},
    exec_cred::{
        ExecAuxIdentity, ExecCredentialEffects, ExecCredentialRequest,
        ExecCredentialSecurityContext, FileCapabilities, PreparedExecCredential,
        SECURITY_CAPABILITY_XATTR_NAME, parse_file_capabilities,
    },
    jobctl::{ContinueResult, PtraceSession, StopReport},
    process::{
        CgroupNamespace, CommittedProcessExit, Dumpability, ExecImageRetirement,
        InitialProcessThreadAdmission, MempolicySnapshot, NetworkNamespace,
        PendingThreadPublication, PidNamespace, Process, ProcessAccessState, ProcessGroup,
        ProcessImageAccessSnapshot, ProcessReparentBatch, ProcessThreadAdmission,
        PtraceReverseLink, Session, ThreadExitTransition, TimeNamespace, UTS_FIELD_LEN,
        UserNamespace, UtsNamespace, ZombieSnapshot, init_process_domain, process_domain,
        process_error,
    },
    restart::*,
    thread::{
        ProcStateHint, TaskParentChoice, TaskParentCredentialPin, TaskParentNode,
        TaskParentPublicationGuard, lock_task_parent_publication,
    },
};

/// Maps policy-neutral credential errors at the kernel adapter boundary.
pub(crate) fn cred_error(error: CredError) -> axerrno::AxError {
    match error {
        CredError::InvalidInput => axerrno::AxError::InvalidInput,
        CredError::NotPermitted => axerrno::AxError::OperationNotPermitted,
        CredError::NoMemory => axerrno::AxError::NoMemory,
        CredError::Capacity => axerrno::LinuxError::ENOSPC.into(),
        _ => axerrno::AxError::OperationNotPermitted,
    }
}
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
