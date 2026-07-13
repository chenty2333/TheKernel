use alloc::{
    boxed::Box,
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
use thekernel_linux_cred::{
    USER_NAMESPACE_OVERFLOW_ID, UserNamespaceDomain, UserNamespaceMapState,
};

// Host unit tests do not initialize the kernel scheduler/current task. Keep
// the production registry sleepable, but let ownership/admission tests execute
// the same critical sections without entering `axsync::Mutex`'s task wait path.
#[cfg(not(test))]
type SignalAccountRegistryMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type SignalAccountRegistryMutex<T> = spin::Mutex<T>;

use super::{
    IdMap, IdMapInputExtent, Kgid, Kuid, UserGid, UserUid,
    accounting::{AtomicTaskUsage, live_process_usage},
    cred_error,
    creds::{Cred, CredentialSlot, PreparedCred},
    exec_cred::PreparedExecCredential,
    futex::FutexTable,
    jobctl::{
        ContinueResult, ExecControlState, JobControlState, PtraceControlState, PtraceSession,
        StopKind, StopReport, StopState, VforkControlState,
    },
    resources::Rlimits,
    signal::PtraceSignalRecord,
    thread::{TaskParentPublicationGuard, lock_task_parent_publication},
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
/// Authoritative bounded process child-to-reaper handoff from the core.
pub(crate) type ProcessReparentBatch = starry_process::ProcessReparentBatch<Arc<Cred>>;
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
/// Maximum number of live or publication-reserved reverse ptrace links owned
/// by one tracer process.
pub(crate) const PTRACE_REVERSE_LINK_HARD_LIMIT: usize = 4_096;
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

pub(crate) struct UserNamespace {
    _admission: UserNamespaceAdmission,
    id: u64,
    domain: UserNamespaceDomain<UserNamespace>,
    map_state: SpinNoIrq<UserNamespaceMapState>,
    signal_accounts: SignalAccountRegistryMutex<HashMap<Kuid, Weak<SignalQueueAccount>>>,
    global_signal_account: Arc<SignalQueueAccount>,
}

impl UserNamespace {
    pub(crate) fn try_new_root() -> AxResult<Arc<Self>> {
        let map_state = UserNamespaceMapState::try_initial().map_err(cred_error)?;
        let global_signal_account = SignalQueueAccount::try_new(SIGNAL_QUEUE_GLOBAL_HARD_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let admission = UserNamespaceAdmission::try_new()?;
        Arc::try_new(Self {
            _admission: admission,
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            domain: UserNamespaceDomain::initial(),
            map_state: SpinNoIrq::new(map_state),
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
        let (uid_map, gid_map, setgroups_allowed) = {
            let state = self.map_state.lock();
            (state.uid_map(), state.gid_map(), state.setgroups_allowed())
        };
        let domain = UserNamespaceDomain::try_child(
            self,
            &uid_map,
            &gid_map,
            owner,
            group,
            parent_could_setfcap,
        )
        .map_err(cred_error)?;
        let map_state = UserNamespaceMapState::try_child(setgroups_allowed).map_err(cred_error)?;
        let admission = UserNamespaceAdmission::try_new()?;
        Arc::try_new(Self {
            _admission: admission,
            id: PROC_NS_ID.fetch_add(1, Ordering::Relaxed),
            domain,
            map_state: SpinNoIrq::new(map_state),
            signal_accounts: SignalAccountRegistryMutex::new(HashMap::new()),
            global_signal_account: self.global_signal_account.clone(),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.domain.parent()
    }

    pub(crate) fn is_initial(&self) -> bool {
        self.domain.is_initial()
    }

    pub(crate) fn owner_kuid(&self) -> Kuid {
        self.domain.owner_kuid()
    }

    pub(crate) fn parent_could_setfcap(&self) -> bool {
        self.domain.parent_could_setfcap()
    }

    pub(crate) fn uid_map(&self) -> Arc<IdMap> {
        self.map_state.lock().uid_map()
    }

    pub(crate) fn gid_map(&self) -> Arc<IdMap> {
        self.map_state.lock().gid_map()
    }

    fn map_display_namespace(self: &Arc<Self>, viewer: &Arc<Self>) -> Arc<Self> {
        // Linux uses seq_user_ns() for map reads, except that a task reading
        // its own namespace map sees lower IDs in the immediate parent.
        if Arc::ptr_eq(self, viewer) {
            self.parent().unwrap_or_else(|| viewer.clone())
        } else {
            viewer.clone()
        }
    }

    pub(crate) fn try_uid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.uid_map();
        let lower_map = self.map_display_namespace(viewer).uid_map();
        map.try_extents_for_lower(&lower_map).map_err(cred_error)
    }

    pub(crate) fn try_gid_map_rows(
        self: &Arc<Self>,
        viewer: &Arc<Self>,
    ) -> AxResult<Vec<IdMapInputExtent>> {
        let map = self.gid_map();
        let lower_map = self.map_display_namespace(viewer).gid_map();
        map.try_extents_for_lower(&lower_map).map_err(cred_error)
    }

    pub(crate) fn try_build_uid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent(input, &parent_map).map_err(cred_error)
    }

    pub(crate) fn try_build_uid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.uid_map();
        IdMap::try_from_parent_slice(input, &parent_map).map_err(cred_error)
    }

    pub(crate) fn try_build_gid_map(&self, input: Vec<IdMapInputExtent>) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent(input, &parent_map).map_err(cred_error)
    }

    pub(crate) fn try_build_gid_map_from_slice(
        &self,
        input: &[IdMapInputExtent],
    ) -> AxResult<Arc<IdMap>> {
        let parent = self.parent().ok_or(AxError::OperationNotPermitted)?;
        let parent_map = parent.gid_map();
        IdMap::try_from_parent_slice(input, &parent_map).map_err(cred_error)
    }

    /// Publishes a fully built UID map exactly once. Construction and parent
    /// resolution happen before the short map-state guard. The core borrows
    /// `map` and clones it into an empty slot, so no map ownership is retired or
    /// returned by the guarded operation.
    pub(crate) fn publish_uid_map(&self, map: Arc<IdMap>) -> AxResult<()> {
        let result = {
            let mut state = self.map_state.lock();
            state.try_publish_uid_map(&map)
        };
        result.map_err(cred_error)
    }

    /// Publishes a fully built GID map exactly once. Unprivileged callers pass
    /// `require_setgroups_denied`; that check is made under the same map-state
    /// guard as publication, closing the deny/write race.
    pub(crate) fn publish_gid_map(
        &self,
        map: Arc<IdMap>,
        require_setgroups_denied: bool,
    ) -> AxResult<()> {
        let result = {
            let mut state = self.map_state.lock();
            state.try_publish_gid_map(&map, require_setgroups_denied)
        };
        result.map_err(cred_error)
    }

    pub(crate) fn setgroups_allowed(&self) -> bool {
        self.map_state.lock().setgroups_allowed()
    }

    pub(crate) fn uid_map_written(&self) -> bool {
        self.map_state.lock().uid_map_written()
    }

    pub(crate) fn gid_map_written(&self) -> bool {
        self.map_state.lock().gid_map_written()
    }

    pub(crate) fn may_setgroups(&self) -> bool {
        self.map_state.lock().may_setgroups()
    }

    pub(crate) fn update_setgroups_policy(&self, allow: bool) -> AxResult<()> {
        self.map_state
            .lock()
            .try_update_setgroups_policy(allow)
            .map_err(cred_error)
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
            .unwrap_or(USER_NAMESPACE_OVERFLOW_ID)
    }

    pub(crate) fn from_kgid_munged(&self, gid: Kgid) -> u32 {
        self.kernel_gid_to_user(gid)
            .map(UserGid::into_raw)
            .unwrap_or(USER_NAMESPACE_OVERFLOW_ID)
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

impl thekernel_linux_cred::UserNamespaceView for UserNamespace {
    fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
        self.domain.parent()
    }

    fn level(&self) -> u32 {
        self.domain.level()
    }

    fn owner_kuid(&self) -> Kuid {
        self.domain.owner_kuid()
    }

    fn root_kuid(&self) -> Option<Kuid> {
        UserNamespace::root_kuid(self)
    }

    fn is_initial(&self) -> bool {
        self.domain.is_initial()
    }
}

impl thekernel_linux_cred::ExecUserNamespaceView for UserNamespace {
    fn exec_id_map_snapshot(&self) -> (Arc<IdMap>, Arc<IdMap>) {
        let state = self.map_state.lock();
        (state.uid_map(), state.gid_map())
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

/// Immutable NUMA-policy view bound to one process image.
///
/// `/proc/PID/numa_maps` keeps this snapshot together with its pinned address
/// space so a later exec cannot pair the old VMAs with the new image's policy
/// state (or the inverse).
#[derive(Clone, Debug)]
pub(crate) struct MempolicySnapshot {
    process_policy: Mempolicy,
    ranges: Vec<MempolicyRange>,
}

impl MempolicySnapshot {
    pub(crate) fn policy_for_addr(&self, addr: usize) -> Mempolicy {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map_or(self.process_policy, |range| range.policy)
    }
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

/// Linux process dumpability values implemented by this kernel.
///
/// `SUID_DUMP_ROOT` remains unsupported until core-pattern ownership and the
/// corresponding sysctl policy exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Dumpability {
    NotDumpable  = 0,
    UserDumpable = 1,
}

impl TryFrom<usize> for Dumpability {
    type Error = AxError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NotDumpable),
            1 => Ok(Self::UserDumpable),
            _ => Err(AxError::InvalidInput),
        }
    }
}

#[derive(Clone, Copy)]
struct ProcessSecurityState {
    dumpability: Dumpability,
}

/// Credential/dumpability publication barrier owned by one Linux address
/// space. Processes created with `CLONE_VM` share this owner; ordinary fork
/// receives a new owner initialized from the same snapshot.
pub(crate) struct ProcessAccessState {
    owner_user_ns: Arc<UserNamespace>,
    security: SpinNoIrq<ProcessSecurityState>,
}

impl ProcessAccessState {
    pub(crate) fn try_new(
        dumpability: Dumpability,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            owner_user_ns,
            security: SpinNoIrq::new(ProcessSecurityState { dumpability }),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) fn dumpability(&self) -> Dumpability {
        self.security.lock().dumpability
    }

    fn set_dumpability(&self, dumpability: Dumpability) {
        self.security.lock().dumpability = dumpability;
    }

    #[cfg(test)]
    pub(crate) fn set_dumpability_for_test(&self, dumpability: Dumpability) {
        self.set_dumpability(dumpability);
    }

    fn publish_credential<'a>(
        &self,
        prepared: PreparedCred<'a>,
        pdeath_signal: &AtomicU32,
    ) -> super::creds::CredentialPublication<'a> {
        let mut security = self.security.lock();
        if prepared.requires_dumpability_drop() {
            security.dumpability = Dumpability::NotDumpable;
            pdeath_signal.store(0, Ordering::Release);
        }
        let publication = prepared.publish();
        drop(security);
        publication
    }
}

/// Address space and the exact access-state owner governing it.
///
/// Keeping the pair under one lock makes exec replace both atomically and
/// prevents process_vm from authorizing one image and then operating on a
/// later image.
struct ProcessImageBinding<A> {
    aspace: A,
    access_state: Arc<ProcessAccessState>,
}

type LiveProcessImageBinding = ProcessImageBinding<Arc<Mutex<AddrSpace>>>;

pub(crate) struct ProcessImageAccessSnapshot {
    credential: Arc<Cred>,
    dumpability: Dumpability,
    owner_user_ns: Arc<UserNamespace>,
    aspace: Arc<Mutex<AddrSpace>>,
    access_state: Arc<ProcessAccessState>,
    exact_target: Option<(Pid, Arc<CredentialSlot>)>,
}

impl ProcessImageAccessSnapshot {
    pub(crate) fn credential(&self) -> &Cred {
        &self.credential
    }

    pub(crate) fn dumpability(&self) -> Dumpability {
        self.dumpability
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    /// Borrows the exact image identity presented to an authorization hook.
    pub(crate) fn aspace(&self) -> &Arc<Mutex<AddrSpace>> {
        &self.aspace
    }

    pub(crate) fn into_aspace(self) -> Arc<Mutex<AddrSpace>> {
        self.aspace
    }

    fn exact_target_matches(&self, target: &super::Thread) -> bool {
        let Some((tid, slot)) = &self.exact_target else {
            return false;
        };
        *tid == target.kernel_tid() && Arc::ptr_eq(slot, &target.credential_slot())
    }
}

fn snapshot_credential_image<A: Clone>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    slot: &CredentialSlot,
) -> (Arc<Cred>, Dumpability, A, Arc<ProcessAccessState>) {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let credential = slot.current();
    let dumpability = security.dumpability;
    let aspace = image.aspace.clone();
    let access_state = image.access_state.clone();
    drop(security);
    drop(image);
    (credential, dumpability, aspace, access_state)
}

fn snapshot_group_credential_image<A: Clone>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    group_leader: &GroupLeaderCredentialBinding,
) -> (
    Arc<Cred>,
    Dumpability,
    Arc<UserNamespace>,
    A,
    Arc<ProcessAccessState>,
) {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let credential = group_leader.current_cred();
    let dumpability = security.dumpability;
    let owner_user_ns = image.access_state.owner_user_ns.clone();
    let aspace = image.aspace.clone();
    let access_state = image.access_state.clone();
    drop(security);
    drop(image);
    (credential, dumpability, owner_user_ns, aspace, access_state)
}

