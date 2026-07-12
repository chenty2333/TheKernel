use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axhal::time::monotonic_time_nanos;
use axnet::NetStack;
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use hashbrown::HashMap;
use scope_local::Scope;
use spin::{Once, RwLock};
use starry_process::{Pid, ProcessError};
use starry_signal::{
    SignalInfo, SignalQueueAccount, Signo,
    api::{ProcessSignalManager, SignalActions},
};

// Host unit tests do not initialize the kernel scheduler/current task. Keep
// the production registry sleepable, but let ownership/admission tests execute
// the same critical sections without entering `axsync::Mutex`'s task wait path.
#[cfg(not(test))]
type SignalAccountRegistryMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type SignalAccountRegistryMutex<T> = spin::Mutex<T>;

#[cfg(not(test))]
type UserNamespaceStateMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type UserNamespaceStateMutex<T> = spin::Mutex<T>;

use super::{
    accounting::{AtomicTaskUsage, live_process_usage},
    creds::{Cred, CredentialSlot, PreparedCred},
    futex::FutexTable,
    idmap::{IdMap, IdMapInputExtent, Kgid, Kuid, UserGid, UserUid},
    jobctl::{
        ContinueResult, ExecControlState, JobControlState, PtraceControlState, StopKind,
        StopReport, StopState, VforkControlState,
    },
    resources::Rlimits,
    signal::PtraceSignalRecord,
    timer::PosixTimer,
};
use crate::{
    file::{
        FdTable,
        executable::{self, ExecutableKey},
    },
    mm::AddrSpace,
    time::wall_time,
};

/// Linux process identity bound to the immutable group-leader credential
/// retained in the durable zombie payload.
pub(crate) type Process = starry_process::Process<Arc<Cred>>;
/// Linux process-group identity in the kernel-owned process domain.
pub(crate) type ProcessGroup = starry_process::ProcessGroup<Arc<Cred>>;
/// Linux session identity in the kernel-owned process domain.
pub(crate) type Session = starry_process::Session<Arc<Cred>>;
/// Durable process-exit payload used by wait, procfs, and permission paths.
pub(crate) type ZombieSnapshot = starry_process::ZombieSnapshot<Arc<Cred>>;
/// Fallibly reserved storage consumed by the final process exit.
pub(crate) type PreparedZombieSnapshot = starry_process::PreparedZombieSnapshot<Arc<Cred>>;
/// Prepared payload bound to a validated final-exit transaction.
pub(crate) type PreparedZombieExit = starry_process::PreparedZombieExit<Arc<Cred>>;
/// Fully validated final process-exit transaction.
pub(crate) type ProcessExitAdmission = starry_process::ProcessExitAdmission<Arc<Cred>>;
/// Completed final-exit transaction with its linearized parent and reaper.
pub(crate) type CommittedProcessExit = starry_process::CommittedProcessExit<Arc<Cred>>;
/// Domain-coordinated thread removal and optional final-exit reservation.
pub(crate) type ThreadExitTransition = starry_process::ThreadExitTransition<Arc<Cred>>;
/// Type-bound unpublished process plus initial-thread publication transaction.
pub(crate) type InitialProcessAdmission = starry_process::InitialProcessAdmission<Arc<Cred>>;
/// The kernel's sole process lifecycle and topology owner.
pub(crate) type ProcessDomain = starry_process::ProcessDomain<Arc<Cred>>;
type StarryThreadAdmission = starry_process::ThreadAdmission<Arc<Cred>>;

static PROCESS_DOMAIN: Once<ProcessDomain> = Once::new();

/// Initializes the sole kernel-owned process domain before publishing init.
pub(crate) fn init_process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.try_call_once(|| ProcessDomain::try_new().map_err(process_error))
}

/// Returns the process domain after boot initialization.
pub(crate) fn process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.get().ok_or(AxError::BadState)
}

pub(crate) const UTS_FIELD_LEN: usize = 64;
const PROC_NS_INO_BASE: u64 = 0x9_0000_0000;
static PROC_NS_ID: AtomicU64 = AtomicU64::new(1);

/// Implementation ceiling for queued RT nodes charged to one (user_ns, ruid).
/// RLIMIT_SIGPENDING may lower this value but cannot raise it.
pub(crate) const SIGNAL_QUEUE_PER_USER_HARD_LIMIT: usize = 4_096;
/// Implementation ceiling for all queued RT nodes in one root user-namespace
/// hierarchy. Descendant NEWUSER namespaces share this account.
pub(crate) const SIGNAL_QUEUE_GLOBAL_HARD_LIMIT: usize = 16_384;
/// Hard ceiling for simultaneously retained user namespaces. Namespace fds
/// can outlive their creator, so RLIMIT_NPROC alone is not a lifetime bound.
pub(crate) const USER_NAMESPACE_HARD_LIMIT: usize = 4_096;
static LIVE_USER_NAMESPACES: AtomicUsize = AtomicUsize::new(0);

fn try_increment_bounded(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .is_ok()
}

struct UserNamespaceAdmission;

impl UserNamespaceAdmission {
    fn try_new() -> AxResult<Self> {
        if try_increment_bounded(&LIVE_USER_NAMESPACES, USER_NAMESPACE_HARD_LIMIT) {
            Ok(Self)
        } else {
            Err(axerrno::LinuxError::ENOSPC.into())
        }
    }
}

impl Drop for UserNamespaceAdmission {
    fn drop(&mut self) {
        LIVE_USER_NAMESPACES.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct CgroupNamespace {
    id: u64,
    owner_user_ns: Arc<UserNamespace>,
}

impl CgroupNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(owner_user_ns)
    }

