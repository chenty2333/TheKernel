use alloc::{string::String, vec::Vec};

use axerrno::AxResult;
#[cfg(not(test))]
use axsync::Mutex as KeyManagerMutex;
#[cfg(test)]
use spin::Mutex as KeyManagerMutex;

use super::manager::{ForkUndo, KeyManager};
pub(crate) use super::{
    accounting::{
        key_maxbytes, key_maxkeys, key_root_maxbytes, key_root_maxkeys, set_key_maxbytes,
        set_key_maxkeys, set_key_root_maxbytes, set_key_root_maxkeys,
    },
    contract::{KeyActor, KeyTaskOwner, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    object::KeyTypeKind,
};
use crate::task::{Kgid, Kuid};

// Host tests have no initialized axtask current slot, so the production
// sleepable mutex cannot be entered even when uncontended.
static KEY_MANAGER: KeyManagerMutex<KeyManager> = KeyManagerMutex::new(KeyManager::new());

/// Rollback ownership for key subscriptions staged under a still-private
/// child identity. This value must be declared before clone's lifecycle guard,
/// so an unwind releases every outer process lock before Drop takes the sole
/// key-manager mutex.
#[must_use = "a prepared keyring fork must commit after child identity publication"]
pub(crate) struct PendingKeyFork {
    undo: Option<ForkUndo>,
}

impl PendingKeyFork {
    /// Transfers the staged subscriptions to the published child. Preparation
    /// performed every allocation and reference update, so this is infallible.
    pub(crate) fn commit(mut self) {
        self.undo.take();
    }
}

impl Drop for PendingKeyFork {
    fn drop(&mut self) {
        let Some(undo) = self.undo.take() else {
            return;
        };
        KEY_MANAGER
            .lock()
            .rollback_fork(undo)
            .unwrap_or_else(|error| {
                panic!("prepared keyring fork rollback lost exact child ownership: {error}")
            });
    }
}

pub(crate) fn prepare_fork(
    parent: KeyTaskOwner,
    child: KeyTaskOwner,
    clone_thread: bool,
    child_ruid: Kuid,
    child_rgid: Kgid,
) -> AxResult<PendingKeyFork> {
    let undo =
        KEY_MANAGER
            .lock()
            .prepare_fork(parent, child, clone_thread, child_ruid, child_rgid)?;
    Ok(PendingKeyFork { undo: Some(undo) })
}

pub(crate) fn exec_committed(owner: KeyTaskOwner) -> AxResult<()> {
    KEY_MANAGER.lock().exec_committed(owner)
}

pub(crate) fn exit_committed(owner: KeyTaskOwner, final_thread: bool) -> AxResult<()> {
    KEY_MANAGER.lock().exit_committed(owner, final_thread)
}

pub(crate) fn credential_fsids_precommit(
    thread_owner: u32,
    new_fsuid: Kuid,
    new_fsgid: Kgid,
) -> AxResult<()> {
    KEY_MANAGER
        .lock()
        .credential_fsids_precommit(thread_owner, new_fsuid, new_fsgid)
}

pub(crate) fn add_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: String,
    payload: Vec<u8>,
    keyring: i32,
) -> AxResult<isize> {
    KEY_MANAGER
        .lock()
        .add_key(actor, kind, description, payload, keyring)
}

pub(crate) fn request_key(
    actor: &KeyActor,
    kind: KeyTypeKind,
    description: &str,
    callout_present: bool,
    dest_keyring: i32,
) -> AxResult<isize> {
    KEY_MANAGER
        .lock()
        .request_key(actor, kind, description, callout_present, dest_keyring)
}

pub(crate) fn keyctl(actor: &KeyActor, command: KeyctlCommand) -> AxResult<KeyctlOutput> {
    KEY_MANAGER.lock().keyctl(actor, command)
}

pub(crate) fn key_user_records() -> AxResult<Vec<KeyUserRecord>> {
    KEY_MANAGER.lock().key_user_records()
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};

    use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

    use super::{
        super::manager::{KEY_SPEC_SESSION_KEYRING, KEY_SPEC_THREAD_KEYRING},
        *,
    };
    use crate::task::{Credentials, DacCredentialView, UserNamespace};

    fn actor(thread_owner: u32, process_owner: u32, user_ns: Arc<UserNamespace>) -> KeyActor {
        let uid = Kuid::from_raw(61_000).unwrap();
        let gid = Kgid::from_raw(61_001).unwrap();
        let groups = GroupInfo::try_new(Vec::new()).unwrap();
        KeyActor::new(
            thread_owner,
            process_owner,
            thread_owner,
            process_owner,
            Credentials {
                ruid: uid,
                euid: uid,
                suid: uid,
                fsuid: uid,
                rgid: gid,
                egid: gid,
                sgid: gid,
                fsgid: gid,
            },
            DacCredentialView::new(uid, gid, groups, [0; CAPABILITY_WORDS], true),
            user_ns,
            false,
            false,
        )
    }

    #[test]
    fn armed_pending_fork_drop_restores_the_exact_child_owner() {
        const PROCESS_OWNER: u32 = 0xfffe_1000;
        const PARENT_OWNER: u32 = 0xfffe_1001;
        const CHILD_OWNER: u32 = 0xfffe_1002;

        let user_ns = UserNamespace::try_new_root().unwrap();
        let parent = actor(PARENT_OWNER, PROCESS_OWNER, user_ns.clone());
        let child = actor(CHILD_OWNER, PROCESS_OWNER, user_ns);
        keyctl(
            &parent,
            KeyctlCommand::GetKeyringId {
                keyring: KEY_SPEC_THREAD_KEYRING,
                create: true,
            },
        )
        .unwrap();
        keyctl(
            &parent,
            KeyctlCommand::GetKeyringId {
                keyring: KEY_SPEC_SESSION_KEYRING,
                create: true,
            },
        )
        .unwrap();
        keyctl(
            &parent,
            KeyctlCommand::SetReqKeyring {
                setting: ReqKeyDefault::Session,
            },
        )
        .unwrap();

        let parent_owner = KeyTaskOwner::new(PARENT_OWNER, PROCESS_OWNER);
        let child_owner = KeyTaskOwner::new(CHILD_OWNER, PROCESS_OWNER);
        let pending = prepare_fork(
            parent_owner,
            child_owner,
            true,
            child.real_uid(),
            child.real_gid(),
        )
        .unwrap();
        drop(pending);

        // A stale child thread/session/default root would make the exact retry
        // fail with BadState. Commit the retry, then retire both live owners so
        // this global-service test leaves no task-scoped state behind.
        prepare_fork(
            parent_owner,
            child_owner,
            true,
            child.real_uid(),
            child.real_gid(),
        )
        .unwrap()
        .commit();
        exit_committed(child_owner, false).unwrap();
        exit_committed(parent_owner, true).unwrap();
    }
}
