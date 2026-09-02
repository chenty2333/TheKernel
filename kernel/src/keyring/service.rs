use alloc::{string::String, vec::Vec};
use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::monotonic_time;
use axpoll::PollSet;
#[cfg(not(test))]
use axsync::Mutex as KeyManagerMutex;
#[cfg(test)]
use spin::Mutex as KeyManagerMutex;
use thekernel_linux_keyring::{
    ForkPlan as LinuxForkPlan, KeyTaskOwner as LinuxKeyTaskOwner,
    LifecyclePlan as LinuxLifecyclePlan, ProcessOwnerId, TaskOwnerId, plan_exec, plan_exit,
};

use super::manager::{ForkUndo, KeyManager, RequestKeyBegin};
pub(crate) use super::{
    accounting::{
        key_maxbytes, key_maxkeys, key_root_maxbytes, key_root_maxkeys, set_key_maxbytes,
        set_key_maxkeys, set_key_root_maxbytes, set_key_root_maxkeys,
    },
    contract::{KeyActor, KeyTaskOwner, KeyUserRecord, KeyctlCommand, KeyctlOutput, ReqKeyDefault},
    object::KeyTypeKind,
};
use crate::task::{Kgid, Kuid};

const REQUEST_KEY_TIMEOUT: Duration = Duration::from_secs(60);
static REQUEST_KEY_WAITERS: PollSet = PollSet::new();

/// Kernel-only RPCSEC_GSS mechanism material.  It deliberately lives behind
/// the keyring service lock rather than in rpc_pipefs or an ordinary VFS
/// xattr: rpc.gssd may import a context, but neither the daemon nor a process
/// holding the pipe file can read that mechanism-private blob back.
/// Not `Debug`: this owns raw Kerberos session-key import material until the
/// mechanism consumes it.  A diagnostic must never render a secret.
pub(crate) struct NfsGssContextKey {
    pub(crate) serial: u64,
    pub(crate) uid: u32,
    target: Vec<u8>,
    service: Vec<u8>,
    pub(crate) payload: Vec<u8>,
}
impl Drop for NfsGssContextKey {
    fn drop(&mut self) {
        self.payload.fill(0);
        self.target.fill(0);
        self.service.fill(0);
    }
}

static NFS_GSS_CONTEXTS: KeyManagerMutex<Vec<NfsGssContextKey>> = KeyManagerMutex::new(Vec::new());
static NFS_GSS_NEXT_SERIAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

pub(crate) fn publish_nfs_gss_context(
    uid: u32,
    target: &[u8],
    service: &[u8],
    payload: &[u8],
) -> AxResult<u64> {
    if payload.is_empty() || payload.len() > 1024 || target.is_empty() || service.is_empty() {
        return Err(AxError::InvalidInput);
    }
    let mut contexts = NFS_GSS_CONTEXTS.lock();
    contexts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(payload.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.extend_from_slice(payload);
    let serial = NFS_GSS_NEXT_SERIAL
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        .max(1);
    contexts.push(NfsGssContextKey {
        serial,
        uid,
        target: target.to_vec(),
        service: service.to_vec(),
        payload: owned,
    });
    Ok(serial)
}

/// One-shot handoff to the NFS mechanism.  The context cannot be copied or
/// reused by a later mount; a failed mount drops it with the reply.
pub(crate) fn take_nfs_gss_context(
    serial: u64,
    uid: u32,
    target: &[u8],
    service: &[u8],
) -> AxResult<Vec<u8>> {
    let mut contexts = NFS_GSS_CONTEXTS.lock();
    let index = contexts
        .iter()
        .position(|context| {
            context.serial == serial
                && context.uid == uid
                && context.target == target
                && context.service == service
        })
        .ok_or(LinuxError::ENOKEY)?;
    let mut context = contexts.remove(index);
    let mut payload = Vec::new();
    core::mem::swap(&mut payload, &mut context.payload);
    Ok(payload)
}

pub(crate) fn revoke_nfs_gss_context(serial: u64) {
    let mut contexts = NFS_GSS_CONTEXTS.lock();
    if let Some(index) = contexts.iter().position(|context| context.serial == serial) {
        contexts.remove(index);
    }
}

pub(crate) fn notify_request_key_waiters() {
    REQUEST_KEY_WAITERS.wake();
}

/// Called by the usermode-helper factory after TASK_TABLE/process publication
/// but before the prepared task is made runnable.
pub(crate) fn install_request_key_authority(serial: i32, helper_thread_owner: u32) -> AxResult<()> {
    KEY_MANAGER
        .lock()
        .install_construction_authority(serial, helper_thread_owner)
}

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
    /// The policy plan owns only transition validation/state.  The manager
    /// remains the sole owner of roots, locks, quota and the concrete undo.
    plan: Option<LinuxForkPlan>,
}

impl PendingKeyFork {
    /// Transfers the staged subscriptions to the published child. Preparation
    /// performed every allocation and reference update, so this is infallible.
    pub(crate) fn commit(mut self) {
        self.plan
            .take()
            .expect("prepared keyring fork is missing its policy plan")
            .commit()
            .expect("prepared keyring fork policy transition is invalid");
        self.undo.take();
    }
}

