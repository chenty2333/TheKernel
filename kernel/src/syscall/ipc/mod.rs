use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use self::shm::Mutex as ShmMutex;

mod mqueue;
mod msg;
mod sem;
mod shm;
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::{CAP_CHOWN, CAP_IPC_OWNER, CAP_SYS_ADMIN, CAP_SYS_RESOURCE, *},
};

pub use self::{mqueue::*, msg::*, sem::*, shm::*};
use crate::task::{Cred, Kgid, Kuid, UserNamespace, ns_capable};

static IPC_NAMESPACE_ID: AtomicU64 = AtomicU64::new(1);

/// All IPC objects visible through one Linux IPC namespace.
///
/// The namespace owns its SysV tables and POSIX mqueue name space.  Nothing
/// in these managers is global: cloning a process keeps this `Arc`, while
/// `CLONE_NEWIPC` constructs a fresh instance.  The independent cursors make
/// ID reuse local to the namespace as Linux requires.
pub(crate) struct IpcNamespace {
    id: u64,
    owner_user_ns: Arc<UserNamespace>,
    msg: Mutex<MsgManager>,
    sem: Mutex<SemManager>,
    shm: ShmMutex<ShmManager>,
    shm_transaction: ShmMutex<()>,
    mqueue: Mutex<MqManager>,
    mqueue_notifications: Mutex<MqNotificationRegistry>,
    shm_locked_bytes: Mutex<BTreeMap<Kuid, usize>>,
    mq_accounted_bytes: Mutex<BTreeMap<Kuid, usize>>,
    msg_next_id: AtomicI32,
    sem_next_id: AtomicI32,
    shm_next_id: AtomicI32,
    sequence: AtomicU64,
}