    fn try_new(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(
        self: &Arc<Self>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(owner_user_ns)
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }
}

#[derive(Clone)]
pub(crate) struct PidNamespace {
    id: u64,
    parent: Option<Arc<PidNamespace>>,
    init_pid: Option<Pid>,
    owner_user_ns: Arc<UserNamespace>,
}

impl PidNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(None, None, owner_user_ns)
    }

    fn try_new(
        parent: Option<Arc<Self>>,
        init_pid: Option<Pid>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            parent,
            init_pid,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(
        self: &Arc<Self>,
        init_pid: Pid,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(Some(self.clone()), Some(init_pid), owner_user_ns)
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) fn visible_pid(&self, global_pid: Pid) -> Pid {
        if self.init_pid == Some(global_pid) {
            1
        } else {
            global_pid
        }
    }

    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

#[derive(Clone, Copy)]
struct UserNamespaceMapState {
    uid_written: bool,
    gid_written: bool,
    setgroups_allowed: bool,
}

pub(crate) struct UserNamespace {
    _admission: UserNamespaceAdmission,
    id: u64,
    level: u32,
    parent: Option<Arc<UserNamespace>>,
    owner: Kuid,
    // Linux records the creator's group even though current namespace
    // capability and NS_GET_OWNER_UID rules consume only the owner UID.
    _group: Kgid,
    parent_could_setfcap: bool,
    uid_map: SpinNoIrq<Arc<IdMap>>,
    gid_map: SpinNoIrq<Arc<IdMap>>,
    map_state: UserNamespaceStateMutex<UserNamespaceMapState>,
    signal_accounts: SignalAccountRegistryMutex<HashMap<Kuid, Weak<SignalQueueAccount>>>,
    global_signal_account: Arc<SignalQueueAccount>,
}

impl UserNamespace {
    const OVERFLOW_ID: u32 = 65_534;

    pub(crate) fn try_new_root() -> AxResult<Arc<Self>> {
        let identity = IdMap::try_identity()?;
        let global_signal_account = SignalQueueAccount::try_new(SIGNAL_QUEUE_GLOBAL_HARD_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let admission = UserNamespaceAdmission::try_new()?;
        Arc::try_new(Self {
            _admission: admission,
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            level: 0,
            parent: None,
            owner: Kuid::INITIAL_ROOT,
            _group: Kgid::INITIAL_ROOT,
            parent_could_setfcap: true,
            uid_map: SpinNoIrq::new(identity.clone()),
            gid_map: SpinNoIrq::new(identity),
            map_state: UserNamespaceStateMutex::new(UserNamespaceMapState {
                uid_written: true,
                gid_written: true,
                setgroups_allowed: true,
            }),
            signal_accounts: SignalAccountRegistryMutex::new(HashMap::new()),
            global_signal_account,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(
        self: &Arc<Self>,
        owner: Kuid,
        group: Kgid,
        parent_could_setfcap: bool,
    ) -> AxResult<Arc<Self>> {
        // Linux rejects creation once the parent nesting level is already
        // above 32. Keep the same finite ancestry walk bound.
        if self.level > 32 {
            return Err(axerrno::LinuxError::ENOSPC.into());
        }
        if self.kernel_uid_to_user(owner).is_none() || self.kernel_gid_to_user(group).is_none() {
            return Err(AxError::OperationNotPermitted);
        }
        let uid_map = IdMap::try_empty()?;
        let gid_map = IdMap::try_empty()?;
        let setgroups_allowed = self.map_state.lock().setgroups_allowed;
        let admission = UserNamespaceAdmission::try_new()?;
        Arc::try_new(Self {
            _admission: admission,
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            level: self.level + 1,
            parent: Some(self.clone()),
            owner,
            _group: group,
            parent_could_setfcap,
            uid_map: SpinNoIrq::new(uid_map),
            gid_map: SpinNoIrq::new(gid_map),
            map_state: UserNamespaceStateMutex::new(UserNamespaceMapState {
                uid_written: false,
                gid_written: false,
                setgroups_allowed,
            }),
            signal_accounts: SignalAccountRegistryMutex::new(HashMap::new()),
            global_signal_account: self.global_signal_account.clone(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    pub(crate) fn level(&self) -> u32 {
        self.level
    }

    pub(crate) fn is_initial(&self) -> bool {
        self.parent.is_none()
    }

    pub(crate) fn owner_kuid(&self) -> Kuid {
        self.owner
    }

    pub(crate) fn parent_could_setfcap(&self) -> bool {
        self.parent_could_setfcap
    }

    pub(crate) fn uid_map(&self) -> Arc<IdMap> {
        self.uid_map.lock().clone()
    }

    pub(crate) fn gid_map(&self) -> Arc<IdMap> {
        self.gid_map.lock().clone()
    }

    fn map_display_namespace<'a>(self: &'a Arc<Self>, viewer: &'a Arc<Self>) -> &'a Arc<Self> {
        // Linux uses seq_user_ns() for map reads, except that a task reading
        // its own namespace map sees lower IDs in the immediate parent.
        if Arc::ptr_eq(self, viewer) {
            self.parent.as_ref().unwrap_or(viewer)
        } else {
            viewer
        }
    }

    pub(crate) fn try_uid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.uid_map();
        let lower_map = self.map_display_namespace(viewer).uid_map();
        map.try_extents_for_lower(&lower_map)
    }

    pub(crate) fn try_gid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.gid_map();
        let lower_map = self.map_display_namespace(viewer).gid_map();
        map.try_extents_for_lower(&lower_map)
    }

    pub(crate) fn try_build_uid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent.as_ref().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent(input, &parent_map)
    }

    pub(crate) fn try_build_uid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent.as_ref().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent_slice(input, &parent_map)
    }

    pub(crate) fn try_build_gid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent.as_ref().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent(input, &parent_map)
    }

    pub(crate) fn try_build_gid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent.as_ref().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent_slice(input, &parent_map)
    }

    /// Publishes a fully built UID map exactly once. Construction and parent
    /// resolution happen before the namespace state mutex, and retired map
    /// ownership is dropped after both locks have been released.
    pub(crate) fn publish_uid_map(&self, map: Arc<IdMap>) -> AxResult<()> {
        if map.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.map_state.lock();
        if state.uid_written {
            return Err(AxError::OperationNotPermitted);
        }
        let retired = core::mem::replace(&mut *self.uid_map.lock(), map);
        state.uid_written = true;
        drop(state);
        drop(retired);
        Ok(())
    }

    /// Publishes a fully built GID map exactly once. Unprivileged callers pass
    /// `require_setgroups_denied`; that check is made in the same state
    /// transaction as publication, closing the deny/write race.
    pub(crate) fn publish_gid_map(
        &self,
        map: Arc<IdMap>,
        require_setgroups_denied: bool,
    ) -> AxResult<()> {
        if map.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.map_state.lock();
        if state.gid_written || (require_setgroups_denied && state.setgroups_allowed) {
            return Err(AxError::OperationNotPermitted);
        }
        let retired = core::mem::replace(&mut *self.gid_map.lock(), map);
        state.gid_written = true;
        drop(state);
        drop(retired);
        Ok(())
    }

    pub(crate) fn setgroups_allowed(&self) -> bool {
        self.map_state.lock().setgroups_allowed
    }

    pub(crate) fn uid_map_written(&self) -> bool {
        self.map_state.lock().uid_written
    }

    pub(crate) fn gid_map_written(&self) -> bool {
        self.map_state.lock().gid_written
    }

    pub(crate) fn may_setgroups(&self) -> bool {
        let state = self.map_state.lock();
        state.gid_written && state.setgroups_allowed
    }

    pub(crate) fn update_setgroups_policy(&self, allow: bool) -> AxResult<()> {
        let mut state = self.map_state.lock();
        if allow {
            if !state.setgroups_allowed {
                return Err(AxError::OperationNotPermitted);
            }
        } else {
            if state.gid_written {
                return Err(AxError::OperationNotPermitted);
            }
            state.setgroups_allowed = false;
        }
        Ok(())
    }

    pub(crate) fn user_uid_to_kernel(&self, uid: UserUid) -> Option<Kuid> {
        self.uid_map().user_uid_to_kernel(uid)
    }

    pub(crate) fn kernel_uid_to_user(&self, uid: Kuid) -> Option<UserUid> {
        self.uid_map().kernel_uid_to_user(uid)
    }

    pub(crate) fn user_gid_to_kernel(&self, gid: UserGid) -> Option<Kgid> {
        self.gid_map().user_gid_to_kernel(gid)
    }

    pub(crate) fn kernel_gid_to_user(&self, gid: Kgid) -> Option<UserGid> {
        self.gid_map().kernel_gid_to_user(gid)
    }

    pub(crate) fn make_kuid(&self, uid: u32) -> Option<Kuid> {
        UserUid::from_raw(uid).and_then(|uid| self.user_uid_to_kernel(uid))
    }

    pub(crate) fn root_kuid(&self) -> Option<Kuid> {
        self.user_uid_to_kernel(UserUid::ROOT)
    }

    pub(crate) fn make_kgid(&self, gid: u32) -> Option<Kgid> {
        UserGid::from_raw(gid).and_then(|gid| self.user_gid_to_kernel(gid))
    }

    pub(crate) fn root_kgid(&self) -> Option<Kgid> {
        self.user_gid_to_kernel(UserGid::ROOT)
    }

    pub(crate) fn from_kuid_munged(&self, uid: Kuid) -> u32 {
        self.kernel_uid_to_user(uid)
            .map(UserUid::into_raw)
            .unwrap_or(Self::OVERFLOW_ID)
    }

    pub(crate) fn from_kgid_munged(&self, gid: Kgid) -> u32 {
        self.kernel_gid_to_user(gid)
            .map(UserGid::into_raw)
            .unwrap_or(Self::OVERFLOW_ID)
    }

    /// Returns the RT signal queue accounts for a real UID in this namespace.
    ///
    /// Registry allocation is fallible and happens under a sleepable mutex,
    /// never under a signal pending SpinNoIrq guard. A losing candidate is
    /// dropped only after the registry guard has been released.
    pub(crate) fn try_signal_queue_accounts(
        &self,
        real_uid: Kuid,
    ) -> AxResult<(Arc<SignalQueueAccount>, Arc<SignalQueueAccount>)> {
        let existing = {
            let accounts = self.signal_accounts.lock();
            accounts.get(&real_uid).and_then(Weak::upgrade)
        };
        if let Some(existing) = existing {
            return Ok((existing, self.global_signal_account.clone()));
        }

        let candidate = SignalQueueAccount::try_new(SIGNAL_QUEUE_PER_USER_HARD_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let winner = {
            let mut accounts = self.signal_accounts.lock();
            if let Some(existing) = accounts.get(&real_uid).and_then(Weak::upgrade) {
                Some(existing)
            } else {
                accounts.retain(|_, account| account.strong_count() != 0);
                accounts.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                accounts.insert(real_uid, Arc::downgrade(&candidate));
                None
            }
        };

        if let Some(winner) = winner {
            drop(candidate);
            Ok((winner, self.global_signal_account.clone()))
        } else {
            Ok((candidate, self.global_signal_account.clone()))
        }
    }

    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

#[derive(Clone, Copy)]
struct UtsState {
    nodename: [u8; UTS_FIELD_LEN],
    nodename_len: usize,
    domainname: [u8; UTS_FIELD_LEN],
    domainname_len: usize,
}

const fn copy_uts_field(dst: &mut [u8; UTS_FIELD_LEN], src: &[u8]) -> usize {
    let len = if src.len() < UTS_FIELD_LEN {
        src.len()
    } else {
        UTS_FIELD_LEN
    };
    let mut index = 0;
    while index < len {
        dst[index] = src[index];
        index += 1;
    }
    len
}

const fn init_uts_state() -> UtsState {
    let mut state = UtsState {
        nodename: [0; UTS_FIELD_LEN],
        nodename_len: 0,
        domainname: [0; UTS_FIELD_LEN],
        domainname_len: 0,
    };
    state.nodename_len = copy_uts_field(&mut state.nodename, b"thekernel");
    state.domainname_len = copy_uts_field(&mut state.domainname, b"(none)");
    state
}

impl UtsState {
    fn set_nodename(&mut self, value: &[u8]) {
        self.nodename = [0; UTS_FIELD_LEN];
        self.nodename_len = copy_uts_field(&mut self.nodename, value);
    }

    fn set_domainname(&mut self, value: &[u8]) {
        self.domainname = [0; UTS_FIELD_LEN];
        self.domainname_len = copy_uts_field(&mut self.domainname, value);
    }
}

pub(crate) struct UtsNamespace {
    state: SpinNoIrq<UtsState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl UtsNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            state: SpinNoIrq::new(init_uts_state()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) fn nodename(&self) -> Vec<u8> {
        let state = *self.state.lock();
        state.nodename[..state.nodename_len].to_vec()
    }

    pub(crate) fn domainname(&self) -> Vec<u8> {
        let state = *self.state.lock();
        state.domainname[..state.domainname_len].to_vec()
    }

    pub(crate) fn set_nodename(&self, value: &[u8]) {
        self.state.lock().set_nodename(value);
    }

    pub(crate) fn set_domainname(&self, value: &[u8]) {
        self.state.lock().set_domainname(value);
    }
}

#[derive(Clone, Copy, Default)]
struct TimeNamespaceState {
    monotonic_offset_ns: i64,
    boottime_offset_ns: i64,
}

pub(crate) struct TimeNamespace {
    state: SpinNoIrq<TimeNamespaceState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl TimeNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            state: SpinNoIrq::new(TimeNamespaceState::default()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    fn offset_ns(&self, boottime: bool) -> i64 {
        let state = self.state.lock();
        if boottime {
            state.boottime_offset_ns
        } else {
            state.monotonic_offset_ns
        }
    }

    pub(crate) fn apply_monotonic_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false))
    }

    pub(crate) fn apply_boottime_offset(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true))
    }

    pub(crate) fn host_monotonic_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(false).saturating_neg())
    }

    pub(crate) fn host_boottime_deadline(&self, value: Duration) -> Duration {
        apply_time_offset(value, self.offset_ns(true).saturating_neg())
    }

    pub(crate) fn set_monotonic_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().monotonic_offset_ns = offset_to_nanos(secs, nsecs);
    }

    pub(crate) fn set_boottime_offset(&self, secs: i64, nsecs: u32) {
        self.state.lock().boottime_offset_ns = offset_to_nanos(secs, nsecs);
    }

    pub(crate) fn render_offsets(&self) -> Vec<u8> {
        let state = *self.state.lock();
        let (mono_sec, mono_nsec) = nanos_to_offset(state.monotonic_offset_ns);
        let (boot_sec, boot_nsec) = nanos_to_offset(state.boottime_offset_ns);
        format!("monotonic  {mono_sec:10} {mono_nsec:9}\nboottime   {boot_sec:10} {boot_nsec:9}\n")
            .into_bytes()
    }
}

