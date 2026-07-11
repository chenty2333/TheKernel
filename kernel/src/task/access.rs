use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{
    CAP_KILL, CAP_SETFCAP, CAP_SETGID, CAP_SETUID, CAP_SYS_ADMIN, CAP_SYS_PTRACE, CAP_SYS_RESOURCE,
};
use starry_signal::Signo;

use super::{
    AsThread, Cred, Credentials, ProcessData, UserNamespace,
    idmap::{IdMapInputExtent, Kgid, Kuid, UserGid, UserUid},
};

#[derive(Clone, Copy)]
pub(crate) enum PtraceCredentialMode {
    Real,
    Fs,
}

/// Linux `ns_capable()` topology over one immutable actor snapshot.
///
/// Effective capabilities apply in the actor's own user namespace and all of
/// its descendants. In addition, the kernel-global owner of a direct child
/// namespace has every capability in that child and its descendants even if
/// the corresponding bit is absent from the actor's effective set.
pub(crate) fn ns_capable(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    capability: u32,
) -> bool {
    let actor_user_ns = actor.user_ns();
    let Some(actor_euid) = Kuid::from_raw(actor.ids().euid) else {
        return false;
    };
    let mut namespace = target_user_ns.clone();
    loop {
        if Arc::ptr_eq(&namespace, actor_user_ns) {
            return actor.has_effective_capability_in_own_user_ns(capability);
        }
        if namespace.level() <= actor_user_ns.level() {
            return false;
        }
        let Some(parent) = namespace.parent() else {
            return false;
        };
        if Arc::ptr_eq(&parent, actor_user_ns) && namespace.owner_kuid() == actor_euid {
            return true;
        }
        namespace = parent;
    }
}

fn idmap_writer_parent(opener: &Cred, target: &Arc<UserNamespace>) -> Option<Arc<UserNamespace>> {
    let parent = target.parent()?;
    let opener_ns = opener.user_ns();
    if !Arc::ptr_eq(opener_ns, target) && !Arc::ptr_eq(opener_ns, &parent) {
        return None;
    }
    ns_capable(opener, target, CAP_SYS_ADMIN).then_some(parent)
}

pub(crate) fn may_update_setgroups_policy(opener: &Cred, target: &Arc<UserNamespace>) -> bool {
    // Linux checks ns_capable(target, CAP_SYS_ADMIN) when a writable
    // /proc/PID/setgroups file is opened. Unlike uid_map/gid_map, this file
    // is not restricted to an opener in exactly the target or its parent.
    ns_capable(opener, target, CAP_SYS_ADMIN)
}

/// Checks the pre-parse gates Linux applies to a UID-map write: the map is
/// still empty, the opener namespace is the target or direct parent, and the
/// frozen opener has CAP_SYS_ADMIN over the target namespace.
pub(crate) fn may_begin_uid_map_write(opener: &Cred, target: &Arc<UserNamespace>) -> bool {
    !target.uid_map_written() && idmap_writer_parent(opener, target).is_some()
}

/// GID-map counterpart to [`may_begin_uid_map_write`].
pub(crate) fn may_begin_gid_map_write(opener: &Cred, target: &Arc<UserNamespace>) -> bool {
    !target.gid_map_written() && idmap_writer_parent(opener, target).is_some()
}

fn single_unprivileged_uid_mapping(
    opener: &Cred,
    target: &UserNamespace,
    parent: &UserNamespace,
    rows: &[IdMapInputExtent],
) -> bool {
    let [row] = rows else {
        return false;
    };
    if row.count != 1 {
        return false;
    }
    let Some(parent_uid) =
        UserUid::from_raw(row.lower_first).and_then(|uid| parent.user_uid_to_kernel(uid))
    else {
        return false;
    };
    Kuid::from_raw(opener.ids().euid)
        .is_some_and(|euid| target.owner_kuid() == euid && parent_uid == euid)
}

fn single_unprivileged_gid_mapping(
    opener: &Cred,
    target: &UserNamespace,
    parent: &UserNamespace,
    rows: &[IdMapInputExtent],
) -> bool {
    let [row] = rows else {
        return false;
    };
    if row.count != 1 || target.setgroups_allowed() {
        return false;
    }
    let Some(parent_gid) =
        UserGid::from_raw(row.lower_first).and_then(|gid| parent.user_gid_to_kernel(gid))
    else {
        return false;
    };
    let Some(opener_euid) = Kuid::from_raw(opener.ids().euid) else {
        return false;
    };
    Kgid::from_raw(opener.ids().egid)
        .is_some_and(|egid| target.owner_kuid() == opener_euid && parent_gid == egid)
}

