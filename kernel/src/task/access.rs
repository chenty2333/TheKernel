use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{CAP_KILL, CAP_SYS_PTRACE, CAP_SYS_RESOURCE};
use starry_signal::Signo;

use super::{AsThread, Cred, Credentials, ProcessData, UserNamespace};

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

/// Applies an effective capability bit captured in `actor_user_ns` to a
/// target namespace. The simplified hierarchy grants it to the same namespace
/// and descendants, never to ancestors or siblings.
pub(crate) fn capability_snapshot_applies_to_user_namespace(
    has_effective_capability: bool,
    actor_user_ns: &Arc<UserNamespace>,
    target_user_ns: &Arc<UserNamespace>,
) -> bool {
    has_effective_capability && user_namespace_is_same_or_descendant(actor_user_ns, target_user_ns)
}

fn has_capability_over(actor: &Cred, target: &Cred, capability: u32) -> bool {
    capability_snapshot_applies_to_user_namespace(
        actor.has_effective_capability(capability),
        actor.user_ns(),
        target.user_ns(),
    )
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

    let actor_cred = actor.current_cred();
    let target_cred = target.current_cred();
    let actor_creds = actor_cred.ids();
    let (caller_uid, caller_gid) = match mode {
        PtraceCredentialMode::Real => (actor_creds.ruid, actor_creds.rgid),
        PtraceCredentialMode::Fs => (actor_creds.fsuid, actor_creds.fsgid),
    };
    if caller_id_matches_all_target_ids(caller_uid, caller_gid, target_cred.ids())
        || has_capability_over(&actor_cred, &target_cred, CAP_SYS_PTRACE)
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

    let actor_cred = actor.current_cred();
    let target_cred = target.current_cred();
    let actor_creds = actor_cred.ids();
    if caller_id_matches_all_target_ids(actor_creds.ruid, actor_creds.rgid, target_cred.ids())
        || has_capability_over(&actor_cred, &target_cred, CAP_SYS_RESOURCE)
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

    let actor_cred = actor.current_cred();
    let target_cred = target.current_cred();
    let actor_creds = actor_cred.ids();
    let target_creds = target_cred.ids();
    let ids_match = [actor_creds.ruid, actor_creds.euid]
        .into_iter()
        .any(|uid| uid == target_creds.ruid || uid == target_creds.suid);
    let same_session = signal == Some(Signo::SIGCONT)
        && actor.proc.group().session().sid() == target.proc.group().session().sid();
    if ids_match || same_session || has_capability_over(&actor_cred, &target_cred, CAP_KILL) {
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
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(1000).unwrap();
        let grandchild = child.try_fork(1000).unwrap();

        assert!(capability_snapshot_applies_to_user_namespace(
            true, &root, &root
        ));
        assert!(capability_snapshot_applies_to_user_namespace(
            true,
            &root,
            &grandchild
        ));
        assert!(!capability_snapshot_applies_to_user_namespace(
            true, &child, &root
        ));
        assert!(!capability_snapshot_applies_to_user_namespace(
            false,
            &root,
            &grandchild
        ));
    }
}