/// Linux-visible network namespace identity over one generic network stack.
///
/// `NetStack` stays in the generic mechanism layer. The owning user namespace
/// belongs to this Linux-ABI object so authority remains bound to the object
/// even after the creating process exits or a socket crosses processes.
pub(crate) struct NetworkNamespace {
    stack: Arc<NetStack>,
    owner_user_ns: Arc<UserNamespace>,
}

impl NetworkNamespace {
    pub(crate) fn try_new(
        stack: Arc<NetStack>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            stack,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_new_loopback_only(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(NetStack::try_new_loopback_only()?, owner_user_ns)
    }

    pub(crate) fn stack(&self) -> &Arc<NetStack> {
        &self.stack
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }
}

fn offset_to_nanos(secs: i64, nsecs: u32) -> i64 {
    secs.saturating_mul(1_000_000_000)
        .saturating_add(nsecs as i64)
}

fn nanos_to_offset(nanos: i64) -> (i64, u32) {
    let secs = nanos.div_euclid(1_000_000_000);
    let nsecs = nanos.rem_euclid(1_000_000_000) as u32;
    (secs, nsecs)
}

fn apply_time_offset(value: Duration, offset_ns: i64) -> Duration {
    let adjusted = value.as_nanos() as i128 + offset_ns as i128;
    Duration::from_nanos(adjusted.clamp(0, u64::MAX as i128) as u64)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mempolicy {
    pub mode: u32,
    pub nodemask: usize,
}

impl Mempolicy {
    pub const fn new(mode: u32, nodemask: usize) -> Self {
        Self { mode, nodemask }
    }
}

#[derive(Clone, Copy, Debug)]
struct MempolicyRange {
    start: usize,
    end: usize,
    policy: Mempolicy,
}

#[derive(Clone, Debug)]
struct MempolicyState {
    process_policy: Mempolicy,
    ranges: Vec<MempolicyRange>,
}

impl Default for MempolicyState {
    fn default() -> Self {
        Self {
            process_policy: Mempolicy::new(0, 0),
            ranges: Vec::new(),
        }
    }
}

impl MempolicyState {
    fn first_node(mask: usize) -> Option<usize> {
        (mask != 0).then(|| mask.trailing_zeros() as usize)
    }

    fn node_mask(node: usize) -> usize {
        1usize.checked_shl(node as u32).unwrap_or(0)
    }

    fn node_ordinal(mask: usize, node: usize) -> Option<usize> {
        let mut ordinal = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if candidate == node {
                return Some(ordinal);
            }
            ordinal += 1;
        }
        None
    }

    fn nth_node(mask: usize, ordinal: usize) -> Option<usize> {
        let mut seen = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if seen == ordinal {
                return Some(candidate);
            }
            seen += 1;
        }
        None
    }

    fn migration_destination(
        old_mask: usize,
        new_mask: usize,
        source_node: usize,
    ) -> Option<usize> {
        let source_mask = Self::node_mask(source_node);
        if old_mask & source_mask == 0 || new_mask == 0 {
            return None;
        }

        if old_mask == new_mask {
            return None;
        }

        if old_mask.count_ones() == new_mask.count_ones() {
            return Self::node_ordinal(old_mask, source_node)
                .and_then(|ordinal| Self::nth_node(new_mask, ordinal));
        }

        if new_mask & source_mask != 0 {
            return None;
        }

        Self::first_node(new_mask)
    }

    fn migrate_policy(policy: &mut Mempolicy, old_mask: usize, new_mask: usize) -> bool {
        let source_node = Self::first_node(policy.nodemask).unwrap_or(0);
        let Some(dest_node) = Self::migration_destination(old_mask, new_mask, source_node) else {
            return false;
        };
        if dest_node == source_node {
            return false;
        }
        policy.nodemask = Self::node_mask(dest_node);
        true
    }

    fn migrate_ranges(&mut self, old_mask: usize, new_mask: usize) -> usize {
        let mut migrated = 0;
        for range in &mut self.ranges {
            migrated += usize::from(Self::migrate_policy(&mut range.policy, old_mask, new_mask));
        }
        migrated
    }

    fn remove_range(&mut self, start: usize, end: usize) {
        let old_ranges = core::mem::take(&mut self.ranges);
        for range in old_ranges {
            if range.end <= start || range.start >= end {
                self.ranges.push(range);
                continue;
            }
            if range.start < start {
                self.ranges.push(MempolicyRange {
                    start: range.start,
                    end: start,
                    policy: range.policy,
                });
            }
            if range.end > end {
                self.ranges.push(MempolicyRange {
                    start: end,
                    end: range.end,
                    policy: range.policy,
                });
            }
        }
    }

    fn policy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map(|range| range.policy)
    }
}

/// Persistent binding to the task-local publication slot that currently owns
/// Linux thread-group-leader identity.
struct GroupLeaderCredentialBinding {
    current: SpinNoIrq<Arc<CredentialSlot>>,
}

impl GroupLeaderCredentialBinding {
    fn new(initial: Arc<CredentialSlot>) -> Self {
        Self {
            current: SpinNoIrq::new(initial),
        }
    }

    fn current_cred(&self) -> Arc<Cred> {
        let slot = self.current.lock().clone();
        slot.current()
    }

    fn publish_handoff<'a>(
        &self,
        credential: Arc<CredentialSlot>,
        prepared: Option<PreparedCred<'a>>,
    ) -> GroupLeaderRetirement<'a> {
        let mut current = self.current.lock();
        let publication = prepared.map(PreparedCred::publish);
        let retired = core::mem::replace(&mut *current, credential);
        drop(current);
        GroupLeaderRetirement {
            _publication: publication,
            _slot: retired,
        }
    }
}

/// [`Process`]-shared data.
pub struct ProcessData {
    /// The process.
    pub proc: Arc<Process>,
    /// Serializes child admission through publication against final exit and
    /// reparenting for this process.
    process_lifecycle: Mutex<()>,
    /// The only allocation needed to publish this process's durable zombie
    /// payload. It is reserved before the process becomes visible.
    prepared_zombie_snapshot: SpinNoIrq<Option<PreparedZombieSnapshot>>,
    /// Stable identity of the Linux thread-group leader credential owner.
    ///
    /// This is a strong reference to the leader task's sole publication slot,
    /// not a copied credential or process-level shadow state. It deliberately
    /// outlives an exited leader task while sibling threads keep the process
    /// alive, matching Linux's persistent thread-group identity.
    group_leader_credential: GroupLeaderCredentialBinding,
    /// The executable path
    pub exe_path: RwLock<String>,
    /// The inode currently held busy as this process image.
    pub(crate) executable: SpinNoIrq<Option<ExecutableKey>>,
    /// The command line arguments
    pub cmdline: RwLock<Arc<Vec<String>>>,
    /// Realtime process creation timestamp, in seconds.
    start_realtime_sec: u64,
    /// Monotonic process creation timestamp, in nanoseconds.
    start_monotonic_ns: u64,
    /// The virtual memory address space.
    // TODO: scopify
    aspace_handle: RwLock<Arc<Mutex<AddrSpace>>>,
    /// The resource scope
    pub scope: RwLock<Scope>,
    /// Real empty files table prepared at process creation for final exit swap.
    exit_fd_table: Arc<FdTable>,
    /// The user heap top
    heap_top: AtomicUsize,

    /// The resource limits
    pub rlim: RwLock<Rlimits>,