fn coredump_image_snapshot<A: Clone>(image_binding: &RwLock<ProcessImageBinding<A>>) -> Option<A> {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let snapshot =
        (security.dumpability == Dumpability::UserDumpable).then(|| image.aspace.clone());
    drop(security);
    drop(image);
    snapshot
}

fn ptrace_image_snapshot_if_session<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    session: PtraceSession,
) -> Option<A> {
    // Keep the global ptrace/image order aligned with relationship
    // publication: image first, then ptrace control. Holding both gates across
    // the clone gives remote-memory operations one linearization point and
    // prevents a detach/reattach ABA without deadlocking a competing attach
    // which has already pinned the image and is waiting for ptrace control.
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    if ptrace_ctl.active_session() != Some(session) {
        return None;
    }
    let snapshot = image.aspace.clone();
    drop(ptrace_ctl);
    drop(image);
    Some(snapshot)
}

fn ptrace_image_snapshot_if_owned<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    tracer: Pid,
) -> Option<(PtraceSession, A)> {
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    let session = ptrace_ctl
        .active_session()
        .filter(|session| session.tracer == tracer)?;
    let snapshot = image.aspace.clone();
    drop(ptrace_ctl);
    drop(image);
    Some((session, snapshot))
}

fn ptrace_inactive_image_snapshot_if_session<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    job_ctl: &SpinNoIrq<JobControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    session: PtraceSession,
) -> Option<A> {
    // Preserve the image -> ptrace order used by every image/session
    // snapshot, then include the job-control gate in the same
    // linearization point. A successful clone therefore belongs to this
    // exact stopped relationship, never merely to a reused tracer PID.
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    let job_ctl = job_ctl.lock();
    if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session) {
        return None;
    }
    let snapshot = image.aspace.clone();
    drop(job_ctl);
    drop(ptrace_ctl);
    drop(image);
    Some(snapshot)
}

fn replace_process_image_with_group_handoff<'a, A>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    group_leader: &GroupLeaderCredentialBinding,
    credential: Arc<CredentialSlot>,
    prepared: Option<PreparedCred<'a>>,
    new_image: ProcessImageBinding<A>,
    finish_image_publication: impl FnOnce(),
) -> (GroupLeaderRetirement<'a>, ProcessImageBinding<A>) {
    let mut image = image_binding.write();
    let group_leader = group_leader.publish_handoff(credential, prepared);
    let retired_image = core::mem::replace(&mut *image, new_image);
    finish_image_publication();
    drop(image);
    (group_leader, retired_image)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PtraceReverseLink {
    tracee: Pid,
    session: PtraceSession,
}

impl PtraceReverseLink {
    pub(crate) fn new(tracee: Pid, session: PtraceSession) -> Self {
        Self { tracee, session }
    }

    pub(crate) fn tracee(self) -> Pid {
        self.tracee
    }

    pub(crate) fn session(self) -> PtraceSession {
        self.session
    }
}

struct PtraceReverseLinkNode {
    tracee: Pid,
    session: PtraceSession,
    next: Option<Box<Self>>,
}

#[derive(Default)]
struct PtraceReverseLinks {
    head: Option<Box<PtraceReverseLinkNode>>,
    len: usize,
    reservations: usize,
    closed: bool,
}

impl PtraceReverseLinks {
    fn try_reserve(&mut self) -> AxResult<()> {
        if self.closed {
            return Err(AxError::NoSuchProcess);
        }
        let Some(total) = self.len.checked_add(self.reservations) else {
            return Err(AxError::NoMemory);
        };
        if total >= PTRACE_REVERSE_LINK_HARD_LIMIT {
            return Err(AxError::NoMemory);
        }
        let Some(reservations) = self.reservations.checked_add(1) else {
            return Err(AxError::NoMemory);
        };
        self.reservations = reservations;
        Ok(())
    }

    fn cancel_reservation(&mut self) {
        let old = self.reservations;
        debug_assert!(old != 0);
        if old != 0 {
            self.reservations = old - 1;
        }
    }

    /// Allocation-free partition used when one tracer task exits while its
    /// thread group remains live. Nodes are only relinked under the spin lock;
    /// the returned chain is consumed and destroyed after the guard drops.
    fn drain_task(&mut self, tracer_kernel_tid: Pid) -> Option<Box<PtraceReverseLinkNode>> {
        let mut source = self.head.take();
        let mut retained = None;
        let mut drained = None;
        let mut retained_len = 0usize;
        while let Some(mut node) = source {
            source = node.next.take();
            if node.session.tracer_kernel_tid == tracer_kernel_tid {
                node.next = drained.take();
                drained = Some(node);
            } else {
                retained_len += 1;
                node.next = retained.take();
                retained = Some(node);
            }
        }
        self.head = retained;
        self.len = retained_len;
        drained
    }
}

/// Preallocated and hard-limit-accounted reverse-link publication token.
///
/// Allocation happens before the reservation spin lock is acquired. Dropping
/// an unpublished token releases its reservation and node outside the lock.
pub(crate) struct PreparedPtraceReverseLink<'a> {
    owner: &'a SpinNoIrq<PtraceReverseLinks>,
    tracer: Pid,
    tracer_kernel_tid: Pid,
    node: Option<Box<PtraceReverseLinkNode>>,
    reserved: bool,
}