/// Linux `new_idmap_permitted()` policy for `/proc/PID/uid_map`.
/// `current` is sampled at write time and `opener` is the immutable credential
/// captured when the proc file object was opened/resolved.
pub(crate) fn may_write_uid_map(
    current: &Cred,
    opener: &Cred,
    target: &Arc<UserNamespace>,
    rows: &[IdMapInputExtent],
) -> bool {
    let Some(parent) = idmap_writer_parent(opener, target) else {
        return false;
    };

    // Mapping parent-visible UID 0 grants file-capability authority back into
    // an ancestor. Linux 5.12+ requires CAP_SETFCAP provenance for that row.
    if rows.iter().any(|row| row.lower_first == 0) {
        let root_map_allowed = if Arc::ptr_eq(opener.user_ns(), target) {
            target.parent_could_setfcap()
        } else {
            ns_capable(opener, &parent, CAP_SETFCAP)
        };
        if !root_map_allowed {
            return false;
        }
    }

    single_unprivileged_uid_mapping(opener, target, &parent, rows)
        || (ns_capable(current, &parent, CAP_SETUID) && ns_capable(opener, &parent, CAP_SETUID))
}

/// Linux `new_idmap_permitted()` policy for `/proc/PID/gid_map`.
pub(crate) fn may_write_gid_map(
    current: &Cred,
    opener: &Cred,
    target: &Arc<UserNamespace>,
    rows: &[IdMapInputExtent],
) -> bool {
    let Some(parent) = idmap_writer_parent(opener, target) else {
        return false;
    };
    single_unprivileged_gid_mapping(opener, target, &parent, rows)
        || (ns_capable(current, &parent, CAP_SETGID) && ns_capable(opener, &parent, CAP_SETGID))
}

fn has_capability_over(actor: &Cred, target: &Cred, capability: u32) -> bool {
    ns_capable(actor, target.user_ns(), capability)
}

fn caller_id_matches_all_target_ids(caller_uid: u32, caller_gid: u32, target: Credentials) -> bool {
    caller_uid == target.ruid
        && caller_uid == target.euid
        && caller_uid == target.suid
        && caller_gid == target.rgid
        && caller_gid == target.egid
        && caller_gid == target.sgid
}

fn ptrace_credential_allows(
    actor_cred: &Cred,
    target_cred: &Cred,
    mode: PtraceCredentialMode,
) -> bool {
    let actor_creds = actor_cred.ids();
    let (caller_uid, caller_gid) = match mode {
        PtraceCredentialMode::Real => (actor_creds.ruid, actor_creds.rgid),
        PtraceCredentialMode::Fs => (actor_creds.fsuid, actor_creds.fsgid),
    };
    caller_id_matches_all_target_ids(caller_uid, caller_gid, target_cred.ids())
        || has_capability_over(actor_cred, target_cred, CAP_SYS_PTRACE)
}