    /// The child exit wait event
    pub child_exit_event: Arc<PollSet>,
    /// Self exit event
    pub exit_event: Arc<PollSet>,
    /// Woken when exec de-thread state changes or a sibling exits.
    pub exec_event: Arc<PollSet>,
    /// The exit signal of the thread
    pub exit_signal: Option<Signo>,

    /// The process signal manager
    pub signal: Arc<ProcessSignalManager>,

    /// The futex table.
    pub(in crate::task) futex_table: Arc<FutexTable>,

    /// The default mask for file permissions.
    umask: AtomicU32,
    /// Linux personality flags shared by all threads in the process.
    personality: AtomicU32,
    /// NUMA memory policy state for the single-node kernel memory model.
    mempolicy: SpinNoIrq<MempolicyState>,
    /// Parent-death signal configured through prctl(PR_SET_PDEATHSIG).
    pdeath_signal: AtomicU32,
    /// Current timer slack in nanoseconds.
    timerslack_current_ns: AtomicUsize,
    /// Default timer slack in nanoseconds, used when PR_SET_TIMERSLACK is 0.
    timerslack_default_ns: AtomicUsize,
    /// POSIX interval timers created by this process.
    pub(crate) posix_timers: SpinNoIrq<Vec<Option<PosixTimer>>>,

    /// CPU time accumulated from sibling threads that have already exited.
    pub(in crate::task) exited_threads_usage: AtomicTaskUsage,
    /// CPU time accumulated from waited-for child subtrees.
    waited_children_usage: AtomicTaskUsage,
    /// Maximum resident set size observed for this process, in kilobytes.
    maxrss_kb: AtomicU64,

    /// Serializes wait* selection and consumption for this process.
    pub wait_lock: Mutex<()>,

    /// Job-control stop state shared by all threads in the process.
    job_ctl: SpinNoIrq<JobControlState>,
    /// ptrace ownership and options shared by all threads in the process.
    ptrace_ctl: SpinNoIrq<PtraceControlState>,
    /// Exact queued signal retained while stopped at a ptrace delivery boundary.
    ptrace_signal: Mutex<Option<PtraceSignalRecord>>,
    /// Processes currently traced by this process.
    ptrace_tracees: SpinNoIrq<Vec<Pid>>,
    /// Multi-thread exec coordination state.
    exec_ctl: SpinNoIrq<ExecControlState>,
    /// CLONE_VFORK coordination state.
    vfork_ctl: SpinNoIrq<VforkControlState>,
    /// Woken when threads should resume from stopped state.
    pub stop_event: Arc<PollSet>,
    /// Woken when a vfork child releases the parent.
    pub vfork_event: Arc<PollSet>,

    /// The network namespace (network stack) for this process.
    pub(crate) net_ns: Arc<NetworkNamespace>,
    /// Lightweight cgroup namespace identity for cgroup.procs open-time checks.
    cgroup_ns: Arc<CgroupNamespace>,
    /// Lightweight PID namespace identity for procfs namespace fd ABI.
    pid_ns: Arc<PidNamespace>,
    /// The UTS namespace for this process.
    uts_ns: RwLock<Arc<UtsNamespace>>,
    /// The time namespace visible to this process.
    time_ns: RwLock<Arc<TimeNamespace>>,
    /// The time namespace inherited by children created after unshare/setns.
    time_ns_for_children: RwLock<Arc<TimeNamespace>>,
}

/// Deferred destruction produced by an exec group-leader handoff.
///
/// The value must be dropped only after every registry/binding lock involved
/// in the composite publication has been released.
pub(crate) struct GroupLeaderRetirement<'a> {
    _publication: Option<super::creds::CredentialPublication<'a>>,
    _slot: Arc<CredentialSlot>,
}

pub(crate) fn process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive | ProcessError::NotInitialized => {
            AxError::NoSuchProcess
        }
        ProcessError::Busy => AxError::ResourceBusy,
        ProcessError::WrongDomain => AxError::BadState,
        _ => AxError::BadState,
    }
}

/// Exec exclusion charge held across fallible clone construction.
struct PendingThreadAddition {
    proc_data: Arc<ProcessData>,
    armed: bool,
}

impl PendingThreadAddition {
    fn finish_locked(&mut self, exec_ctl: &mut ExecControlState) -> bool {
        debug_assert!(exec_ctl.pending_thread_additions != 0);
        exec_ctl.pending_thread_additions -= 1;
        self.armed = false;
        exec_ctl.pending_thread_additions == 0
    }

    fn finish(mut self) {
        let proc_data = self.proc_data.clone();
        let mut exec_ctl = proc_data.exec_ctl.lock();
        let wake = self.finish_locked(&mut exec_ctl);
        drop(exec_ctl);
        if wake {
            proc_data.exec_event.wake();
        }
    }
}

impl Drop for PendingThreadAddition {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let proc_data = self.proc_data.clone();
        let mut exec_ctl = proc_data.exec_ctl.lock();
        let wake = self.finish_locked(&mut exec_ctl);
        drop(exec_ctl);
        if wake {
            proc_data.exec_event.wake();
        }
    }
}

/// Completion guard for a thread whose core identity is published but whose
/// signal/task-table/runqueue transaction is still externally incomplete.
#[must_use = "thread publication remains pending until this guard is finished"]
pub(crate) struct PendingThreadPublication {
    pending: PendingThreadAddition,
    group_exited_at_core: bool,
}

const fn group_exit_handoff_requires_kill(core_observed: bool, late_gate_observed: bool) -> bool {
    core_observed || late_gate_observed
}

impl PendingThreadPublication {
    /// Samples the permanent group-exit gate after TASK_TABLE publication.
    ///
    /// If core publication itself observed group exit, or the gate linearized
    /// between that point and this late sample, the exact prepared task must
    /// receive SIGKILL before entering the runqueue.
    pub(crate) fn must_terminate_for_group_exit(&self) -> bool {
        // `group_exit_in_progress` acquires exec_ctl again after TASK_TABLE
        // publication. This is deliberately not a cached core-commit result:
        // it covers group_exit linearizing after core publication but before
        // TASK_TABLE, when the first TID scan cannot resolve the task yet.
        group_exit_handoff_requires_kill(
            self.group_exited_at_core,
            self.pending.proc_data.group_exit_in_progress(),
        )
    }

    /// Makes the completed task/runqueue publication visible to exec/group-exit.
    pub(crate) fn finish(self) {
        self.pending.finish();
    }
}

/// Live-process thread membership held across fallible clone construction.
pub(crate) struct ProcessThreadAdmission {
    // Drop the core reservation before making exec observe no pending clone.
    membership: StarryThreadAdmission,
    pending: PendingThreadAddition,
}

impl ProcessThreadAdmission {
    /// Publishes the reserved TID while keeping exec exclusion atomic with the
    /// thread-group mutation.
    pub(crate) fn commit(self) -> PendingThreadPublication {
        let Self {
            membership,
            pending,
        } = self;
        let outcome = membership.commit_infallible();
        PendingThreadPublication {
            pending,
            group_exited_at_core: outcome == starry_process::ThreadPublicationOutcome::GroupExited,
        }
    }
}

/// Unpublished process plus initial thread held across runtime construction.
pub(crate) struct InitialProcessThreadAdmission {
    // Roll back the core composite before making exec observe no pending clone.
    publication: InitialProcessAdmission,
    pending: PendingThreadAddition,
}

impl InitialProcessThreadAdmission {
    /// Publishes the type-bound process/initial-thread pair before making exec
    /// observe that clone construction has completed.
    pub(crate) fn commit(self) -> (Arc<Process>, PendingThreadPublication) {
        let Self {
            publication,
            pending,
        } = self;
        let process = publication.commit();
        (
            process,
            PendingThreadPublication {
                pending,
                group_exited_at_core: false,
            },
        )
    }
}

