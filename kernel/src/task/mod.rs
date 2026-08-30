//! User task management.

mod access;
mod accounting;
pub(crate) mod coredump;
mod creds;
mod exec_cred;
mod futex;
mod jobctl;
mod loadavg;
mod ops;
mod process;
mod resources;
mod restart;
mod rseq;
mod seccomp;
pub(crate) mod security;
mod signal;
mod stat;
mod thread;
mod thread_cred;
mod timer;
mod user;

// Re-exports from split sub-modules — keep the old `crate::task::*` paths unchanged.
#[cfg(test)]
pub(crate) use creds::CapabilityState;
pub(crate) use thekernel_linux_cred::{
    CredError, FileCapabilities, ID_MAP_MAX_EXTENTS, IdMap, IdMapInputExtent, Kgid, Kuid,
    SECURITY_CAPABILITY_XATTR_NAME, SignalDeliveryScope, SignalNumber, SignalSecurityOperation,
    SignalSecuritySource, UserGid, UserUid, XATTR_NAME_MAX, validate_id_map_input,
};

pub(crate) use self::{
    access::{
        PtraceAccessMode, check_current_pinned_process_identity_signal_access,
        check_current_pinned_process_signal_access, check_current_pinned_thread_signal_access,
        check_current_process_prlimit_access, check_current_process_ptrace_access,
        check_current_ptrace_image_snapshot, check_current_thread_ptrace_image_access,
        check_current_zombie_signal_access, check_thread_ptrace_image_access_with_actor,
        may_begin_gid_map_write, may_begin_uid_map_write, may_update_setgroups_policy,
        may_write_gid_map, may_write_uid_map, ns_capable, ns_capable_for_setid,
    },
    creds::{Cred, CredentialSlot, Credentials, DacCredentialView},
    exec_cred::{
        CommittingExecCredential, ExecAuxIdentity, ExecCommitRuntime, ExecCredentialInput,
        ExecCredentialSecurityContext, ExecExecutableRole, ExecFileIdentity, ExecFileOwner,
        ExecFileSecurityObject, ExecImageIdentity, ExecImageReadability, ExecMountPrivilege,
        ExecTraceState, map_exec_dumpability, parse_file_capabilities,
    },
    jobctl::{
        ContinueResult, PtraceRelationshipOrigin, PtraceRelationshipSnapshot, PtraceSession,
        StopReport,
    },
    loadavg::{load_average_sample_now, load_average_sysinfo},
    process::{
        CgroupNamespace, CommittedProcessExit, Dumpability, ExecImageCommit,
        InitialProcessThreadAdmission, MempolicySnapshot, NetworkNamespace,
        PendingThreadPublication, PidNamespace, Process, ProcessAccessState, ProcessGroup,
        ProcessImageAccessSnapshot, ProcessInitialAdmission, ProcessReparentBatch,
        ProcessThreadAdmission, PtraceReverseLink, Session, ThreadExitTransition, TimeNamespace,
        UTS_FIELD_LEN, UserNamespace, UserNamespaceId, UtsNamespace, ZombieSnapshot,
        init_process_domain, prepare_session_sid_binding, process_domain, process_error,
        reap_process, release_dead_session_sid_binding, set_zombie_affinity, set_zombie_ioprio,
        zombie_ioprio, zombie_pid_ns, zombie_scheduler_state,
    },
    restart::*,
    rseq::{AT_RSEQ_ALIGN, AT_RSEQ_FEATURE_SIZE},
    seccomp::{SeccompPublicationError, init_seccomp_filter_budget, seccomp_filter_budget},
    security::{PendingCredentialPublication, SignalTargetKind},
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
    thread::{AsThread, FdTableSlot, FsContextSlot, Thread},
    timer::*,
    user::*,
};

pub(crate) use self::thread::SchedulerSeed;

/// Linearizes creation/replacement of a task's `fs_struct` with namespace-root
/// replacement. The required lock order is this gate, then an individual
/// `FsContext` mutex; pivot_root takes the gate before snapshotting tasks.
pub(crate) static FS_CONTEXT_PUBLICATION: axsync::Mutex<()> = axsync::Mutex::new(());

#[inline]
pub(crate) fn fs_context_publication() -> axsync::MutexGuard<'static, ()> {
    FS_CONTEXT_PUBLICATION.lock()
}

/// Snapshot the calling task's Linux `fs_struct`.
#[inline]
pub(crate) fn current_fs_context() -> alloc::sync::Arc<axsync::Mutex<axfs::FsContext>> {
    axtask::current().as_thread().fs_context()
}