pub(crate) fn check_ptrace_access(
    actor: &ProcessData,
    actor_cred: &Cred,
    target: &ProcessData,
    target_cred: &Cred,
    mode: PtraceCredentialMode,
) -> AxResult<()> {
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    if ptrace_credential_allows(actor_cred, target_cred, mode) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn check_current_ptrace_access(
    target: &ProcessData,
    target_cred: &Cred,
    mode: PtraceCredentialMode,
) -> AxResult<()> {
    let current = current();
    let actor = current.as_thread();
    let actor_cred = actor.current_cred();
    check_ptrace_access(&actor.proc_data, &actor_cred, target, target_cred, mode)
}

/// Checks a process-directed ptrace-style operation against the Linux group
/// leader credential. TID-directed callers must resolve the exact task and use
/// `check_current_ptrace_access` instead.
pub(crate) fn check_current_process_ptrace_access(
    target: &ProcessData,
    mode: PtraceCredentialMode,
) -> AxResult<()> {
    let target_cred = target.group_leader_cred();
    check_current_ptrace_access(target, &target_cred, mode)
}

pub(crate) fn check_current_prlimit_access(
    target: &ProcessData,
    target_cred: &Cred,
) -> AxResult<()> {
    let current = current();
    let actor = current.as_thread();
    if actor.proc_data.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    let actor_cred = actor.current_cred();
    let actor_creds = actor_cred.ids();
    if caller_id_matches_all_target_ids(actor_creds.ruid, actor_creds.rgid, target_cred.ids())
        || has_capability_over(&actor_cred, target_cred, CAP_SYS_RESOURCE)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

/// `prlimit64(pid, ...)` is process-directed and samples the group leader once.
pub(crate) fn check_current_process_prlimit_access(target: &ProcessData) -> AxResult<()> {
    let target_cred = target.group_leader_cred();
    check_current_prlimit_access(target, &target_cred)
}

pub(crate) fn check_signal_access(
    actor: &ProcessData,
    actor_cred: &Cred,
    target: &ProcessData,
    target_cred: &Cred,
    signal: Option<Signo>,
) -> AxResult<()> {
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    let actor_creds = actor_cred.ids();
    let target_creds = target_cred.ids();
    let ids_match = [actor_creds.ruid, actor_creds.euid]
        .into_iter()
        .any(|uid| uid == target_creds.ruid || uid == target_creds.suid);
    let same_session = signal == Some(Signo::SIGCONT)
        && actor.proc.group().session().sid() == target.proc.group().session().sid();
    if ids_match || same_session || has_capability_over(actor_cred, target_cred, CAP_KILL) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub(crate) fn check_current_signal_access(
    target: &ProcessData,
    target_cred: &Cred,
    signal: Option<Signo>,
) -> AxResult<()> {
    let current = current();
    let actor = current.as_thread();
    let actor_cred = actor.current_cred();
    check_signal_access(&actor.proc_data, &actor_cred, target, target_cred, signal)
}

/// Process-directed signal permission follows the Linux group leader. Exact
/// TID signal paths must pass the selected thread snapshot directly.
pub(crate) fn check_current_process_signal_access(
    target: &ProcessData,
    signal: Option<Signo>,
) -> AxResult<()> {
    let target_cred = target.group_leader_cred();
    check_current_signal_access(target, &target_cred, signal)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use std::{sync::Barrier, thread};

    use super::*;
    use crate::task::creds::{CAPABILITY_WORDS, CredentialSlot};

    fn publish_ids(slot: &CredentialSlot, uid: u32, gid: u32) -> Arc<Cred> {
        let mut update = slot.prepare();
        update.builder.ids = Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        update.builder.caps.effective = [0; CAPABILITY_WORDS];
        update.builder.caps.permitted = [0; CAPABILITY_WORDS];
        update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit()
    }

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
        let child = root.try_fork(1000, 100, false).unwrap();
        let sibling = root.try_fork(2000, 200, false).unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let child_cred = Cred::try_with_user_ns(&root_cred, child.clone()).unwrap();

        assert!(ns_capable(&root_cred, &root, CAP_KILL));
        assert!(ns_capable(&root_cred, &child, CAP_KILL));
        assert!(!ns_capable(&child_cred, &root, CAP_KILL));

        let owner_slot = CredentialSlot::new(root_cred);
        let owner = publish_ids(&owner_slot, 1000, 100);
        assert!(ns_capable(&owner, &child, CAP_KILL));
        assert!(!ns_capable(&owner, &sibling, CAP_KILL));
    }

    #[test]
    fn setgroups_open_allows_capable_ancestors_but_idmap_write_does_not() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let child = root.try_fork(1000, 100, false).unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                    .unwrap(),
            )
            .unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, 100, 1)])
                    .unwrap(),
                false,
            )
            .unwrap();
        let grandchild = child.try_fork(1000, 100, false).unwrap();

        assert!(may_update_setgroups_policy(&root_cred, &grandchild));
        assert!(!may_write_uid_map(
            &root_cred,
            &root_cred,
            &grandchild,
            &[IdMapInputExtent::new(0, 0, 1)]
        ));
    }

    #[test]
    fn unprivileged_id_maps_are_single_owner_rows_with_gid_deny_gate() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let owner_slot = CredentialSlot::new(root_cred.clone());
        let owner = publish_ids(&owner_slot, 1000, 100);
        let child = root.try_fork(1000, 100, false).unwrap();
        let child_opener = Cred::try_with_user_ns(&owner, child.clone()).unwrap();

        let uid_row = [IdMapInputExtent::new(0, 1000, 1)];
        assert!(may_write_uid_map(
            &child_opener,
            &child_opener,
            &child,
            &uid_row
        ));
        assert!(!may_write_uid_map(
            &child_opener,
            &child_opener,
            &child,
            &[IdMapInputExtent::new(0, 0, 1)]
        ));
        assert!(!may_write_uid_map(
            &child_opener,
            &child_opener,
            &child,
            &[
                IdMapInputExtent::new(0, 1000, 1),
                IdMapInputExtent::new(1, 1001, 1),
            ]
        ));

        let gid_row = [IdMapInputExtent::new(0, 100, 1)];
        assert!(!may_write_gid_map(
            &child_opener,
            &child_opener,
            &child,
            &gid_row
        ));
        child.update_setgroups_policy(false).unwrap();
        assert!(may_write_gid_map(
            &child_opener,
            &child_opener,
            &child,
            &gid_row
        ));

        let privileged_rows = [
            IdMapInputExtent::new(0, 2000, 2),
            IdMapInputExtent::new(100, 3000, 2),
        ];
        assert!(may_write_uid_map(
            &root_cred,
            &root_cred,
            &child,
            &privileged_rows
        ));
    }

    #[test]
    fn target_access_uses_one_frozen_snapshot_during_concurrent_commits() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor_slot = CredentialSlot::new(root.clone());
        let target_slot = Arc::new(CredentialSlot::new(root));
        let actor = publish_ids(&actor_slot, 1000, 100);
        let frozen_target = publish_ids(&target_slot, 1000, 100);

        let start = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let writer = {
            let target_slot = target_slot.clone();
            let start = start.clone();
            let finish = finish.clone();
            thread::spawn(move || {
                start.wait();
                for uid in 2000..3000 {
                    publish_ids(&target_slot, uid, 200);
                }
                finish.wait();
            })
        };

        start.wait();
        for _ in 0..2000 {
            assert!(ptrace_credential_allows(
                &actor,
                &frozen_target,
                PtraceCredentialMode::Real,
            ));
        }
        finish.wait();
        writer.join().unwrap();

        let fresh_target = target_slot.current();
        assert!(!ptrace_credential_allows(
            &actor,
            &fresh_target,
            PtraceCredentialMode::Real,
        ));
    }
}