impl ProcessData {
    /// Fallibly creates unpublished process runtime state.
    pub(crate) fn try_new(
        proc: Arc<Process>,
        prepared_zombie_snapshot: PreparedZombieSnapshot,
        group_leader_credential: Arc<CredentialSlot>,
        exe_path: String,
        executable: Option<ExecutableKey>,
        cmdline: Arc<Vec<String>>,
        aspace: Arc<Mutex<AddrSpace>>,
        scope: Scope,
        exit_fd_table: Arc<FdTable>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        exit_signal: Option<Signo>,
        net_ns: Arc<NetworkNamespace>,
        cgroup_ns: Arc<CgroupNamespace>,
        pid_ns: Arc<PidNamespace>,
        uts_ns: Arc<UtsNamespace>,
        time_ns: Arc<TimeNamespace>,
    ) -> AxResult<Arc<Self>> {
        struct ExecutableRollback(Option<ExecutableKey>);

        impl Drop for ExecutableRollback {
            fn drop(&mut self) {
                executable::release(self.0.take());
            }
        }

        let start_realtime_sec = wall_time().as_secs();
        let start_monotonic_ns = monotonic_time_nanos();
        let mut executable_rollback = ExecutableRollback(executable);
        let child_exit_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let exit_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let exec_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let signal = Arc::try_new(ProcessSignalManager::new(
            signal_actions,
            crate::config::SIGNAL_TRAMPOLINE,
        ))
        .map_err(|_| AxError::NoMemory)?;
        let futex_table = Arc::try_new(FutexTable::new()).map_err(|_| AxError::NoMemory)?;
        let stop_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let vfork_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let data = Self {
            proc,
            process_lifecycle: Mutex::new(()),
            prepared_zombie_snapshot: SpinNoIrq::new(Some(prepared_zombie_snapshot)),
            group_leader_credential: GroupLeaderCredentialBinding::new(group_leader_credential),
            exe_path: RwLock::new(exe_path),
            executable: SpinNoIrq::new(executable),
            cmdline: RwLock::new(cmdline),
            start_realtime_sec,
            start_monotonic_ns,
            aspace_handle: RwLock::new(aspace),
            scope: RwLock::new(scope),
            exit_fd_table,
            heap_top: AtomicUsize::new(
                crate::config::USER_HEAP_BASE + crate::config::USER_HEAP_SIZE,
            ),

            rlim: RwLock::default(),

            child_exit_event,
            exit_event,
            exec_event,
            exit_signal,

            signal,

            futex_table,

            umask: AtomicU32::new(0o022),
            personality: AtomicU32::new(0),
            mempolicy: SpinNoIrq::new(MempolicyState::default()),
            pdeath_signal: AtomicU32::new(0),
            timerslack_current_ns: AtomicUsize::new(50_000),
            timerslack_default_ns: AtomicUsize::new(50_000),
            posix_timers: SpinNoIrq::new(Vec::new()),
            exited_threads_usage: AtomicTaskUsage::new(),
            waited_children_usage: AtomicTaskUsage::new(),
            maxrss_kb: AtomicU64::new(0),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            ptrace_ctl: SpinNoIrq::new(PtraceControlState::default()),
            ptrace_signal: Mutex::new(None),
            ptrace_tracees: SpinNoIrq::new(Vec::new()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            vfork_ctl: SpinNoIrq::new(VforkControlState::default()),
            stop_event,
            vfork_event,

            net_ns,
            cgroup_ns,
            pid_ns,
            uts_ns: RwLock::new(uts_ns),
            time_ns: RwLock::new(time_ns.clone()),
            time_ns_for_children: RwLock::new(time_ns),
        };
        executable_rollback.0 = None;
        Arc::try_new(data).map_err(|_| AxError::NoMemory)
    }

    /// Reserves the fixed-cost zombie payload allocation before process
    /// publication. Final exit only fills this storage and cannot allocate.
    pub(crate) fn try_prepare_zombie_snapshot() -> AxResult<PreparedZombieSnapshot> {
        PreparedZombieSnapshot::try_new().map_err(|_| AxError::NoMemory)
    }

    /// Serializes fork admission through commit against final exit.
    pub(crate) fn lock_process_lifecycle(&self) -> axsync::MutexGuard<'_, ()> {
        self.process_lifecycle.lock()
    }

    /// Binds this process's sole payload reservation only after the process
    /// domain has validated and exclusively reserved final exit.
    pub(crate) fn prepare_zombie_exit(
        &self,
        exit: ProcessExitAdmission,
    ) -> AxResult<PreparedZombieExit> {
        let prepared = self
            .prepared_zombie_snapshot
            .lock()
            .take()
            .ok_or(AxError::BadState)?;
        Ok(prepared.bind_exit(exit))
    }

    /// Takes one immutable snapshot from the currently bound Linux
    /// thread-group leader slot. This remains available after a premature
    /// leader exit and changes only during a successful non-leader exec.
    pub(crate) fn group_leader_cred(&self) -> Arc<Cred> {
        self.group_leader_credential.current_cred()
    }

    /// Publishes an optional exec credential and switches the group-leader
    /// slot as one process-visible transition. Retired `Arc`s are destroyed
    /// only after the binding lock is released.
    pub(in crate::task) fn publish_group_leader_handoff<'a>(
        &self,
        owner: Pid,
        thread: &super::Thread,
        prepared: Option<PreparedCred<'a>>,
    ) -> GroupLeaderRetirement<'a> {
        debug_assert!(self.is_exec_owner(owner));
        debug_assert_eq!(thread.proc_data.proc.pid(), self.proc.pid());
        let credential = thread.credential_slot();
        self.group_leader_credential
            .publish_handoff(credential, prepared)
    }

    /// Clones the preallocated real empty files table for final process exit.
    pub(crate) fn exit_fd_table(&self) -> Arc<FdTable> {
        self.exit_fd_table.clone()
    }

    /// Get the top address of the user heap.
    pub fn get_heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Fallibly snapshots the executable path without allocator work under
    /// the process metadata lock.
    pub(crate) fn try_exe_path(&self) -> AxResult<String> {
        let mut path = String::new();
        loop {
            path.clear();
            let required = self.exe_path.read().len();
            if path.capacity() < required {
                path.try_reserve_exact(required)
                    .map_err(|_| AxError::NoMemory)?;
            }
            let current = self.exe_path.read();
            if path.capacity() < current.len() {
                drop(current);
                continue;
            }
            path.push_str(&current);
            return Ok(path);
        }
    }

    /// Returns the current address-space handle for this process.
    pub fn aspace(&self) -> Arc<Mutex<AddrSpace>> {
        self.aspace_handle.read().clone()
    }

    /// Rebinds the process to a new address-space handle and returns the old one.
    pub fn replace_aspace(&self, aspace: Arc<Mutex<AddrSpace>>) -> Arc<Mutex<AddrSpace>> {
        core::mem::replace(&mut *self.aspace_handle.write(), aspace)
    }

    pub(crate) fn uts_ns(&self) -> Arc<UtsNamespace> {
        self.uts_ns.read().clone()
    }

    pub(crate) fn cgroup_ns(&self) -> Arc<CgroupNamespace> {
        self.cgroup_ns.clone()
    }

    pub(crate) fn cgroup_ns_id(&self) -> u64 {
        self.cgroup_ns.id()
    }

    pub(crate) fn pid_ns(&self) -> Arc<PidNamespace> {
        self.pid_ns.clone()
    }

    pub(crate) fn replace_uts_ns(&self, uts_ns: Arc<UtsNamespace>) {
        let old = core::mem::replace(&mut *self.uts_ns.write(), uts_ns);
        drop(old);
    }

    pub(crate) fn time_ns(&self) -> Arc<TimeNamespace> {
        self.time_ns.read().clone()
    }

    pub(crate) fn time_ns_for_children(&self) -> Arc<TimeNamespace> {
        self.time_ns_for_children.read().clone()
    }

    pub(crate) fn replace_time_ns(&self, time_ns: Arc<TimeNamespace>) {
        let old_current = core::mem::replace(&mut *self.time_ns.write(), time_ns.clone());
        let old_children = core::mem::replace(&mut *self.time_ns_for_children.write(), time_ns);
        drop((old_current, old_children));
    }

    pub(crate) fn try_unshared_time_ns(
        &self,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<TimeNamespace>> {
        self.time_ns_for_children().try_fork(owner_user_ns)
    }

    pub(crate) fn replace_time_ns_for_children(&self, new_ns: Arc<TimeNamespace>) {
        let old = core::mem::replace(&mut *self.time_ns_for_children.write(), new_ns);
        drop(old);
    }

    pub(crate) fn start_realtime_sec(&self) -> u64 {
        self.start_realtime_sec
    }

    pub(crate) fn start_monotonic_ns(&self) -> u64 {
        self.start_monotonic_ns
    }

    pub(crate) fn executable(&self) -> Option<ExecutableKey> {
        *self.executable.lock()
    }

    pub(crate) fn retain_executable(&self) -> AxResult<Option<ExecutableKey>> {
        executable::retain(self.executable())
    }

    pub(crate) fn replace_executable(&self, new_executable: Option<ExecutableKey>) {
        let old_executable = core::mem::replace(&mut *self.executable.lock(), new_executable);
        executable::release(old_executable);
    }

    pub(crate) fn release_executable(&self) {
        self.replace_executable(None);
    }

    /// Set the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }

    pub fn pdeath_signal(&self) -> u32 {
        self.pdeath_signal.load(Ordering::Acquire)
    }

    pub fn set_pdeath_signal(&self, signo: u32) {
        self.pdeath_signal.store(signo, Ordering::Release)
    }

    pub fn timerslack_ns(&self) -> usize {
        self.timerslack_current_ns.load(Ordering::Acquire)
    }

    pub fn set_timerslack_ns(&self, value: usize) {
        let value = if value == 0 {
            self.timerslack_default_ns.load(Ordering::Acquire)
        } else {
            value
        };
        self.timerslack_current_ns.store(value, Ordering::Release)
    }

    pub fn inherit_timerslack_from(&self, parent: &Self) {
        let value = parent.timerslack_ns();
        self.timerslack_current_ns.store(value, Ordering::Release);
        self.timerslack_default_ns.store(value, Ordering::Release);
    }
}

impl ProcessData {
    /// Linux manual: A "clone" child is one which delivers no signal, or a
    /// signal other than SIGCHLD to its parent upon termination.
    pub fn is_clone_child(&self) -> bool {
        self.exit_signal != Some(Signo::SIGCHLD)
    }

    /// Returns process CPU usage, including live threads and exited siblings.
    pub fn self_usage(&self) -> super::accounting::TaskUsage {
        live_process_usage(self).with_maxrss_floor(self.sample_maxrss_kb())
    }

    /// Returns waited-for child CPU usage accumulated for this process.
    pub fn children_usage(&self) -> super::accounting::TaskUsage {
        self.waited_children_usage.snapshot()
    }

    /// Returns the total usage that should be published when this process exits.
    pub fn total_usage(&self) -> super::accounting::TaskUsage {
        self.self_usage().saturating_add(self.children_usage())
    }

