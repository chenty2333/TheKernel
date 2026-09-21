use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use tk_linux_process_adapter::Pid;

use self::shm::Mutex as ShmMutex;

mod mqueue;
mod msg;
mod sem;
mod shm;
use bytemuck::AnyBitPattern;
use linux_raw_sys::{
    ctypes::{c_ulong, c_ushort},
    general::{CAP_IPC_OWNER, CAP_SYS_ADMIN, CAP_SYS_RESOURCE, *},
};
use tk_linux_ipc::{IpcId, IpcIdTable};

pub use self::{mqueue::*, msg::*, sem::*, shm::*};
use crate::task::{AsThread, Cred, Kgid, Kuid, UserNamespace, ns_capable};

static IPC_NAMESPACE_ID: AtomicU64 = AtomicU64::new(1);

/// Linux `pid_vnr()` for a task-group identity retained by a SysV record.
///
/// A SysV object keeps a kernel-wide `struct pid` and renders it in the
/// *reader's* PID namespace when the record is copied out - `ipc/msg.c:574-575`
/// (`msg_lspid`/`msg_lrpid`), `ipc/shm.c:1144-1145`
/// (`shm_cpid`/`shm_lpid`) and `ipc/sem.c:1549` (`GETPID`) all go through
/// `pid_vnr()`, and `/proc/sysvipc/*` uses `pid_nr_ns()` against the reader's
/// `current->nsproxy->pid_ns_for_children` (`ipc/msg.c:1355-1356`,
/// `ipc/shm.c:1869-1870`).  A field that was never written holds no `struct
/// pid` at all, for which `pid_nr_ns()` reports zero.
pub(crate) fn render_task_pid(pid: Pid) -> __kernel_pid_t {
    if pid == 0 {
        return 0;
    }
    // A `struct pid` with no number in the reader's namespace renders as zero
    // (`pid_nr_ns()` leaves `nr = 0` when `upid->ns != ns`), so a creator that
    // lives in a sibling or descendant PID namespace must not leak its
    // init-namespace number into `shm_cpid`/`shm_lpid`/`msg_l*rpid`.
    current()
        .as_thread()
        .pid_ns()
        .visible_pid_checked(pid)
        .unwrap_or(0) as __kernel_pid_t
}

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
    /// Linux `ns->msg_ctlmni`.  Each SysV ceiling is a sysctl of the *IPC
    /// namespace*: `ipc/ipc_sysctl.c:117-124` registers the entry against
    /// `init_ipc_ns.msg_ctlmni` and `setup_ipc_sysctls()` (`:262-279`) rebinds
    /// it to the namespace being created, so a value written inside one
    /// namespace is never observable in another.
    msgmni: AtomicUsize,
    /// Linux `ns->sem_ctls`: `{ semmsl, semmns, semopm, semmni }`, the SysV
    /// semaphore ceilings of this IPC namespace
    /// (`ipc/ipc_sysctl.c:145-160`, `ipc/sem.c:249-256`).
    semmsl: AtomicUsize,
    semmns: AtomicUsize,
    semopm: AtomicUsize,
    semmni: AtomicUsize,
    /// Linux `ns->shm_ctlmax`, `ns->shm_ctlall`, `ns->shm_ctlmni`, seeded by
    /// `shm_init_ns()` (`ipc/shm.c:112-114`) and reachable through
    /// `/proc/sys/kernel/shm{max,all,mni}`.
    shm_ctlmax: AtomicUsize,
    shm_ctlall: AtomicUsize,
    shm_ctlmni: AtomicUsize,
    /// Linux `ns->mq_queues_max`, `ns->mq_msg_max`, `ns->mq_msgsize_max`,
    /// seeded by `mq_init_ns()` (`ipc/mqueue.c:1624-1629`) and reachable
    /// through `/proc/sys/fs/mqueue/*`.
    mq_queues_max: AtomicUsize,
    mq_msg_max: AtomicUsize,
    mq_msgsize_max: AtomicUsize,
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
            msgmni: AtomicUsize::new(msg::MSGMNI),
            semmsl: AtomicUsize::new(sem::SEMMSL),
            semmns: AtomicUsize::new(sem::SEMMNS),
            semopm: AtomicUsize::new(sem::SEMOPM),
            semmni: AtomicUsize::new(sem::SEMMNI),
            shm_ctlmax: AtomicUsize::new(DEFAULT_SHMMAX),
            shm_ctlall: AtomicUsize::new(DEFAULT_SHMALL),
            shm_ctlmni: AtomicUsize::new(DEFAULT_SHMMNI),
            mq_queues_max: AtomicUsize::new(tk_linux_ipc::MQ_QUEUES_MAX_DEFAULT),
            mq_msg_max: AtomicUsize::new(tk_linux_ipc::MQ_MSG_MAX_DEFAULT),
            mq_msgsize_max: AtomicUsize::new(tk_linux_ipc::MQ_MSGSIZE_MAX_DEFAULT),
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

    /// Linux `ns->msg_ctlmni`.
    pub(crate) fn msgmni(&self) -> usize {
        self.msgmni.load(Ordering::Relaxed)
    }

    /// Store a new `msgmni`.
    ///
    /// Linux `ipc/ipc_sysctl.c:120-124` binds the entry to
    /// `proc_dointvec_minmax` with `extra1 = SYSCTL_ZERO` and
    /// `extra2 = &ipc_mni`, so a write outside `[0, ipc_mni]` fails with
    /// EINVAL and leaves the stored value untouched.  `ipc_addid()` clamps
    /// again defensively (`ipc/util.c:287-288`).
    pub(crate) fn set_msgmni(&self, value: usize) -> AxResult<()> {
        if value > tk_linux_ipc::IPCMNI as usize {
            return Err(AxError::from(LinuxError::EINVAL));
        }
        self.msgmni.store(value, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn sem_limits(&self) -> (usize, usize, usize, usize) {
        (
            self.semmsl.load(Ordering::Relaxed),
            self.semmns.load(Ordering::Relaxed),
            self.semopm.load(Ordering::Relaxed),
            self.semmni.load(Ordering::Relaxed),
        )
    }

    /// Store a new `/proc/sys/kernel/sem` tuple.
    ///
    /// Linux `ipc/ipc_sysctl.c:53-70,145-160` writes all four with
    /// `proc_dointvec()` - no lower or upper bound - and then validates only
    /// the identifier ceiling with `sem_check_semmni()`, which returns ERANGE
    /// unless `semmni` is in `[0, ipc_mni]` (`ipc/util.h:248-255`); a failed
    /// check restores the previous `semmni`.
    pub(crate) fn set_sem_limits(
        &self,
        semmsl: usize,
        semmns: usize,
        semopm: usize,
        semmni: usize,
    ) -> AxResult<()> {
        self.semmsl.store(semmsl, Ordering::Relaxed);
        self.semmns.store(semmns, Ordering::Relaxed);
        self.semopm.store(semopm, Ordering::Relaxed);
        let previous = self.semmni.swap(semmni, Ordering::Relaxed);
        if semmni > tk_linux_ipc::IPCMNI as usize {
            self.semmni.store(previous, Ordering::Relaxed);
            return Err(AxError::from(LinuxError::ERANGE));
        }
        Ok(())
    }

    /// Whether the caller may write `/proc/sys/kernel/*_next_id`.
    ///
    /// Linux `ipc/ipc_sysctl.c:ipc_permissions()` exposes those three files,
    /// reachable only under `CONFIG_CHECKPOINT_RESTORE`, with mode 0444 and
    /// upgrades them to writable only for a task that is
    /// `checkpoint_restore_ns_capable()` over the IPC namespace's user
    /// namespace, i.e. `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN` there.
    /// Owning the user namespace by euid is *not* enough: an ordinary root
    /// whose both capabilities were dropped still sees mode 0444 and must not
    /// write `next_id`.  Arming a chosen identifier is exactly the primitive
    /// that could otherwise defeat `ipc_checkid()`, so the capability is what
    /// keeps identifier aliasing a privileged operation.
    pub(crate) fn may_set_next_id(&self) -> bool {
        let curr = axtask::current();
        let thread = curr.as_thread();
        let cred = thread.current_cred();
        ns_capable(
            &cred,
            self.owner_user_ns(),
            linux_raw_sys::general::CAP_CHECKPOINT_RESTORE,
        ) || ns_capable(&cred, self.owner_user_ns(), CAP_SYS_ADMIN)
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

/// Allocates the next SysV identifier for one object table.
///
/// The caller holds the manager lock, so the whole read-decide-register
/// sequence is atomic with respect to every other lookup in this namespace.
/// `requested` is the raw `*_next_id` sysctl value: Linux'
/// `ipc/util.c:ipc_idr_alloc()` consumes it once and then restores `next_id`
/// to `-1`, which is why the namespace atomic is reset before allocating.
pub(crate) fn allocate_ipc_id<F>(
    next_id: &AtomicI32,
    table: &mut IpcIdTable,
    is_occupied: F,
) -> AxResult<IpcId>
where
    F: Fn(i32) -> bool,
{
    let requested = next_id.swap(-1, Ordering::Relaxed);
    table
        .allocate((requested >= 0).then_some(requested), is_occupied)
        .map_err(|_| AxError::from(LinuxError::ENOSPC))
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
/// `include/uapi/linux/shm.h:19-21`:
///
/// ```c
/// #define SHMMAX (ULONG_MAX - (1UL << 24)) /* max shared seg size (bytes) */
/// #define SHMALL (ULONG_MAX - (1UL << 24)) /* max shm system wide (pages) */
/// ```
///
/// `shm_init_ns()` seeds every IPC namespace with them (`ipc/shm.c:112-113`),
/// and the header explains the value: as large as possible without letting
/// userspace overflow a "read the limit, add X, write it back" adjustment.
const DEFAULT_SHMMAX: usize = usize::MAX - (1 << 24);
/// `SHMMNI` (`include/uapi/linux/shm.h`), the value `shm_init_ns()` seeds.
const DEFAULT_SHMMNI: usize = 4096;
const DEFAULT_SHMALL: usize = usize::MAX - (1 << 24);

impl IpcNamespace {
    /// Linux `ns->shm_ctlmax`, checked first by `newseg()` (`ipc/shm.c:717`).
    pub(crate) fn shm_ctlmax(&self) -> usize {
        self.shm_ctlmax.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/kernel/shmmax`.
    ///
    /// Linux binds the entry to `proc_doulongvec_minmax()` with no `extra1` or
    /// `extra2` (`ipc/ipc_sysctl.c:79-84`), so every `unsigned long` is
    /// accepted and the value is stored verbatim.
    pub(crate) fn set_shm_ctlmax(&self, value: usize) {
        self.shm_ctlmax.store(value, Ordering::Relaxed);
    }

    /// Linux `ns->shm_ctlall`, the page ceiling `newseg()` adds the request to
    /// (`ipc/shm.c:722-726`).
    pub(crate) fn shm_ctlall(&self) -> usize {
        self.shm_ctlall.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/kernel/shmall`, unbounded like `shmmax`
    /// (`ipc/ipc_sysctl.c:85-90`).
    pub(crate) fn set_shm_ctlall(&self, value: usize) {
        self.shm_ctlall.store(value, Ordering::Relaxed);
    }

    /// Linux `ns->shm_ctlmni`, the `ipc_addid()` ceiling (`ipc/shm.c:782`).
    pub(crate) fn shm_ctlmni(&self) -> usize {
        self.shm_ctlmni.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/kernel/shmmni`.
    ///
    /// Linux `ipc/ipc_sysctl.c:91-98` uses `proc_dointvec_minmax()` with
    /// `extra1 = SYSCTL_ZERO` and `extra2 = &ipc_mni`, so a value outside
    /// `[0, IPCMNI]` is `EINVAL` and the stored limit is untouched.
    pub(crate) fn set_shm_ctlmni(&self, value: usize) -> AxResult<()> {
        if value > tk_linux_ipc::IPCMNI as usize {
            return Err(AxError::InvalidInput);
        }
        self.shm_ctlmni.store(value, Ordering::Relaxed);
        Ok(())
    }

    /// Linux `ns->mq_queues_max`, the first admission check of
    /// `mqueue_create_attr()`.
    pub(crate) fn mq_queues_max(&self) -> usize {
        self.mq_queues_max.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/fs/mqueue/queues_max`.
    ///
    /// Linux `ipc/mq_sysctl.c:24-29` binds it to a plain `proc_dointvec()`, so
    /// unlike its two siblings it has no range and a write of `0` succeeds - it
    /// only makes every further unprivileged `mq_open()` `ENOSPC`.
    pub(crate) fn set_mq_queues_max(&self, value: usize) {
        self.mq_queues_max.store(value, Ordering::Relaxed);
    }

    /// Linux `ns->mq_msg_max`, the unprivileged per-queue message ceiling.
    pub(crate) fn mq_msg_max(&self) -> usize {
        self.mq_msg_max.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/fs/mqueue/msg_max`.
    ///
    /// Linux `ipc/mq_sysctl.c:30-37` bounds it with `extra1 = &MIN_MSGMAX` and
    /// `extra2 = &HARD_MSGMAX` (`include/linux/ipc_namespace.h:121,123`), so
    /// anything outside `[1, 65536]` is `EINVAL`.
    pub(crate) fn set_mq_msg_max(&self, value: usize) -> AxResult<()> {
        if value < MQ_MSG_MAX_MIN || value > tk_linux_ipc::MQ_MSG_MAX_HARD as usize {
            return Err(AxError::InvalidInput);
        }
        self.mq_msg_max.store(value, Ordering::Relaxed);
        Ok(())
    }

    /// Linux `ns->mq_msgsize_max`, the unprivileged per-message size ceiling.
    pub(crate) fn mq_msgsize_max(&self) -> usize {
        self.mq_msgsize_max.load(Ordering::Relaxed)
    }

    /// Store a new `/proc/sys/fs/mqueue/msgsize_max`.
    ///
    /// Linux `ipc/mq_sysctl.c:38-45` bounds it with
    /// `extra1 = &MIN_MSGSIZEMAX` and `extra2 = &HARD_MSGSIZEMAX`
    /// (`include/linux/ipc_namespace.h:124,126`), so the writable window is
    /// `[128, 16 MiB]` and a smaller value - which would silently clamp every
    /// new queue - is `EINVAL` instead.
    pub(crate) fn set_mq_msgsize_max(&self, value: usize) -> AxResult<()> {
        if value < MQ_MSGSIZE_MAX_MIN
            || value > tk_linux_ipc::MQ_MSGSIZE_MAX_HARD as usize
        {
            return Err(AxError::InvalidInput);
        }
        self.mq_msgsize_max.store(value, Ordering::Relaxed);
        Ok(())
    }
}

/// `MIN_MSGMAX` (`include/linux/ipc_namespace.h:121`): the lowest `msg_max` an
/// administrator may leave the namespace with.
const MQ_MSG_MAX_MIN: usize = 1;
/// `MIN_MSGSIZEMAX` (`include/linux/ipc_namespace.h:124`).
const MQ_MSGSIZE_MAX_MIN: usize = 128;

pub(crate) fn shmmax_limit() -> usize {
    current().as_thread().ipc_ns().shm_ctlmax()
}

pub(crate) fn set_shmmax_limit(value: usize) {
    current().as_thread().ipc_ns().set_shm_ctlmax(value);
}

pub(crate) fn shmmni_limit() -> usize {
    current().as_thread().ipc_ns().shm_ctlmni()
}

pub(crate) fn set_shmmni_limit(value: usize) -> AxResult<()> {
    current().as_thread().ipc_ns().set_shm_ctlmni(value)
}

pub(crate) fn shmall_limit() -> usize {
    current().as_thread().ipc_ns().shm_ctlall()
}

pub(crate) fn set_shmall_limit(value: usize) {
    current().as_thread().ipc_ns().set_shm_ctlall(value);
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
    /// `capable(CAP_SYS_RESOURCE)` - privilege measured against the **initial**
    /// user namespace, which is what Linux's plain `capable()` checks
    /// (`kernel/capability.c:414-417` routes it to `ns_capable(&init_user_ns,
    /// cap)`).  `ipc/msg.c:434-435` uses exactly that form for the `msg_qbytes`
    /// ceiling, so membership of the IPC namespace's own user namespace is not
    /// enough.
    resource_override: bool,
    lock_override: bool,
}

/// Linux `capable(cap)`: a capability check against the initial user
/// namespace.
fn initial_user_namespace_capable(actor: &Cred, capability: u32) -> bool {
    let mut root = actor.user_ns().clone();
    while let Some(parent) = root.parent() {
        root = parent;
    }
    ns_capable(actor, &root, capability)
}

impl IpcAuthority {
    const NONE: Self = Self {
        access_override: false,
        control_override: false,
        resource_override: false,
        lock_override: false,
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
            resource_override: initial_user_namespace_capable(&actor, CAP_SYS_RESOURCE),
            lock_override: ns_capable(&actor, &governing_user_ns, CAP_IPC_LOCK),
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

    /// Linux `ipc/util.c:ipc_update_perm()`:
    ///
    /// ```c
    /// 	kuid_t uid = make_kuid(current_user_ns(), in->uid);
    /// 	kgid_t gid = make_kgid(current_user_ns(), in->gid);
    /// 	if (!uid_valid(uid) || !gid_valid(gid))
    /// 		return -EINVAL;
    ///
    /// 	out->uid = uid;
    /// 	out->gid = gid;
    /// 	out->mode = (out->mode & ~S_IRWXUGO)
    /// 		| (in->mode & S_IRWXUGO);
    /// ```
    ///
    /// The *right* to change an object is settled earlier, by
    /// `ipcctl_obtain_check()`'s owner-or-CAP_SYS_ADMIN test; the update
    /// itself refuses only an owner id that has no mapping in the caller's
    /// user namespace.  Linux therefore lets a plain owner hand the object to
    /// any representable uid or gid, and does not require CAP_CHOWN or confine
    /// the new owner to ids the caller may assume.  `map_permission_update()`
    /// has already performed the `make_kuid()`/`make_kgid()` translation, so
    /// the remaining check here is only the authority to touch the object at
    /// all (repeated because some callers reach this without an explicit
    /// `may_control()`).
    fn prepare_permission_update(
        &self,
        current: &IpcPerm,
        request: IpcPermissionUpdateRequest,
    ) -> AxResult<PreparedIpcPermissionUpdate> {
        if !self.may_control(current) {
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
    use core::sync::atomic::{AtomicI32, Ordering};

    use tk_linux_ipc::{IPC_MIN_CYCLE, IPCMNI, ipcid_compose, ipcid_to_seqx};

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
        assert!(resource.resource_override);
        assert!(!resource.control_override);
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

    /// Linux `ipc_update_perm()` hands the object to whatever uid and gid the
    /// owner asked for, as long as both have a mapping in the caller's user
    /// namespace.  It does not require CAP_CHOWN, it does not confine the new
    /// owner to ids the caller currently holds, and it does not look at the
    /// *live* owner ids at all - only the requested ones are translated.  The
    /// old check here refused an arbitrary owner change with EPERM, which a
    /// plain owner never sees on Linux.
    #[test]
    fn credential_caller_owner_change_needs_only_a_mappable_id() {
        let context = root_context_with_authority(IpcAuthority::NONE);
        let mut object = perm(0, 0, 0o600);
        object.cuid = context.effective_uid_raw();
        let request = IpcPermissionUpdateRequest {
            uid: kuid(2000),
            gid: kgid(3000),
            mode: 0o600,
        };
        let prepared = context.prepare_permission_update(&object, request).unwrap();
        prepared.commit(&mut object);
        assert_eq!((object.uid, object.gid), (2000, 3000));
    }

    #[test]
    fn credential_caller_control_capability_is_the_only_gate_on_ipc_set() {
        let context = root_context_with_authority(IpcAuthority {
            control_override: true,
            ..IpcAuthority::NONE
        });
        let object = perm(2000, 200, 0o600);
        let change_owner = IpcPermissionUpdateRequest {
            uid: kuid(3000),
            gid: kgid(4000),
            mode: 0o644,
        };
        let prepared = context
            .prepare_permission_update(&object, change_owner)
            .unwrap();
        assert_eq!((prepared.uid, prepared.gid), (kuid(3000), kgid(4000)));
    }

    /// A caller that neither owns the object nor holds CAP_SYS_ADMIN is
    /// refused before the requested ids are translated, exactly as
    /// `ipcctl_obtain_check()` refuses it ahead of `ipc_update_perm()`.
    #[test]
    fn credential_caller_without_control_is_refused_before_translation() {
        let context = root_context_with_authority(IpcAuthority::NONE);
        let object = perm(2000, 200, 0o600);
        let request = IpcPermissionUpdateRequest {
            uid: kuid(3000),
            gid: kgid(200),
            mode: 0o600,
        };
        assert!(matches!(
            context.prepare_permission_update(&object, request),
            Err(AxError::OperationNotPermitted)
        ));
    }

    /// An owner id with no mapping is refused by `make_kuid()`/
    /// `make_kgid()`, not by `prepare_permission_update()`; the caller has to
    /// reach `map_permission_update()` to see the EINVAL.
    #[test]
    fn credential_caller_unmappable_owner_ids_are_einval() {
        let context = root_context_with_authority(IpcAuthority::NONE);
        assert!(matches!(
            context.map_permission_update(0, u32::MAX, 0o600),
            Err(AxError::InvalidInput)
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

        let root_over_child = IpcAccessContext::new(root_cred, child_ns.clone());
        assert!(root_over_child.authority.access_override);
        assert!(root_over_child.authority.control_override);
        assert!(root_over_child.authority.resource_override);
        assert!(Arc::ptr_eq(root_over_child.governing_user_ns(), &child_ns));
    }

    #[test]
    fn ipc_id_allocator_hands_out_sequential_identifiers() {
        let next_id = AtomicI32::new(-1);
        let mut table = IpcIdTable::new();
        let mut live: BTreeSet<i32> = BTreeSet::new();
        for expected in 0..8 {
            let id = allocate_ipc_id(&next_id, &mut table, |candidate| live.contains(&candidate))
                .unwrap();
            assert_eq!(id.index(), expected);
            assert_eq!(id.sequence(), 0);
            // The published identifier is the index while the sequence is 0.
            assert_eq!(id.raw(), expected);
            assert!(live.insert(id.index()));
        }
        assert_eq!(table.in_use(), 8);
        assert_eq!(table.max_index(), 7);
    }

    /// Regression: the production wrappers must not hand back a retired
    /// identifier.  The previous allocator reset its cursor with
    /// `swap(-1)` on every call, so removing the lowest identifier and
    /// creating again reissued it immediately and a stale handle resolved to
    /// the successor object.
    #[test]
    fn ipc_id_allocator_does_not_reissue_a_retired_identifier() {
        let next_id = AtomicI32::new(-1);
        let mut table = IpcIdTable::new();
        let mut live: BTreeSet<i32> = BTreeSet::new();
        let first =
            allocate_ipc_id(&next_id, &mut table, |candidate| live.contains(&candidate)).unwrap();
        let second =
            allocate_ipc_id(&next_id, &mut table, |candidate| live.contains(&candidate)).unwrap();
        assert_eq!((first.index(), second.index()), (0, 1));
        live.remove(&first.index());
        table.release(first.index(), |candidate| live.contains(&candidate));
        let third =
            allocate_ipc_id(&next_id, &mut table, |candidate| live.contains(&candidate)).unwrap();
        assert_eq!(third.index(), 2, "index 0 must stay retired");
        assert_ne!(third.raw(), first.raw());
        assert_eq!(ipcid_to_seqx(first.raw()), 0);
        assert_eq!(ipcid_to_seqx(third.raw()), third.sequence());
    }

    #[test]
    fn ipc_id_allocator_advances_the_sequence_when_the_window_wraps() {
        let next_id = AtomicI32::new(-1);
        let mut table = IpcIdTable::new();
        // Churn the whole `ipc_min_cycle` window so the cursor reaches its end
        // while the table never grows, exactly like a create/remove loop.
        for expected in 0..IPC_MIN_CYCLE {
            let id = allocate_ipc_id(&next_id, &mut table, |_| false).unwrap();
            assert_eq!(id.index(), expected);
            assert_eq!(id.sequence(), 0);
            table.release(id.index(), |_| false);
        }
        let wrapped = allocate_ipc_id(&next_id, &mut table, |_| false).unwrap();
        assert_eq!(wrapped.index(), 0);
        assert_eq!(wrapped.sequence(), 1);
        assert_eq!(wrapped.raw(), IPCMNI);
    }

    #[test]
    fn ipc_id_allocator_reports_exhaustion_instead_of_reusing_a_live_id() {
        let next_id = AtomicI32::new(-1);
        let mut table = IpcIdTable::new();
        assert_eq!(
            allocate_ipc_id(&next_id, &mut table, |_| true),
            Err(AxError::from(LinuxError::ENOSPC))
        );
        assert_eq!(table.in_use(), 0);
    }

    #[test]
    fn ipc_id_allocator_consumes_next_id_once_and_keeps_its_sequence() {
        let next_id = AtomicI32::new(ipcid_compose(5, 2));
        let mut table = IpcIdTable::new();
        let requested = allocate_ipc_id(&next_id, &mut table, |_| false).unwrap();
        assert_eq!(requested.raw(), ipcid_compose(5, 2));
        // Linux `ipc_idr_alloc()` restores `next_id` to -1 after consuming it,
        // so the following create is an ordinary cyclic allocation.
        assert_eq!(next_id.load(Ordering::Relaxed), -1);
        let following = allocate_ipc_id(&next_id, &mut table, |_| false).unwrap();
        assert_eq!(following.index(), 0);
        assert_eq!(following.sequence(), 0);
    }
}
