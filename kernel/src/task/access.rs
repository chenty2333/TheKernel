use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{CAP_KILL, CAP_SYS_PTRACE, CAP_SYS_RESOURCE};
use starry_signal::Signo;

use super::{AsThread, Credentials, ProcessData, UserNamespace};

#[derive(Clone, Copy)]
pub(crate) enum PtraceCredentialMode {
    Real,
    Fs,
}

fn user_namespace_is_same_or_descendant(
    ancestor: &Arc<UserNamespace>,
    namespace: &Arc<UserNamespace>,
) -> bool {
    let mut current = Some(namespace.clone());
    while let Some(candidate) = current {
        if Arc::ptr_eq(ancestor, &candidate) {
            return true;
        }
        current = candidate.parent();
    }
    false
}

fn has_capability_over(actor: &ProcessData, target: &ProcessData, capability: u32) -> bool {
    actor.has_effective_capability(capability)
        && user_namespace_is_same_or_descendant(&actor.user_ns(), &target.user_ns())
}

fn caller_id_matches_all_target_ids(caller_uid: u32, caller_gid: u32, target: Credentials) -> bool {
    caller_uid == target.ruid
        && caller_uid == target.euid
        && caller_uid == target.suid
        && caller_gid == target.rgid
        && caller_gid == target.egid
        && caller_gid == target.sgid
}

pub(crate) fn check_ptrace_access(
    actor: &ProcessData,
    target: &ProcessData,
    mode: PtraceCredentialMode,
) -> AxResult<()> {
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    let actor_creds = actor.credentials();
    let (caller_uid, caller_gid) = match mode {
        PtraceCredentialMode::Real => (actor_creds.ruid, actor_creds.rgid),
        PtraceCredentialMode::Fs => (actor_creds.fsuid, actor_creds.fsgid),
    };
    if caller_id_matches_all_target_ids(caller_uid, caller_gid, target.credentials())
        || has_capability_over(actor, target, CAP_SYS_PTRACE)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn check_current_ptrace_access(
    target: &ProcessData,
    mode: PtraceCredentialMode,
) -> AxResult<()> {
    let current = current();
    check_ptrace_access(&current.as_thread().proc_data, target, mode)
}

pub(crate) fn check_current_prlimit_access(target: &ProcessData) -> AxResult<()> {
    let current = current();
    let actor = &current.as_thread().proc_data;
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    let actor_creds = actor.credentials();
    if caller_id_matches_all_target_ids(actor_creds.ruid, actor_creds.rgid, target.credentials())
        || has_capability_over(actor, target, CAP_SYS_RESOURCE)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn check_signal_access(
    actor: &ProcessData,
    target: &ProcessData,
    signal: Option<Signo>,
) -> AxResult<()> {
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    let actor_creds = actor.credentials();
    let target_creds = target.credentials();
    let ids_match = [actor_creds.ruid, actor_creds.euid]
        .into_iter()
        .any(|uid| uid == target_creds.ruid || uid == target_creds.suid);
    let same_session = signal == Some(Signo::SIGCONT)
        && actor.proc.group().session().sid() == target.proc.group().session().sid();
    if ids_match || same_session || has_capability_over(actor, target, CAP_KILL) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn check_current_signal_access(
    target: &ProcessData,
    signal: Option<Signo>,
) -> AxResult<()> {
    let current = current();
    check_signal_access(&current.as_thread().proc_data, target, signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptrace_id_match_requires_all_target_ids() {
        let mut target = Credentials {
            ruid: 1000,
            euid: 1000,
            suid: 1000,
            rgid: 100,
            egid: 100,
            sgid: 100,
            ..Default::default()
        };
        assert!(caller_id_matches_all_target_ids(1000, 100, target));

        target.suid = 0;
        assert!(!caller_id_matches_all_target_ids(1000, 100, target));
        target.suid = 1000;
        target.egid = 200;
        assert!(!caller_id_matches_all_target_ids(1000, 100, target));
    }

    #[test]
    fn user_namespace_capability_direction_is_parent_to_child() {
        let root = UserNamespace::new_root();
        let child = root.fork(1000);
        let grandchild = child.fork(1000);

        assert!(user_namespace_is_same_or_descendant(&root, &root));
        assert!(user_namespace_is_same_or_descendant(&root, &grandchild));
        assert!(!user_namespace_is_same_or_descendant(&child, &root));
    }
}