    /// Records the final CPU usage of a thread that is exiting.
    pub fn account_exited_thread(&self, usage: super::accounting::TaskUsage) {
        self.exited_threads_usage.add(usage);
    }

    /// Records a waited-for child subtree into the process's child ledger.
    pub fn account_waited_child(&self, usage: super::accounting::TaskUsage) {
        self.waited_children_usage.add(usage);
    }

    fn sample_maxrss_kb(&self) -> u64 {
        let resident_kb = self.aspace().lock().resident_user_bytes() as u64 / 1024;
        let mut current = self.maxrss_kb.load(Ordering::Acquire);
        while resident_kb > current {
            match self.maxrss_kb.compare_exchange_weak(
                current,
                resident_kb,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return resident_kb,
                Err(observed) => current = observed,
            }
        }
        current
    }

    /// Get the umask.
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::SeqCst)
    }

    /// Set the umask.
    pub fn set_umask(&self, umask: u32) {
        self.umask.store(umask, Ordering::SeqCst);
    }

    /// Set the umask and return the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.umask.swap(umask, Ordering::SeqCst)
    }
}

impl ProcessData {
    pub fn personality(&self) -> u32 {
        self.personality.load(Ordering::Acquire)
    }

    pub fn set_personality(&self, personality: u32) {
        self.personality.store(personality, Ordering::Release);
    }

    pub fn mempolicy(&self) -> Mempolicy {
        self.mempolicy.lock().process_policy
    }

    pub fn set_mempolicy(&self, policy: Mempolicy) {
        self.mempolicy.lock().process_policy = policy;
    }

    pub fn try_inherit_mempolicy_from(&self, parent: &Self) -> AxResult<()> {
        let mut ranges = Vec::new();
        let process_policy = loop {
            ranges.clear();
            let required = parent.mempolicy.lock().ranges.len();
            if ranges.capacity() < required {
                ranges
                    .try_reserve_exact(required)
                    .map_err(|_| AxError::NoMemory)?;
            }
            let state = parent.mempolicy.lock();
            if ranges.capacity() < state.ranges.len() {
                drop(state);
                continue;
            }
            ranges.extend(state.ranges.iter().copied());
            break state.process_policy;
        };
        let parent_state = MempolicyState {
            process_policy,
            ranges,
        };
        let old = core::mem::replace(&mut *self.mempolicy.lock(), parent_state);
        drop(old);
        Ok(())
    }

    pub fn bind_mempolicy_range(&self, start: usize, size: usize, policy: Mempolicy) {
        if size == 0 {
            return;
        }
        let Some(end) = start.checked_add(size) else {
            return;
        };
        let mut state = self.mempolicy.lock();
        state.remove_range(start, end);
        state.ranges.push(MempolicyRange { start, end, policy });
    }

    pub fn clear_mempolicy_range(&self, start: usize, size: usize) {
        if size == 0 {
            return;
        }
        let Some(end) = start.checked_add(size) else {
            return;
        };
        self.mempolicy.lock().remove_range(start, end);
    }

    pub fn clear_mempolicy_ranges(&self) {
        self.mempolicy.lock().ranges.clear();
    }

    pub fn migrate_mempolicy_ranges(&self, old_mask: usize, new_mask: usize) -> usize {
        self.mempolicy.lock().migrate_ranges(old_mask, new_mask)
    }

    pub fn mempolicy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.mempolicy.lock().policy_for_addr(addr)
    }
}

impl ProcessData {
    pub fn ptrace_tracer(&self) -> Option<Pid> {
        self.ptrace_ctl.lock().tracer
    }

    pub fn ptrace_options(&self) -> u32 {
        self.ptrace_ctl.lock().options
    }

    pub fn ptrace_event_message(&self) -> usize {
        self.ptrace_ctl.lock().event_message
    }

    pub fn ptrace_set_options(&self, options: u32) {
        self.ptrace_ctl.lock().options = options;
    }

    pub fn ptrace_set_event_message(&self, event_message: usize) {
        self.ptrace_ctl.lock().event_message = event_message;
    }

    pub fn is_traced_by(&self, tracer: Pid) -> bool {
        self.ptrace_tracer() == Some(tracer)
    }

