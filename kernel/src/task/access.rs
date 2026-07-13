use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{
    CAP_KILL, CAP_SETFCAP, CAP_SETGID, CAP_SETUID, CAP_SYS_ADMIN, CAP_SYS_PTRACE, CAP_SYS_RESOURCE,
};
use starry_signal::Signo;

use super::{
    AsThread, Cred, Credentials, Dumpability, IdMapInputExtent, Kgid, Kuid, ProcessData,
    ProcessImageAccessSnapshot, Thread, UserGid, UserNamespace, UserUid,
    security::{
        ProcessImageSecurityRef, PtraceAccessContext, PtraceAccessKind, PtraceCredentialKind,
        SecuritySubject, dispatch_ptrace_access,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PtraceAccessMode {
    ReadReal,
    ReadFs,
    AttachReal,
}

impl PtraceAccessMode {
    fn uses_fs_credentials(self) -> bool {
        matches!(self, Self::ReadFs)
    }

    fn access_kind(self) -> PtraceAccessKind {
        match self {
            Self::ReadReal | Self::ReadFs => PtraceAccessKind::Read,
            Self::AttachReal => PtraceAccessKind::Attach,
        }
    }

    fn credential_kind(self) -> PtraceCredentialKind {
        if self.uses_fs_credentials() {
            PtraceCredentialKind::Fs
        } else {
            PtraceCredentialKind::Real
        }
    }
}

/// Compatibility adapter for the Linux-credential crate's namespace-relative
/// capability rule.
pub(crate) fn ns_capable(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    capability: u32,
) -> bool {
    thekernel_linux_cred::ns_capable(actor, target_user_ns, capability)
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
    let euid = opener.ids().euid;
    target.owner_kuid() == euid && parent_uid == euid
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
    let opener_ids = opener.ids();
    target.owner_kuid() == opener_ids.euid && parent_gid == opener_ids.egid
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

fn caller_id_matches_all_target_ids(
    caller_uid: Kuid,
    caller_gid: Kgid,
    target: Credentials,
) -> bool {
    caller_uid == target.ruid
        && caller_uid == target.euid
        && caller_uid == target.suid
        && caller_gid == target.rgid
        && caller_gid == target.egid
        && caller_gid == target.sgid
}

/// Linux ptrace core identity and dumpability gates. The independent
/// commoncap permitted-set rule runs through the typed hook stack only after
/// these checks admit the exact frozen image.
fn ptrace_core_allows(
    actor_cred: &Cred,
    target_cred: &Cred,
    target_dumpability: Dumpability,
    target_owner_user_ns: &Arc<UserNamespace>,
    mode: PtraceAccessMode,
) -> bool {
    let actor_creds = actor_cred.ids();
    let (caller_uid, caller_gid) = if mode.uses_fs_credentials() {
        (actor_creds.fsuid, actor_creds.fsgid)
    } else {
        (actor_creds.ruid, actor_creds.rgid)
    };
    let identity_allowed =
        caller_id_matches_all_target_ids(caller_uid, caller_gid, target_cred.ids())
            || has_capability_over(actor_cred, target_cred, CAP_SYS_PTRACE);
    if !identity_allowed {
        return false;
    }

    if target_dumpability != Dumpability::UserDumpable
        && !ns_capable(actor_cred, target_owner_user_ns, CAP_SYS_PTRACE)
    {
        return false;
    }

    true
}

fn check_ptrace_access(
    actor: &ProcessData,
    actor_cred: &Cred,
    target: &ProcessData,
    target_image: &ProcessImageAccessSnapshot,
    mode: PtraceAccessMode,
) -> AxResult<()> {
    if actor.proc.pid() == target.proc.pid() {
        return Ok(());
    }

    if ptrace_core_allows(
        actor_cred,
        target_image.credential(),
        target_image.dumpability(),
        target_image.owner_user_ns(),
        mode,
    ) {
        let context = PtraceAccessContext::new(
            SecuritySubject::new(actor_cred),
            SecuritySubject::new(target_image.credential()),
            ProcessImageSecurityRef::new(target_image.owner_user_ns(), target_image.aspace()),
            mode.access_kind(),
            mode.credential_kind(),
        );
        // Snapshot acquisition releases the image/access locks before this
        // function is entered. Dispatch is allocation-free and runs before
        // any ptrace publication spin lock is acquired.
        dispatch_ptrace_access(&context).map_err(|error| {
            if mode == PtraceAccessMode::ReadFs {
                AxError::PermissionDenied
            } else {
                error
            }
        })
    } else {
        Err(if mode == PtraceAccessMode::ReadFs {
            AxError::PermissionDenied
        } else {
            AxError::OperationNotPermitted
        })
    }
}

pub(crate) fn check_current_ptrace_image_snapshot(
    target: &ProcessData,
    snapshot: &ProcessImageAccessSnapshot,
    mode: PtraceAccessMode,
) -> AxResult<()> {
    let current = current();
    let actor = current.as_thread();
    let actor_cred = actor.current_cred();
    check_ptrace_access(&actor.proc_data, &actor_cred, target, snapshot, mode)
}

pub(crate) fn check_current_thread_ptrace_image_access(
    target: &Thread,
    mode: PtraceAccessMode,
) -> AxResult<ProcessImageAccessSnapshot> {
    let snapshot = target.proc_data.thread_image_access_snapshot(target)?;
    check_current_ptrace_image_snapshot(&target.proc_data, &snapshot, mode)?;
    Ok(snapshot)
}

/// Checks a process-directed ptrace-style operation against the Linux group
/// leader credential. TID-directed callers must resolve the exact task and use
/// `check_current_thread_ptrace_image_access` instead.
///
/// Image-bound consumers must operate on the returned coherent snapshot rather
/// than resampling the process after authorization.
pub(crate) fn check_current_process_ptrace_access(
    target: &ProcessData,
    mode: PtraceAccessMode,
) -> AxResult<ProcessImageAccessSnapshot> {
    let snapshot = target.group_leader_image_access_snapshot();
    check_current_ptrace_image_snapshot(target, &snapshot, mode)?;
    Ok(snapshot)
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

    let same_session = signal == Some(Signo::SIGCONT)
        && actor.proc.group().session().sid() == target.proc.group().session().sid();
    if signal_credential_allows(actor_cred, target_cred) || same_session {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

/// Frozen Linux signal-credential policy shared by live and zombie targets.
/// Process identity and SIGCONT session exceptions remain with their callers.
pub(crate) fn signal_credential_allows(actor: &Cred, target: &Cred) -> bool {
    let actor_ids = actor.ids();
    let target_ids = target.ids();
    [actor_ids.ruid, actor_ids.euid]
        .into_iter()
        .any(|uid| uid == target_ids.ruid || uid == target_ids.suid)
        || has_capability_over(actor, target, CAP_KILL)
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
    use crate::task::{
        UtsNamespace,
        creds::{CAPABILITY_WORDS, CredentialSlot},
    };

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn publish_ids(slot: &CredentialSlot, uid: u32, gid: u32) -> Arc<Cred> {
        let uid = kuid(uid);
        let gid = kgid(gid);
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
    fn process_access_ptrace_enforces_dumpability_image_owner_and_commoncap() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root_ns.clone()).unwrap();
        let actor_slot = CredentialSlot::new(root_cred.clone());
        let target_slot = CredentialSlot::new(root_cred.clone());
        let actor = publish_ids(&actor_slot, 1000, 100);
        let target = publish_ids(&target_slot, 1000, 100);

        assert!(ptrace_core_allows(
            &actor,
            &target,
            Dumpability::UserDumpable,
            &root_ns,
            PtraceAccessMode::AttachReal,
        ));
        assert!(!ptrace_core_allows(
            &actor,
            &target,
            Dumpability::NotDumpable,
            &root_ns,
            PtraceAccessMode::AttachReal,
        ));

        let privileged_actor = root_cred.clone();
        assert!(ptrace_core_allows(
            &privileged_actor,
            &target,
            Dumpability::NotDumpable,
            &root_ns,
            PtraceAccessMode::AttachReal,
        ));

        let child_a = root_ns
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_b = root_ns
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let sibling_actor = Cred::try_with_user_ns(&root_cred, child_a).unwrap();
        let sibling_target = Cred::try_with_user_ns(&root_cred, child_b.clone()).unwrap();
        assert!(!ptrace_core_allows(
            &sibling_actor,
            &sibling_target,
            Dumpability::NotDumpable,
            &child_b,
            PtraceAccessMode::AttachReal,
        ));

        let (word, mask) =
            crate::task::CapabilityState::cap_mask(linux_raw_sys::general::CAP_CHOWN).unwrap();
        let mut target_gain = target_slot.prepare();
        target_gain.builder.caps.permitted[word] |= mask;
        target_gain.builder.caps.effective[word] |= mask;
        let target_with_cap = target_gain.finish().unwrap().commit();
        assert!(ptrace_core_allows(
            &actor,
            &target_with_cap,
            Dumpability::UserDumpable,
            &root_ns,
            PtraceAccessMode::AttachReal,
        ));
        let image = Arc::new(());
        let context = PtraceAccessContext::new(
            SecuritySubject::new(&actor),
            SecuritySubject::new(&target_with_cap),
            ProcessImageSecurityRef::new(&root_ns, &image),
            PtraceAccessKind::Attach,
            PtraceCredentialKind::Real,
        );
        assert_eq!(
            dispatch_ptrace_access(&context),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn ptrace_id_match_requires_all_target_ids() {
        let mut target = Credentials {
            ruid: kuid(1000),
            euid: kuid(1000),
            suid: kuid(1000),
            fsuid: kuid(1000),
            rgid: kgid(100),
            egid: kgid(100),
            sgid: kgid(100),
            fsgid: kgid(100),
        };
        assert!(caller_id_matches_all_target_ids(
            kuid(1000),
            kgid(100),
            target
        ));

        target.suid = Kuid::INITIAL_ROOT;
        assert!(!caller_id_matches_all_target_ids(
            kuid(1000),
            kgid(100),
            target
        ));
        target.suid = kuid(1000);
        target.egid = kgid(200);
        assert!(!caller_id_matches_all_target_ids(
            kuid(1000),
            kgid(100),
            target
        ));
    }

    #[test]
    fn namespace_owner_ns_capable_follows_ancestry_not_siblings() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(100), false).unwrap();
        let sibling = root.try_fork(kuid(2000), kgid(200), false).unwrap();
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
        let grandchild = child.try_fork(kuid(1000), kgid(100), false).unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let owner_slot = CredentialSlot::new(root_cred.clone());
        let owner = publish_ids(&owner_slot, 1000, 100);
        let sibling_owner_slot = CredentialSlot::new(root_cred.clone());
        let sibling_owner = publish_ids(&sibling_owner_slot, 2000, 200);
        let child_cred = Cred::try_with_user_ns(&owner, child.clone()).unwrap();
        let sibling_cred = Cred::try_with_user_ns(&sibling_owner, sibling.clone()).unwrap();
        let grandchild_cred = Cred::try_with_user_ns(&child_cred, grandchild.clone()).unwrap();

        assert!(ns_capable(&root_cred, &root, CAP_KILL));
        assert!(ns_capable(&root_cred, &child, CAP_KILL));
        assert!(ns_capable(&root_cred, &sibling, CAP_KILL));
        assert!(ns_capable(&root_cred, &grandchild, CAP_KILL));
        assert!(ns_capable(&child_cred, &child, CAP_KILL));
        assert!(ns_capable(&child_cred, &grandchild, CAP_KILL));
        assert!(!ns_capable(&child_cred, &root, CAP_KILL));
        assert!(!ns_capable(&child_cred, &sibling, CAP_KILL));
        assert!(ns_capable(&sibling_cred, &sibling, CAP_KILL));
        assert!(!ns_capable(&sibling_cred, &root, CAP_KILL));
        assert!(!ns_capable(&sibling_cred, &child, CAP_KILL));
        assert!(!ns_capable(&sibling_cred, &grandchild, CAP_KILL));
        assert!(ns_capable(&grandchild_cred, &grandchild, CAP_KILL));
        assert!(!ns_capable(&grandchild_cred, &root, CAP_KILL));
        assert!(!ns_capable(&grandchild_cred, &child, CAP_KILL));
        assert!(!ns_capable(&grandchild_cred, &sibling, CAP_KILL));

        assert!(ns_capable(&owner, &child, CAP_KILL));
        assert!(ns_capable(&owner, &grandchild, CAP_KILL));
        assert!(!ns_capable(&owner, &sibling, CAP_KILL));
    }

    #[test]
    fn credential_caller_signal_policy_covers_saved_uid_and_target_namespace() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();

        let actor_slot = CredentialSlot::new(root_cred.clone());
        let actor = publish_ids(&actor_slot, 1000, 100);
        let target_slot = CredentialSlot::new(root_cred.clone());
        let mut target_update = target_slot.prepare();
        target_update.builder.ids = Credentials {
            ruid: kuid(2000),
            euid: kuid(2000),
            suid: kuid(1000),
            fsuid: kuid(2000),
            rgid: kgid(200),
            egid: kgid(200),
            sgid: kgid(200),
            fsgid: kgid(200),
        };
        target_update.builder.caps.effective = [0; CAPABILITY_WORDS];
        target_update.builder.caps.permitted = [0; CAPABILITY_WORDS];
        target_update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        target_update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        let saved_uid_target = target_update.finish().unwrap().commit();
        assert!(signal_credential_allows(&actor, &saved_uid_target));

        let root_target_slot = CredentialSlot::new(root_cred.clone());
        let root_target = publish_ids(&root_target_slot, 3000, 300);
        let child = root.try_fork(kuid(1000), kgid(100), false).unwrap();
        let sibling = root.try_fork(kuid(2000), kgid(200), false).unwrap();
        let child_parent_slot = CredentialSlot::new(root_cred.clone());
        let child_parent = publish_ids(&child_parent_slot, 1000, 100);
        let child_actor = Cred::try_with_user_ns(&child_parent, child.clone()).unwrap();
        let child_target_slot = CredentialSlot::new(child_actor.clone());
        let child_target = publish_ids(&child_target_slot, 3000, 300);
        let sibling_parent_slot = CredentialSlot::new(root_cred.clone());
        let sibling_parent = publish_ids(&sibling_parent_slot, 2000, 200);
        let sibling_cred = Cred::try_with_user_ns(&sibling_parent, sibling).unwrap();
        let sibling_target_slot = CredentialSlot::new(sibling_cred);
        let sibling_target = publish_ids(&sibling_target_slot, 3000, 300);

        assert!(!signal_credential_allows(&child_actor, &root_target));
        assert!(signal_credential_allows(&root_cred, &child_target));
        assert!(signal_credential_allows(&child_actor, &child_target));
        assert!(!signal_credential_allows(&child_actor, &sibling_target));
    }

    #[test]
    fn namespace_owner_authority_uses_object_owner_not_actor_namespace() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let child = root.try_fork(kuid(1000), kgid(100), false).unwrap();
        let sibling = root.try_fork(kuid(2000), kgid(200), false).unwrap();
        let child_parent_slot = CredentialSlot::new(root_cred.clone());
        let child_parent = publish_ids(&child_parent_slot, 1000, 100);
        let sibling_parent_slot = CredentialSlot::new(root_cred.clone());
        let sibling_parent = publish_ids(&sibling_parent_slot, 2000, 200);
        let child_cred = Cred::try_with_user_ns(&child_parent, child.clone()).unwrap();
        let sibling_cred = Cred::try_with_user_ns(&sibling_parent, sibling).unwrap();
        let root_uts = UtsNamespace::try_new_root(root).unwrap();
        let child_uts = UtsNamespace::try_new_root(child).unwrap();

        assert!(ns_capable(
            &root_cred,
            root_uts.owner_user_ns(),
            CAP_SYS_ADMIN
        ));
        assert!(ns_capable(
            &root_cred,
            child_uts.owner_user_ns(),
            CAP_SYS_ADMIN
        ));
        assert!(!ns_capable(
            &child_cred,
            root_uts.owner_user_ns(),
            CAP_SYS_ADMIN
        ));
        assert!(ns_capable(
            &child_cred,
            child_uts.owner_user_ns(),
            CAP_SYS_ADMIN
        ));
        assert!(!ns_capable(
            &sibling_cred,
            child_uts.owner_user_ns(),
            CAP_SYS_ADMIN
        ));
    }

    #[test]
    fn setgroups_open_allows_capable_ancestors_but_idmap_write_does_not() {
        let root = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root.clone()).unwrap();
        let child = root.try_fork(kuid(1000), kgid(100), false).unwrap();
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
        let grandchild = child.try_fork(kuid(1000), kgid(100), false).unwrap();

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
        let child = root.try_fork(kuid(1000), kgid(100), false).unwrap();
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
            assert!(ptrace_core_allows(
                &actor,
                &frozen_target,
                Dumpability::UserDumpable,
                frozen_target.user_ns(),
                PtraceAccessMode::AttachReal,
            ));
        }
        finish.wait();
        writer.join().unwrap();

        let fresh_target = target_slot.current();
        assert!(!ptrace_core_allows(
            &actor,
            &fresh_target,
            Dumpability::UserDumpable,
            fresh_target.user_ns(),
            PtraceAccessMode::AttachReal,
        ));
    }
}