impl PreparedPtraceReverseLink<'_> {
    fn publish(mut self, session: PtraceSession) -> Result<(), (AxError, Self)> {
        let Some(mut node) = self.node.take() else {
            return Err((AxError::BadState, self));
        };
        node.session = session;
        let mut links = self.owner.lock();
        if !self.reserved || links.reservations == 0 || links.closed {
            drop(links);
            self.node = Some(node);
            return Err((AxError::BadState, self));
        }
        node.next = links.head.take();
        links.head = Some(node);
        links.len += 1;
        links.cancel_reservation();
        self.reserved = false;
        drop(links);
        Ok(())
    }
}

impl Drop for PreparedPtraceReverseLink<'_> {
    fn drop(&mut self) {
        if self.reserved {
            let mut links = self.owner.lock();
            links.cancel_reservation();
            self.reserved = false;
            drop(links);
        }
        // `node` is deliberately dropped only after the spin guard above.
    }
}

pub(crate) struct PtraceReverseLinkDrain {
    next: Option<Box<PtraceReverseLinkNode>>,
}

impl Iterator for PtraceReverseLinkDrain {
    type Item = PtraceReverseLink;

    fn next(&mut self) -> Option<Self::Item> {
        let mut node = self.next.take()?;
        self.next = node.next.take();
        Some(PtraceReverseLink {
            tracee: node.tracee,
            session: node.session,
        })
    }
}

/// [`Process`]-shared data.
pub struct ProcessData {
    /// The process.
    pub(crate) proc: Arc<Process>,
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
    /// Executable address space and its coherent process-access owner.
    image_binding: RwLock<LiveProcessImageBinding>,
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
    /// Sleepable outer gate for ptrace relationship actions.
    ///
    /// Syscall operations hold this across exact-session/state validation and
    /// their use (including userspace copies or address-space locking). Spin
    /// guards remain short-lived inside the gate. This prevents a sibling
    /// tracer thread from CONT/DETACH racing a PEEK/POKE or state mutation.
    ptrace_actions: Mutex<()>,
    /// Exact queued signal retained while stopped at a ptrace delivery boundary.
    ptrace_signal: Mutex<Option<PtraceSignalRecord>>,
    /// Bounded, preallocated reverse links for processes traced by this one.
    ptrace_tracees: SpinNoIrq<PtraceReverseLinks>,
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

/// Composite outer gate for ptrace relationship publication.
///
/// Exit owns `process_lifecycle` before it removes a core thread membership
/// and later takes `ptrace_actions` during relationship cleanup. Publishing in
/// the same order prevents an attach from racing past the only exit cleanup or
/// deadlocking it with the inverse `ptrace_actions -> process_lifecycle` order.
pub(crate) struct PtracePublicationGuard<'a> {
    owner: &'a ProcessData,
    tracer_owner: Option<&'a ProcessData>,
    // Fields are declared in release order: action users leave before a new
    // lifecycle transition can enter.
    _actions: axsync::MutexGuard<'a, ()>,
    task_parent: TaskParentPublicationGuard<'static>,
    _second_lifecycle: Option<axsync::MutexGuard<'a, ()>>,
    _first_lifecycle: axsync::MutexGuard<'a, ()>,
}

impl PtracePublicationGuard<'_> {
    pub(crate) fn task_parent_publication(&self) -> &TaskParentPublicationGuard<'static> {
        &self.task_parent
    }
}

fn ptrace_lifecycle_first(left: &ProcessData, right: &ProcessData) -> bool {
    ptrace_lifecycle_first_key(
        left as *const ProcessData as usize,
        right as *const ProcessData as usize,
    )
}

fn ptrace_lifecycle_first_key(left: usize, right: usize) -> bool {
    left < right
}

/// Deferred destruction produced by an exec group-leader handoff.
///
/// The value must be dropped only after every registry/binding lock involved
/// in the composite publication has been released.
pub(crate) struct GroupLeaderRetirement<'a> {
    _publication: Option<super::creds::CredentialPublication<'a>>,
    _slot: Arc<CredentialSlot>,
}