    pub fn begin_ptrace(&self, tracer: Pid) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.tracer.is_some() {
            return false;
        }
        ptrace_ctl.tracer = Some(tracer);
        ptrace_ctl.options = 0;
        ptrace_ctl.event_message = 0;
        true
    }

    /// Stops at a signal-delivery boundary while transferring exact queue
    /// ownership into ptrace state. On failure the caller gets the untouched
    /// record back and may publish it normally.
    pub(crate) fn try_ptrace_signal_stop(
        &self,
        record: PtraceSignalRecord,
    ) -> Result<(), PtraceSignalRecord> {
        let mut pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let mut job_ctl = self.job_ctl.lock();
        if ptrace_ctl.tracer.is_none() || job_ctl.state != StopState::Running || pending.is_some() {
            return Err(record);
        }

        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = record.info().signo() as u8;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        *pending = Some(record);
        Ok(())
    }

    pub(crate) fn ptrace_signal_info(&self) -> Option<SignalInfo> {
        self.ptrace_signal
            .lock()
            .as_ref()
            .map(|record| record.info().clone())
    }

    pub(crate) fn replace_ptrace_signal_info(&self, info: SignalInfo) -> bool {
        let mut pending = self.ptrace_signal.lock();
        pending
            .as_mut()
            .is_some_and(|record| record.replace_info(info).is_some())
    }

    /// Resumes a ptrace stop and atomically takes its retained signal record.
    /// If `detach` is true, tracer ownership is cleared under the same gate so
    /// no new delivery stop can appear between resume and detach.
    pub(crate) fn resume_ptrace(
        &self,
        tracer: Pid,
        detach: bool,
    ) -> Option<(ContinueResult, Option<PtraceSignalRecord>)> {
        let mut pending = self.ptrace_signal.lock();
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.tracer != Some(tracer) {
            return None;
        }
        if detach {
            *ptrace_ctl = PtraceControlState::default();
        }

        let mut job_ctl = self.job_ctl.lock();
        let result = match job_ctl.state {
            StopState::Running => ContinueResult::None,
            StopState::Stopping => {
                job_ctl.state = StopState::Running;
                ContinueResult::CanceledStopping
            }
            StopState::Stopped => {
                job_ctl.state = StopState::Running;
                if job_ctl.stop_kind == StopKind::JobControl {
                    job_ctl.continued = true;
                }
                ContinueResult::ResumedStopped
            }
        };
        let record = pending.take();
        drop(job_ctl);
        drop(ptrace_ctl);
        drop(pending);
        Some((result, record))
    }

    /// Publishes the wake only after the caller has resolved the retained
    /// ptrace signal record. This prevents a tracee from returning to user mode
    /// before a requested reinjection has become pending.
    pub(crate) fn finish_ptrace_resume(&self, result: ContinueResult) {
        if result != ContinueResult::None {
            self.stop_event.wake();
        }
    }

    pub fn end_ptrace(&self, tracer: Pid) -> bool {
        let Some((result, record)) = self.resume_ptrace(tracer, true) else {
            return false;
        };
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        self.finish_ptrace_resume(result);
        true
    }

    pub fn clear_ptrace(&self) -> Option<Pid> {
        let (tracer, record) = {
            let mut pending = self.ptrace_signal.lock();
            let mut ptrace_ctl = self.ptrace_ctl.lock();
            let tracer = ptrace_ctl.tracer;
            *ptrace_ctl = PtraceControlState::default();
            (tracer, pending.take())
        };
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        tracer
    }

    pub fn try_ptrace_tracees(&self) -> AxResult<Vec<Pid>> {
        let mut snapshot = Vec::new();
        loop {
            let required = self.ptrace_tracees.lock().len();
            if snapshot.capacity() < required {
                snapshot
                    .try_reserve_exact(required)
                    .map_err(|_| AxError::NoMemory)?;
            }
            let tracees = self.ptrace_tracees.lock();
            if snapshot.capacity() < tracees.len() {
                drop(tracees);
                continue;
            }
            snapshot.clear();
            for &pid in tracees.iter() {
                snapshot.push(pid);
            }
            return Ok(snapshot);
        }
    }

    pub fn add_ptrace_tracee(&self, tracee: Pid) {
        let mut tracees = self.ptrace_tracees.lock();
        if !tracees.contains(&tracee) {
            tracees.push(tracee);
        }
    }

    pub fn remove_ptrace_tracee(&self, tracee: Pid) {
        self.ptrace_tracees.lock().retain(|pid| *pid != tracee);
    }

    pub fn clear_ptrace_tracees(&self) -> Vec<Pid> {
        let mut tracees = self.ptrace_tracees.lock();
        let old = tracees.clone();
        tracees.clear();
        old
    }

    fn stop_state(&self) -> StopState {
        self.job_ctl.lock().state
    }

    /// Returns whether the process is currently stopped.
    pub fn is_stopped(&self) -> bool {
        self.stop_state() == StopState::Stopped
    }

    /// Returns whether threads should park for a job-control stop.
    pub fn should_wait_for_stop(&self) -> bool {
        self.stop_state() != StopState::Running
    }

    /// Begins a job-control stop transition.
    pub fn begin_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopping;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::JobControl;
        true
    }

    /// Finalizes a stop transition if it has not been canceled by SIGCONT.
    pub fn finish_stop(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Stopping {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Stops a traced process at a signal-delivery or attach boundary.
    pub fn ptrace_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Resumes or cancels a job-control stop transition.
    pub(crate) fn continue_job(&self) -> ContinueResult {
        let traced = self.ptrace_tracer().is_some();
        let result = {
            let mut job_ctl = self.job_ctl.lock();
            match job_ctl.state {
                StopState::Running => ContinueResult::None,
                StopState::Stopping => {
                    job_ctl.state = StopState::Running;
                    ContinueResult::CanceledStopping
                }
                StopState::Stopped => {
                    if job_ctl.stop_kind == StopKind::Ptrace && traced {
                        return ContinueResult::None;
                    }
                    job_ctl.state = StopState::Running;
                    if job_ctl.stop_kind == StopKind::JobControl {
                        job_ctl.continued = true;
                    }
                    ContinueResult::ResumedStopped
                }
            }
        };
        if result != ContinueResult::None {
            self.stop_event.wake();
        }
        result
    }

    /// Atomically takes the continued flag (returns true at most once per continuation).
    pub fn take_continued(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        let continued = job_ctl.continued;
        job_ctl.continued = false;
        continued
    }

    /// Takes the current stopped status for waitpid reporting, if it has not been reported yet.
    pub(crate) fn take_stop_status(&self) -> Option<StopReport> {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            job_ctl.stop_reported = true;
            Some(StopReport {
                signal: job_ctl.stop_signal,
                traced: job_ctl.stop_kind == StopKind::Ptrace,
            })
        } else {
            None
        }
    }

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    pub(crate) fn peek_stop_status(&self) -> Option<StopReport> {
        let job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped && !job_ctl.stop_reported {
            Some(StopReport {
                signal: job_ctl.stop_signal,
                traced: job_ctl.stop_kind == StopKind::Ptrace,
            })
        } else {
            None
        }
    }

    /// Claims the pending stop report so a waiter can complete userspace copies first.
    pub(crate) fn claim_stop_status(&self) -> Option<StopReport> {
        self.take_stop_status()
    }

    /// Restores a previously claimed stop report after a failed userspace copy.
    pub(crate) fn restore_stop_status(&self, report: StopReport) {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state == StopState::Stopped
            && job_ctl.stop_signal == report.signal
            && (job_ctl.stop_kind == StopKind::Ptrace) == report.traced
        {
            job_ctl.stop_reported = false;
        }
    }

    /// Peeks at the continued flag without consuming it (for WNOWAIT).
    pub fn peek_continued(&self) -> bool {
        self.job_ctl.lock().continued
    }

    /// Claims the pending continued report so a waiter can complete userspace copies first.
    pub fn claim_continued(&self) -> bool {
        self.take_continued()
    }

    /// Restores a previously claimed continued report after a failed userspace copy.
    pub fn restore_continued(&self) {
        let mut job_ctl = self.job_ctl.lock();
        job_ctl.continued = true;
    }

    /// Begins a multi-thread exec de-threading phase.
    pub fn begin_exec(&self, owner: Pid) -> bool {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.group_exit {
            return false;
        }
        match exec_ctl.owner {
            Some(curr) => curr == owner,
            None if exec_ctl.pending_thread_additions == 0 => {
                exec_ctl.owner = Some(owner);
                true
            }
            None => false,
        }
    }

    /// Excludes thread creation while a process-scope pointer is replaced.
    ///
    /// Thread publication takes `exec_ctl` before the process thread-group
    /// lock. Taking the locks in the same order makes the single-thread test
    /// and gate publication atomic with respect to CLONE_THREAD: either clone
    /// publishes first and this returns `false`, or the gate publishes first
    /// and clone rolls back. Callers must pair success with `end_exec(owner)`.
    pub fn begin_single_thread_scope_change(&self, owner: Pid) -> bool {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.group_exit
            || exec_ctl.owner.is_some()
            || exec_ctl.pending_thread_additions != 0
            || !self.proc.has_only_thread(owner)
        {
            return false;
        }
        exec_ctl.owner = Some(owner);
        true
    }

    /// Returns whether this thread should exit because another thread is committing execve().
    pub fn should_exit_for_exec(&self, tid: Pid) -> bool {
        matches!(self.exec_ctl.lock().owner, Some(owner) if owner != tid)
    }

    /// Returns whether the given thread still owns the in-flight exec.
    pub fn is_exec_owner(&self, tid: Pid) -> bool {
        self.exec_ctl.lock().owner == Some(tid)
    }

    /// Returns whether an exec de-thread phase is currently in progress.
    pub fn exec_in_progress(&self) -> bool {
        self.exec_ctl.lock().owner.is_some()
    }

    /// Closes thread admission for a group-wide exit and cancels any exec gate.
    ///
    /// The first caller atomically establishes both the kernel admission gate
    /// and core group-exit state, then returns `true` so it can scan currently
    /// published TIDs. A pre-gate clone that publishes after that scan observes
    /// this permanent gate before runqueue insertion and self-arms SIGKILL.
    pub(crate) fn begin_group_exit(&self, exit_code: i32) -> bool {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.group_exit {
            return false;
        }
        exec_ctl.group_exit = true;
        let cancelled_exec = exec_ctl.owner.take().is_some();
        let established = self.proc.group_exit(exit_code);
        debug_assert!(established, "kernel group-exit gate lost core ownership");
        drop(exec_ctl);
        if cancelled_exec {
            self.exec_event.wake();
        }
        true
    }

    /// Returns whether the permanent group-exit gate has linearized.
    pub(crate) fn group_exit_in_progress(&self) -> bool {
        self.exec_ctl.lock().group_exit
    }

    /// Reserves process membership for a thread unless exec has gated creation.
    pub(crate) fn prepare_thread(self: &Arc<Self>, tid: Pid) -> AxResult<ProcessThreadAdmission> {
        // The intrusive membership node is allocated before entering the
        // exec-control SpinNoIrq domain. It remains invisible until commit.
        let membership = process_domain()?
            .prepare_thread(&self.proc, tid)
            .map_err(process_error)?;
        self.prepare_thread_membership(membership)
    }

    /// Binds an unpublished fork's initial thread reservation to this runtime
    /// object while preserving the exec/thread-addition exclusion contract.
    pub(crate) fn prepare_initial_thread(
        self: &Arc<Self>,
        publication: InitialProcessAdmission,
    ) -> AxResult<InitialProcessThreadAdmission> {
        let pending = self.prepare_thread_addition()?;
        Ok(InitialProcessThreadAdmission {
            publication,
            pending,
        })
    }

    fn prepare_thread_membership(
        self: &Arc<Self>,
        membership: StarryThreadAdmission,
    ) -> AxResult<ProcessThreadAdmission> {
        let pending = self.prepare_thread_addition()?;
        Ok(ProcessThreadAdmission {
            membership,
            pending,
        })
    }

    fn prepare_thread_addition(self: &Arc<Self>) -> AxResult<PendingThreadAddition> {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner.is_some() || exec_ctl.group_exit {
            drop(exec_ctl);
            return Err(AxError::Interrupted);
        }
        let Some(pending) = exec_ctl.pending_thread_additions.checked_add(1) else {
            drop(exec_ctl);
            return Err(AxError::NoMemory);
        };
        exec_ctl.pending_thread_additions = pending;
        drop(exec_ctl);
        Ok(PendingThreadAddition {
            proc_data: self.clone(),
            armed: true,
        })
    }

    /// Returns whether the thread group has drained to the exec owner only.
    pub fn exec_ready(&self, owner: Pid) -> bool {
        self.is_exec_owner(owner) && self.proc.has_only_thread(owner)
    }

    /// Finishes or cancels the in-flight exec owned by `owner`.
    pub fn end_exec(&self, owner: Pid) {
        let mut exec_ctl = self.exec_ctl.lock();
        if exec_ctl.owner == Some(owner) {
            exec_ctl.owner = None;
            drop(exec_ctl);
            self.exec_event.wake();
        }
    }

    /// Marks the process as a vfork child whose parent thread must remain blocked.
    pub fn begin_vfork(&self, parent_tid: Pid) {
        self.vfork_ctl.lock().parent_tid = Some(parent_tid);
    }

    /// Returns whether an active CLONE_VFORK relationship is still blocking the parent.
    pub fn vfork_in_progress(&self) -> bool {
        self.vfork_ctl.lock().parent_tid.is_some()
    }

    /// Releases a blocked vfork parent after execve commits or the last thread exits.
    pub fn release_vfork(&self) {
        let mut vfork_ctl = self.vfork_ctl.lock();
        if vfork_ctl.parent_tid.take().is_some() {
            drop(vfork_ctl);
            self.vfork_event.wake();
        }
    }
}