impl IpcNamespace {
    pub(crate) fn try_new(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let id = IPC_NAMESPACE_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| AxError::from(LinuxError::ENOSPC))?;
        Arc::try_new(Self {
            id,
            owner_user_ns,
            msg: Mutex::new(MsgManager::new()),
            sem: Mutex::new(SemManager::new()),
            shm: ShmMutex::new(ShmManager::new()),
            shm_transaction: ShmMutex::new(()),
            mqueue: Mutex::new(MqManager::new()),
            mqueue_notifications: Mutex::new(MqNotificationRegistry::new()),
            shm_locked_bytes: Mutex::new(BTreeMap::new()),
            mq_accounted_bytes: Mutex::new(BTreeMap::new()),
            msg_next_id: AtomicI32::new(-1),
            sem_next_id: AtomicI32::new(-1),
            shm_next_id: AtomicI32::new(-1),
            sequence: AtomicU64::new(0),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn msg_manager(&self) -> &Mutex<MsgManager> {
        &self.msg
    }
    pub(crate) fn sem_manager(&self) -> &Mutex<SemManager> {
        &self.sem
    }
    pub(crate) fn shm_manager(&self) -> &ShmMutex<ShmManager> {
        &self.shm
    }
    pub(crate) fn shm_transaction(&self) -> &ShmMutex<()> {
        &self.shm_transaction
    }
    pub(crate) fn mqueue_manager(&self) -> &Mutex<MqManager> {
        &self.mqueue
    }
    pub(crate) fn mqueue_notifications(&self) -> &Mutex<MqNotificationRegistry> {
        &self.mqueue_notifications
    }

    pub(crate) fn next_msg_id(&self) -> &AtomicI32 {
        &self.msg_next_id
    }
    pub(crate) fn next_sem_id(&self) -> &AtomicI32 {
        &self.sem_next_id
    }
    pub(crate) fn next_shm_id(&self) -> &AtomicI32 {
        &self.shm_next_id
    }

    pub(crate) fn next_sequence(&self) -> u16 {
        self.sequence.fetch_add(1, Ordering::Relaxed) as u16
    }

    /// Charges an SHM_LOCK pin to the caller's real user.  The returned token
    /// is retained by the segment while it is locked and releases exactly the
    /// charged bytes on SHM_UNLOCK, IPC_RMID finalization, or namespace drop.
    pub(crate) fn try_charge_shm_lock(
        self: &Arc<Self>,
        user: Kuid,
        bytes: usize,
        limit: u64,
    ) -> AxResult<ShmLockCharge> {
        let mut charges = self.shm_locked_bytes.lock();
        let current = charges.get(&user).copied().unwrap_or(0);
        let next = current.checked_add(bytes).ok_or(AxError::NoMemory)?;
        if limit != linux_raw_sys::general::RLIM_INFINITY as i64 as u64
            && u64::try_from(next).map_err(|_| AxError::NoMemory)? > limit
        {
            return Err(AxError::from(LinuxError::ENOMEM));
        }
        charges.insert(user, next);
        Ok(ShmLockCharge {
            namespace: self.clone(),
            user,
            bytes,
        })
    }

    fn release_shm_lock_charge(&self, user: Kuid, bytes: usize) {
        let mut charges = self.shm_locked_bytes.lock();
        let Some(current) = charges.get_mut(&user) else {
            return;
        };
        *current = current.saturating_sub(bytes);
        if *current == 0 {
            charges.remove(&user);
        }
    }

    /// Reserves POSIX mqueue capacity against the caller's `RLIMIT_MSGQUEUE`.
    /// The charge token belongs to the queue, rather than an open descriptor,
    /// so unlink does not release it until the final file reference vanishes.
    pub(crate) fn try_charge_mqueue(
        self: &Arc<Self>,
        user: Kuid,
        bytes: usize,
        limit: u64,
    ) -> AxResult<MqCharge> {
        let mut charges = self.mq_accounted_bytes.lock();
        let current = charges.get(&user).copied().unwrap_or(0);
        let next = current.checked_add(bytes).ok_or(AxError::NoMemory)?;
        if limit != linux_raw_sys::general::RLIM_INFINITY as i64 as u64
            && u64::try_from(next).map_err(|_| AxError::NoMemory)? > limit
        {
            return Err(AxError::from(LinuxError::EMFILE));
        }
        charges.insert(user, next);
        Ok(MqCharge {
            namespace: self.clone(),
            user,
            bytes,
        })
    }

    fn release_mqueue_charge(&self, user: Kuid, bytes: usize) {
        let mut charges = self.mq_accounted_bytes.lock();
        let Some(current) = charges.get_mut(&user) else {
            return;
        };
        *current = current.saturating_sub(bytes);
        if *current == 0 {
            charges.remove(&user);
        }
    }
}

/// RAII ownership for a real `SHM_LOCK` memlock charge.
pub(crate) struct ShmLockCharge {
    namespace: Arc<IpcNamespace>,
    user: Kuid,
    bytes: usize,
}

impl Drop for ShmLockCharge {
    fn drop(&mut self) {
        self.namespace
            .release_shm_lock_charge(self.user, self.bytes);
    }
}

/// RAII ownership for one namespace-local POSIX mqueue resource charge.
pub(crate) struct MqCharge {
    namespace: Arc<IpcNamespace>,
    user: Kuid,
    bytes: usize,
}

impl Drop for MqCharge {
    fn drop(&mut self) {
        self.namespace.release_mqueue_charge(self.user, self.bytes);
    }
}

/// Allocate a Linux-visible SysV IPC ID while the caller holds its manager
/// lock. With `n` live IDs, no more than `n + 1` distinct probes are needed
/// to find a free ID unless the representable ID space is exhausted.
pub(crate) fn allocate_ipc_id<F>(
    cursor: &AtomicI32,
    requested: Option<i32>,
    occupied_count: usize,
    is_occupied: F,
) -> AxResult<i32>
where
    F: FnMut(i32) -> bool,
{
    allocate_ipc_id_in_range(cursor, requested, occupied_count, i32::MAX, is_occupied)
}

fn allocate_ipc_id_in_range<F>(
    cursor: &AtomicI32,
    requested: Option<i32>,
    occupied_count: usize,
    maximum: i32,
    mut is_occupied: F,
) -> AxResult<i32>
where
    F: FnMut(i32) -> bool,
{
    debug_assert!(maximum >= 0);
    if let Some(id) = requested
        && id >= 0
        && id <= maximum
        && !is_occupied(id)
    {
        return Ok(id);
    }

    let probes = (occupied_count as u64)
        .saturating_add(1)
        .min(maximum as u64 + 1);
    for _ in 0..probes {
        let previous = cursor
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                let candidate = if current < 0 || current > maximum {
                    0
                } else {
                    current
                };
                Some(if candidate == maximum {
                    0
                } else {
                    candidate + 1
                })
            })
            .unwrap_or(0);
        let id = if previous < 0 || previous > maximum {
            0
        } else {
            previous
        };
        if !is_occupied(id) {
            return Ok(id);
        }
    }
    Err(AxError::from(LinuxError::ENOSPC))
}