/// Retired image and credential ownership from one exec publication. The
/// caller keeps this alive until after switching the hardware page-table root.
pub(crate) struct ExecImageRetirement<'a> {
    _group_leader: GroupLeaderRetirement<'a>,
    _image: LiveProcessImageBinding,
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
        access_state: Arc<ProcessAccessState>,
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
            image_binding: RwLock::new(ProcessImageBinding {
                aspace,
                access_state,
            }),
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
            timerslack_current_ns: AtomicUsize::new(50_000),
            timerslack_default_ns: AtomicUsize::new(50_000),
            posix_timers: SpinNoIrq::new(Vec::new()),
            exited_threads_usage: AtomicTaskUsage::new(),
            waited_children_usage: AtomicTaskUsage::new(),
            maxrss_kb: AtomicU64::new(0),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            ptrace_ctl: SpinNoIrq::new(PtraceControlState::default()),
            ptrace_actions: Mutex::new(()),
            ptrace_signal: Mutex::new(None),
            ptrace_tracees: SpinNoIrq::new(PtraceReverseLinks::default()),
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

    /// Takes process-directed identity, dumpability, and image through one
    /// coherent snapshot of the persistent group-leader binding.
    pub(crate) fn group_leader_image_access_snapshot(&self) -> ProcessImageAccessSnapshot {
        let (credential, dumpability, owner_user_ns, aspace, access_state) =
            snapshot_group_credential_image(&self.image_binding, &self.group_leader_credential);
        ProcessImageAccessSnapshot {
            credential,
            dumpability,
            owner_user_ns,
            aspace,
            access_state,
            exact_target: None,
        }
    }

    /// Takes one exact live task identity with the image/access owner it
    /// names. Callers must operate on the returned address-space handle.
    pub(crate) fn thread_image_access_snapshot(
        &self,
        thread: &super::Thread,
    ) -> AxResult<ProcessImageAccessSnapshot> {
        debug_assert!(core::ptr::eq(&*thread.proc_data, self));
        if thread.exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        let slot = thread.credential_slot();
        let (credential, dumpability, aspace, access_state) =
            snapshot_credential_image(&self.image_binding, &slot);
        if thread.exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        Ok(ProcessImageAccessSnapshot {
            credential,
            dumpability,
            owner_user_ns: access_state.owner_user_ns.clone(),
            aspace,
            access_state,
            exact_target: Some((thread.kernel_tid(), slot)),
        })
    }

    pub(crate) fn credential_image_access_snapshot(
        &self,
        slot: &CredentialSlot,
    ) -> ProcessImageAccessSnapshot {
        let (credential, dumpability, aspace, access_state) =
            snapshot_credential_image(&self.image_binding, slot);
        ProcessImageAccessSnapshot {
            credential,
            dumpability,
            owner_user_ns: access_state.owner_user_ns.clone(),
            aspace,
            access_state,
            exact_target: None,
        }
    }

    pub(crate) fn dumpability(&self) -> Dumpability {
        let image = self.image_binding.read();
        let dumpability = image.access_state.dumpability();
        drop(image);
        dumpability
    }

    pub(crate) fn set_dumpability(&self, dumpability: Dumpability) {
        let image = self.image_binding.read();
        image.access_state.set_dumpability(dumpability);
        drop(image);
    }

    pub(crate) fn fork_image_credential_snapshot(
        &self,
        thread: &super::Thread,
    ) -> (
        Arc<Cred>,
        Dumpability,
        Arc<Mutex<AddrSpace>>,
        Arc<ProcessAccessState>,
    ) {
        debug_assert!(core::ptr::eq(&*thread.proc_data, self));
        let slot = thread.credential_slot();
        let (credential, dumpability, aspace, access_state) =
            snapshot_credential_image(&self.image_binding, &slot);
        (credential, dumpability, aspace, access_state)
    }

    /// The sole normal-task credential publication path.
    pub(in crate::task) fn publish_credential<'a>(
        &self,
        prepared: PreparedCred<'a>,
        pdeath_signal: &AtomicU32,
    ) -> Arc<Cred> {
        let image = self.image_binding.read();
        let publication = image
            .access_state
            .publish_credential(prepared, pdeath_signal);
        let proposed = publication.proposed();
        drop(image);
        drop(publication);
        proposed
    }

    /// Publishes the mandatory fully derived exec credential and switches the
    /// group-leader slot as one process-visible transition. Retired `Arc`s are
    /// destroyed only after the binding lock is released.
    pub(in crate::task) fn publish_exec_image<'a>(
        &self,
        owner: Pid,
        thread: &super::Thread,
        prepared: PreparedExecCredential<'a>,
        new_aspace: Arc<Mutex<AddrSpace>>,
        new_access_state: Arc<ProcessAccessState>,
    ) -> ExecImageRetirement<'a> {
        debug_assert!(self.is_exec_owner(owner));
        debug_assert_eq!(thread.proc_data.proc.pid(), self.proc.pid());
        let credential = thread.credential_slot();
        let effects = prepared.effects();
        if effects.clear_pdeath_signal() {
            // Linux pdeath_signal is task-local: only the executor crosses
            // this credential transition, never its former siblings.
            thread.set_pdeath_signal(0);
        }
        let prepared = prepared.into_prepared();
        let (group_leader, retired_image) = replace_process_image_with_group_handoff(
            &self.image_binding,
            &self.group_leader_credential,
            credential,
            Some(prepared),
            ProcessImageBinding {
                aspace: new_aspace,
                access_state: new_access_state,
            },
            || self.mempolicy.lock().ranges.clear(),
        );
        ExecImageRetirement {
            _group_leader: group_leader,
            _image: retired_image,
        }
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
        self.image_binding.read().aspace.clone()
    }

    pub(crate) fn image_matches(&self, aspace: &Arc<Mutex<AddrSpace>>) -> bool {
        Arc::ptr_eq(&self.image_binding.read().aspace, aspace)
    }

    /// Pins the current image only when the same coherent access snapshot is
    /// user-dumpable. The returned Arc remains bound to that image across any
    /// later exec publication.
    pub(crate) fn coredump_aspace(&self) -> Option<Arc<Mutex<AddrSpace>>> {
        coredump_image_snapshot(&self.image_binding)
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

    /// Fallibly freezes NUMA policy for an already-authorized image.
    ///
    /// Allocation happens before taking the image publication lock. The
    /// second capacity check fails closed if a concurrent policy update grew
    /// the range vector; callers may retry the open operation explicitly.
    pub(crate) fn try_mempolicy_snapshot_for_image(
        &self,
        expected_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> AxResult<MempolicySnapshot> {
        let required = self.mempolicy.lock().ranges.len();
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(required)
            .map_err(|_| AxError::NoMemory)?;

        let image = self.image_binding.read();
        if !Arc::ptr_eq(&image.aspace, expected_aspace) {
            return Err(AxError::ResourceBusy);
        }
        let state = self.mempolicy.lock();
        if ranges.capacity() < state.ranges.len() {
            return Err(AxError::ResourceBusy);
        }
        ranges.extend(state.ranges.iter().copied());
        let snapshot = MempolicySnapshot {
            process_policy: state.process_policy,
            ranges,
        };
        drop(state);
        drop(image);
        Ok(snapshot)
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
    /// Acquires the fixed ptrace publication order used by attach/traceme and
    /// process exit: lifecycle first, then the sleepable action gate.
    pub(crate) fn lock_ptrace_publication(&self) -> PtracePublicationGuard<'_> {
        let lifecycle = self.process_lifecycle.lock();
        let task_parent = lock_task_parent_publication();
        let actions = self.ptrace_actions.lock();
        PtracePublicationGuard {
            owner: self,
            tracer_owner: None,
            _actions: actions,
            task_parent,
            _second_lifecycle: None,
            _first_lifecycle: lifecycle,
        }
    }

    /// Pins both the tracee and exact prospective tracer process against task
    /// exit/reparenting. Distinct ProcessData lifecycle locks use immutable
    /// object-address order, followed by the tracee action gate.
    pub(crate) fn lock_ptrace_traceme_publication<'a>(
        &'a self,
        tracer: &'a ProcessData,
    ) -> AxResult<PtracePublicationGuard<'a>> {
        if core::ptr::eq(self, tracer) {
            return Err(AxError::OperationNotPermitted);
        }
        let (first_owner, second_owner) = if ptrace_lifecycle_first(self, tracer) {
            (self, tracer)
        } else {
            (tracer, self)
        };
        let first_lifecycle = first_owner.process_lifecycle.lock();
        let second_lifecycle = second_owner.process_lifecycle.lock();
        let task_parent = lock_task_parent_publication();
        let actions = self.ptrace_actions.lock();
        Ok(PtracePublicationGuard {
            owner: self,
            tracer_owner: Some(tracer),
            _actions: actions,
            task_parent,
            _second_lifecycle: Some(second_lifecycle),
            _first_lifecycle: first_lifecycle,
        })
    }

    pub(crate) fn lock_ptrace_actions(&self) -> axsync::MutexGuard<'_, ()> {
        self.ptrace_actions.lock()
    }

    pub fn ptrace_tracer(&self) -> Option<Pid> {
        self.ptrace_ctl.lock().tracer
    }

    pub(crate) fn ptrace_active_session(&self) -> Option<PtraceSession> {
        self.ptrace_ctl.lock().active_session()
    }

    pub(crate) fn ptrace_session_if_traced_by(
        &self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
    ) -> Option<PtraceSession> {
        self.ptrace_ctl
            .lock()
            .active_session_if_owned_by(tracer, tracer_kernel_tid)
    }

    pub(crate) fn ptrace_session_if_traced_by_process(&self, tracer: Pid) -> Option<PtraceSession> {
        self.ptrace_ctl
            .lock()
            .active_session()
            .filter(|session| session.tracer == tracer)
    }

    /// Returns the caller-owned relationship only when its exact generation
    /// also owns the current ptrace stop.
    pub(crate) fn ptrace_inactive_session_if_traced_by(
        &self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
    ) -> Option<PtraceSession> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let session = ptrace_ctl.active_session_if_owned_by(tracer, tracer_kernel_tid)?;
        let job_ctl = self.job_ctl.lock();
        job_ctl.is_ptrace_inactive_for(session).then_some(session)
    }

    pub(crate) fn ptrace_set_options(&self, session: PtraceSession, options: u32) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session)
        {
            return false;
        }
        ptrace_ctl.options = options;
        true
    }

    pub(crate) fn ptrace_event_message(&self, session: PtraceSession) -> Option<usize> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.job_ctl.lock();
        (ptrace_ctl.active_session() == Some(session) && job_ctl.is_ptrace_inactive_for(session))
            .then_some(ptrace_ctl.event_message)
    }

    pub(crate) fn ptrace_set_event_message(
        &self,
        session: PtraceSession,
        event_message: usize,
    ) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        ptrace_ctl.event_message = event_message;
        true
    }

    /// Pins the image only if the exact relationship still owns an inactive
    /// ptrace stop at the image/session/job-control linearization point.
    pub(crate) fn ptrace_inactive_image_if_session(
        &self,
        session: PtraceSession,
    ) -> Option<Arc<Mutex<AddrSpace>>> {
        ptrace_inactive_image_snapshot_if_session(
            &self.ptrace_ctl,
            &self.job_ctl,
            &self.image_binding,
            session,
        )
    }

    pub(crate) fn try_prepare_ptrace_reverse_link(
        &self,
        tracee: Pid,
        tracer_kernel_tid: Pid,
    ) -> AxResult<PreparedPtraceReverseLink<'_>> {
        let node = Box::try_new(PtraceReverseLinkNode {
            tracee,
            session: PtraceSession {
                tracer: 0,
                tracer_kernel_tid: 0,
                generation: 0,
            },
            next: None,
        })
        .map_err(|_| AxError::NoMemory)?;
        let mut links = self.ptrace_tracees.lock();
        let admitted = links.try_reserve();
        drop(links);
        admitted?;
        Ok(PreparedPtraceReverseLink {
            owner: &self.ptrace_tracees,
            tracer: self.proc.pid(),
            tracer_kernel_tid,
            node: Some(node),
            reserved: true,
        })
    }

    /// Publishes both directions of one ptrace relationship after revalidating
    /// the exact hook-authorized task/image snapshot. Hooks run before this
    /// method; the fixed lock order here is exec gate, image, access security,
    /// exact credential slot, ptrace control, then tracer reverse links.
    pub(crate) fn publish_ptrace_relationship(
        &self,
        publication: &PtracePublicationGuard<'_>,
        target: &super::Thread,
        tracer: Pid,
        tracer_kernel_tid: Pid,
        seized: bool,
        initial_options: u32,
        authorized: &ProcessImageAccessSnapshot,
        reverse_link: PreparedPtraceReverseLink<'_>,
    ) -> AxResult<PtraceSession> {
        if !core::ptr::eq(publication.owner, self) {
            return Err(AxError::BadState);
        }
        if let Some(tracer_owner) = publication.tracer_owner
            && (tracer_owner.proc.pid() != tracer
                || !tracer_owner
                    .proc
                    .thread_ids()
                    .any(|tid| tid == tracer_kernel_tid))
        {
            return Err(AxError::NoSuchProcess);
        }
        if reverse_link.tracer != tracer
            || reverse_link.tracer_kernel_tid != tracer_kernel_tid
            || reverse_link
                .node
                .as_ref()
                .is_none_or(|node| node.tracee != self.proc.pid())
        {
            return Err(AxError::BadState);
        }
        if !core::ptr::eq(&*target.proc_data, self)
            || target.exit.load(Ordering::Acquire)
            || !self.proc.thread_ids().any(|tid| tid == target.kernel_tid())
        {
            return Err(AxError::NoSuchProcess);
        }
        let exec_ctl = self.exec_ctl.lock();
        if exec_ctl.group_exit || target.exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        if exec_ctl.owner.is_some() {
            return Err(AxError::OperationNotPermitted);
        }
        let image = self.image_binding.read();
        if !Arc::ptr_eq(&image.aspace, &authorized.aspace)
            || !Arc::ptr_eq(&image.access_state, &authorized.access_state)
            || !authorized.exact_target_matches(target)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let security = image.access_state.security.lock();
        if security.dumpability != authorized.dumpability
            || !Arc::ptr_eq(&image.access_state.owner_user_ns, &authorized.owner_user_ns)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let current_credential = target.credential_slot().current();
        if !Arc::ptr_eq(&current_credential, &authorized.credential)
            || target.exit.load(Ordering::Acquire)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        let old_generation = ptrace_ctl.generation;
        let Some(session) =
            ptrace_ctl.try_begin(tracer, tracer_kernel_tid, seized, initial_options)
        else {
            return Err(if ptrace_ctl.tracer.is_some() {
                AxError::OperationNotPermitted
            } else {
                AxError::OutOfRange
            });
        };
        if let Err((error, reverse_link)) = reverse_link.publish(session) {
            ptrace_ctl.tracer = None;
            ptrace_ctl.tracer_kernel_tid = 0;
            ptrace_ctl.generation = old_generation;
            ptrace_ctl.seized = false;
            ptrace_ctl.options = 0;
            ptrace_ctl.event_message = 0;
            drop(ptrace_ctl);
            drop(current_credential);
            drop(security);
            drop(image);
            drop(exec_ctl);
            // The preallocated node and reservation token are destroyed only
            // after every publication spin/image guard has been released.
            drop(reverse_link);
            return Err(error);
        }
        drop(ptrace_ctl);
        drop(current_credential);
        drop(security);
        drop(image);
        drop(exec_ctl);
        Ok(session)
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
        let Some(session) = ptrace_ctl.active_session() else {
            return Err(record);
        };
        if job_ctl.state != StopState::Running || pending.is_some() {
            return Err(record);
        }

        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = record.info().signo() as u8;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        *pending = Some(record);
        Ok(())
    }

    pub(crate) fn ptrace_signal_info(&self, session: PtraceSession) -> Option<SignalInfo> {
        let pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session)
        {
            return None;
        }
        pending.as_ref().map(|record| record.info().clone())
    }

    pub(crate) fn replace_ptrace_signal_info(
        &self,
        session: PtraceSession,
        info: SignalInfo,
    ) -> AxResult<()> {
        let mut pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session)
        {
            return Err(AxError::NoSuchProcess);
        }
        let record = pending.as_mut().ok_or(AxError::InvalidInput)?;
        if record.info().signo() != info.try_signo().ok_or(AxError::InvalidInput)? {
            return Err(AxError::InvalidInput);
        }
        record
            .replace_info(info)
            .map(|_| ())
            .ok_or(AxError::InvalidInput)
    }

    /// Resumes a ptrace stop and atomically takes its retained signal record.
    /// If `detach` is true, tracer ownership is cleared under the same gate so
    /// no new delivery stop can appear between resume and detach.
    pub(crate) fn resume_ptrace(
        &self,
        session: PtraceSession,
        detach: bool,
    ) -> Option<(ContinueResult, Option<PtraceSignalRecord>)> {
        self.resume_ptrace_inner(session, detach, true)
    }

    fn resume_ptrace_inner(
        &self,
        session: PtraceSession,
        detach: bool,
        require_inactive: bool,
    ) -> Option<(ContinueResult, Option<PtraceSignalRecord>)> {
        let mut pending = self.ptrace_signal.lock();
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return None;
        }

        let mut job_ctl = self.job_ctl.lock();
        if require_inactive && !job_ctl.is_ptrace_inactive_for(session) {
            return None;
        }
        if detach {
            let cleared = ptrace_ctl.clear_session(session);
            debug_assert!(cleared);
        }

        let result = match job_ctl.state {
            StopState::Running => ContinueResult::None,
            StopState::Stopping if !require_inactive && job_ctl.stop_kind == StopKind::Ptrace => {
                job_ctl.state = StopState::Running;
                job_ctl.ptrace_session = None;
                ContinueResult::CanceledStopping
            }
            StopState::Stopping => ContinueResult::None,
            StopState::Stopped => {
                if job_ctl.is_ptrace_inactive_for(session) {
                    job_ctl.state = StopState::Running;
                    job_ctl.ptrace_session = None;
                    ContinueResult::ResumedStopped
                } else {
                    ContinueResult::None
                }
            }
        };
        let record = if result == ContinueResult::None && !require_inactive {
            None
        } else {
            pending.take()
        };
        drop(job_ctl);
        drop(ptrace_ctl);
        drop(pending);
        Some((result, record))
    }

    /// Publishes one stop only for the exact relationship which requested it.
    /// A stale attach/exec completion cannot stop a later reattachment that
    /// happens to use the same numeric tracer PID.
    pub(crate) fn ptrace_stop(&self, session: PtraceSession, signo: u8) -> bool {
        let ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session) {
            return false;
        }
        if job_ctl.stop_kind == StopKind::Ptrace && job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Applies `PTRACE_INTERRUPT` to an exact seized relationship. Unlike
    /// ordinary actions this is allowed while the tracee is running.
    pub(crate) fn ptrace_interrupt(&self, session: PtraceSession, signo: u8) -> Option<bool> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !ptrace_ctl.seized {
            return None;
        }
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session) {
            // Linux queues a second trap when INTERRUPT races an existing
            // ptrace stop. Until that pending-trap state is represented, fail
            // closed instead of reporting a success that CONT would lose.
            return None;
        }
        if job_ctl.stop_kind == StopKind::Ptrace && job_ctl.state != StopState::Running {
            return None;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        Some(true)
    }

    /// Publishes the wake only after the caller has resolved the retained
    /// ptrace signal record. This prevents a tracee from returning to user mode
    /// before a requested reinjection has become pending.
    pub(crate) fn finish_ptrace_resume(&self, result: ContinueResult) {
        if result != ContinueResult::None {
            self.stop_event.wake();
        }
    }

    pub(crate) fn end_ptrace(&self, session: PtraceSession) -> bool {
        let Some((result, record)) = self.resume_ptrace_inner(session, true, false) else {
            return false;
        };
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        self.finish_ptrace_resume(result);
        true
    }

    pub(crate) fn clear_ptrace(&self) -> Option<PtraceSession> {
        let (session, record) = {
            let mut pending = self.ptrace_signal.lock();
            let mut ptrace_ctl = self.ptrace_ctl.lock();
            let session = ptrace_ctl.clear_active();
            (session, pending.take())
        };
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        session
    }

    pub(crate) fn try_ptrace_tracees(&self) -> AxResult<Vec<PtraceReverseLink>> {
        let mut snapshot = Vec::new();
        loop {
            let required = self.ptrace_tracees.lock().len;
            if snapshot.capacity() < required {
                snapshot
                    .try_reserve_exact(required)
                    .map_err(|_| AxError::NoMemory)?;
            }
            let tracees = self.ptrace_tracees.lock();
            if snapshot.capacity() < tracees.len {
                drop(tracees);
                continue;
            }
            snapshot.clear();
            let mut cursor = tracees.head.as_deref();
            while let Some(node) = cursor {
                snapshot.push(PtraceReverseLink {
                    tracee: node.tracee,
                    session: node.session,
                });
                cursor = node.next.as_deref();
            }
            return Ok(snapshot);
        }
    }

    pub(crate) fn remove_ptrace_tracee(&self, link: PtraceReverseLink) -> bool {
        let mut tracees = self.ptrace_tracees.lock();
        let mut cursor = &mut tracees.head;
        let removed = loop {
            match cursor {
                Some(node) if node.tracee == link.tracee && node.session == link.session => {
                    let mut removed = cursor.take();
                    if let Some(node) = removed.as_mut() {
                        *cursor = node.next.take();
                    }
                    tracees.len -= 1;
                    break removed;
                }
                Some(node) => cursor = &mut node.next,
                None => break None,
            }
        };
        let found = removed.is_some();
        drop(tracees);
        drop(removed);
        found
    }

    pub(crate) fn clear_ptrace_tracees(&self) -> PtraceReverseLinkDrain {
        let mut tracees = self.ptrace_tracees.lock();
        let next = tracees.head.take();
        tracees.len = 0;
        // Final tracer cleanup closes publication before releasing the lock.
        // Already-prepared tokens will observe this bit and refund their
        // reservations instead of recreating a reverse link after the drain.
        tracees.closed = true;
        drop(tracees);
        PtraceReverseLinkDrain { next }
    }

    pub(crate) fn clear_ptrace_tracees_for_task(
        &self,
        tracer_kernel_tid: Pid,
    ) -> PtraceReverseLinkDrain {
        let mut tracees = self.ptrace_tracees.lock();
        let next = tracees.drain_task(tracer_kernel_tid);
        drop(tracees);
        PtraceReverseLinkDrain { next }
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
        job_ctl.ptrace_session = None;
        true
    }

    /// Finalizes a stop transition if it has not been canceled by SIGCONT.
    pub fn finish_stop(&self) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Stopping {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.ptrace_session = None;
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
                    job_ctl.ptrace_session = None;
                    ContinueResult::CanceledStopping
                }
                StopState::Stopped => {
                    if job_ctl.stop_kind == StopKind::Ptrace && traced {
                        return ContinueResult::None;
                    }
                    job_ctl.state = StopState::Running;
                    job_ctl.ptrace_session = None;
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
    pub(crate) fn take_stop_status(
        &self,
        expected_ptrace_session: Option<PtraceSession>,
    ) -> Option<StopReport> {
        let mut job_ctl = self.job_ctl.lock();
        let report = job_ctl.stop_report_for(expected_ptrace_session)?;
        job_ctl.stop_reported = true;
        Some(report)
    }

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    pub(crate) fn peek_stop_status(
        &self,
        expected_ptrace_session: Option<PtraceSession>,
    ) -> Option<StopReport> {
        let job_ctl = self.job_ctl.lock();
        job_ctl.stop_report_for(expected_ptrace_session)
    }

    /// Claims the pending stop report so a waiter can complete userspace copies first.
    pub(crate) fn claim_stop_status(
        &self,
        expected_ptrace_session: Option<PtraceSession>,
    ) -> Option<StopReport> {
        self.take_stop_status(expected_ptrace_session)
    }

    /// Restores a previously claimed stop report after a failed userspace copy.
    pub(crate) fn restore_stop_status(&self, report: StopReport) {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.current_stop_report() == Some(report) {
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

    use alloc::{boxed::Box, sync::Arc, vec};
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::{sync::Barrier, thread, vec::Vec};

    use axerrno::AxError;
    use axsync::spin::SpinNoIrq;
    use linux_raw_sys::general::CAP_CHOWN;

    use super::{
        CgroupNamespace, Dumpability, GroupLeaderCredentialBinding, Mempolicy, MempolicyRange,
        MempolicySnapshot, MempolicyState, NetworkNamespace, PTRACE_REVERSE_LINK_HARD_LIMIT,
        PidNamespace, PreparedPtraceReverseLink, ProcessAccessState, ProcessImageBinding,
        PtraceReverseLinkDrain, PtraceReverseLinkNode, PtraceReverseLinks,
        SIGNAL_QUEUE_GLOBAL_HARD_LIMIT, SIGNAL_QUEUE_PER_USER_HARD_LIMIT, TimeNamespace,
        UserNamespace, UtsNamespace, coredump_image_snapshot, group_exit_handoff_requires_kill,
        init_uts_state, ptrace_image_snapshot_if_owned, ptrace_image_snapshot_if_session,
        ptrace_inactive_image_snapshot_if_session, ptrace_lifecycle_first_key,
        replace_process_image_with_group_handoff, snapshot_credential_image,
        snapshot_group_credential_image, try_increment_bounded,
    };
    use crate::task::{
        CapabilityState, Cred, CredentialSlot, IdMap, IdMapInputExtent, Kgid, Kuid,
        jobctl::{JobControlState, PtraceControlState, PtraceSession, StopKind, StopState},
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
    fn process_access_identity_and_capability_gain_lower_dumpability_and_clear_pdeath() {
        for field in 0..4 {
            let namespace = UserNamespace::try_new_root().unwrap();
            let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
            let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
            let pdeath = AtomicU32::new(9);
            let mut update = slot.prepare();
            match field {
                0 => update.builder.ids.euid = kuid(1000),
                1 => update.builder.ids.egid = kgid(1000),
                2 => update.builder.ids.fsuid = kuid(1000),
                _ => update.builder.ids.fsgid = kgid(1000),
            }
            let publication = state.publish_credential(update.finish().unwrap(), &pdeath);
            drop(publication);
            assert_eq!(state.dumpability(), Dumpability::NotDumpable);
            assert_eq!(pdeath.load(Ordering::Acquire), 0);
        }

        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
        let mut lower = slot.prepare();
        lower.builder.caps.effective = [0; 2];
        lower.builder.caps.permitted = [0; 2];
        lower.builder.caps.inheritable = [0; 2];
        lower.builder.caps.ambient = [0; 2];
        lower.finish().unwrap().commit();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
        let pdeath = AtomicU32::new(12);
        let (word, mask) = CapabilityState::cap_mask(CAP_CHOWN).unwrap();
        let mut gain = slot.prepare();
        gain.builder.caps.permitted[word] |= mask;
        gain.builder.caps.effective[word] |= mask;
        let publication = state.publish_credential(gain.finish().unwrap(), &pdeath);
        drop(publication);
        assert_eq!(state.dumpability(), Dumpability::NotDumpable);
        assert_eq!(pdeath.load(Ordering::Acquire), 0);
    }

    #[test]
    fn process_access_real_and_saved_id_only_changes_do_not_lower_dumpability() {
        for field in 0..4 {
            let namespace = UserNamespace::try_new_root().unwrap();
            let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
            let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
            let pdeath = AtomicU32::new(7);
            let mut update = slot.prepare();
            match field {
                0 => update.builder.ids.ruid = kuid(1000),
                1 => update.builder.ids.suid = kuid(1000),
                2 => update.builder.ids.rgid = kgid(1000),
                _ => update.builder.ids.sgid = kgid(1000),
            }
            let publication = state.publish_credential(update.finish().unwrap(), &pdeath);
            drop(publication);
            assert_eq!(state.dumpability(), Dumpability::UserDumpable);
            assert_eq!(pdeath.load(Ordering::Acquire), 7);
        }
    }

    #[test]
    fn process_access_snapshot_never_pairs_new_identity_with_user_dumpable() {
        const WRITES: usize = 2_000;
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
        let initial = kuid(1000);
        let stronger = kuid(2000);
        let mut update = slot.prepare();
        update.builder.ids.ruid = initial;
        update.builder.ids.euid = initial;
        update.builder.ids.suid = initial;
        update.builder.ids.fsuid = initial;
        update.finish().unwrap().commit();

        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
        let binding = Arc::new(spin::RwLock::new(ProcessImageBinding {
            aspace: 1usize,
            access_state: state.clone(),
        }));
        let pdeath = Arc::new(AtomicU32::new(1));
        let stronger_ready = Arc::new(Barrier::new(2));
        let stronger_sampled = Arc::new(Barrier::new(2));
        let writer = {
            let slot = slot.clone();
            let state = state.clone();
            let pdeath = pdeath.clone();
            let stronger_ready = stronger_ready.clone();
            let stronger_sampled = stronger_sampled.clone();
            thread::spawn(move || {
                for _ in 0..WRITES {
                    let mut gain = slot.prepare();
                    gain.builder.ids.ruid = stronger;
                    gain.builder.ids.euid = stronger;
                    gain.builder.ids.suid = stronger;
                    gain.builder.ids.fsuid = stronger;
                    drop(state.publish_credential(gain.finish().unwrap(), &pdeath));
                    stronger_ready.wait();
                    stronger_sampled.wait();

                    let mut restore = slot.prepare();
                    restore.builder.ids.ruid = initial;
                    restore.builder.ids.euid = initial;
                    restore.builder.ids.suid = initial;
                    restore.builder.ids.fsuid = initial;
                    drop(state.publish_credential(restore.finish().unwrap(), &pdeath));
                    state.set_dumpability(Dumpability::UserDumpable);
                }
            })
        };

        for _ in 0..WRITES {
            stronger_ready.wait();
            let (credential, dumpability, ..) = snapshot_credential_image(&binding, &slot);
            assert_eq!(credential.ids().euid, stronger);
            assert_eq!(dumpability, Dumpability::NotDumpable);
            stronger_sampled.wait();
        }
        writer.join().unwrap();
    }

    #[test]
    fn process_access_exact_slot_and_group_leader_view_are_distinct() {
        let leader = credential_slot(1000);
        let exact = credential_slot(2000);
        let owner = leader.current().user_ns().clone();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
        let image = spin::RwLock::new(ProcessImageBinding {
            aspace: 7usize,
            access_state: state,
        });
        let group = GroupLeaderCredentialBinding::new(leader);

        let (exact_cred, exact_dumpability, exact_image, _) =
            snapshot_credential_image(&image, &exact);
        let (leader_cred, leader_dumpability, _, leader_image, _) =
            snapshot_group_credential_image(&image, &group);
        assert_eq!(exact_cred.ids().euid, kuid(2000));
        assert_eq!(leader_cred.ids().euid, kuid(1000));
        assert_eq!(exact_dumpability, Dumpability::UserDumpable);
        assert_eq!(leader_dumpability, Dumpability::UserDumpable);
        assert_eq!(exact_image, leader_image);
    }

    #[test]
    fn process_access_coredump_pins_only_the_coherent_dumpable_image() {
        let owner = UserNamespace::try_new_root().unwrap();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner.clone()).unwrap();
        let image = spin::RwLock::new(ProcessImageBinding {
            aspace: 41usize,
            access_state: state.clone(),
        });
        assert_eq!(coredump_image_snapshot(&image), Some(41));
        state.set_dumpability(Dumpability::NotDumpable);
        assert_eq!(coredump_image_snapshot(&image), None);

        let replacement = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
        *image.write() = ProcessImageBinding {
            aspace: 42,
            access_state: replacement,
        };
        assert_eq!(coredump_image_snapshot(&image), Some(42));
    }

    #[test]
    fn process_access_ptrace_session_and_image_pin_share_one_snapshot() {
        let owner = UserNamespace::try_new_root().unwrap();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
        let image = spin::RwLock::new(ProcessImageBinding {
            aspace: 41usize,
            access_state: state,
        });
        let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());

        assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7), None);
        let first = ptrace_ctl.lock().try_begin(7, 70, false, 0).unwrap();
        assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 8), None);
        assert_eq!(
            ptrace_image_snapshot_if_session(&ptrace_ctl, &image, first),
            Some(41)
        );
        assert_eq!(
            ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7),
            Some((
                PtraceSession {
                    tracer: 7,
                    tracer_kernel_tid: 70,
                    generation: 1
                },
                41
            ))
        );

        // A detached session cannot retain its earlier authorization. After
        // reattach, the same tracer PID observes only the newly bound image.
        assert!(ptrace_ctl.lock().clear_session(first));
        image.write().aspace = 42;
        assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7), None);
        let second = ptrace_ctl.lock().try_begin(7, 70, false, 0).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            ptrace_image_snapshot_if_session(&ptrace_ctl, &image, first),
            None
        );
        assert_eq!(
            ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7),
            Some((
                PtraceSession {
                    tracer: 7,
                    tracer_kernel_tid: 70,
                    generation: 2
                },
                42
            ))
        );
    }

    #[test]
    fn process_access_ptrace_remote_image_requires_exact_inactive_session() {
        let owner = UserNamespace::try_new_root().unwrap();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
        let image = spin::RwLock::new(ProcessImageBinding {
            aspace: 41usize,
            access_state: state,
        });
        let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());
        let job_ctl = SpinNoIrq::new(JobControlState::default());

        let first = ptrace_ctl.lock().try_begin(7, 70, false, 0).unwrap();
        assert_eq!(
            ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, first),
            None
        );
        {
            let mut job = job_ctl.lock();
            job.state = StopState::Stopped;
            job.stop_kind = StopKind::Ptrace;
            job.ptrace_session = Some(first);
        }
        assert_eq!(
            ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, first),
            Some(41)
        );

        assert!(ptrace_ctl.lock().clear_session(first));
        let second = ptrace_ctl.lock().try_begin(7, 70, false, 0).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, second),
            None
        );
        job_ctl.lock().ptrace_session = Some(second);
        assert_eq!(
            ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, second),
            Some(41)
        );
    }

    #[test]
    fn process_access_ptrace_generation_exhaustion_never_wraps_or_saturates() {
        let mut state = PtraceControlState {
            generation: u64::MAX,
            ..PtraceControlState::default()
        };
        assert_eq!(state.try_begin(7, 70, false, 0), None);
        assert_eq!(state.active_session(), None);
        assert_eq!(state.generation, u64::MAX);
    }

    #[test]
    fn process_access_ptrace_reverse_link_abort_and_limit_roll_back() {
        let links = SpinNoIrq::new(PtraceReverseLinks::default());
        links.lock().try_reserve().unwrap();
        let token = PreparedPtraceReverseLink {
            owner: &links,
            tracer: 7,
            tracer_kernel_tid: 70,
            node: Some(Box::new(PtraceReverseLinkNode {
                tracee: 9,
                session: PtraceSession {
                    tracer: 0,
                    tracer_kernel_tid: 0,
                    generation: 0,
                },
                next: None,
            })),
            reserved: true,
        };
        drop(token);
        assert_eq!(links.lock().reservations, 0);

        links.lock().len = PTRACE_REVERSE_LINK_HARD_LIMIT;
        assert_eq!(links.lock().try_reserve(), Err(AxError::NoMemory));
        assert_eq!(links.lock().reservations, 0);

        links.lock().len = 0;
        links.lock().closed = true;
        assert_eq!(links.lock().try_reserve(), Err(AxError::NoSuchProcess));
        assert_eq!(links.lock().reservations, 0);
    }

    #[test]
    fn process_access_ptrace_reverse_links_drain_exact_tracer_task() {
        fn session(tracer_kernel_tid: u32, generation: u64) -> PtraceSession {
            PtraceSession {
                tracer: 7,
                tracer_kernel_tid,
                generation,
            }
        }

        let links = SpinNoIrq::new(PtraceReverseLinks {
            head: Some(Box::new(PtraceReverseLinkNode {
                tracee: 11,
                session: session(70, 1),
                next: Some(Box::new(PtraceReverseLinkNode {
                    tracee: 12,
                    session: session(71, 2),
                    next: Some(Box::new(PtraceReverseLinkNode {
                        tracee: 13,
                        session: session(70, 3),
                        next: None,
                    })),
                })),
            })),
            len: 3,
            reservations: 1,
            closed: false,
        });

        let drained = links.lock().drain_task(70);
        let drained: Vec<_> = PtraceReverseLinkDrain { next: drained }.collect();
        assert_eq!(drained.len(), 2);
        assert!(
            drained
                .iter()
                .all(|link| link.session().tracer_kernel_tid == 70)
        );

        let links = links.lock();
        assert_eq!(links.len, 1);
        assert_eq!(links.reservations, 1);
        assert!(!links.closed);
        let retained = links.head.as_ref().unwrap();
        assert_eq!(retained.tracee, 12);
        assert_eq!(retained.session.tracer_kernel_tid, 71);
        assert!(retained.next.is_none());
    }

    #[test]
    fn process_access_ptrace_dual_lifecycle_order_is_total() {
        assert!(ptrace_lifecycle_first_key(0x1000, 0x2000));
        assert!(!ptrace_lifecycle_first_key(0x2000, 0x1000));
        assert!(!ptrace_lifecycle_first_key(0x1000, 0x1000));
    }

    #[test]
    fn process_access_numa_policy_snapshot_is_immutable_and_range_specific() {
        let snapshot = MempolicySnapshot {
            process_policy: Mempolicy::new(0, 1),
            ranges: vec![
                MempolicyRange {
                    start: 0x1000,
                    end: 0x5000,
                    policy: Mempolicy::new(2, 2),
                },
                MempolicyRange {
                    start: 0x2000,
                    end: 0x3000,
                    policy: Mempolicy::new(3, 4),
                },
            ],
        };

        assert_eq!(snapshot.policy_for_addr(0), Mempolicy::new(0, 1));
        assert_eq!(snapshot.policy_for_addr(0x1800), Mempolicy::new(2, 2));
        assert_eq!(snapshot.policy_for_addr(0x2800), Mempolicy::new(3, 4));
    }

    #[test]
    fn process_access_group_leader_exec_handoff_is_image_coherent() {
        let old_slot = credential_slot(1000);
        let new_slot = credential_slot(2000);
        let old_owner = old_slot.current().user_ns().clone();
        let new_owner = new_slot.current().user_ns().clone();
        let old_state = ProcessAccessState::try_new(Dumpability::UserDumpable, old_owner).unwrap();
        let clone_vm_peer_state = old_state.clone();
        let new_state = ProcessAccessState::try_new(Dumpability::NotDumpable, new_owner).unwrap();
        let image = Arc::new(spin::RwLock::new(ProcessImageBinding {
            aspace: 1usize,
            access_state: old_state,
        }));
        let mempolicy = MempolicyState {
            process_policy: Mempolicy::new(0, 1),
            ranges: vec![MempolicyRange {
                start: 0x1000,
                end: 0x2000,
                policy: Mempolicy::new(2, 2),
            }],
        };
        let mempolicy = SpinNoIrq::new(mempolicy);
        let reset_under_image_lock = AtomicBool::new(false);
        let group = Arc::new(GroupLeaderCredentialBinding::new(old_slot));
        let old_seen = Arc::new(Barrier::new(2));
        let new_ready = Arc::new(Barrier::new(2));
        let reader = {
            let image = image.clone();
            let group = group.clone();
            let old_seen = old_seen.clone();
            let new_ready = new_ready.clone();
            thread::spawn(move || {
                let (cred, dumpability, _, aspace, _) =
                    snapshot_group_credential_image(&image, &group);
                assert_eq!(
                    (cred.ids().euid, dumpability, aspace),
                    (kuid(1000), Dumpability::UserDumpable, 1)
                );
                old_seen.wait();
                new_ready.wait();
                let (cred, dumpability, _, aspace, _) =
                    snapshot_group_credential_image(&image, &group);
                assert_eq!(
                    (cred.ids().euid, dumpability, aspace),
                    (kuid(3000), Dumpability::NotDumpable, 2)
                );
            })
        };

        let mut update = new_slot.prepare();
        update.builder.ids.ruid = kuid(3000);
        update.builder.ids.euid = kuid(3000);
        update.builder.ids.suid = kuid(3000);
        update.builder.ids.fsuid = kuid(3000);
        let prepared = update.finish().unwrap();
        old_seen.wait();
        let retirement = replace_process_image_with_group_handoff(
            &image,
            &group,
            new_slot.clone(),
            Some(prepared),
            ProcessImageBinding {
                aspace: 2,
                access_state: new_state,
            },
            || {
                assert!(image.try_read().is_none());
                mempolicy.lock().ranges.clear();
                reset_under_image_lock.store(true, Ordering::Release);
            },
        );
        drop(retirement);
        assert!(reset_under_image_lock.load(Ordering::Acquire));
        assert!(mempolicy.lock().ranges.is_empty());
        let current_state = image.read().access_state.clone();
        assert!(!Arc::ptr_eq(&current_state, &clone_vm_peer_state));
        assert_eq!(clone_vm_peer_state.dumpability(), Dumpability::UserDumpable);
        new_ready.wait();
        reader.join().unwrap();
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
    fn concurrent_uid_map_publish_has_exactly_one_winner() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let first = child
            .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        let second = child
            .try_build_uid_map(vec![IdMapInputExtent::new(1, 2000, 1)])
            .unwrap();
        let start = Arc::new(Barrier::new(2));

        let first_publisher = {
            let child = child.clone();
            let map = first.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                child.publish_uid_map(map)
            })
        };
        let second_publisher = {
            let child = child.clone();
            let map = second.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                child.publish_uid_map(map)
            })
        };

        let mut successes = 0;
        let mut duplicate_rejections = 0;
        for result in [
            first_publisher.join().unwrap(),
            second_publisher.join().unwrap(),
        ] {
            match result {
                Ok(()) => successes += 1,
                Err(AxError::OperationNotPermitted) => duplicate_rejections += 1,
                other => panic!("unexpected UID map publication result: {other:?}"),
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(duplicate_rejections, 1);

        let published = child.uid_map();
        assert!(Arc::ptr_eq(&published, &first) || Arc::ptr_eq(&published, &second));
    }

    #[test]
    fn uid_map_reader_race_observes_only_empty_or_complete_immutable_snapshots() {
        const READS: usize = 20_000;

        fn assert_complete(map: &IdMap) {
            assert_eq!(map.len(), 2);
            assert_eq!(
                map.kernel_uid_to_user(kuid(1000)).map(|uid| uid.into_raw()),
                Some(0)
            );
            assert_eq!(
                map.kernel_uid_to_user(kuid(1001)).map(|uid| uid.into_raw()),
                Some(1)
            );
            assert_eq!(
                map.kernel_uid_to_user(kuid(2000)).map(|uid| uid.into_raw()),
                Some(100)
            );
            assert_eq!(
                map.kernel_uid_to_user(kuid(2001)).map(|uid| uid.into_raw()),
                Some(101)
            );
            assert_eq!(map.kernel_uid_to_user(kuid(1500)), None);
        }

        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let replacement = child
            .try_build_uid_map(vec![
                IdMapInputExtent::new(0, 1000, 2),
                IdMapInputExtent::new(100, 2000, 2),
            ])
            .unwrap();
        let empty_snapshot = child.uid_map();
        assert!(empty_snapshot.is_empty());

        let start = Arc::new(Barrier::new(2));
        let publisher = {
            let child = child.clone();
            let replacement = replacement.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                child.publish_uid_map(replacement)
            })
        };

        start.wait();
        for index in 0..READS {
            let snapshot = child.uid_map();
            if !snapshot.is_empty() {
                assert_complete(&snapshot);
            }
            if index % 64 == 0 {
                thread::yield_now();
            }
        }
        publisher.join().unwrap().unwrap();

        assert!(empty_snapshot.is_empty());
        let published = child.uid_map();
        assert!(Arc::ptr_eq(&published, &replacement));
        assert_complete(&published);
    }

    #[test]
    fn setgroups_deny_race_preserves_gid_gate_and_failed_publish_is_retryable() {
        const RACES: usize = 64;
        const SAMPLES_PER_RACE: usize = 128;

        // Exercise the publication-first result deterministically: failure
        // keeps the slot empty, and the exact prebuilt map can be retried after
        // the irreversible deny transition.
        let retry_root = UserNamespace::try_new_root().unwrap();
        let retry_child = retry_root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let retry_map = retry_child
            .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        assert_eq!(
            retry_child.publish_gid_map(retry_map.clone(), true),
            Err(AxError::OperationNotPermitted)
        );
        assert!(!retry_child.gid_map_written());
        retry_child.update_setgroups_policy(false).unwrap();
        retry_child
            .publish_gid_map(retry_map.clone(), true)
            .unwrap();
        assert!(Arc::ptr_eq(&retry_child.gid_map(), &retry_map));

        for _ in 0..RACES {
            let root = UserNamespace::try_new_root().unwrap();
            let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
            let map = child
                .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                .unwrap();
            let start = Arc::new(Barrier::new(3));

            let deny = {
                let child = child.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    child.update_setgroups_policy(false)
                })
            };
            let publish = {
                let child = child.clone();
                let map = map.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    child.publish_gid_map(map, true)
                })
            };

            start.wait();
            for sample in 0..SAMPLES_PER_RACE {
                let state = child.map_state.lock();
                assert!(
                    !state.setgroups_allowed() || !state.gid_map_written(),
                    "require-denied GID map became visible while setgroups was allowed"
                );
                drop(state);
                if sample % 16 == 0 {
                    thread::yield_now();
                }
            }

            deny.join().unwrap().unwrap();
            let publish_result = publish.join().unwrap();
            assert!(!child.setgroups_allowed());
            match publish_result {
                Ok(()) => assert!(child.gid_map_written()),
                Err(AxError::OperationNotPermitted) => {
                    assert!(!child.gid_map_written());
                    child.publish_gid_map(map.clone(), true).unwrap();
                }
                other => panic!("unexpected GID map publication result: {other:?}"),
            }
            let state = child.map_state.lock();
            assert!(!state.setgroups_allowed());
            assert!(state.gid_map_written());
            assert!(!state.may_setgroups());
            drop(state);
            assert!(Arc::ptr_eq(&child.gid_map(), &map));
        }
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