impl Drop for ProcessData {
    fn drop(&mut self) {
        let executable = *self.executable.lock();
        executable::release(executable);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::{sync::Barrier, thread, vec::Vec};

    use axerrno::AxError;

    use super::{
        CgroupNamespace, GroupLeaderCredentialBinding, NetworkNamespace, PidNamespace,
        SIGNAL_QUEUE_GLOBAL_HARD_LIMIT, SIGNAL_QUEUE_PER_USER_HARD_LIMIT, TimeNamespace,
        UserNamespace, UtsNamespace, group_exit_handoff_requires_kill, init_uts_state,
        try_increment_bounded,
    };
    use crate::task::{
        Cred, CredentialSlot,
        idmap::{IdMapInputExtent, Kgid, Kuid},
    };

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn credential_slot(uid: u32) -> Arc<CredentialSlot> {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace).unwrap()).unwrap();
        if uid != 0 {
            let uid = kuid(uid);
            let mut update = slot.prepare();
            update.builder.ids.ruid = uid;
            update.builder.ids.euid = uid;
            update.builder.ids.suid = uid;
            update.builder.ids.fsuid = uid;
            update.finish().unwrap().commit();
        }
        slot
    }

    #[test]
    fn default_uts_identity_is_product_neutral() {
        let state = init_uts_state();
        assert_eq!(&state.nodename[..state.nodename_len], b"thekernel");
        assert_eq!(&state.domainname[..state.domainname_len], b"(none)");
    }

    #[test]
    fn user_namespace_admission_has_a_reusable_hard_ceiling() {
        let counter = AtomicUsize::new(0);
        assert!(try_increment_bounded(&counter, 2));
        assert!(try_increment_bounded(&counter, 2));
        assert!(!try_increment_bounded(&counter, 2));
        assert_eq!(counter.fetch_sub(1, Ordering::Release), 2);
        assert!(try_increment_bounded(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[test]
    fn late_group_exit_gate_covers_core_to_task_table_window() {
        assert!(!group_exit_handoff_requires_kill(false, false));
        assert!(group_exit_handoff_requires_kill(true, false));
        assert!(group_exit_handoff_requires_kill(false, true));
        assert!(group_exit_handoff_requires_kill(true, true));
    }

    #[test]
    fn group_leader_binding_keeps_the_single_slot_alive() {
        let slot = credential_slot(1000);
        let weak = Arc::downgrade(&slot);
        let binding = GroupLeaderCredentialBinding::new(slot.clone());
        drop(slot);

        assert_eq!(binding.current_cred().ids().ruid, kuid(1000));
        assert!(weak.upgrade().is_some());
        drop(binding);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn group_leader_handoff_never_exposes_the_unprepared_slot() {
        const READS: usize = 20_000;

        let old = credential_slot(1000);
        let new = credential_slot(2000);
        let binding = Arc::new(GroupLeaderCredentialBinding::new(old));
        let start = Arc::new(Barrier::new(2));
        let reader = {
            let binding = binding.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for _ in 0..READS {
                    let uid = binding.current_cred().ids().ruid;
                    assert!(
                        uid == kuid(1000) || uid == kuid(3000),
                        "mixed handoff uid {uid:?}"
                    );
                }
            })
        };

        let mut update = new.prepare();
        update.builder.ids.ruid = kuid(3000);
        update.builder.ids.euid = kuid(3000);
        update.builder.ids.suid = kuid(3000);
        update.builder.ids.fsuid = kuid(3000);
        let prepared = update.finish().unwrap();
        start.wait();
        let retirement = binding.publish_handoff(new.clone(), Some(prepared));
        assert_eq!(binding.current_cred().ids().ruid, kuid(3000));
        drop(retirement);
        reader.join().unwrap();
    }

    #[test]
    fn signal_accounts_are_keyed_by_user_namespace_and_real_uid() {
        let first_ns = UserNamespace::try_new_root().unwrap();
        let second_ns = UserNamespace::try_new_root().unwrap();

        let (first, first_global) = first_ns.try_signal_queue_accounts(kuid(1000)).unwrap();
        let (same, same_global) = first_ns.try_signal_queue_accounts(kuid(1000)).unwrap();
        let (other_uid, _) = first_ns.try_signal_queue_accounts(kuid(1001)).unwrap();
        let (other_ns, other_global) = second_ns.try_signal_queue_accounts(kuid(1000)).unwrap();

        assert!(Arc::ptr_eq(&first, &same));
        assert!(Arc::ptr_eq(&first_global, &same_global));
        assert!(!Arc::ptr_eq(&first, &other_uid));
        assert!(!Arc::ptr_eq(&first, &other_ns));
        assert!(!Arc::ptr_eq(&first_global, &other_global));
    }

    #[test]
    fn descendant_user_namespaces_share_the_root_global_account_only() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
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
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                    .unwrap(),
                false,
            )
            .unwrap();
        let grandchild = child.try_fork(kuid(1000), kgid(1000), false).unwrap();

        let (root_user, root_global) = root.try_signal_queue_accounts(kuid(1000)).unwrap();
        let (child_user, child_global) = child.try_signal_queue_accounts(kuid(1000)).unwrap();
        let (grandchild_user, grandchild_global) =
            grandchild.try_signal_queue_accounts(kuid(1000)).unwrap();

        assert!(!Arc::ptr_eq(&root_user, &child_user));
        assert!(!Arc::ptr_eq(&child_user, &grandchild_user));
        assert!(Arc::ptr_eq(&root_global, &child_global));
        assert!(Arc::ptr_eq(&root_global, &grandchild_global));
    }

    #[test]
    fn user_namespace_maps_publish_once_and_setgroups_deny_is_irreversible() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        assert!(child.uid_map().is_empty());
        assert!(child.gid_map().is_empty());

        let uid_map = child
            .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        child.publish_uid_map(uid_map).unwrap();
        assert_eq!(child.kernel_uid_to_user(kuid(1000)).unwrap().into_raw(), 0);
        assert_eq!(
            child.publish_uid_map(
                child
                    .try_build_uid_map(vec![IdMapInputExtent::new(1, 1001, 1)])
                    .unwrap()
            ),
            Err(AxError::OperationNotPermitted)
        );

        let gid_map = child
            .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        assert_eq!(
            child.publish_gid_map(gid_map.clone(), true),
            Err(AxError::OperationNotPermitted)
        );
        child.update_setgroups_policy(false).unwrap();
        assert!(!child.setgroups_allowed());
        assert_eq!(
            child.update_setgroups_policy(true),
            Err(AxError::OperationNotPermitted)
        );
        child.publish_gid_map(gid_map, true).unwrap();
        assert!(!child.may_setgroups());
        assert_eq!(
            child.update_setgroups_policy(false),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn nested_user_namespace_owner_must_be_mapped_in_parent() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        assert!(matches!(
            child.try_fork(kuid(1000), kgid(1000), false),
            Err(AxError::OperationNotPermitted)
        ));
    }

    #[test]
    fn namespace_owner_objects_retain_explicit_snapshot_and_forked_state() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let child_weak = Arc::downgrade(&child);

        let cgroup_root = CgroupNamespace::try_new_root(root.clone()).unwrap();
        let cgroup_child = cgroup_root.try_fork(child.clone()).unwrap();
        assert!(Arc::ptr_eq(cgroup_root.owner_user_ns(), &root));
        assert!(Arc::ptr_eq(cgroup_child.owner_user_ns(), &child));

        let pid_root = PidNamespace::try_new_root(root.clone()).unwrap();
        let pid_child = pid_root.try_fork(42, child.clone()).unwrap();
        assert!(Arc::ptr_eq(pid_root.owner_user_ns(), &root));
        assert!(Arc::ptr_eq(pid_child.owner_user_ns(), &child));

        let uts_root = UtsNamespace::try_new_root(root.clone()).unwrap();
        uts_root.set_nodename(b"owner-snapshot");
        let uts_child = uts_root.try_fork(child.clone()).unwrap();
        assert!(Arc::ptr_eq(uts_child.owner_user_ns(), &child));
        assert_eq!(uts_child.nodename(), b"owner-snapshot");

        let time_root = TimeNamespace::try_new_root(root.clone()).unwrap();
        time_root.set_monotonic_offset(7, 11);
        time_root.set_boottime_offset(-3, 19);
        let time_child = time_root.try_fork(child.clone()).unwrap();
        assert!(Arc::ptr_eq(time_child.owner_user_ns(), &child));
        assert_eq!(time_child.render_offsets(), time_root.render_offsets());

        let network_child = NetworkNamespace::try_new_loopback_only(child.clone()).unwrap();
        assert!(Arc::ptr_eq(network_child.owner_user_ns(), &child));

        drop(child);
        assert!(child_weak.upgrade().is_some());
        drop((
            cgroup_child,
            pid_child,
            uts_child,
            time_child,
            network_child,
        ));
        assert!(child_weak.upgrade().is_none());
    }

    #[test]
    fn concurrent_registry_admission_publishes_one_live_winner() {
        const THREADS: usize = 16;

        let namespace = UserNamespace::try_new_root().unwrap();
        let start = Arc::new(Barrier::new(THREADS));
        let hold = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let namespace = namespace.clone();
                let start = start.clone();
                let hold = hold.clone();
                thread::spawn(move || {
                    start.wait();
                    let account = namespace.try_signal_queue_accounts(kuid(1000)).unwrap().0;
                    // Keep every returned strong reference alive until all
                    // racing lookups have completed.
                    hold.wait();
                    account
                })
            })
            .collect();
        let accounts: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert!(
            accounts[1..]
                .iter()
                .all(|account| Arc::ptr_eq(&accounts[0], account))
        );
    }

    #[test]
    fn implementation_signal_queue_ceilings_are_bounded() {
        assert_eq!(SIGNAL_QUEUE_PER_USER_HARD_LIMIT, 4_096);
        assert_eq!(SIGNAL_QUEUE_GLOBAL_HARD_LIMIT, 16_384);
    }
}