impl Drop for PendingKeyFork {
    fn drop(&mut self) {
        if let Some(plan) = self.plan.as_mut() {
            plan.rollback()
                .expect("prepared keyring fork policy rollback is invalid");
        }
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
    let plan = LinuxForkPlan::prepare(
        linux_task_owner(parent)?,
        linux_task_owner(child)?,
        clone_thread,
    )
    .map_err(|_| axerrno::AxError::BadState)?;
    let undo =
        KEY_MANAGER
            .lock()
            .prepare_fork(parent, child, clone_thread, child_ruid, child_rgid)?;
    Ok(PendingKeyFork {
        undo: Some(undo),
        plan: Some(plan),
    })
}

pub(crate) fn exec_committed(owner: KeyTaskOwner) -> AxResult<()> {
    match plan_exec(linux_task_owner(owner)?) {
        LinuxLifecyclePlan::Exec { .. } => {}
        LinuxLifecyclePlan::Exit { .. } => return Err(axerrno::AxError::BadState),
    }
    KEY_MANAGER.lock().exec_committed(owner)
}

pub(crate) fn exit_committed(owner: KeyTaskOwner, final_thread: bool) -> AxResult<()> {
    match plan_exit(linux_task_owner(owner)?, final_thread) {
        LinuxLifecyclePlan::Exit { .. } => {}
        LinuxLifecyclePlan::Exec { .. } => return Err(axerrno::AxError::BadState),
    }
    KEY_MANAGER.lock().exit_committed(owner, final_thread)
}

fn linux_task_owner(owner: KeyTaskOwner) -> AxResult<LinuxKeyTaskOwner> {
    let thread = TaskOwnerId::new(owner.thread_owner()).ok_or(axerrno::AxError::BadState)?;
    let process = ProcessOwnerId::new(owner.process_owner()).ok_or(axerrno::AxError::BadState)?;
    Ok(LinuxKeyTaskOwner { thread, process })
}

pub(crate) fn credential_fsids_precommit(
    thread_owner: u32,
    fsuid_change: Option<Kuid>,
    fsgid_change: Option<Kgid>,
) -> AxResult<()> {
    KEY_MANAGER
        .lock()
        .credential_fsids_precommit(thread_owner, fsuid_change, fsgid_change)
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
    callout: Option<&str>,
    dest_keyring: i32,
) -> AxResult<isize> {
    let begin =
        KEY_MANAGER
            .lock()
            .begin_request_key(actor, kind, description, callout, dest_keyring)?;
    let mut helper = None;
    // Only the request which inserted the pending construction owns helper
    // cancellation. Coalesced callers may time out independently without
    // tearing down a construction still awaited by another requester.
    let mut owns_construction = false;
    let serial = match begin {
        RequestKeyBegin::Resolved(serial) => return Ok(serial),
        RequestKeyBegin::Pending(serial) => serial,
        RequestKeyBegin::Construction(construction) => {
            // Process publication precedes authority installation. The helper
            // receives a deliberately reduced key view plus exactly one
            // construction serial, never the requester's ambient keyrings.
            let spawned_helper = crate::task::spawn_request_key_helper(
                construction.serial,
                construction.kind,
                construction.description,
                construction.callout,
            );
            let spawned_helper = match spawned_helper {
                Ok(helper) => helper,
                Err(error) => {
                    let _ = KEY_MANAGER.lock().abort_request_key(construction.serial);
                    notify_request_key_waiters();
                    return Err(error);
                }
            };
            debug_assert_eq!(spawned_helper.key_authority, Some(construction.serial));
            helper = Some(spawned_helper);
            owns_construction = true;
            construction.serial
        }
    };

    let deadline = monotonic_time()
        .checked_add(REQUEST_KEY_TIMEOUT)
        .unwrap_or(Duration::MAX);
    let waited =
        crate::readiness::block_on_poll_set_until(&REQUEST_KEY_WAITERS, Some(deadline), || {
            match KEY_MANAGER
                .lock()
                .finish_request_key(actor, serial, dest_keyring)
            {
                Err(error) if error == LinuxError::EINPROGRESS.into() => Err(AxError::WouldBlock),
                result => result,
            }
        });
    match waited {
        Ok(result) => result,
        Err(_) => {
            if owns_construction {
                // Completion and abort share this sole manager transaction.
                // Do not cancel before this edge: a helper that already
                // completed is allowed to publish and win the race.
                let result =
                    KEY_MANAGER
                        .lock()
                        .finish_or_abort_request_key(actor, serial, dest_keyring);
                if result == Err(LinuxError::ENOKEY.into()) {
                    if let Some(helper) = helper.as_ref() {
                        helper.cancel();
                    }
                    notify_request_key_waiters();
                }
                result
            } else {
                // A joined request owns no helper and cannot abort the
                // shared construction; return its ordinary timeout result.
                match KEY_MANAGER
                    .lock()
                    .finish_request_key(actor, serial, dest_keyring)
                {
                    Ok(result) => Ok(result),
                    Err(error) if error == LinuxError::EINPROGRESS.into() => {
                        Err(LinuxError::ENOKEY.into())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }
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