// IPC command constants
const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: i32 = 0o1000;
const IPC_EXCL: i32 = 0o2000;
const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;
const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;
const MSG_STAT_ANY: i32 = 13;
const GETPID: i32 = 11;
const GETVAL: i32 = 12;
const GETALL: i32 = 13;
const GETNCNT: i32 = 14;
const GETZCNT: i32 = 15;
const SETVAL: i32 = 16;
const SETALL: i32 = 17;
const SEM_STAT: i32 = 18;
const SEM_INFO: i32 = 19;
const SEM_STAT_ANY: i32 = 20;
pub(crate) const SHM_LOCK: i32 = 11;
pub(crate) const SHM_UNLOCK: i32 = 12;
pub(crate) const SHM_STAT: i32 = 13;
pub(crate) const SHM_INFO: i32 = 14;
pub(crate) const SHM_STAT_ANY: i32 = 15;

// Permission bits
const USER_READ: c_ushort = 0o400;
const USER_WRITE: c_ushort = 0o200;
const USER_EXEC: c_ushort = 0o100;
const GROUP_READ: c_ushort = 0o040;
const GROUP_WRITE: c_ushort = 0o020;
const GROUP_EXEC: c_ushort = 0o010;
const OTHER_READ: c_ushort = 0o004;
const OTHER_WRITE: c_ushort = 0o002;
const OTHER_EXEC: c_ushort = 0o001;
const IPC_MODE_MASK: c_ushort = 0o777;
pub(crate) const SHM_DEST: u32 = 0o1000;
pub(crate) const SHM_LOCKED: u32 = 0o2000;
pub(crate) const SHMMIN: usize = 1;
const DEFAULT_SHMMAX: usize = 0xFFFF_FFFF;
const DEFAULT_SHMMNI: usize = 4096;
const DEFAULT_SHMALL: usize = 0xFFFF_FFFF;
const MAX_SHMMNI: usize = 32_768;

static SHM_MAX_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMMAX);
static SHM_MNI_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMMNI);
static SHM_ALL_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_SHMALL);

pub(crate) fn shmmax_limit() -> usize {
    SHM_MAX_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_shmmax_limit(value: usize) {
    SHM_MAX_LIMIT.store(value, Ordering::Relaxed);
}

pub(crate) fn shmmni_limit() -> usize {
    SHM_MNI_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_shmmni_limit(value: usize) -> AxResult<()> {
    if value > MAX_SHMMNI {
        return Err(AxError::InvalidInput);
    }
    SHM_MNI_LIMIT.store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn shmall_limit() -> usize {
    SHM_ALL_LIMIT.load(Ordering::Relaxed)
}

pub(crate) fn set_shmall_limit(value: usize) {
    SHM_ALL_LIMIT.store(value, Ordering::Relaxed);
}

/// Data structure used to pass permission information to IPC operations.
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct IpcPerm {
    /// Key supplied to msgget(2)
    pub key: __kernel_key_t,
    /// Effective UID of owner
    pub uid: __kernel_uid_t,
    /// Effective GID of owner
    pub gid: __kernel_gid_t,
    /// Effective UID of creator
    pub cuid: __kernel_uid_t,
    /// Effective GID of creator
    pub cgid: __kernel_gid_t,
    /// Permissions (least significant 9 bits define access permissions)
    pub mode: c_ushort,
    /// Padding
    pub pad1: c_ushort,
    /// Sequence number
    pub seq: c_ushort,
    /// Padding
    pub pad2: c_ushort,
    /// Unused field
    pub unused0: c_ulong,
    /// Unused field
    pub unused1: c_ulong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IpcAccess {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IpcAuthority {
    access_override: bool,
    control_override: bool,
    resource_override: bool,
    lock_override: bool,
    chown_override: bool,
}

impl IpcAuthority {
    const NONE: Self = Self {
        access_override: false,
        control_override: false,
        resource_override: false,
        lock_override: false,
        chown_override: false,
    };
}

#[derive(Clone, Copy)]
struct IpcIdentity<'a> {
    euid: Kuid,
    egid: Kgid,
    supplementary_groups: &'a [Kgid],
}

impl IpcIdentity<'_> {
    fn owns_or_created(self, perm: &IpcPerm) -> bool {
        [perm.uid, perm.cuid]
            .into_iter()
            .filter_map(Kuid::from_raw)
            .any(|uid| uid == self.euid)
    }

    fn matches_owner_group(self, perm: &IpcPerm) -> bool {
        [perm.gid, perm.cgid]
            .into_iter()
            .filter_map(Kgid::from_raw)
            .any(|gid| gid == self.egid || self.supplementary_groups.binary_search(&gid).is_ok())
    }

    fn may_assume_group(self, gid: Kgid) -> bool {
        gid == self.egid || self.supplementary_groups.binary_search(&gid).is_ok()
    }
}

/// Immutable actor credentials and namespace-relative authority for one SysV
/// IPC syscall. Dispatch binds this to the owning IPC namespace's user
/// namespace; the initial-namespace constructor remains only for bootstrap
/// paths that have not yet been moved through NamespaceProxy.
pub(crate) struct IpcAccessContext {
    actor: Arc<Cred>,
    governing_user_ns: Arc<UserNamespace>,
    euid: Kuid,
    egid: Kgid,
    authority: IpcAuthority,
}

impl IpcAccessContext {
    fn new(actor: Arc<Cred>, governing_user_ns: Arc<UserNamespace>) -> Self {
        let ids = actor.ids();
        let authority = IpcAuthority {
            access_override: ns_capable(&actor, &governing_user_ns, CAP_IPC_OWNER),
            control_override: ns_capable(&actor, &governing_user_ns, CAP_SYS_ADMIN),
            resource_override: ns_capable(&actor, &governing_user_ns, CAP_SYS_RESOURCE),
            lock_override: ns_capable(&actor, &governing_user_ns, CAP_IPC_LOCK),
            chown_override: ns_capable(&actor, &governing_user_ns, CAP_CHOWN),
        };
        Self {
            actor,
            governing_user_ns,
            euid: ids.euid,
            egid: ids.egid,
            authority,
        }
    }

    fn for_initial_user_namespace(actor: Arc<Cred>) -> Self {
        let mut governing_user_ns = actor.user_ns().clone();
        while let Some(parent) = governing_user_ns.parent() {
            governing_user_ns = parent;
        }
        debug_assert!(governing_user_ns.is_initial());
        Self::new(actor, governing_user_ns)
    }

    pub(crate) fn for_ipc_namespace(actor: Arc<Cred>, ipc_ns: &IpcNamespace) -> Self {
        Self::new(actor, ipc_ns.owner_user_ns().clone())
    }

    fn identity(&self) -> IpcIdentity<'_> {
        IpcIdentity {
            euid: self.euid,
            egid: self.egid,
            supplementary_groups: self.actor.groups().as_slice(),
        }
    }

    fn effective_uid_raw(&self) -> u32 {
        self.euid.into_raw()
    }

    fn effective_gid_raw(&self) -> u32 {
        self.egid.into_raw()
    }

    fn allows(&self, perm: &IpcPerm, access: IpcAccess) -> bool {
        ipc_mode_allows(self.identity(), self.authority, perm, access)
    }

    /// Match Linux `ipcperms()`: the low permission bits in a get request
    /// are collapsed across owner/group/other classes, then compared with the
    /// class selected for this caller.  This intentionally includes execute
    /// bits even though no SysV data operation consumes an execute access;
    /// `msgget`/`semget`/`shmget` still pass the complete requested mode to
    /// the common IPC permission rule.
    fn allows_requested_mode(&self, perm: &IpcPerm, requested: c_ushort) -> bool {
        let requested = ((requested >> 6) | (requested >> 3) | requested) & 0o7;
        let granted = if self.identity().owns_or_created(perm) {
            perm.mode >> 6
        } else if self.identity().matches_owner_group(perm) {
            perm.mode >> 3
        } else {
            perm.mode
        } & 0o7;
        requested & !granted == 0 || self.authority.access_override
    }

    fn may_control(&self, perm: &IpcPerm) -> bool {
        self.identity().owns_or_created(perm) || self.authority.control_override
    }

    /// SHM_LOCK/SHM_UNLOCK use their own privilege rule.  Since Linux 2.6.10
    /// an owner or creator may operate on a segment (subject to memlock for
    /// lock), while CAP_IPC_LOCK both authorizes it and bypasses that limit;
    /// CAP_SYS_ADMIN is not a substitute.
    fn may_lock_shm(&self, perm: &IpcPerm) -> bool {
        self.identity().owns_or_created(perm) || self.authority.lock_override
    }

    fn bypasses_shm_memlock_limit(&self) -> bool {
        self.authority.lock_override
    }

    fn may_raise_resource_limit(&self) -> bool {
        self.authority.resource_override
    }

    fn map_permission_update(
        &self,
        uid: __kernel_uid_t,
        gid: __kernel_gid_t,
        mode: c_ushort,
    ) -> AxResult<IpcPermissionUpdateRequest> {
        let uid = self
            .actor
            .user_ns()
            .make_kuid(uid)
            .ok_or(AxError::InvalidInput)?;
        let gid = self
            .actor
            .user_ns()
            .make_kgid(gid)
            .ok_or(AxError::InvalidInput)?;
        Ok(IpcPermissionUpdateRequest { uid, gid, mode })
    }

    fn prepare_permission_update(
        &self,
        current: &IpcPerm,
        request: IpcPermissionUpdateRequest,
    ) -> AxResult<PreparedIpcPermissionUpdate> {
        if !self.may_control(current) {
            return Err(AxError::OperationNotPermitted);
        }
        let identity = self.identity();
        let current_uid = Kuid::from_raw(current.uid).ok_or(AxError::BadState)?;
        let current_gid = Kgid::from_raw(current.gid).ok_or(AxError::BadState)?;
        let uid_allowed = request.uid == current_uid
            || request.uid == identity.euid
            || self.authority.chown_override;
        let gid_allowed = request.gid == current_gid
            || identity.may_assume_group(request.gid)
            || self.authority.chown_override;
        if !uid_allowed || !gid_allowed {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(PreparedIpcPermissionUpdate {
            uid: request.uid,
            gid: request.gid,
            mode: (current.mode & !IPC_MODE_MASK) | (request.mode & IPC_MODE_MASK),
        })
    }

    fn governing_user_ns(&self) -> &Arc<UserNamespace> {
        &self.governing_user_ns
    }
}

fn ipc_mode_allows(
    identity: IpcIdentity<'_>,
    authority: IpcAuthority,
    perm: &IpcPerm,
    access: IpcAccess,
) -> bool {
    let bit = if identity.owns_or_created(perm) {
        match access {
            IpcAccess::Read => USER_READ,
            IpcAccess::Write => USER_WRITE,
            IpcAccess::Execute => USER_EXEC,
        }
    } else if identity.matches_owner_group(perm) {
        match access {
            IpcAccess::Read => GROUP_READ,
            IpcAccess::Write => GROUP_WRITE,
            IpcAccess::Execute => GROUP_EXEC,
        }
    } else {
        match access {
            IpcAccess::Read => OTHER_READ,
            IpcAccess::Write => OTHER_WRITE,
            IpcAccess::Execute => OTHER_EXEC,
        }
    };
    perm.mode & bit != 0 || authority.access_override
}

#[derive(Clone, Copy)]
struct IpcPermissionUpdateRequest {
    uid: Kuid,
    gid: Kgid,
    mode: c_ushort,
}

#[derive(Clone, Copy)]
struct PreparedIpcPermissionUpdate {
    uid: Kuid,
    gid: Kgid,
    mode: c_ushort,
}

impl PreparedIpcPermissionUpdate {
    fn commit(self, perm: &mut IpcPerm) {
        perm.uid = self.uid.into_raw();
        perm.gid = self.gid.into_raw();
        perm.mode = self.mode;
    }
}

#[cfg(test)]
mod credential_caller_tests {
    use alloc::{collections::BTreeSet, sync::Arc};
    use core::sync::atomic::AtomicI32;

    use super::*;
    use crate::task::{Cred, UserNamespace};

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn perm(uid: u32, gid: u32, mode: c_ushort) -> IpcPerm {
        IpcPerm {
            key: 1,
            uid,
            gid,
            cuid: uid,
            cgid: gid,
            mode,
            pad1: 0,
            seq: 0,
            pad2: 0,
            unused0: 0,
            unused1: 0,
        }
    }

    #[test]
    fn credential_caller_owner_class_is_selected_exclusively() {
        let groups = [];
        let actor = IpcIdentity {
            euid: kuid(1000),
            egid: kgid(100),
            supplementary_groups: &groups,
        };
        let object = perm(1000, 100, OTHER_READ | GROUP_READ);
        assert!(!ipc_mode_allows(
            actor,
            IpcAuthority::NONE,
            &object,
            IpcAccess::Read
        ));
    }

    #[test]
    fn credential_caller_supplementary_group_selects_group_class() {
        let groups = [kgid(200)];
        let actor = IpcIdentity {
            euid: kuid(1000),
            egid: kgid(100),
            supplementary_groups: &groups,
        };
        let object = perm(2000, 200, GROUP_WRITE);
        assert!(ipc_mode_allows(
            actor,
            IpcAuthority::NONE,
            &object,
            IpcAccess::Write
        ));
    }

    #[test]
    fn credential_caller_cap_ipc_owner_does_not_grant_control() {
        let groups = [];
        let identity = IpcIdentity {
            euid: kuid(1000),
            egid: kgid(100),
            supplementary_groups: &groups,
        };
        let authority = IpcAuthority {
            access_override: true,
            ..IpcAuthority::NONE
        };
        let object = perm(2000, 200, 0);
        assert!(ipc_mode_allows(
            identity,
            authority,
            &object,
            IpcAccess::Read
        ));
        assert!(!identity.owns_or_created(&object) && !authority.control_override);
    }

    #[test]
    fn credential_caller_control_and_resource_capabilities_stay_separate() {
        let admin = IpcAuthority {
            control_override: true,
            ..IpcAuthority::NONE
        };
        let resource = IpcAuthority {
            resource_override: true,
            ..IpcAuthority::NONE
        };
        assert!(admin.control_override);
        assert!(!admin.resource_override);
        assert!(!admin.chown_override);
        assert!(resource.resource_override);
        assert!(!resource.control_override);
        assert!(!resource.chown_override);
        assert!(!admin.access_override && !resource.access_override);
    }

    fn root_context_with_authority(authority: IpcAuthority) -> IpcAccessContext {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_ns.clone()).unwrap();
        let mut context = IpcAccessContext::new(actor, root_ns);
        context.authority = authority;
        context
    }

    #[test]
    fn credential_caller_creator_can_keep_owner_and_change_only_mode() {
        let context = root_context_with_authority(IpcAuthority::NONE);
        let mut object = perm(2000, 200, 0o600);
        object.cuid = context.effective_uid_raw();
        let request = IpcPermissionUpdateRequest {
            uid: kuid(2000),
            gid: kgid(200),
            mode: 0o640,
        };
        let prepared = context.prepare_permission_update(&object, request).unwrap();
        prepared.commit(&mut object);
        assert_eq!((object.uid, object.gid, object.mode), (2000, 200, 0o640));
    }

    #[test]
    fn credential_caller_arbitrary_owner_change_requires_cap_chown() {
        let mut context = root_context_with_authority(IpcAuthority::NONE);
        let object = perm(0, 0, 0o600);
        let request = IpcPermissionUpdateRequest {
            uid: kuid(2000),
            gid: kgid(0),
            mode: 0o600,
        };
        assert!(matches!(
            context.prepare_permission_update(&object, request),
            Err(AxError::OperationNotPermitted)
        ));
        context.authority.chown_override = true;
        assert!(context.prepare_permission_update(&object, request).is_ok());
    }

    #[test]
    fn credential_caller_cap_sys_admin_control_does_not_imply_cap_chown() {
        let context = root_context_with_authority(IpcAuthority {
            control_override: true,
            ..IpcAuthority::NONE
        });
        let object = perm(2000, 200, 0o600);
        let keep_owner = IpcPermissionUpdateRequest {
            uid: kuid(2000),
            gid: kgid(200),
            mode: 0o644,
        };
        assert!(
            context
                .prepare_permission_update(&object, keep_owner)
                .is_ok()
        );
        let change_owner = IpcPermissionUpdateRequest {
            uid: kuid(3000),
            ..keep_owner
        };
        assert!(matches!(
            context.prepare_permission_update(&object, change_owner),
            Err(AxError::OperationNotPermitted)
        ));
    }

    #[test]
    fn credential_caller_invalid_live_owner_ids_fail_closed() {
        let context = root_context_with_authority(IpcAuthority {
            control_override: true,
            chown_override: true,
            ..IpcAuthority::NONE
        });
        let mut object = perm(0, 0, 0o600);
        object.uid = u32::MAX;
        let request = IpcPermissionUpdateRequest {
            uid: kuid(0),
            gid: kgid(0),
            mode: 0o600,
        };
        assert!(matches!(
            context.prepare_permission_update(&object, request),
            Err(AxError::BadState)
        ));
    }

    #[test]
    fn credential_caller_ns_capable_follows_ancestor_direction() {
        let root_ns = UserNamespace::try_new_root().unwrap();
        let root_cred = Cred::try_root(root_ns.clone()).unwrap();
        let child_ns = root_ns
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_cred = Cred::try_with_user_namespace(&root_cred, child_ns.clone()).unwrap();

        let child_over_root = IpcAccessContext::new(child_cred, root_ns.clone());
        assert!(!child_over_root.authority.access_override);
        assert!(!child_over_root.authority.control_override);
        assert!(!child_over_root.authority.resource_override);
        assert!(!child_over_root.authority.chown_override);

        let root_over_child = IpcAccessContext::new(root_cred, child_ns.clone());
        assert!(root_over_child.authority.access_override);
        assert!(root_over_child.authority.control_override);
        assert!(root_over_child.authority.resource_override);
        assert!(root_over_child.authority.chown_override);
        assert!(Arc::ptr_eq(root_over_child.governing_user_ns(), &child_ns));
    }

    #[test]
    fn ipc_id_allocator_wraps_without_negative_ids_and_skips_live_ids() {
        let cursor = AtomicI32::new(i32::MAX - 1);
        assert_eq!(
            allocate_ipc_id_in_range(&cursor, None, 0, i32::MAX, |_| false),
            Ok(i32::MAX - 1)
        );
        assert_eq!(
            allocate_ipc_id_in_range(&cursor, None, 0, i32::MAX, |_| false),
            Ok(i32::MAX)
        );
        assert_eq!(
            allocate_ipc_id_in_range(&cursor, None, 1, i32::MAX, |id| id == 0),
            Ok(1)
        );

        let negative_cursor = AtomicI32::new(-7);
        assert_eq!(
            allocate_ipc_id_in_range(&negative_cursor, None, 0, 7, |_| false),
            Ok(0)
        );
        assert_eq!(
            allocate_ipc_id_in_range(&AtomicI32::new(3), Some(-1), 0, 7, |_| false,),
            Ok(3)
        );
    }

    #[test]
    fn ipc_id_allocator_rejects_a_full_test_range_without_unbounded_scan() {
        let cursor = AtomicI32::new(0);
        assert_eq!(
            allocate_ipc_id_in_range(&cursor, None, 4, 3, |_| true),
            Err(AxError::from(LinuxError::ENOSPC))
        );
    }

    #[test]
    fn ipc_id_allocation_and_publication_sequence_never_reuses_a_live_id() {
        let cursor = AtomicI32::new(0);
        let mut live = BTreeSet::new();
        for _ in 0..8 {
            let id = allocate_ipc_id_in_range(&cursor, None, live.len(), 7, |candidate| {
                live.contains(&candidate)
            })
            .unwrap();
            assert!(live.insert(id));
        }
        assert_eq!(live.len(), 8);
    }
}
