use alloc::{
    boxed::Box,
    format,
    sync::{Arc, Weak},
    vec::Vec,
};
#[cfg(test)]
extern crate std;
#[cfg(test)]
use core::cell::Cell;
use core::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::FsPathBuf;
use axhal::{paging::MappingFlags, time::monotonic_time_nanos};
use axnet::{NetStack, PacketAction, PacketContext, PacketHookPoint};
use axpoll::PollSet;
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::{
    AxCpuMask, AxTaskRef, SchedClass, SchedState, TaskSchedulingSnapshot, current,
    task_scheduling_snapshot,
};
use hashbrown::HashMap;
use scope_local::Scope;
use spin::{Lazy, Once, RwLock};
use tk_linux_cred::{USER_NAMESPACE_OVERFLOW_ID, UserNamespaceDomain, UserNamespaceMapState};
use tk_linux_process_adapter::{Pid, ProcessError};
use tk_linux_signal::{
    SignalInfo, SignalQueueAccount, Signo,
    api::{ProcessSignalManager, SharedSignalActions, ThreadSignalManager},
};

use crate::syscall::ipc::{IpcNamespace, SemUndo, apply_sem_undo};

/// x86 protection-key allocation belongs to an mm, not to an individual
/// thread. Key zero is permanently reserved. `pkey_free` deliberately only
/// changes this bitmap: Linux leaves existing PTE key fields intact.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProtectionKeyState {
    allocated: u16,
}

impl Default for ProtectionKeyState {
    fn default() -> Self {
        Self { allocated: 1 }
    }
}

impl ProtectionKeyState {
    pub(crate) const KEYS: usize = 16;

    pub(crate) fn allocate(&mut self) -> AxResult<u8> {
        for key in 1..Self::KEYS {
            let bit = 1u16 << key;
            if self.allocated & bit == 0 {
                self.allocated |= bit;
                return Ok(key as u8);
            }
        }
        Err(AxError::StorageFull)
    }

    pub(crate) fn free(&mut self, key: i32) -> AxResult<()> {
        if !(1..Self::KEYS as i32).contains(&key) {
            return Err(AxError::InvalidInput);
        }
        let bit = 1u16 << key;
        if self.allocated & bit == 0 {
            return Err(AxError::InvalidInput);
        }
        self.allocated &= !bit;
        Ok(())
    }

    pub(crate) fn is_allocated(&self, key: i32) -> bool {
        (0..Self::KEYS as i32).contains(&key) && self.allocated & (1u16 << key) != 0
    }
}

// Host unit tests do not initialize the kernel scheduler/current task. Keep
// the production registry sleepable, but let ownership/admission tests execute
// the same critical sections without entering `axsync::Mutex`'s task wait path.
#[cfg(not(test))]
type SignalAccountRegistryMutex<T> = axsync::Mutex<T>;
#[cfg(test)]
type SignalAccountRegistryMutex<T> = spin::Mutex<T>;

#[cfg(test)]
std::thread_local! {
    static PROCESS_SECURITY_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    static PROCESS_IMAGE_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    static GROUP_LEADER_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
    static PTRACE_ACTION_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum PostCommitLockKind {
    ProcessSecurity,
    ProcessImage,
    GroupLeader,
    PtraceAction,
}

#[cfg(test)]
struct PostCommitLockProbe(PostCommitLockKind);

#[cfg(test)]
impl PostCommitLockProbe {
    fn new(kind: PostCommitLockKind) -> Self {
        post_commit_lock_depth(kind, |depth| depth.set(depth.get() + 1));
        Self(kind)
    }
}

#[cfg(test)]
impl Drop for PostCommitLockProbe {
    fn drop(&mut self) {
        post_commit_lock_depth(self.0, |depth| {
            let held = depth.get();
            debug_assert!(held != 0);
            depth.set(held - 1);
        });
    }
}

#[cfg(test)]
fn post_commit_lock_depth(kind: PostCommitLockKind, apply: impl FnOnce(&Cell<u32>)) {
    match kind {
        PostCommitLockKind::ProcessSecurity => PROCESS_SECURITY_LOCK_DEPTH.with(apply),
        PostCommitLockKind::ProcessImage => PROCESS_IMAGE_LOCK_DEPTH.with(apply),
        PostCommitLockKind::GroupLeader => GROUP_LEADER_LOCK_DEPTH.with(apply),
        PostCommitLockKind::PtraceAction => PTRACE_ACTION_LOCK_DEPTH.with(apply),
    }
}

#[cfg(test)]
pub(in crate::task) fn process_security_lock_held() -> bool {
    PROCESS_SECURITY_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

#[cfg(test)]
pub(in crate::task) fn process_image_lock_held() -> bool {
    PROCESS_IMAGE_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

#[cfg(test)]
pub(in crate::task) fn group_leader_lock_held() -> bool {
    GROUP_LEADER_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

#[cfg(test)]
pub(in crate::task) fn ptrace_action_lock_held() -> bool {
    PTRACE_ACTION_LOCK_DEPTH.with(|depth| depth.get() != 0)
}

use super::{
    IdMap, IdMapInputExtent, Kgid, Kuid, UserGid, UserUid,
    accounting::{AtomicTaskUsage, live_process_usage},
    cred_error,
    creds::{Cred, CredentialSlot, CredentialSnapshotGuard, PreparedCred},
    exec_cred::CommittingExecCredential,
    futex::FutexTable,
    jobctl::{
        ContinueResult, ExecControlState, JobControlState, PtraceControlState,
        PtraceRelationshipOrigin, PtraceRelationshipSnapshot, PtraceSession, StopKind, StopReport,
        StopState, VforkControlState,
    },
    resources::Rlimits,
    security::LandlockDomain,
    signal::PtraceSignalRecord,
    thread::{TaskParentPublicationGuard, lock_task_parent_publication},
    timer::{ForeignCpuTimerSubscriberPool, PosixTimer, ProcessITimerWorkNode, ProcessITimers},
};
use crate::{
    file::{
        FdTable,
        executable::{self, CredentialReadLease, ExecutableKey},
    },
    mm::{AddrSpace, TlbState, register_address_space, unregister_address_space},
    time::wall_time,
};

/// Immutable registration token for the private signal endpoint currently
/// owning Linux thread-group-leader identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ZombieSchedulerSnapshot {
    pub(crate) class: SchedClass,
    pub(crate) nice: i8,
    /// The real-time priority belongs to the same scheduler transaction as
    /// class, nice, reset-on-fork and version.  It must survive the live task
    /// disappearing so sched_getparam can observe an unreaped zombie exactly
    /// as Linux does.
    pub(crate) rt_priority: u8,
    /// Linux's policy query exposes this flag as part of the returned policy,
    /// including while the group leader is an unreaped zombie.
    pub(crate) reset_on_fork: bool,
    /// Raw uclamp request plus per-side ownership retained after the live
    /// scheduler entity disappears.
    pub(crate) uclamp_min: u16,
    pub(crate) uclamp_max: u16,
    pub(crate) uclamp_min_user_defined: bool,
    pub(crate) uclamp_max_user_defined: bool,
    pub(crate) uclamp_effective_min: u16,
    pub(crate) uclamp_effective_max: u16,
    /// The last successfully installed CPU affinity.  The group-leader
    /// identity retains this cell after its live task has exited, matching the
    /// still-addressable unreaped PID lifecycle.
    pub(crate) affinity: AxCpuMask,
    /// Generation of the persistent group-leader binding that owned this
    /// scheduler state. Scheduler commit versions are local to a task, so
    /// they are comparable only within one binding generation.
    identity_epoch: u64,
    version: u64,
}

impl Default for ZombieSchedulerSnapshot {
    fn default() -> Self {
        Self {
            class: SchedClass::Normal,
            nice: 0,
            rt_priority: 0,
            reset_on_fork: false,
            uclamp_min: 0,
            uclamp_max: 1024,
            uclamp_min_user_defined: false,
            uclamp_max_user_defined: false,
            uclamp_effective_min: 0,
            uclamp_effective_max: 1024,
            affinity: AxCpuMask::full(),
            identity_epoch: 0,
            version: 0,
        }
    }
}

impl From<SchedState> for ZombieSchedulerSnapshot {
    fn from(state: SchedState) -> Self {
        Self {
            class: state.class,
            nice: state.nice,
            rt_priority: state.rt_priority,
            reset_on_fork: false,
            uclamp_min: 0,
            uclamp_max: 1024,
            uclamp_min_user_defined: false,
            uclamp_max_user_defined: false,
            uclamp_effective_min: 0,
            uclamp_effective_max: 1024,
            affinity: AxCpuMask::full(),
            identity_epoch: 0,
            version: 0,
        }
    }
}

fn scheduler_version_is_newer_or_equal(candidate: u64, published: u64) -> bool {
    candidate.wrapping_sub(published) < (1_u64 << 63)
}

fn scheduler_publication_matches(
    published_token: u64,
    expected_token: u64,
    commit: TaskSchedulingSnapshot,
    current: Option<TaskSchedulingSnapshot>,
) -> bool {
    published_token == expected_token && current == Some(commit)
}

#[derive(Clone)]
pub(crate) struct GroupLeaderSignalIdentity {
    registration_tid: Pid,
    manager: Arc<ThreadSignalManager>,
    /// PID namespace in which the retained process identity lives. This is
    /// needed to filter a zombie from callers in unrelated namespaces.
    pid_ns: Option<Arc<PidNamespace>>,
    /// Shared scheduler snapshot updated by successful scheduler transactions
    /// and retained by the zombie owner after the live scheduler node disappears.
    scheduler: Option<Arc<SpinNoIrq<ZombieSchedulerSnapshot>>>,
    /// Uniquely identifies this installed leader endpoint.  It changes on
    /// every exec replacement, including when per-task scheduler versions
    /// restart from zero on the executor.
    scheduler_identity_token: u64,
    /// Shared with the durable group-leader binding so an exec replacement is
    /// reflected in the owner retained by a zombie payload.
    landlock: Arc<SpinNoIrq<LandlockDomain>>,
}

impl GroupLeaderSignalIdentity {
    fn new(registration_tid: Pid, manager: Arc<ThreadSignalManager>) -> Self {
        Self {
            registration_tid,
            manager,
            pid_ns: None,
            scheduler: None,
            scheduler_identity_token: 0,
            landlock: Arc::new(SpinNoIrq::new(LandlockDomain::default())),
        }
    }

    fn with_pid_namespace_and_scheduler(
        registration_tid: Pid,
        manager: Arc<ThreadSignalManager>,
        pid_ns: Option<Arc<PidNamespace>>,
        scheduler: Arc<SpinNoIrq<ZombieSchedulerSnapshot>>,
        landlock: Arc<SpinNoIrq<LandlockDomain>>,
    ) -> Self {
        Self {
            registration_tid,
            manager,
            pid_ns,
            scheduler: Some(scheduler),
            scheduler_identity_token: 0,
            landlock,
        }
    }

    fn same_endpoint(&self, other: &Self) -> bool {
        self.registration_tid == other.registration_tid
            && Arc::ptr_eq(&self.manager, &other.manager)
    }
}

/// Shared owner moved through exec handoff and retained in the durable zombie
/// payload. Successful reap takes the sole endpoint from this slot even when a
/// pidfd or wait event still owns the surrounding snapshot.
pub(crate) type GroupLeaderSignalOwner = Arc<SpinNoIrq<Option<GroupLeaderSignalIdentity>>>;

/// Linux process identity bound to immutable exit credential and signal-owner
/// provenance retained in the durable zombie payload.
pub(crate) type Process = tk_linux_process_adapter::Process<Arc<Cred>, GroupLeaderSignalOwner>;

/// Gets the label retained by a durable zombie identity after its runtime
/// ProcessData has left PROCESS_TABLE.
pub(crate) fn zombie_landlock_domain(process: &Process) -> Option<LandlockDomain> {
    process.zombie_payload().and_then(|snapshot| {
        snapshot
            .reap_owner
            .lock()
            .as_ref()
            .map(|owner| owner.landlock.lock().clone())
    })
}
/// Linux process-group identity in the kernel-owned process domain.
pub(crate) type ProcessGroup =
    tk_linux_process_adapter::ProcessGroup<Arc<Cred>, GroupLeaderSignalOwner>;
/// Linux session identity in the kernel-owned process domain.
pub(crate) type Session = tk_linux_process_adapter::Session<Arc<Cred>, GroupLeaderSignalOwner>;
/// Durable process-exit payload used by wait, procfs, and permission paths.
pub(crate) type ZombieSnapshot =
    tk_linux_process_adapter::ZombieSnapshot<Arc<Cred>, GroupLeaderSignalOwner>;
/// Fallibly reserved storage consumed by the final process exit.
pub(crate) type PreparedZombieSnapshot =
    tk_linux_process_adapter::PreparedZombieSnapshot<Arc<Cred>, GroupLeaderSignalOwner>;
/// Prepared payload bound to a validated final-exit transaction.
pub(crate) type PreparedZombieExit =
    tk_linux_process_adapter::PreparedZombieExit<Arc<Cred>, GroupLeaderSignalOwner>;
/// Fully validated final process-exit transaction.
pub(crate) type ProcessExitAdmission =
    tk_linux_process_adapter::ProcessExitAdmission<Arc<Cred>, GroupLeaderSignalOwner>;
/// Completed final-exit transaction with its linearized parent and reaper.
pub(crate) type CommittedProcessExit =
    tk_linux_process_adapter::CommittedProcessExit<Arc<Cred>, GroupLeaderSignalOwner>;
/// Authoritative bounded process child-to-reaper handoff from the core.
pub(crate) type ProcessReparentBatch =
    tk_linux_process_adapter::ProcessReparentBatch<Arc<Cred>, GroupLeaderSignalOwner>;
/// Domain-coordinated thread removal and optional final-exit reservation.
pub(crate) type ThreadExitTransition =
    tk_linux_process_adapter::ThreadExitTransition<Arc<Cred>, GroupLeaderSignalOwner>;
/// Type-bound unpublished process plus initial-thread publication transaction.
pub(crate) type InitialProcessAdmission =
    tk_linux_process_adapter::InitialProcessAdmission<Arc<Cred>, GroupLeaderSignalOwner>;
pub(crate) type ScopedInitialProcessAdmission =
    tk_linux_process_adapter::ScopedInitialProcessAdmission<Arc<Cred>, GroupLeaderSignalOwner>;
/// The kernel's sole process lifecycle and topology owner.
pub(crate) type ProcessDomain =
    tk_linux_process_adapter::ProcessDomain<Arc<Cred>, GroupLeaderSignalOwner>;
/// Core reparenting scope bound one-for-one to a live PID namespace.
pub(crate) type ProcessReaperScope =
    tk_linux_process_adapter::ReaperScope<Arc<Cred>, GroupLeaderSignalOwner>;
type StarryThreadAdmission =
    tk_linux_process_adapter::ThreadAdmission<Arc<Cred>, GroupLeaderSignalOwner>;

struct SessionSidBinding {
    session: Arc<Session>,
    pid_ns: Arc<PidNamespace>,
}

struct SessionSidBindings {
    entries: Vec<SessionSidBinding>,
    pending: usize,
}

static SESSION_SID_BINDINGS: Once<Mutex<SessionSidBindings>> = Once::new();

fn session_sid_bindings() -> &'static Mutex<SessionSidBindings> {
    SESSION_SID_BINDINGS.call_once(|| {
        Mutex::new(SessionSidBindings {
            entries: Vec::new(),
            pending: 0,
        })
    })
}

pub(crate) struct PreparedSessionSidBinding {
    armed: bool,
}

pub(crate) fn prepare_session_sid_binding() -> AxResult<PreparedSessionSidBinding> {
    let mut bindings = session_sid_bindings().lock();
    let needed = bindings.pending.checked_add(1).ok_or(AxError::NoMemory)?;
    bindings
        .entries
        .try_reserve(needed)
        .map_err(|_| AxError::NoMemory)?;
    bindings.pending = needed;
    Ok(PreparedSessionSidBinding { armed: true })
}

impl PreparedSessionSidBinding {
    pub(crate) fn commit(mut self, session: Arc<Session>, pid_ns: Arc<PidNamespace>) {
        let mut bindings = session_sid_bindings().lock();
        debug_assert!(bindings.pending != 0);
        bindings.pending -= 1;
        bindings.entries.push(SessionSidBinding { session, pid_ns });
        self.armed = false;
    }
}

impl Drop for PreparedSessionSidBinding {
    fn drop(&mut self) {
        if self.armed {
            let mut bindings = session_sid_bindings().lock();
            debug_assert!(bindings.pending != 0);
            bindings.pending -= 1;
        }
    }
}

pub(crate) fn release_dead_session_sid_binding(
    session: &Arc<Session>,
    fallback: &Arc<PidNamespace>,
) {
    let pid_ns = {
        let mut bindings = session_sid_bindings().lock();
        bindings
            .entries
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.session, session))
            .map(|index| bindings.entries.swap_remove(index).pid_ns)
            .unwrap_or_else(|| fallback.clone())
    };
    pid_ns.release_reaped_process(session.sid());
}

static PROCESS_DOMAIN: Once<ProcessDomain> = Once::new();

/// Initializes the sole kernel-owned process domain before publishing init.
pub(crate) fn init_process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.try_call_once(|| ProcessDomain::try_new().map_err(process_error))
}

/// Returns the process domain after boot initialization.
pub(crate) fn process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.get().ok_or(AxError::BadState)
}

/// Reaps one zombie and releases its private/shared signal queues exactly once.
///
/// The core deliberately retains caller payloads after registry unlink because
/// pidfds and wait events may still own snapshot Arcs. The shared owner slot is
/// therefore taken only after a successful core reap and before any manager or
/// queue ownership is destroyed.
pub(crate) fn reap_process(process: &Process) -> AxResult<bool> {
    let snapshot = process.zombie_payload();
    // A session's SID is the leader's PID, but the session can outlive that
    // leader while another process group still belongs to it. Keep the
    // namespace binding through that lifetime so getsid can render its SID.
    let session = process.group().session();
    let group = process.group();
    let group_pgid = group.pgid();
    let session_sid = session.sid();
    let pid_ns = process.identity::<Arc<PidNamespace>>();
    let reaped = process_domain()?.reap(process).map_err(process_error)?;
    if !reaped {
        return Ok(false);
    }

    if let Some(pid_ns) = pid_ns {
        let group_live = group.is_live();
        if !session.is_live() {
            // The last process group left this session. This may release a
            // previously reaped leader's SID binding from a different group.
            release_dead_session_sid_binding(&session, pid_ns);
        }
        if !group_live {
            // PIDTYPE_PGID survives a reaped group leader while another
            // member remains in the group.  The final membership retirement
            // is the single edge that returns that namespace PID to the
            // allocator, including when the last member is not the leader.
            pid_ns.release_reaped_process(group_pgid);
        }
        if process.pid() != session_sid && (process.pid() != group_pgid || !group_live) {
            pid_ns.release_reaped_process(process.pid());
        } else if !session.is_live() {
            // The leader was also the last group member; the SID release
            // above is its ordinary PID release.
        } else {
            // Preserve the leader's PID binding while the session remains
            // live, matching Linux's PIDTYPE_SID lifetime.
        }
    }

    let snapshot = snapshot.ok_or(AxError::BadState)?;
    // The final thread remains visible as a zombie until this successful
    // wait/reap edge; release it here rather than when scheduler references
    // happen to disappear.
    super::ops::account_released_thread();
    retire_group_leader_signal_owner(&snapshot.reap_owner);
    Ok(true)
}

/// Resolves the raw ioprio of an unreaped zombie process.
pub(crate) fn zombie_ioprio(process: &Process) -> AxResult<u16> {
    ensure_authoritative_zombie(process)?;
    process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    // Linux drops the task's io_context while it exits. The unreaped
    // task_struct remains addressable, but ioprio_get observes the default
    // CLASS_NONE value rather than the last live priority.
    Ok(0)
}

/// Applies Linux's successful-but-no-op zombie ioprio setter semantics.
pub(crate) fn set_zombie_ioprio(process: &Process, priority: u16) -> AxResult<()> {
    let _ = priority;
    ensure_authoritative_zombie(process)?;
    process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    // Linux accepts a setter for an unreaped zombie, but there is no live
    // io_context left to mutate. Keep the authoritative zombie reachability
    // check above and otherwise make this a successful no-op.
    Ok(())
}

/// Updates the affinity retained for an authoritative unreaped zombie.  The
/// durable leader owner is also the reap serialization point: if reap wins
/// before we obtain it, this returns ESRCH; otherwise the update is ordered
/// before that reap edge.
pub(crate) fn set_zombie_affinity(process: &Process, affinity: AxCpuMask) -> AxResult<()> {
    ensure_authoritative_zombie(process)?;
    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    let owner = snapshot.reap_owner.lock();
    let scheduler = owner
        .as_ref()
        .and_then(|identity| identity.scheduler.as_ref())
        .ok_or(AxError::NoSuchProcess)?;
    scheduler.lock().affinity = affinity;
    Ok(())
}

/// Returns the PID namespace retained by an unreaped zombie process.
pub(crate) fn zombie_pid_ns(process: &Process) -> Option<Arc<PidNamespace>> {
    let current = process_domain().ok()?.registry().get(process.pid())?;
    if !core::ptr::eq(&*current, process) || !process.is_zombie() {
        return None;
    }
    process
        .zombie_payload()
        .and_then(|snapshot| snapshot.reap_owner.lock().as_ref()?.pid_ns.clone())
}

/// Returns the scheduler state retained by an authoritative unreaped zombie.
/// The live scheduler object is gone by this point, so this returns the last
/// successful scheduler transaction retained through the durable group-leader
/// identity.
pub(crate) fn zombie_scheduler_state(process: &Process) -> AxResult<ZombieSchedulerSnapshot> {
    ensure_authoritative_zombie(process)?;
    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    snapshot
        .reap_owner
        .lock()
        .as_ref()
        .and_then(|identity| identity.scheduler.as_ref())
        .map(|scheduler| *scheduler.lock())
        .ok_or(AxError::NoSuchProcess)
}

fn ensure_authoritative_zombie(process: &Process) -> AxResult<()> {
    let current = process_domain()?
        .registry()
        .get(process.pid())
        .ok_or(AxError::NoSuchProcess)?;
    if !core::ptr::eq(&*current, process) || !process.is_zombie() {
        return Err(AxError::NoSuchProcess);
    }
    Ok(())
}

/// Releases the endpoint and both private/shared pending queues retained by a
/// durable zombie payload. Taking the slot first makes duplicate release a
/// no-op and ensures an independently retained snapshot Arc cannot prolong
/// signal-queue accounting after the process has been reaped.
fn retire_group_leader_signal_owner(owner: &GroupLeaderSignalOwner) -> bool {
    let leader = owner.lock().take();
    if let Some(leader) = leader {
        leader
            .manager
            .retire_registration(leader.registration_tid, false);
        // Keep cleanup explicit even if a corrupted/stale registry entry made
        // exact retirement a no-op. The endpoint lifecycle normally guarantees
        // this second drain is empty.
        leader.manager.flush_pending();
        leader.manager.process().retire_pending();
        true
    } else {
        false
    }
}

pub(crate) const UTS_FIELD_LEN: usize = 64;
const PROC_NS_INO_BASE: u64 = 0x9_0000_0000;
static PROC_NS_ID: AtomicU64 = AtomicU64::new(1);

fn try_allocate_namespace_id(counter: &AtomicU64) -> AxResult<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| axerrno::LinuxError::ENOSPC.into())
}

fn try_allocate_proc_namespace_id() -> AxResult<u64> {
    try_allocate_namespace_id(&PROC_NS_ID)
}

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

/// Stable identity for one user-namespace object.
///
/// The identifier is allocated once and never reused. Consumers that need to
/// namespace internal state can therefore use this value without retaining an
/// `Arc<UserNamespace>` and extending the namespace lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct UserNamespaceId(u64);

impl UserNamespaceId {
    /// Stable scalar form for bounded kernel-owned accounting tables.  IDs
    /// are allocated once and never reused, so retaining this value does not
    /// extend the namespace lifetime or permit an ABA match.
    pub(crate) const fn into_raw(self) -> u64 {
        self.0
    }
}

fn try_increment_bounded(counter: &AtomicUsize, limit: usize) -> bool {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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
    /// The cgroup roots which were visible when this namespace was created.
    ///
    /// A cgroup namespace is not a second hierarchy: it is an immutable view
    /// rooted at the creator's live membership.  Retaining the opaque roots
    /// here keeps that view alive even if the task subsequently migrates, and
    /// lets the cgroup filesystem apply the same root to proc rendering and
    /// pathname visibility after setns().
    roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
}

/// Namespace-local mount attachment identity.
///
/// Mount topology remains owned by `crate::mounts`; this object is the
/// process-visible namespace handle and the hand-off point for the topology
/// implementation.  Keeping the identity separate from the global VFS
/// mechanisms lets clone/unshare/setns prepare an attach without publishing a
/// partially changed task namespace set.
pub(crate) struct MountNamespace {
    id: u64,
    owner_user_ns: Arc<UserNamespace>,
    topology: Arc<crate::mounts::MountTopology>,
    // FUSE/NFS mount-ID registrations are external provider state.  Keep the
    // clone-owned IDs with the namespace lifetime so dropping the final task
    // or nsfd cannot strand a provider registration after namespace teardown.
    provider_registrations: Mutex<Vec<crate::mounts::ClonedProviderMount>>,
}

/// Namespace IDs in statmount/listmount are references to live namespace
/// objects, not an alias for the caller's current mount graph.  Keep only
/// weak entries: `/proc/*/ns/mnt` and tasks remain the lifetime authority.
static MOUNT_NAMESPACE_REGISTRY: Lazy<Mutex<HashMap<u64, Weak<MountNamespace>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl MountNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        let topology = crate::mounts::MountTopology::try_bootstrap(id)?;
        let namespace = Arc::try_new(Self {
            id,
            owner_user_ns,
            topology,
            provider_registrations: Mutex::new(Vec::new()),
        })
        .map_err(|_| AxError::NoMemory)?;
        Self::register(&namespace)?;
        Ok(namespace)
    }

    pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        // A CLONE_NEWNS/UNSHARE_NEWNS paired with a new user namespace must
        // retain the copied mounts but lock their placement-sensitive state.
        // This is Linux's `lock_mnt_tree()` restriction, not a move_mount
        // policy bit; it therefore belongs to topology construction and is
        // preserved by later namespace clones.
        let lock_mounts = !Arc::ptr_eq(&self.owner_user_ns, &owner_user_ns);
        let mut topology = self.topology.try_prepare_clone_namespace(id, lock_mounts)?;
        let namespace = Arc::try_new(Self {
            id,
            owner_user_ns,
            topology: topology.topology(),
            provider_registrations: Mutex::new(Vec::new()),
        })
        .map_err(|_| AxError::NoMemory)?;
        // Provider registrations are external state.  Do not activate them
        // until the clone has an owned namespace object; if either this
        // activation or nsfs registry admission fails, the prepared clone's
        // Drop receipt removes every FUSE/NFS registration before the private
        // topology is released.
        topology.activate_provider_mounts()?;
        if let Err(error) = Self::register(&namespace) {
            return Err(error);
        }
        // No fallible work remains after registry admission. Transfer the
        // active registrations into the namespace lifetime before the
        // prepared receipt is dropped.
        *namespace.provider_registrations.lock() = topology.take_active_provider_mounts();
        Ok(namespace)
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }
    pub(crate) fn topology(&self) -> Arc<crate::mounts::MountTopology> {
        self.topology.clone()
    }

    pub(crate) fn root_location(&self) -> AxResult<axfs_ng_vfs::Location> {
        self.topology.root_location()
    }

    fn register(namespace: &Arc<Self>) -> AxResult<()> {
        let mut namespaces = MOUNT_NAMESPACE_REGISTRY.lock();
        namespaces.retain(|_, entry| entry.strong_count() != 0);
        namespaces.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        namespaces.insert(namespace.id(), Arc::downgrade(namespace));
        Ok(())
    }

    /// Resolves the stable namespace ID exposed by statmount/listmount and
    /// NS_GET_ID.  This is deliberately distinct from the nsfs inode number
    /// used by `/proc/*/ns/mnt`.
    pub(crate) fn lookup(id: u64) -> AxResult<Arc<Self>> {
        MOUNT_NAMESPACE_REGISTRY
            .lock()
            .get(&id)
            .and_then(Weak::upgrade)
            .ok_or(AxError::NotFound)
    }

    /// Snapshot every live mount namespace while the caller owns the mount
    /// namespace operation lock.  Propagation is a relationship between
    /// mount instances, not merely between tasks in the current namespace;
    /// keeping this lookup here prevents the mount layer from manufacturing a
    /// second namespace registry.
    pub(crate) fn live() -> AxResult<Vec<Arc<Self>>> {
        let mut namespaces = MOUNT_NAMESPACE_REGISTRY.lock();
        namespaces.retain(|_, entry| entry.strong_count() != 0);
        let mut live = Vec::new();
        live.try_reserve_exact(namespaces.len())
            .map_err(|_| AxError::NoMemory)?;
        for entry in namespaces.values() {
            if let Some(namespace) = entry.upgrade() {
                live.push(namespace);
            }
        }
        Ok(live)
    }
}

impl Drop for MountNamespace {
    fn drop(&mut self) {
        crate::mounts::unregister_cloned_provider_mounts(&mut *self.provider_registrations.lock());
    }
}

impl CgroupNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(
            owner_user_ns,
            crate::pseudofs::cgroup::root_namespace_roots()?,
        )
    }

    fn try_new(
        owner_user_ns: Arc<UserNamespace>,
        roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
    ) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            id,
            owner_user_ns,
            roots,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(
        _source: &Arc<Self>,
        owner_user_ns: Arc<UserNamespace>,
        roots: crate::pseudofs::cgroup::CgroupNamespaceRoots,
    ) -> AxResult<Arc<Self>> {
        // The new namespace root is selected from the caller's *current*
        // membership, not inherited from `self`.  A task may have migrated
        // after it entered this cgroup namespace, so copying `self.roots`
        // would incorrectly expose the old subtree to CLONE_NEWCGROUP.
        Self::try_new(owner_user_ns, roots)
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) fn roots(&self) -> &crate::pseudofs::cgroup::CgroupNamespaceRoots {
        &self.roots
    }
}

pub(crate) struct PidNamespace {
    id: u64,
    parent: Option<Arc<PidNamespace>>,
    /// Namespace-local PID bindings. A process owns one binding in its own
    /// namespace and every ancestor. The bindings are retained through zombie
    /// state and released only by `reap_process`.
    pids: SpinNoIrq<PidNamespacePids>,
    reaper_scope: Option<Arc<ProcessReaperScope>>,
    owner_user_ns: Arc<UserNamespace>,
}

/// Linux's initial PID namespace starts with the upstream `PID_MAX_DEFAULT`.
/// `pid_max` is an exclusive bound, so ordinary allocation uses
/// `1..pid_max`.
const PID_MAX_DEFAULT: Pid = 0x8000;

/// x86_64 Linux v6.18's `PID_MAX_LIMIT`.  Child PID namespaces are created
/// with this limit; the initial namespace may subsequently be constrained by
/// its `pid_max` sysctl.
const PID_MAX_LIMIT: Pid = 4 * 1024 * 1024;

/// Linux keeps the low PID region available during the first allocation pass
/// and restarts cyclic allocation from this point after reaching `pid_max`.
const RESERVED_PIDS: Pid = 300;
const PIDS_PER_CPU_DEFAULT: Pid = 1024;
const PIDS_PER_CPU_MIN: Pid = 8;

fn possible_cpu_count() -> Pid {
    (axhal::cpu_num().max(1).min(PID_MAX_LIMIT as usize)) as Pid
}

fn pid_max_min() -> Pid {
    (RESERVED_PIDS + 1).max(PIDS_PER_CPU_MIN.saturating_mul(possible_cpu_count()))
}

fn initial_pid_max() -> Pid {
    PID_MAX_DEFAULT
        .max(PIDS_PER_CPU_DEFAULT.saturating_mul(possible_cpu_count()))
        .min(PID_MAX_LIMIT)
}

struct PidNamespacePids {
    by_global: HashMap<Pid, Pid>,
    by_local: HashMap<Pid, Pid>,
    /// Exclusive local-PID ceiling for this particular namespace.
    pid_max: Pid,
    next: Pid,
    allocation_disabled: bool,
    pending_publications: usize,
}

impl PidNamespacePids {
    fn try_new(init_pid: Option<Pid>) -> AxResult<Self> {
        Self::try_new_with_pid_max(init_pid, PID_MAX_LIMIT)
    }

    fn try_new_with_pid_max(init_pid: Option<Pid>, pid_max: Pid) -> AxResult<Self> {
        if !(pid_max_min()..=PID_MAX_LIMIT).contains(&pid_max) {
            return Err(AxError::InvalidInput);
        }
        let mut pids = Self {
            by_global: HashMap::new(),
            by_local: HashMap::new(),
            pid_max,
            next: 1,
            allocation_disabled: false,
            pending_publications: 0,
        };
        if let Some(init_pid) = init_pid {
            pids.try_insert(init_pid, 1)?;
            pids.next = 2;
        }
        Ok(pids)
    }

    fn try_insert(&mut self, global_pid: Pid, local_pid: Pid) -> AxResult<()> {
        if !(1..self.pid_max).contains(&local_pid) {
            return Err(AxError::NoMemory);
        }
        self.by_global
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.by_local
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        if self.by_global.contains_key(&global_pid) || self.by_local.contains_key(&local_pid) {
            return Err(AxError::AlreadyExists);
        }
        self.by_global.insert(global_pid, local_pid);
        self.by_local.insert(local_pid, global_pid);
        Ok(())
    }

    fn try_reserve(&mut self, global_pid: Pid) -> AxResult<bool> {
        if self.by_global.contains_key(&global_pid) {
            return Ok(false);
        }
        // A `pid_max` sysctl write does not reset Linux's IDR cursor.  If
        // that cursor now lies beyond the new exclusive bound, the next
        // cyclic allocation wraps through the post-reserved PID range.
        let first = if self.next >= self.pid_max {
            RESERVED_PIDS
        } else {
            self.next.max(1)
        };
        let mut candidate = first;
        loop {
            if !self.by_local.contains_key(&candidate) {
                self.try_insert(global_pid, candidate)?;
                // Preserve the actual cyclic cursor, including the exclusive
                // bound itself.  A later `pid_max` increase must continue at
                // that former bound rather than prematurely wrap to 300.
                self.next = candidate + 1;
                return Ok(true);
            }
            candidate = if candidate + 1 == self.pid_max {
                RESERVED_PIDS
            } else {
                candidate + 1
            };
            if candidate == first {
                // Linux's PID allocator reports ID-space exhaustion as
                // EAGAIN. Keep allocation failures from `try_insert` above
                // as ENOMEM instead.
                return Err(LinuxError::EAGAIN.into());
            }
        }
    }

    fn try_reserve_exact(&mut self, global_pid: Pid, local_pid: Pid) -> AxResult<bool> {
        if !(1..self.pid_max).contains(&local_pid) {
            return Err(AxError::InvalidInput);
        }
        if let Some(existing) = self.by_global.get(&global_pid) {
            return (*existing == local_pid)
                .then_some(false)
                .ok_or(AxError::AlreadyExists);
        }
        self.try_insert(global_pid, local_pid)?;
        Ok(true)
    }

    fn reserve_publication(&mut self, global_pid: Pid, local_pid: Option<Pid>) -> AxResult<bool> {
        if self.allocation_disabled {
            return Err(AxError::NoMemory);
        }
        let allocated = match local_pid {
            Some(pid) => self.try_reserve_exact(global_pid, pid)?,
            None => self.try_reserve(global_pid)?,
        };
        self.pending_publications += 1;
        Ok(allocated)
    }

    fn pid_max(&self) -> Pid {
        self.pid_max
    }

    fn try_set_pid_max(&mut self, pid_max: Pid) -> AxResult<()> {
        if !(pid_max_min()..=PID_MAX_LIMIT).contains(&pid_max) {
            return Err(AxError::InvalidInput);
        }
        self.pid_max = pid_max;
        Ok(())
    }

    fn release(&mut self, global_pid: Pid) {
        let Some(local_pid) = self.by_global.remove(&global_pid) else {
            return;
        };
        let removed = self.by_local.remove(&local_pid);
        debug_assert_eq!(removed, Some(global_pid));
    }
}

/// Rollback guard for a pre-publication process PID binding. Commit leaves the
/// binding owned by the namespace until successful process reap.
pub(crate) struct PidNamespaceReservation {
    namespace: Arc<PidNamespace>,
    global_pid: Pid,
    allocated_here: bool,
    parent: Option<Box<PidNamespaceReservation>>,
    committed: bool,
}

impl PidNamespaceReservation {
    pub(crate) fn commit(mut self) {
        self.commit_recursive();
    }

    fn commit_recursive(&mut self) {
        self.committed = true;
        if let Some(parent) = self.parent.as_mut() {
            parent.commit_recursive();
        }
    }
}

impl Drop for PidNamespaceReservation {
    fn drop(&mut self) {
        let mut pids = self.namespace.pids.lock();
        if !self.committed && self.allocated_here {
            pids.release(self.global_pid);
        }
        pids.pending_publications -= 1;
    }
}

impl PidNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(None, None, None, owner_user_ns)
    }

    pub(crate) fn try_new_root_with_reaper_scope(
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(None, None, Some(reaper_scope), owner_user_ns)
    }

    fn try_new(
        parent: Option<Arc<Self>>,
        init_pid: Option<Pid>,
        reaper_scope: Option<Arc<ProcessReaperScope>>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let id = try_allocate_proc_namespace_id()?;
        let nested = parent.is_some();
        Arc::try_new(Self {
            id,
            parent,
            pids: SpinNoIrq::new(PidNamespacePids::try_new_with_pid_max(
                init_pid,
                if nested {
                    PID_MAX_LIMIT
                } else {
                    initial_pid_max()
                },
            )?),
            reaper_scope,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(
        self: &Arc<Self>,
        init_pid: Pid,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(Some(self.clone()), Some(init_pid), None, owner_user_ns)
    }

    /// `unshare(CLONE_NEWPID)` changes only the PID namespace inherited by
    /// future children.  The calling task remains in its current namespace,
    /// so the deferred child namespace intentionally starts without PID 1.
    pub(crate) fn try_fork_for_children(
        self: &Arc<Self>,
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(Some(self.clone()), None, Some(reaper_scope), owner_user_ns)
    }

    pub(crate) fn try_fork_with_reaper_scope(
        self: &Arc<Self>,
        init_pid: Pid,
        owner_user_ns: Arc<UserNamespace>,
        reaper_scope: Arc<ProcessReaperScope>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(
            Some(self.clone()),
            Some(init_pid),
            Some(reaper_scope),
            owner_user_ns,
        )
    }

    pub(crate) fn parent(&self) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    pub(crate) fn reaper_scope(&self) -> Option<Arc<ProcessReaperScope>> {
        self.reaper_scope.clone()
    }

    pub(crate) fn has_no_init(&self) -> bool {
        self.pids.lock().by_global.is_empty()
    }

    /// Close allocation under the same lock as reservation, then let every
    /// already admitted clone publish or roll back before the exit walk.
    pub(crate) fn disable_allocation(&self) {
        self.pids.lock().allocation_disabled = true;
    }

    pub(crate) fn has_pending_publications(&self) -> bool {
        self.pids.lock().pending_publications != 0
    }

    /// Linux disables PID allocation when a namespace's child reaper exits.
    /// The process core retains whether its scope init was ever bound, so a
    /// reaped init cannot make a dead namespace look newly created. The latter
    /// deliberately reports ENOMEM, matching alloc_pid()'s long-standing
    /// externally visible result rather than leaking a core NotLive detail.
    fn child_reaper_allows_new_processes(&self) -> bool {
        match self.reaper_scope() {
            None => true,
            Some(scope) => match scope.init_process() {
                // A CLONE_NEWPID/unshare first child has reserved a namespace
                // but has not yet atomically published its scope init.
                None => !scope.was_initialized(),
                Some(init) => init.is_live(),
            },
        }
    }

    /// Last PID allocated in this namespace, including subsequently reaped tasks.
    pub(crate) fn last_allocated_pid(&self) -> Pid {
        self.pids.lock().next.saturating_sub(1)
    }

    /// The namespace-local, exclusive PID allocation bound.
    pub(crate) fn pid_max(&self) -> Pid {
        self.pids.lock().pid_max()
    }

    /// Applies a validated namespace-local `pid_max` sysctl value. Existing
    /// bindings remain valid when the ceiling is lowered, as on Linux; only
    /// future automatic or explicit allocations are constrained by it.
    pub(crate) fn try_set_pid_max(&self, pid_max: Pid) -> AxResult<()> {
        self.pids.lock().try_set_pid_max(pid_max)
    }

    /// Reserves one local PID in this namespace and all of its ancestors.
    /// The returned guard must be committed only once the child has reached
    /// process publication; otherwise it restores every newly allocated slot.
    pub(crate) fn reserve_process(
        self: &Arc<Self>,
        global_pid: Pid,
    ) -> AxResult<PidNamespaceReservation> {
        if !self.child_reaper_allows_new_processes() {
            return Err(AxError::NoMemory);
        }
        let parent = self
            .parent()
            .map(|parent| parent.reserve_process(global_pid))
            .transpose()?
            .map(Box::new);
        let allocated_here = self.pids.lock().reserve_publication(global_pid, None)?;
        Ok(PidNamespaceReservation {
            namespace: self.clone(),
            global_pid,
            allocated_here,
            parent,
            committed: false,
        })
    }

    /// Reserves a clone3 `set_tid` vector from this namespace out through its
    /// ancestors. The vector is ordered innermost-to-outermost, and the guard
    /// releases every acquired slot if any later namespace rejects it.
    pub(crate) fn reserve_process_with_ids(
        self: &Arc<Self>,
        global_pid: Pid,
        requested: &[Pid],
        actor: &Cred,
    ) -> AxResult<PidNamespaceReservation> {
        if !self.child_reaper_allows_new_processes() {
            return Err(AxError::NoMemory);
        }
        fn depth(namespace: &PidNamespace) -> usize {
            namespace.parent().map_or(1, |parent| depth(&parent) + 1)
        }
        if requested.len() > depth(self) {
            return Err(AxError::InvalidInput);
        }

        fn reserve(
            namespace: &Arc<PidNamespace>,
            global_pid: Pid,
            requested: &[Pid],
            actor: &Cred,
            level: usize,
        ) -> AxResult<PidNamespaceReservation> {
            // Explicit clone3 IDs must not bypass a dead ancestor's reaper.
            if !namespace.child_reaper_allows_new_processes() {
                return Err(AxError::NoMemory);
            }
            let allocated_here = if let Some(&local_pid) = requested.get(level) {
                {
                    let pids = namespace.pids.lock();
                    if !(1..pids.pid_max()).contains(&local_pid) {
                        return Err(AxError::InvalidInput);
                    }
                    // A namespace can only receive a non-init PID after its
                    // PID 1 exists. Check this before privilege so malformed
                    // namespace state does not become an authorization oracle.
                    if local_pid != 1 && !pids.by_local.contains_key(&1) {
                        return Err(AxError::InvalidInput);
                    }
                }
                if !crate::task::ns_capable(
                    actor,
                    namespace.owner_user_ns(),
                    linux_raw_sys::general::CAP_CHECKPOINT_RESTORE,
                ) && !crate::task::ns_capable(
                    actor,
                    namespace.owner_user_ns(),
                    linux_raw_sys::general::CAP_SYS_ADMIN,
                ) {
                    return Err(AxError::OperationNotPermitted);
                }
                namespace
                    .pids
                    .lock()
                    .reserve_publication(global_pid, Some(local_pid))?
            } else {
                namespace
                    .pids
                    .lock()
                    .reserve_publication(global_pid, None)?
            };
            let mut reservation = PidNamespaceReservation {
                namespace: namespace.clone(),
                global_pid,
                allocated_here,
                parent: None,
                committed: false,
            };
            if let Some(parent) = namespace.parent() {
                reservation.parent = Some(Box::new(reserve(
                    &parent,
                    global_pid,
                    requested,
                    actor,
                    level + 1,
                )?));
            }
            Ok(reservation)
        }

        reserve(self, global_pid, requested, actor, 0)
    }

    /// Releases the namespace PID binding after its final identity owner has
    /// gone. This is normally authoritative reap; `setsid` also uses it for
    /// an already reaped session leader when the final group leaves its
    /// session. Exit intentionally does not call this: zombies retain numeric
    /// identity until wait/autoreap consumes them.
    pub(crate) fn release_reaped_process(&self, global_pid: Pid) {
        self.pids.lock().release(global_pid);
        if let Some(parent) = self.parent() {
            parent.release_reaped_process(global_pid);
        }
    }

    /// Releases a non-leader thread ID after its core membership has been
    /// unlinked. Process IDs deliberately use `release_reaped_process` so a
    /// zombie remains numerically addressable until wait/autoreap.
    pub(crate) fn release_exited_thread(&self, global_tid: Pid) {
        self.pids.lock().release(global_tid);
        if let Some(parent) = self.parent() {
            parent.release_exited_thread(global_tid);
        }
    }

    /// Returns whether `target` is this namespace or one of its descendants.
    /// A caller can address tasks in descendants, but never tasks in an
    /// unrelated or ancestor PID namespace.
    pub(crate) fn contains(&self, target: &Arc<Self>) -> bool {
        let mut candidate = Some(target.clone());
        while let Some(namespace) = candidate {
            if core::ptr::eq(self, &*namespace) {
                return true;
            }
            candidate = namespace.parent();
        }
        false
    }

    /// Renders a global process/thread identifier in this caller namespace,
    /// returning `None` when the target namespace is not visible here.
    pub(crate) fn visible_pid_for(
        &self,
        target_namespace: &Arc<Self>,
        global_pid: Pid,
    ) -> Option<Pid> {
        self.contains(target_namespace)
            .then(|| self.visible_pid_checked(global_pid))
            .flatten()
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) fn visible_pid(&self, global_pid: Pid) -> Pid {
        // Kernel-created processes always hold a binding until reap. Retain a
        // global fallback for synthetic/unit-test identities that predate the
        // allocator and have not been admitted through the kernel lifecycle.
        self.pids
            .lock()
            .by_global
            .get(&global_pid)
            .copied()
            .unwrap_or(global_pid)
    }

    /// Strict syscall-visible rendering: absence of a namespace binding is
    /// not a global PID and must be rendered as zero by pid_vnr-style users.
    pub(crate) fn visible_pid_checked(&self, global_pid: Pid) -> Option<Pid> {
        self.pids.lock().by_global.get(&global_pid).copied()
    }

    /// Resolves a positive PID visible in this namespace to its kernel-wide
    /// identity. Unlike [`Self::visible_pid`], this never invents a fallback:
    /// syscall lookup must not turn an unseen namespace-local number into a
    /// global task lookup.
    pub(crate) fn resolve_visible_pid(&self, visible_pid: Pid) -> Option<Pid> {
        (visible_pid != 0)
            .then(|| self.pids.lock().by_local.get(&visible_pid).copied())
            .flatten()
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
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            _admission: admission,
            id,
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
        let id = try_allocate_proc_namespace_id()?;
        Arc::try_new(Self {
            _admission: admission,
            id,
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

    /// Returns the stable, non-owning identity of this namespace.
    pub(crate) const fn identity(&self) -> UserNamespaceId {
        UserNamespaceId(self.id)
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

    // Named after Linux's `from_kuid_munged(struct user_namespace *, kuid_t)`,
    // where the namespace is the subject and the kuid is the operand. Renaming
    // to satisfy the `from_*` convention would break that correspondence.
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn from_kuid_munged(&self, uid: Kuid) -> u32 {
        self.kernel_uid_to_user(uid)
            .map(UserUid::into_raw)
            .unwrap_or(USER_NAMESPACE_OVERFLOW_ID)
    }

    #[allow(clippy::wrong_self_convention)]
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

impl tk_linux_cred::UserNamespaceView for UserNamespace {
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

impl tk_linux_cred::ExecUserNamespaceView for UserNamespace {
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
    id: u64,
    state: SpinNoIrq<UtsState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl UtsNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(init_uts_state()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }

    pub(crate) fn nodename(&self) -> AxResult<Vec<u8>> {
        let state = *self.state.lock();
        Ok(state.nodename[..state.nodename_len].to_vec())
    }

    pub(crate) fn domainname(&self) -> AxResult<Vec<u8>> {
        let state = *self.state.lock();
        Ok(state.domainname[..state.domainname_len].to_vec())
    }

    /// Snapshot both UTS name fields under one lock acquisition.
    ///
    /// `uname(2)` observes the namespace state as one unit, so its nodename
    /// and domainname must not come from separate writer generations.
    pub(crate) fn names_snapshot(&self) -> ([u8; UTS_FIELD_LEN], [u8; UTS_FIELD_LEN]) {
        let state = *self.state.lock();
        (state.nodename, state.domainname)
    }

    pub(crate) fn set_nodename(&self, value: &[u8]) -> AxResult<()> {
        self.state.lock().set_nodename(value);
        Ok(())
    }

    pub(crate) fn set_domainname(&self, value: &[u8]) -> AxResult<()> {
        self.state.lock().set_domainname(value);
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct TimeNamespaceState {
    monotonic_offset_ns: i64,
    boottime_offset_ns: i64,
}

pub(crate) struct TimeNamespace {
    id: u64,
    state: SpinNoIrq<TimeNamespaceState>,
    owner_user_ns: Arc<UserNamespace>,
}

impl TimeNamespace {
    pub(crate) fn try_new_root(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(TimeNamespaceState::default()),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_fork(&self, owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        let state = *self.state.lock();
        Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            state: SpinNoIrq::new(state),
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
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
    id: u64,
    stack: Arc<NetStack>,
    owner_user_ns: Arc<UserNamespace>,
}

impl NetworkNamespace {
    pub(crate) fn try_new(
        stack: Arc<NetStack>,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        let namespace = Arc::try_new(Self {
            id: try_allocate_proc_namespace_id()?,
            stack,
            owner_user_ns,
        })
        .map_err(|_| AxError::NoMemory)?;
        // The pre-protocol seam is the XDP ingress point: a socket binding is
        // never consulted here.  Only a program attached to this exact
        // namespace/interface can consume the frame through a typed XSKMAP
        // redirect target.
        let weak = Arc::downgrade(&namespace);
        namespace
            .stack
            .set_pre_protocol_hook(Some(Arc::new(move |ifindex, packet| {
                let namespace = weak.upgrade().ok_or(AxError::BadState)?;
                let Some(program) = crate::bpf::xdp_program_snapshot(&namespace, ifindex) else {
                    return Ok(PacketAction::Pass);
                };
                let terminal = program.run_xdp(
                    crate::bpf::helpers::XdpContext {
                        data: 0,
                        data_end: packet.len().try_into().unwrap_or(u32::MAX),
                        data_meta: 0,
                        ingress_ifindex: ifindex,
                        rx_queue_index: 0,
                        egress_ifindex: 0,
                    },
                    packet,
                )?;
                match terminal {
                    crate::bpf::helpers::XdpExecutionResult::Pass => Ok(PacketAction::Pass),
                    crate::bpf::helpers::XdpExecutionResult::Redirect(redirect) => {
                        // The endpoint owns a retained UMEM capability; router
                        // bytes are copied only after the program selected this
                        // exact XSKMAP slot.  A full/invalid target is a failed
                        // redirect, never a fallback to normal protocol input.
                        if redirect.target.accepts_xdp_redirect(&namespace, ifindex)
                            && redirect.target.redirect_packet(packet, 0).unwrap_or(false)
                        {
                            Ok(PacketAction::RedirectConsumed)
                        } else {
                            match redirect.flags & 0x3 {
                                2 => Ok(PacketAction::Pass),
                                3 => Ok(PacketAction::Tx),
                                _ => Ok(PacketAction::Drop),
                            }
                        }
                    }
                    crate::bpf::helpers::XdpExecutionResult::Tx => Ok(PacketAction::Tx),
                    crate::bpf::helpers::XdpExecutionResult::Aborted
                    | crate::bpf::helpers::XdpExecutionResult::Drop
                    | crate::bpf::helpers::XdpExecutionResult::RedirectMiss
                    | crate::bpf::helpers::XdpExecutionResult::Invalid(_) => Ok(PacketAction::Drop),
                }
            })));
        let weak = Arc::downgrade(&namespace);
        namespace
            .stack
            .set_packet_hook(Some(Arc::new(move |context: &PacketContext, packet| {
                let namespace = weak.upgrade().ok_or(AxError::BadState)?;
                let hook = match context.point {
                    PacketHookPoint::Prerouting => crate::file::netlink::NftHook::Prerouting,
                    PacketHookPoint::Input => crate::file::netlink::NftHook::Input,
                    PacketHookPoint::Forward => crate::file::netlink::NftHook::Forward,
                    PacketHookPoint::LocalOutput => crate::file::netlink::NftHook::Output,
                    PacketHookPoint::Postrouting => crate::file::netlink::NftHook::Postrouting,
                };
                let ipt_hook = match context.point {
                    PacketHookPoint::Prerouting => 0,
                    PacketHookPoint::Input => 1,
                    PacketHookPoint::Forward => 2,
                    PacketHookPoint::LocalOutput => 3,
                    PacketHookPoint::Postrouting => 4,
                };
                crate::syscall::iptables_hook_verdict(&namespace, ipt_hook)?;
                // `nft_packet_hook` owns the ordered NF/BPF traversal.  Keeping
                // BPF dispatch there prevents a namespace hook from executing a
                // mutating program twice at one policy seam.
                crate::file::netlink::nft_packet_hook(&namespace, hook, packet)?;
                Ok(PacketAction::Pass)
            })));
        #[cfg(feature = "bpf")]
        {
            let weak = Arc::downgrade(&namespace);
            namespace
                .stack
                .set_packet_defrag_query(Some(Arc::new(move |point, packet| {
                    let Some(namespace) = weak.upgrade() else {
                        return false;
                    };
                    let hook = match point {
                        // Linux's BPF netfilter IP_DEFRAG is meaningful before an
                        // ingress NF hook; other seams already receive a complete
                        // local/forwarded packet in this stack.
                        PacketHookPoint::Prerouting => crate::file::bpf::BpfNetworkHook::Prerouting,
                        PacketHookPoint::Input => crate::file::bpf::BpfNetworkHook::Input,
                        PacketHookPoint::Forward => crate::file::bpf::BpfNetworkHook::Forward,
                        PacketHookPoint::LocalOutput => crate::file::bpf::BpfNetworkHook::Output,
                        PacketHookPoint::Postrouting => {
                            crate::file::bpf::BpfNetworkHook::Postrouting
                        }
                    };
                    crate::bpf::network_packet_defrag_required(&namespace, hook, packet)
                })));
        }
        Ok(namespace)
    }

    pub(crate) fn try_new_loopback_only(owner_user_ns: Arc<UserNamespace>) -> AxResult<Arc<Self>> {
        Self::try_new(NetStack::try_new_loopback_only()?, owner_user_ns)
    }

    pub(crate) fn try_new_network_namespace(
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Self::try_new(NetStack::try_new_network_namespace()?, owner_user_ns)
    }

    pub(crate) fn stack(&self) -> &Arc<NetStack> {
        &self.stack
    }

    pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn proc_inode(&self) -> u64 {
        PROC_NS_INO_BASE + self.id.saturating_mul(8)
    }
}

/// The complete Linux namespace attachment of one task.
///
/// Namespace-changing syscalls construct a complete replacement first and
/// exchange it only at commit, so observers cannot see (for example) a new
/// user namespace paired with an old IPC or mount namespace.
#[derive(Clone)]
pub(crate) struct NamespaceProxy {
    user: Arc<UserNamespace>,
    pid: Arc<PidNamespace>,
    pid_for_children: Arc<PidNamespace>,
    mount: Arc<MountNamespace>,
    ipc: Arc<IpcNamespace>,
    net: Arc<NetworkNamespace>,
    cgroup: Arc<CgroupNamespace>,
    uts: Arc<UtsNamespace>,
    time: Arc<TimeNamespace>,
    time_for_children: Arc<TimeNamespace>,
}

impl NamespaceProxy {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        user: Arc<UserNamespace>,
        pid: Arc<PidNamespace>,
        mount: Arc<MountNamespace>,
        ipc: Arc<IpcNamespace>,
        net: Arc<NetworkNamespace>,
        cgroup: Arc<CgroupNamespace>,
        uts: Arc<UtsNamespace>,
        time: Arc<TimeNamespace>,
    ) -> AxResult<Self> {
        // Namespace objects retain their own owner.  Mixed-owner bundles are
        // valid: CLONE_NEWUSER changes the caller's credential namespace
        // while inherited mount/net/ipc objects remain owned by the namespace
        // which created them.
        Ok(Self {
            user,
            pid: pid.clone(),
            pid_for_children: pid,
            mount,
            ipc,
            net,
            cgroup,
            uts,
            time: time.clone(),
            time_for_children: time,
        })
    }

    pub(crate) fn user(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn pid(&self) -> Arc<PidNamespace> {
        self.pid.clone()
    }
    pub(crate) fn pid_for_children(&self) -> Arc<PidNamespace> {
        self.pid_for_children.clone()
    }
    pub(crate) fn mount(&self) -> Arc<MountNamespace> {
        self.mount.clone()
    }
    pub(crate) fn ipc(&self) -> Arc<IpcNamespace> {
        self.ipc.clone()
    }
    pub(crate) fn net(&self) -> Arc<NetworkNamespace> {
        self.net.clone()
    }
    pub(crate) fn cgroup(&self) -> Arc<CgroupNamespace> {
        self.cgroup.clone()
    }
    pub(crate) fn uts(&self) -> Arc<UtsNamespace> {
        self.uts.clone()
    }
    pub(crate) fn time(&self) -> Arc<TimeNamespace> {
        self.time.clone()
    }
    pub(crate) fn time_for_children(&self) -> Arc<TimeNamespace> {
        self.time_for_children.clone()
    }

    pub(crate) fn replace_uts(&mut self, value: Arc<UtsNamespace>) {
        self.uts = value;
    }
    pub(crate) fn replace_user(&mut self, value: Arc<UserNamespace>) {
        self.user = value;
    }
    pub(crate) fn replace_pid(&mut self, value: Arc<PidNamespace>) {
        self.pid = value;
    }
    pub(crate) fn replace_pid_for_children(&mut self, value: Arc<PidNamespace>) {
        self.pid_for_children = value;
    }
    pub(crate) fn replace_time(&mut self, value: Arc<TimeNamespace>) {
        self.time = value.clone();
        self.time_for_children = value;
    }
    pub(crate) fn replace_time_for_children(&mut self, value: Arc<TimeNamespace>) {
        self.time_for_children = value;
    }
    pub(crate) fn replace_mount(&mut self, value: Arc<MountNamespace>) {
        self.mount = value;
    }
    pub(crate) fn replace_ipc(&mut self, value: Arc<IpcNamespace>) {
        self.ipc = value;
    }
    pub(crate) fn replace_net(&mut self, value: Arc<NetworkNamespace>) {
        self.net = value;
    }
    pub(crate) fn replace_cgroup(&mut self, value: Arc<CgroupNamespace>) {
        self.cgroup = value;
    }
}

/// Prepared namespace aggregate exchange. Every allocation and authority
/// check happens before this token is created; it is committed into the
/// calling task's namespace slot.
pub(crate) struct PreparedNamespaceProxyReplacement {
    pub(crate) replacement: NamespaceProxy,
}

/// Task-owned Linux `sem_undo` list. `CLONE_SYSVSEM` shares this Arc across
/// task owners; ordinary clone snapshots it. The final owner applies its
/// adjustments to the IPC namespace that owns the semaphore arrays.
pub(crate) struct SemUndoState {
    ipc_ns: Arc<IpcNamespace>,
    undo: Mutex<Option<SemUndo>>,
}

impl SemUndoState {
    pub(crate) fn try_new(ipc_ns: Arc<IpcNamespace>) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            ipc_ns,
            undo: Mutex::new(Some(SemUndo::new())),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn try_clone_for(
        ipc_ns: Arc<IpcNamespace>,
        source: &Arc<Self>,
    ) -> AxResult<Arc<Self>> {
        let undo = source
            .undo
            .lock()
            .as_ref()
            .map(SemUndo::try_clone)
            .transpose()?;
        Arc::try_new(Self {
            ipc_ns,
            undo: Mutex::new(undo),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub(crate) fn undo(&self) -> &Mutex<Option<SemUndo>> {
        &self.undo
    }

    pub(crate) fn apply_on_final_exit(&self) {
        let Some(mut undo) = self.undo.lock().take() else {
            return;
        };
        apply_sem_undo(self.ipc_ns.sem_manager(), &mut undo);
    }
}

impl PreparedNamespaceProxyReplacement {
    pub(crate) fn commit(self, thread: &super::thread::Thread) {
        let old = {
            let _publication = super::fs_context_publication();
            self.commit_under_publication(thread)
        };
        drop(old);
    }

    /// Exchanges only the namespace pointer while the caller already owns the
    /// publication gate.  Resource retirement is intentionally returned to
    /// the caller: dropping a proxy can cascade into VFS/IPC teardown and
    /// must never occur inside the IRQ/preemption-off publication region.
    pub(crate) fn commit_under_publication(self, thread: &super::thread::Thread) -> NamespaceProxy {
        core::mem::replace(&mut *thread.namespaces.lock(), self.replacement)
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
    /// Preferred allocation node for `MPOL_BIND` and `MPOL_PREFERRED_MANY`.
    ///
    /// `None` is Linux's `NUMA_NO_NODE`: no home-node preference has been
    /// configured for this policy.
    pub home_node: Option<usize>,
}

impl Mempolicy {
    pub const fn new(mode: u32, nodemask: usize) -> Self {
        Self {
            mode,
            nodemask,
            home_node: None,
        }
    }

    pub const fn with_home_node(mut self, home_node: usize) -> Self {
        self.home_node = Some(home_node);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    /// Linux represents an `mbind(MPOL_DEFAULT)` range by removing its VMA
    /// policy rather than by recording a synthetic default-policy interval.
    fn bind_range(&mut self, start: usize, end: usize, policy: Mempolicy) {
        self.remove_range(start, end);
        if policy.mode != linux_raw_sys::mempolicy::MPOL_DEFAULT as u32 {
            self.ranges.push(MempolicyRange { start, end, policy });
        }
    }

    fn policy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map(|range| range.policy)
    }

    /// Applies a home node to policy intervals intersecting `start..end`.
    ///
    /// `mbind` policy intervals can be narrower than an address-space VMA, so
    /// this operates on the interval topology rather than treating the VMA's
    /// first policy as covering the whole VMA.  The sorted traversal provides
    /// Linux's address-order partial-update behavior if a later policy is not
    /// supported by `set_mempolicy_home_node`.
    fn try_set_home_node_in_range(
        old_ranges: &[MempolicyRange],
        start: usize,
        end: usize,
        home_node: usize,
    ) -> AxResult<(Vec<MempolicyRange>, bool, Option<axerrno::LinuxError>)> {
        let capacity = old_ranges.len().checked_mul(3).ok_or(AxError::NoMemory)?;
        let mut new_ranges = Vec::new();
        new_ranges
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        let mut old_ranges_sorted = Vec::new();
        old_ranges_sorted
            .try_reserve_exact(old_ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        old_ranges_sorted.extend(old_ranges.iter().copied());
        let mut old_ranges = old_ranges_sorted;
        old_ranges.sort_unstable_by_key(|range| range.start);
        let mut updated = false;

        let mut ranges = old_ranges.into_iter();
        while let Some(range) = ranges.next() {
            if range.end <= start || range.start >= end {
                new_ranges.push(range);
                continue;
            }
            if range.policy.mode != linux_raw_sys::mempolicy::MPOL_BIND as u32
                && range.policy.mode != linux_raw_sys::mempolicy::MPOL_PREFERRED_MANY as u32
            {
                new_ranges.push(range);
                new_ranges.extend(ranges);
                return Ok((new_ranges, updated, Some(axerrno::LinuxError::EOPNOTSUPP)));
            }

            let overlap_start = range.start.max(start);
            let overlap_end = range.end.min(end);
            if range.start < overlap_start {
                new_ranges.push(MempolicyRange {
                    start: range.start,
                    end: overlap_start,
                    policy: range.policy,
                });
            }
            let mut policy = range.policy;
            policy.home_node = Some(home_node);
            new_ranges.push(MempolicyRange {
                start: overlap_start,
                end: overlap_end,
                policy,
            });
            if overlap_end < range.end {
                new_ranges.push(MempolicyRange {
                    start: overlap_end,
                    end: range.end,
                    policy: range.policy,
                });
            }
            updated = true;
        }
        Ok((new_ranges, updated, None))
    }
}

/// Monotonically renewed token for one published group-leader identity.
///
/// A successful exec publishes a new token even when the executor already
/// owns the same credential slot and private signal endpoint.  Consumers that
/// authorize an operation before acquiring their final lifecycle gate can use
/// it to reject that otherwise pointer-identical exec handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupLeaderIdentityToken(u64);

/// One coherent, pinned view of the group-leader identity.
///
/// The token is deliberately paired with both owners: an exec can retain the
/// same endpoint, and leader exec can retain the same credential slot.  The
/// token therefore supplies the generation edge those pointer comparisons
/// cannot represent by themselves.
#[derive(Clone)]
pub(crate) struct GroupLeaderIdentitySnapshot {
    token: GroupLeaderIdentityToken,
    credential: Arc<Cred>,
    signal: Arc<ThreadSignalManager>,
}

impl GroupLeaderIdentitySnapshot {
    pub(crate) fn token(&self) -> GroupLeaderIdentityToken {
        self.token
    }

    pub(crate) fn credential(&self) -> &Arc<Cred> {
        &self.credential
    }

    pub(crate) fn signal(&self) -> &Arc<ThreadSignalManager> {
        &self.signal
    }
}

/// Persistent binding to the credential slot and private signal endpoint that
/// currently own Linux thread-group-leader identity.
struct GroupLeaderIdentityBinding {
    current: SpinNoIrq<Arc<CredentialSlot>>,
    signal: GroupLeaderSignalOwner,
    landlock: Arc<SpinNoIrq<LandlockDomain>>,
    /// Starts nonzero and is renewed under the current/signal publication
    /// locks by every exec handoff.  It is read only while those locks are
    /// held, so a snapshot can never pair a new token with old owners.
    identity_token: AtomicU64,
    /// The process PID namespace copied into the durable owner identity.
    pid_ns: Option<Arc<PidNamespace>>,
    /// Changes with each replacement of the private endpoint that owns the
    /// process's group-leader identity. Access is serialized with `current`
    /// and `signal`, which makes a handoff and its scheduler reseed one
    /// durable binding transaction.
    scheduler_identity_epoch: SpinNoIrq<u64>,
    scheduler_identity_token: SpinNoIrq<u64>,
    scheduler: Arc<SpinNoIrq<ZombieSchedulerSnapshot>>,
}

impl GroupLeaderIdentityBinding {
    fn try_new(initial: Arc<CredentialSlot>) -> AxResult<Self> {
        Self::try_new_with_pid_ns(initial, None)
    }

    fn try_new_with_pid_ns(
        initial: Arc<CredentialSlot>,
        pid_ns: Option<Arc<PidNamespace>>,
    ) -> AxResult<Self> {
        Ok(Self {
            current: SpinNoIrq::new(initial),
            signal: Arc::try_new(SpinNoIrq::new(None)).map_err(|_| AxError::NoMemory)?,
            landlock: Arc::try_new(SpinNoIrq::new(LandlockDomain::default()))
                .map_err(|_| AxError::NoMemory)?,
            identity_token: AtomicU64::new(1),
            pid_ns,
            scheduler_identity_epoch: SpinNoIrq::new(0),
            scheduler_identity_token: SpinNoIrq::new(0),
            scheduler: Arc::try_new(SpinNoIrq::new(ZombieSchedulerSnapshot::default()))
                .map_err(|_| AxError::NoMemory)?,
        })
    }

    fn current_cred(&self) -> Arc<Cred> {
        let slot = self.current.lock().clone();
        slot.current()
    }

    fn landlock_domain(&self) -> LandlockDomain {
        self.landlock.lock().clone()
    }
    fn replace_landlock_domain(&self, domain: LandlockDomain) {
        *self.landlock.lock() = domain;
    }

    fn bind_initial_signal(
        &self,
        registration_tid: Pid,
        signal: Arc<ThreadSignalManager>,
    ) -> AxResult<()> {
        let mut current = self.signal.lock();
        if current.is_some() {
            return Err(AxError::BadState);
        }
        *current = Some(GroupLeaderSignalIdentity::with_pid_namespace_and_scheduler(
            registration_tid,
            signal,
            self.pid_ns.clone(),
            self.scheduler.clone(),
            self.landlock.clone(),
        ));
        Ok(())
    }

    fn current_cred_and_signal(&self) -> AxResult<(Arc<Cred>, Arc<ThreadSignalManager>)> {
        let snapshot = self.identity_snapshot()?;
        Ok((snapshot.credential, snapshot.signal))
    }

    fn identity_snapshot(&self) -> AxResult<GroupLeaderIdentitySnapshot> {
        let current = self.current.lock();
        let signal_guard = self.signal.lock();
        let slot = current.clone();
        let signal = signal_guard
            .as_ref()
            .map(|identity| identity.manager.clone())
            .ok_or(AxError::BadState)?;
        let token = GroupLeaderIdentityToken(self.identity_token.load(Ordering::Acquire));
        drop(signal_guard);
        drop(current);
        Ok(GroupLeaderIdentitySnapshot {
            token,
            credential: slot.current(),
            signal,
        })
    }

    fn signal_matches(&self, expected: &Arc<ThreadSignalManager>) -> bool {
        self.signal
            .lock()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.manager, expected))
    }

    fn identity_snapshot_matches(&self, expected: &GroupLeaderIdentitySnapshot) -> bool {
        let current = self.current.lock();
        let signal = self.signal.lock();
        let matches = self.identity_token.load(Ordering::Acquire) == expected.token.0
            && Arc::ptr_eq(&current.current(), &expected.credential)
            && signal
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(&current.manager, &expected.signal));
        drop(signal);
        drop(current);
        matches
    }

    fn signal_owner(&self) -> GroupLeaderSignalOwner {
        self.signal.clone()
    }

    fn publish_affinity_snapshot(&self, registration_tid: Pid, token: u64, affinity: AxCpuMask) {
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid
            || identity.scheduler_identity_token != token
        {
            return;
        }
        self.scheduler.lock().affinity = affinity;
    }

    fn publication_token_for(&self, kernel_tid: Pid) -> Option<u64> {
        self.signal.lock().as_ref().and_then(|identity| {
            (identity.registration_tid == kernel_tid).then_some(identity.scheduler_identity_token)
        })
    }

    /// Seeds the not-yet-bound initial process identity. The first thread is
    /// constructed before its private endpoint exists, so no live identity
    /// can race this one-time initialization.
    fn seed_scheduler_state(
        &self,
        state: SchedState,
        reset_on_fork: bool,
        uclamp: axtask::UclampRequest,
        utilization_bounds: axtask::UtilizationBounds,
        version: u64,
    ) {
        let mut snapshot = self.scheduler.lock();
        debug_assert_eq!(snapshot.identity_epoch, 0);
        // Serial numbers wrap. A candidate is newer when it lies less than
        // half the u64 sequence space ahead of the published version.
        if scheduler_version_is_newer_or_equal(version, snapshot.version) {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: 0,
                version,
                reset_on_fork,
                uclamp_min: uclamp.minimum,
                uclamp_max: uclamp.maximum,
                uclamp_min_user_defined: uclamp.minimum_user_defined,
                uclamp_max_user_defined: uclamp.maximum_user_defined,
                uclamp_effective_min: utilization_bounds.minimum as u16,
                uclamp_effective_max: utilization_bounds.maximum as u16,
                ..state.into()
            };
        }
    }

    /// Publishes a successful scheduler transaction only if its task still
    /// owns the bound group-leader endpoint. Holding the binding locks through
    /// the epoch check prevents a former leader from publishing into the new
    /// leader's durable snapshot after an exec handoff.
    fn publish_scheduler_commit(
        &self,
        registration_tid: Pid,
        token: u64,
        task: &AxTaskRef,
        commit: TaskSchedulingSnapshot,
    ) {
        let current = self.current.lock();
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid {
            return;
        }
        // Reject a delayed publisher after a newer transaction on this same
        // task.  The state and version are one run-queue-lock snapshot, never
        // independently sampled values.
        if !scheduler_publication_matches(
            identity.scheduler_identity_token,
            token,
            commit,
            task_scheduling_snapshot(task).ok(),
        ) {
            return;
        }
        let epoch = *self.scheduler_identity_epoch.lock();
        let mut snapshot = self.scheduler.lock();
        if snapshot.identity_epoch == epoch {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: epoch,
                version: commit.version,
                reset_on_fork: commit.reset_on_spawn,
                uclamp_min: commit.uclamp.minimum,
                uclamp_max: commit.uclamp.maximum,
                uclamp_min_user_defined: commit.uclamp.minimum_user_defined,
                uclamp_max_user_defined: commit.uclamp.maximum_user_defined,
                uclamp_effective_min: commit.utilization_bounds.minimum as u16,
                uclamp_effective_max: commit.utilization_bounds.maximum as u16,
                ..commit.state.into()
            };
        }
        drop(snapshot);
        drop(signal);
        drop(current);
    }

    #[cfg(test)]
    fn publish_scheduler_state(&self, registration_tid: Pid, state: SchedState, version: u64) {
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid {
            return;
        }
        let epoch = *self.scheduler_identity_epoch.lock();
        let mut snapshot = self.scheduler.lock();
        if snapshot.identity_epoch == epoch
            && scheduler_version_is_newer_or_equal(version, snapshot.version)
        {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: epoch,
                version,
                ..state.into()
            };
        }
    }

    fn publish_handoff<'a>(
        &self,
        credential: Arc<CredentialSlot>,
        signal: Option<GroupLeaderSignalIdentity>,
        prepared: Option<PreparedCred<'a>>,
        executor_scheduler: Option<TaskSchedulingSnapshot>,
    ) -> GroupLeaderCommit<'a> {
        let signal = signal.map(|mut signal| {
            signal.pid_ns = self.pid_ns.clone();
            signal.scheduler = Some(self.scheduler.clone());
            signal
        });
        let mut current = self.current.lock();
        let mut current_signal = signal.as_ref().map(|_| self.signal.lock());
        #[cfg(test)]
        let group_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::GroupLeader);
        let publication = prepared.map(PreparedCred::publish);
        let retired = core::mem::replace(&mut *current, credential);
        let (retired_signal, identity_replaced) = match (current_signal.as_mut(), signal) {
            (Some(current), Some(signal)) => match current.as_ref() {
                Some(existing) if existing.same_endpoint(&signal) => (None, false),
                _ => ((**current).replace(signal), true),
            },
            _ => (None, false),
        };
        // Publish a new epoch while both identity owners remain locked.  This
        // must happen even for leader exec, whose slot and endpoint can both
        // be pointer-identical across the handoff.
        self.identity_token.fetch_add(1, Ordering::Release);
        if identity_replaced {
            // Task scheduler versions belong to individual scheduler nodes.
            // Advance the binding epoch and publish the executor's exact
            // state while the identity locks are still held; no old leader
            // publication can be ordered into this new generation.
            let mut epoch = self.scheduler_identity_epoch.lock();
            let mut token = self.scheduler_identity_token.lock();
            *epoch = epoch.wrapping_add(1);
            *token = token.wrapping_add(1);
            // The installed endpoint, epoch advance, and forced executor
            // seed form one leader-identity transaction.
            current_signal
                .as_mut()
                .and_then(|signal| signal.as_mut())
                .expect("replaced leader endpoint must be installed")
                .scheduler_identity_token = *token;
            if let Some(commit) = executor_scheduler {
                *self.scheduler.lock() = ZombieSchedulerSnapshot {
                    identity_epoch: *epoch,
                    version: commit.version,
                    reset_on_fork: commit.reset_on_spawn,
                    uclamp_min: commit.uclamp.minimum,
                    uclamp_max: commit.uclamp.maximum,
                    uclamp_min_user_defined: commit.uclamp.minimum_user_defined,
                    uclamp_max_user_defined: commit.uclamp.maximum_user_defined,
                    uclamp_effective_min: commit.utilization_bounds.minimum as u16,
                    uclamp_effective_max: commit.utilization_bounds.maximum as u16,
                    ..commit.state.into()
                };
            }
        }
        drop(current_signal);
        drop(current);
        #[cfg(test)]
        drop(group_lock_probe);
        GroupLeaderCommit {
            publication,
            retired_slot: retired,
            retired_signal,
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

/// The ABI-visible portion of Linux's `mm_struct`.  This is the sole owner of
/// the `/proc/<pid>/stat` image layout and of PR_SET_MM updates; `brk` uses the
/// same lock through `ProcessData`, so metadata can never publish a heap end
/// that differs from the address-space operation which established it.
#[derive(Clone, Debug)]
pub(crate) struct ProcessMmLayout {
    pub start_code: usize,
    pub end_code: usize,
    pub start_data: usize,
    pub end_data: usize,
    pub start_brk: usize,
    pub brk: usize,
    pub start_stack: usize,
    pub arg_start: usize,
    pub arg_end: usize,
    pub env_start: usize,
    pub env_end: usize,
    pub auxv: Vec<u8>,
    // These describe the real initial anonymous heap VMA installed by exec.
    // PR_SET_MM_START_BRK changes the ABI metadata above but never retargets a
    // live VMA behind the memory manager's back.
    heap_mapping_base: usize,
    heap_mapping_initial_end: usize,
}

impl ProcessMmLayout {
    fn initial() -> Self {
        let start_brk = crate::config::USER_HEAP_BASE;
        Self {
            start_code: 0,
            end_code: 0,
            start_data: start_brk,
            end_data: start_brk,
            start_brk,
            brk: start_brk + crate::config::USER_HEAP_SIZE,
            start_stack: crate::config::USER_STACK_TOP,
            arg_start: 0,
            arg_end: 0,
            env_start: 0,
            env_end: 0,
            auxv: Vec::new(),
            heap_mapping_base: start_brk,
            heap_mapping_initial_end: start_brk + crate::config::USER_HEAP_SIZE,
        }
    }
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
        #[cfg(test)]
        let security_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::ProcessSecurity);
        if prepared.requires_dumpability_drop() {
            security.dumpability = Dumpability::NotDumpable;
            pdeath_signal.store(0, Ordering::Release);
        }
        let publication = prepared.publish();
        drop(security);
        #[cfg(test)]
        drop(security_lock_probe);
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
    group_leader: &GroupLeaderIdentityBinding,
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
    group_leader: &GroupLeaderIdentityBinding,
    credential: Arc<CredentialSlot>,
    signal: Option<GroupLeaderSignalIdentity>,
    prepared: Option<PreparedCred<'a>>,
    executor_scheduler: Option<TaskSchedulingSnapshot>,
    new_image: ProcessImageBinding<A>,
    finish_image_publication: impl FnOnce(),
) -> (GroupLeaderCommit<'a>, ProcessImageBinding<A>) {
    let mut image = image_binding.write();
    #[cfg(test)]
    let image_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::ProcessImage);
    let group_leader =
        group_leader.publish_handoff(credential, signal, prepared, executor_scheduler);
    let retired_image = core::mem::replace(&mut *image, new_image);
    finish_image_publication();
    drop(image);
    #[cfg(test)]
    drop(image_lock_probe);
    (group_leader, retired_image)
}

/// Clones the scheduler-facing TLB owner without joining the process-image
/// lock domain. Exec serializes replacement against scheduling on its sole
/// surviving thread, while this `Arc` keeps either observed generation alive.
fn scheduler_tlb_state_snapshot<T>(image_tlb_state: &RwLock<Arc<T>>) -> Arc<T> {
    image_tlb_state.read().clone()
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
    /// Relationship owner retired while consuming an exit drain. Reusing the
    /// already allocated reverse-link node carries credential destruction to
    /// the caller's post-lifecycle boundary without allocating under locks.
    retired_relationship: Option<PtraceRelationshipSnapshot>,
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
    retained: Option<Box<PtraceReverseLinkNode>>,
}

impl PtraceReverseLinkDrain {
    /// Consumes one reverse link and retains both its preallocated node and an
    /// optional detached relationship until this drain itself is dropped.
    /// Exit uses this to move credential free callbacks beyond process
    /// lifecycle and task-parent publication gates without a new allocation.
    pub(crate) fn retain_next_retirement(
        &mut self,
        retire: impl FnOnce(PtraceReverseLink) -> Option<PtraceRelationshipSnapshot>,
    ) -> bool {
        let Some(mut node) = self.next.take() else {
            return false;
        };
        self.next = node.next.take();
        let link = PtraceReverseLink {
            tracee: node.tracee,
            session: node.session,
        };
        node.retired_relationship = retire(link);
        node.next = self.retained.take();
        self.retained = Some(node);
        true
    }
}

impl Iterator for PtraceReverseLinkDrain {
    type Item = PtraceReverseLink;

    fn next(&mut self) -> Option<Self::Item> {
        let mut node = self.next.take()?;
        self.next = node.next.take();
        debug_assert!(node.retired_relationship.is_none());
        Some(PtraceReverseLink {
            tracee: node.tracee,
            session: node.session,
        })
    }
}

impl Drop for PtraceReverseLinkDrain {
    fn drop(&mut self) {
        fn drop_chain(mut next: Option<Box<PtraceReverseLinkNode>>) {
            while let Some(mut node) = next {
                next = node.next.take();
                drop(node);
            }
        }

        drop_chain(self.next.take());
        drop_chain(self.retained.take());
    }
}

/// Serializes ptrace relationship actions. The wrapper is zero-cost in
/// production and carries a host-test lock-depth probe used by credential
/// post-commit callbacks.
pub(crate) struct PtraceActionGuard<'a> {
    _guard: axsync::MutexGuard<'a, ()>,
    #[cfg(test)]
    _probe: PostCommitLockProbe,
}

/// [`Process`]-shared data.
pub struct ProcessData {
    /// Immutable resource domain, inherited by fork and retained across exec.
    pub(crate) world: crate::task::WorldId,
    /// The process.
    pub(crate) proc: Arc<Process>,
    /// Serializes child admission through publication against final exit and
    /// reparenting for this process.
    process_lifecycle: Mutex<()>,
    /// The only allocation needed to publish this process's durable zombie
    /// payload. It is reserved before the process becomes visible.
    prepared_zombie_snapshot: SpinNoIrq<Option<PreparedZombieSnapshot>>,
    /// Stable identity of the Linux thread-group leader.
    ///
    /// These are strong references to the leader task's sole credential slot
    /// and private signal endpoint, not copied process-level shadow state.
    /// They deliberately outlive an exited leader task while sibling threads
    /// keep the process alive, matching Linux's persistent PID identity,
    /// queued-signal accounting, and exec handoff behavior.
    group_leader_identity: GroupLeaderIdentityBinding,
    /// The executable path
    pub exe_path: RwLock<FsPathBuf>,
    /// The inode currently held busy as this process image.
    pub(crate) executable: SpinNoIrq<Option<ExecutableKey>>,
    /// The command line arguments
    pub cmdline: RwLock<Arc<Vec<Vec<u8>>>>,
    /// Realtime process creation timestamp, in seconds.
    start_realtime_sec: u64,
    /// Monotonic process creation timestamp, in nanoseconds.
    start_monotonic_ns: u64,
    /// Executable address space and its coherent process-access owner.
    image_binding: RwLock<LiveProcessImageBinding>,
    /// Scheduler-facing TLB state for the current image. Exec is the sole
    /// writer and publishes it while its sole surviving thread cannot be
    /// preempted; scheduler readers deliberately never take `image_binding`.
    /// The independent `Arc` pins the observed TLB state across replacement.
    image_tlb_state: RwLock<Arc<TlbState>>,
    /// The resource scope
    pub scope: RwLock<Scope>,
    /// Real empty files table prepared at process creation for final exit swap.
    exit_fd_table: Arc<FdTable>,
    /// Authoritative Linux ABI memory layout. See [`ProcessMmLayout`].
    mm_layout: RwLock<ProcessMmLayout>,
    /// `signal_struct::timer_create_restore_ids`, the CRIU timer-restore mode.
    timer_restore_ids: AtomicBool,
    /// `signal_struct::autoreap`, set only by `clone3(CLONE_AUTOREAP)`.
    ///
    /// The bit belongs to the child, not to its parent: a process created
    /// this way is reaped as it exits and reports nothing, whatever the
    /// parent's `SIGCHLD` disposition is.
    autoreap: AtomicBool,

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
    pub(crate) signal_pending_event: Arc<PollSet>,

    /// The futex table.
    pub(in crate::task) futex_table: Arc<FutexTable>,

    /// Linux personality flags shared by all threads in the process.
    personality: AtomicU32,
    /// x86 PKU allocation map, shared by every thread using this mm.
    pkeys: SpinNoIrq<ProtectionKeyState>,
    /// NUMA memory policy state for the single-node kernel memory model.
    mempolicy: SpinNoIrq<MempolicyState>,
    /// Current timer slack in nanoseconds.
    timerslack_current_ns: AtomicUsize,
    /// Default timer slack in nanoseconds, used when PR_SET_TIMERSLACK is 0.
    timerslack_default_ns: AtomicUsize,
    /// `PR_SET_MDWE` is an mm property, not a policy hint. Keeping it with
    /// the address-space owner makes every thread sharing this mm observe the
    /// same execute-gain prohibition.
    mdwe: AtomicU8,
    /// POSIX interval timers created by this process.
    pub(crate) posix_timers: SpinNoIrq<Vec<Option<PosixTimer>>>,
    /// Process-wide `setitimer(2)` state. Real-time alarm actions and CPU-time
    /// charges are serialized here; no thread-local `RefCell` is accessed by
    /// an alarm worker.
    pub(crate) process_itimers: SpinNoIrq<ProcessITimers>,
    /// Monotonic thread-group CPU clock. Writers publish each task-local
    /// user/system interval exactly once; RLIMIT_CPU consumes this lifetime
    /// total, while armed VIRTUAL/PROF timers use the eligible clocks below.
    pub(crate) process_cpu_total_ns: AtomicU64,
    /// Durable fail-closed marker if any process CPU clock saturates instead
    /// of wrapping into a reused accounting domain.
    pub(crate) process_cpu_accounting_overflowed: AtomicBool,
    /// Independently rebased eligible clocks for ITIMER_VIRTUAL/PROF. Even
    /// epochs are stable; odd epochs fence an arm transition while the owner
    /// waits for already-admitted IRQ writers to retire.
    pub(crate) process_itimer_virtual_epoch: AtomicU64,
    pub(crate) process_itimer_virtual_writers: AtomicUsize,
    pub(crate) process_itimer_virtual_clock_ns: AtomicU64,
    pub(crate) process_itimer_prof_epoch: AtomicU64,
    pub(crate) process_itimer_prof_writers: AtomicUsize,
    pub(crate) process_itimer_prof_clock_ns: AtomicU64,
    /// Lock-free fast-path publication for process CPU timers. Accounting
    /// avoids the shared timer lock while neither VIRTUAL nor PROF is armed.
    pub(crate) process_itimer_cpu_armed: AtomicU8,
    /// True while the canonical RLIMIT_CPU soft limit is finite. The timer IRQ
    /// uses this only to request a later task-context policy boundary.
    pub(crate) process_rlimit_cpu_active: AtomicBool,
    /// Standard timer signals awaiting a scheduler-safe task-context drain.
    pub(crate) process_itimer_pending: AtomicU8,
    /// Encoded owner CPU (+1) for the queued process-timer node. This is the
    /// exact wake target when a producer observes an already queued token;
    /// zero means the consumer handoff is currently unowned.
    pub(crate) process_itimer_work_owner_cpu: AtomicUsize,
    /// Intrusive single-consumer work node used to defer process-timer signal
    /// publication out of IRQ-off context-switch accounting.
    pub(crate) process_itimer_work_queued: AtomicBool,
    pub(crate) process_itimer_work_node: ProcessITimerWorkNode,
    /// Preallocated RCU-published reverse subscriptions for foreign encoded
    /// CPU-clock POSIX timers targeting this process.
    pub(crate) foreign_cpu_timer_subscribers: ForeignCpuTimerSubscriberPool,

    /// CPU time accumulated from sibling threads that have already exited.
    pub(in crate::task) exited_threads_usage: AtomicTaskUsage,
    /// Seqcount closes the exit-thread-list to exited-usage handoff gap.
    pub(in crate::task) usage_transition_epoch: AtomicU64,
    /// CPU time accumulated from waited-for child subtrees.
    waited_children_usage: AtomicTaskUsage,
    /// Serializes wait* selection and consumption for this process.
    pub wait_lock: Mutex<()>,

    /// Job-control stop state shared by all threads in the process.
    job_ctl: SpinNoIrq<JobControlState>,
    /// Cgroup freezer state is intentionally independent of job control and
    /// ptrace stops.  A cgroup thaw must never manufacture SIGCONT-visible
    /// state or resume a process which was stopped for another reason.
    cgroup_freeze_requested: AtomicBool,
    cgroup_frozen_threads: AtomicUsize,
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
    /// Serializes setpgid with successful exec publication; fork starts false.
    pub(crate) exec_committed: Mutex<bool>,
    /// CLONE_VFORK coordination state.
    vfork_ctl: SpinNoIrq<VforkControlState>,
    /// Woken when threads should resume from stopped state.
    pub stop_event: Arc<PollSet>,
    /// Woken when a vfork child releases the parent.
    pub vfork_event: Arc<PollSet>,

    /// Group-leader namespace snapshot used only while constructing the first
    /// task and by process-scoped lifecycle records.  Live namespace ownership
    /// is task-local in `Thread::namespaces`; do not use this for current-task
    /// lookup or namespace-changing syscalls.
    namespaces: RwLock<NamespaceProxy>,
    /// IPC namespaces in which this process has installed a process-wide SHM
    /// attachment or mq_notify registration. A registering thread may switch
    /// namespace or exit before final process teardown.
    touched_ipc_namespaces: SpinNoIrq<Vec<Arc<IpcNamespace>>>,
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

#[cfg(test)]
mod pkey_tests;

/// An exec group-leader handoff whose pointer publication is complete but
/// whose credential post-commit notification has not run yet.
struct GroupLeaderCommit<'a> {
    publication: Option<super::creds::CredentialPublication<'a>>,
    retired_slot: Arc<CredentialSlot>,
    retired_signal: Option<GroupLeaderSignalIdentity>,
}

impl GroupLeaderCommit<'_> {
    fn complete_post_commit(self) -> GroupLeaderRetirement {
        let Self {
            publication,
            retired_slot,
            retired_signal,
        } = self;
        let credential = publication.map(|publication| {
            let (new, retirement) = publication.complete_post_commit();
            // The published slot retains the new credential. This owner is
            // needed only through the callback itself.
            drop(new);
            retirement
        });
        if let Some(retired) = retired_signal.as_ref() {
            // A nonleader exec replaces Linux's old group-leader task. Disable
            // exact publication and drain its private queue before retaining
            // the Arc for the caller's post-switch destruction boundary.
            retired
                .manager
                .retire_registration(retired.registration_tid, false);
        }
        GroupLeaderRetirement {
            _credential: credential,
            _slot: retired_slot,
            _signal: retired_signal,
        }
    }
}

/// Old group-leader and credential ownership retained after notification.
struct GroupLeaderRetirement {
    _credential: Option<super::creds::CredentialRetirement>,
    _slot: Arc<CredentialSlot>,
    _signal: Option<GroupLeaderSignalIdentity>,
}

/// An exec image/credential publication returned only after image,
/// group-leader, and task-alias locks have been released. The caller must run
/// post-commit notification before acquiring later process locks.
#[must_use = "exec publication must complete its credential notification"]
pub(crate) struct ExecImageCommit<'a> {
    group_leader: GroupLeaderCommit<'a>,
    image: LiveProcessImageBinding,
    security: super::security::CommittingExecSecurity,
    credential_lease: CredentialReadLease,
}

impl ExecImageCommit<'_> {
    pub(crate) fn complete_post_commit(self) -> ExecImageRetirement {
        let Self {
            group_leader,
            image,
            security,
            credential_lease,
        } = self;
        ExecImageRetirement {
            _group_leader: group_leader.complete_post_commit(),
            _image: image,
            security: Some(security),
            credential_lease: Some(credential_lease),
        }
    }
}

/// Retired image and credential ownership after the generic credential
/// notification but before the full-image committed notification.
#[must_use = "exec retirement must finish its executable lease and full-image notification"]
pub(crate) struct ExecImageRetirement {
    _group_leader: GroupLeaderRetirement,
    _image: LiveProcessImageBinding,
    security: Option<super::security::CommittingExecSecurity>,
    credential_lease: Option<CredentialReadLease>,
}

impl ExecImageRetirement {
    /// Converts the source metadata/content lease into the persistent active
    /// executable reference installed in the new process image. The pending
    /// exec notification and old image remain owned by this token.
    pub(crate) fn finish_executable_lease(&mut self) -> AxResult<Option<ExecutableKey>> {
        self.credential_lease
            .take()
            .ok_or(AxError::BadState)?
            .finish()
    }

    /// Emits the full-image committed notification and returns the still-owned
    /// retirement state. The caller must already have installed the hardware
    /// root, executable identity, metadata, signal state, and user context and
    /// released the ptrace action gate; it drops the returned owner only after
    /// releasing the exec and vfork gates.
    pub(crate) fn complete_exec_committed(mut self) -> CompletedExecImageRetirement {
        assert!(
            self.credential_lease.is_none(),
            "exec committed before executable lease conversion"
        );
        let security = self
            .security
            .take()
            .expect("exec committed security notification is pending")
            .committed();
        let Self {
            _group_leader,
            _image,
            security: pending_security,
            credential_lease,
        } = self;
        debug_assert!(pending_security.is_none());
        debug_assert!(credential_lease.is_none());
        CompletedExecImageRetirement {
            _group_leader,
            _image,
            _security: security,
        }
    }
}

/// Old image and exact security ownership retained after the full-image hook.
/// Exec drops this only after new thread admission and the vfork parent are
/// released to observe the completed image.
#[must_use = "completed exec retirement must outlive exec and vfork gate release"]
pub(crate) struct CompletedExecImageRetirement {
    _group_leader: GroupLeaderRetirement,
    _image: LiveProcessImageBinding,
    _security: super::security::CompletedExecSecurity,
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
            group_exited_at_core: outcome
                == tk_linux_process_adapter::ThreadPublicationOutcome::GroupExited,
        }
    }
}

/// Unpublished process plus initial thread held across runtime construction.
pub(crate) struct InitialProcessThreadAdmission {
    // Roll back the core composite before making exec observe no pending clone.
    publication: ProcessInitialAdmission,
    pending: PendingThreadAddition,
}

pub(crate) enum ProcessInitialAdmission {
    Ordinary(InitialProcessAdmission),
    ScopeInit(ScopedInitialProcessAdmission),
}

impl ProcessInitialAdmission {
    pub(crate) fn process(&self) -> &Arc<Process> {
        match self {
            Self::Ordinary(admission) => admission.process(),
            Self::ScopeInit(admission) => admission.process(),
        }
    }
}

impl InitialProcessThreadAdmission {
    /// Publishes the type-bound process/initial-thread pair before making exec
    /// observe that clone construction has completed.
    pub(crate) fn commit(self) -> (Arc<Process>, PendingThreadPublication) {
        let Self {
            publication,
            pending,
        } = self;
        let process = match publication {
            ProcessInitialAdmission::Ordinary(publication) => publication.commit(),
            ProcessInitialAdmission::ScopeInit(publication) => publication
                .commit()
                .expect("scoped init publication lost its reserved reaper scope"),
        };
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
        world: crate::task::WorldId,
        proc: Arc<Process>,
        prepared_zombie_snapshot: PreparedZombieSnapshot,
        group_leader_credential: Arc<CredentialSlot>,
        exe_path: FsPathBuf,
        executable: Option<ExecutableKey>,
        cmdline: Arc<Vec<Vec<u8>>>,
        aspace: Arc<Mutex<AddrSpace>>,
        access_state: Arc<ProcessAccessState>,
        scope: Scope,
        exit_fd_table: Arc<FdTable>,
        signal_actions: Arc<SharedSignalActions>,
        exit_signal: Option<Signo>,
        namespaces: NamespaceProxy,
    ) -> AxResult<Arc<Self>> {
        // Resolve the static composition before process resources can publish.
        let _profile = world.profile();
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
        let signal_pending_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let mut signal =
            ProcessSignalManager::new(signal_actions, crate::config::SIGNAL_TRAMPOLINE);
        signal.set_pending_waker(core::task::Waker::from(signal_pending_event.clone()));
        let signal = Arc::try_new(signal).map_err(|_| AxError::NoMemory)?;
        let futex_table = Arc::try_new(FutexTable::new()).map_err(|_| AxError::NoMemory)?;
        let stop_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let vfork_event = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let group_leader_identity = GroupLeaderIdentityBinding::try_new_with_pid_ns(
            group_leader_credential,
            Some(namespaces.pid()),
        )?;
        let mut touched_ipc_namespaces = Vec::new();
        touched_ipc_namespaces
            .try_reserve_exact(1)
            .map_err(|_| AxError::NoMemory)?;
        touched_ipc_namespaces.push(namespaces.ipc());
        let image_tlb_state = {
            let image = aspace.lock();
            image.merge_resident_highwater(image.resident_user_bytes() as u64 / 1024);
            image.tlb_state()
        };
        let data = Self {
            world,
            proc,
            process_lifecycle: Mutex::new(()),
            prepared_zombie_snapshot: SpinNoIrq::new(Some(prepared_zombie_snapshot)),
            group_leader_identity,
            exe_path: RwLock::new(exe_path),
            executable: SpinNoIrq::new(executable),
            cmdline: RwLock::new(cmdline),
            start_realtime_sec,
            start_monotonic_ns,
            image_binding: RwLock::new(ProcessImageBinding {
                aspace,
                access_state,
            }),
            image_tlb_state: RwLock::new(image_tlb_state),
            scope: RwLock::new(scope),
            exit_fd_table,
            mm_layout: RwLock::new(ProcessMmLayout::initial()),
            // `copy_signal()` allocates a zeroed `signal_struct` and copies
            // neither bit, so every process starts with both off; only
            // `clone3(CLONE_AUTOREAP)` and the timer prctl turn them on.
            timer_restore_ids: AtomicBool::new(false),
            autoreap: AtomicBool::new(false),

            rlim: RwLock::default(),

            child_exit_event,
            exit_event,
            exec_event,
            exit_signal,

            signal,
            signal_pending_event,

            futex_table,

            personality: AtomicU32::new(0),
            pkeys: SpinNoIrq::new(ProtectionKeyState::default()),
            mempolicy: SpinNoIrq::new(MempolicyState::default()),
            timerslack_current_ns: AtomicUsize::new(50_000),
            timerslack_default_ns: AtomicUsize::new(50_000),
            mdwe: AtomicU8::new(0),
            posix_timers: SpinNoIrq::new(Vec::new()),
            process_itimers: SpinNoIrq::new(ProcessITimers::new()),
            process_cpu_total_ns: AtomicU64::new(0),
            process_cpu_accounting_overflowed: AtomicBool::new(false),
            process_itimer_virtual_epoch: AtomicU64::new(0),
            process_itimer_virtual_writers: AtomicUsize::new(0),
            process_itimer_virtual_clock_ns: AtomicU64::new(0),
            process_itimer_prof_epoch: AtomicU64::new(0),
            process_itimer_prof_writers: AtomicUsize::new(0),
            process_itimer_prof_clock_ns: AtomicU64::new(0),
            process_itimer_cpu_armed: AtomicU8::new(0),
            process_rlimit_cpu_active: AtomicBool::new(false),
            process_itimer_pending: AtomicU8::new(0),
            process_itimer_work_owner_cpu: AtomicUsize::new(0),
            process_itimer_work_queued: AtomicBool::new(false),
            process_itimer_work_node: ProcessITimerWorkNode::new(),
            foreign_cpu_timer_subscribers: ForeignCpuTimerSubscriberPool::new(),
            exited_threads_usage: AtomicTaskUsage::new(),
            usage_transition_epoch: AtomicU64::new(0),
            waited_children_usage: AtomicTaskUsage::new(),
            wait_lock: Mutex::new(()),

            job_ctl: SpinNoIrq::new(JobControlState::default()),
            cgroup_freeze_requested: AtomicBool::new(false),
            cgroup_frozen_threads: AtomicUsize::new(0),
            ptrace_ctl: SpinNoIrq::new(PtraceControlState::default()),
            ptrace_actions: Mutex::new(()),
            ptrace_signal: Mutex::new(None),
            ptrace_tracees: SpinNoIrq::new(PtraceReverseLinks::default()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            exec_committed: Mutex::new(false),
            vfork_ctl: SpinNoIrq::new(VforkControlState::default()),
            stop_event,
            vfork_event,

            namespaces: RwLock::new(namespaces),
            touched_ipc_namespaces: SpinNoIrq::new(touched_ipc_namespaces),
        };
        executable_rollback.0 = None;
        let data = Arc::try_new(data).map_err(|_| AxError::NoMemory)?;
        register_address_space(&data.aspace());
        Ok(data)
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

    /// Returns whether this process has crossed the only exit states for
    /// which Linux permits `process_mrelease`.  A live sibling sharing an mm
    /// is rejected by the syscall before this state transition is attempted.
    pub(crate) fn oom_reap_eligible(&self) -> bool {
        self.group_exit_in_progress() || self.proc.is_zombie()
    }

    pub(crate) fn allocate_pkey(&self) -> AxResult<u8> {
        self.pkeys.lock().allocate()
    }

    pub(crate) fn free_pkey(&self, key: i32) -> AxResult<()> {
        self.pkeys.lock().free(key)
    }

    pub(crate) fn pkey_is_allocated(&self, key: i32) -> bool {
        self.pkeys.lock().is_allocated(key)
    }

    pub(crate) fn pkey_snapshot(&self) -> ProtectionKeyState {
        *self.pkeys.lock()
    }

    pub(crate) fn install_pkey_snapshot(&self, state: ProtectionKeyState) {
        *self.pkeys.lock() = state;
    }

    pub(crate) fn reset_pkeys_for_exec(&self) {
        *self.pkeys.lock() = ProtectionKeyState::default();
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
        self.group_leader_identity.current_cred()
    }
    pub(crate) fn group_leader_landlock_domain(&self) -> LandlockDomain {
        self.group_leader_identity.landlock_domain()
    }
    pub(crate) fn replace_group_leader_landlock_domain(&self, domain: LandlockDomain) {
        self.group_leader_identity.replace_landlock_domain(domain);
    }

    /// Binds the initial leader's private signal queue before process
    /// publication. Later non-leader exec replaces it atomically with the
    /// credential-owner handoff.
    pub(crate) fn bind_initial_group_leader_signal(
        &self,
        registration_tid: Pid,
        signal: Arc<ThreadSignalManager>,
        landlock: LandlockDomain,
    ) -> AxResult<()> {
        self.group_leader_identity.replace_landlock_domain(landlock);
        self.group_leader_identity
            .bind_initial_signal(registration_tid, signal)
    }

    /// Freezes the persistent leader credential and private pending queue as
    /// one identity snapshot for an exited-leader signal operation.
    pub(crate) fn group_leader_signal_identity(
        &self,
    ) -> AxResult<(Arc<Cred>, Arc<ThreadSignalManager>)> {
        self.group_leader_identity.current_cred_and_signal()
    }

    /// Scheduler state retained for the exited group leader while siblings
    /// still keep this process alive. Callers serialize identity with lifecycle.
    pub(crate) fn group_leader_scheduler_state(&self) -> AxResult<ZombieSchedulerSnapshot> {
        self.group_leader_identity
            .signal
            .lock()
            .as_ref()
            .and_then(|identity| identity.scheduler.as_ref())
            .map(|scheduler| *scheduler.lock())
            .ok_or(AxError::NoSuchProcess)
    }

    /// Captures the complete published leader identity for an operation that
    /// must validate it again after taking a later lifecycle gate.
    pub(crate) fn group_leader_identity_snapshot(&self) -> AxResult<GroupLeaderIdentitySnapshot> {
        self.group_leader_identity.identity_snapshot()
    }

    /// Returns whether `expected` still names this exact exec generation.
    pub(crate) fn group_leader_identity_snapshot_matches(
        &self,
        expected: &GroupLeaderIdentitySnapshot,
    ) -> bool {
        self.group_leader_identity
            .identity_snapshot_matches(expected)
    }

    pub(crate) fn group_leader_signal_identity_matches(
        &self,
        expected: &Arc<ThreadSignalManager>,
    ) -> bool {
        self.group_leader_identity.signal_matches(expected)
    }

    /// Returns the preallocated shared owner published into this process's
    /// eventual zombie snapshot.
    pub(crate) fn group_leader_signal_owner(&self) -> GroupLeaderSignalOwner {
        self.group_leader_identity.signal_owner()
    }

    /// Publishes the current scheduler policy for this process's durable
    /// identity. The owner and its scheduler cell are shared with any
    /// already-published zombie payload, so this remains valid even after the
    /// runtime process membership is removed.
    pub(crate) fn seed_scheduler_state(
        &self,
        state: SchedState,
        reset_on_fork: bool,
        uclamp: axtask::UclampRequest,
        utilization_bounds: axtask::UtilizationBounds,
        version: u64,
    ) {
        self.group_leader_identity.seed_scheduler_state(
            state,
            reset_on_fork,
            uclamp,
            utilization_bounds,
            version,
        );
    }

    pub(crate) fn publish_scheduler_state(
        &self,
        registration_tid: Pid,
        token: u64,
        task: &AxTaskRef,
        commit: TaskSchedulingSnapshot,
    ) {
        self.group_leader_identity
            .publish_scheduler_commit(registration_tid, token, task, commit);
    }

    pub(crate) fn publish_affinity_snapshot(
        &self,
        registration_tid: Pid,
        token: u64,
        affinity: AxCpuMask,
    ) {
        self.group_leader_identity
            .publish_affinity_snapshot(registration_tid, token, affinity);
    }

    /// Returns the token only when `kernel_tid` is the authoritative current
    /// leader endpoint. This intentionally does not consult a task's visible
    /// TID: non-leader exec installs this endpoint before its TID alias is
    /// published. Scheduler publishers query it after their run-queue commit,
    /// so exec excludes a retired leader and admits its executor immediately.
    pub(crate) fn scheduler_publication_token(&self, kernel_tid: Pid) -> Option<u64> {
        self.group_leader_identity.publication_token_for(kernel_tid)
    }

    /// Takes process-directed identity, dumpability, and image through one
    /// coherent snapshot of the persistent group-leader binding.
    pub(crate) fn group_leader_image_access_snapshot(&self) -> ProcessImageAccessSnapshot {
        let (credential, dumpability, owner_user_ns, aspace, access_state) =
            snapshot_group_credential_image(&self.image_binding, &self.group_leader_identity);
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
        #[cfg(test)]
        let image_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::ProcessImage);
        let publication = image
            .access_state
            .publish_credential(prepared, pdeath_signal);
        drop(image);
        #[cfg(test)]
        drop(image_lock_probe);
        let (proposed, retirement) = publication.complete_post_commit();
        drop(retirement);
        proposed
    }

    /// Publishes the mandatory fully derived exec credential and switches the
    /// group-leader slot as one process-visible transition. Retired `Arc`s are
    /// destroyed only after the binding lock is released.
    pub(in crate::task) fn publish_exec_image<'a>(
        &self,
        owner: Pid,
        thread: &super::Thread,
        committing: CommittingExecCredential<'a>,
        new_aspace: Arc<Mutex<AddrSpace>>,
        new_access_state: Arc<ProcessAccessState>,
    ) -> ExecImageCommit<'a> {
        debug_assert!(self.is_exec_owner(owner));
        debug_assert_eq!(thread.proc_data.proc.pid(), self.proc.pid());
        let credential = thread.credential_slot();
        let (prepared, effects, security, credential_lease) = committing.into_parts();
        if effects.clear_pdeath_signal() {
            // Linux pdeath_signal is task-local: only the executor crosses
            // this credential transition, never its former siblings.
            thread.set_pdeath_signal(0);
        }
        // `ru_maxrss` is a process-lifetime high-water mark.  Preserve the
        // old image's peak across exec while the new image starts publishing
        // its own resident pages into the shared mm-level counter.
        let old_aspace = self.aspace();
        // Uprobe return instances and XOL mappings belong to the old image.
        // Retire them before publishing the replacement mm, so no scheduler
        // return edge can observe a trampoline from an executable that is no
        // longer this process image.
        crate::uprobe::on_exec(thread.kernel_tid() as u64, &old_aspace);
        let (old_maxrss_kb, inherited_thp_disable, cet_wake) = {
            let mut old_image = old_aspace.lock();
            // The record is in the old mm, which remains pinned across the
            // handoff.  Taking it first makes exec teardown exactly-once even
            // if a later old-image retirement path observes this task again.
            #[cfg(target_arch = "x86_64")]
            // Keep the lease registered until its VMA transaction succeeds;
            // a vfork alias only retires the alias and leaves the parent VMA.
            let cet_wake = Some(old_image.retire_cet_default_shadow_stack(thread.kernel_tid()));
            #[cfg(not(target_arch = "x86_64"))]
            let cet_wake = None;
            let old_maxrss_kb =
                old_image.merge_resident_highwater(old_image.resident_user_bytes() as u64 / 1024);
            (old_maxrss_kb, old_image.thp_disable_mode(), cet_wake)
        };
        if let Some(wake) = cet_wake {
            wake.finish();
        }
        // Publish the replacement mm to swapoff before making it reachable
        // through the process image. The old registration remains until the
        // image handoff completes, so neither side can escape a snapshot.
        // PR_SET_THP_DISABLE is preserved across exec.  Snapshot it at the
        // old-mm handoff linearization point above, not during the fallible
        // ELF build where a CLONE_VM peer could still change it before exec
        // commits.
        new_aspace
            .lock()
            .set_thp_disable_mode(inherited_thp_disable);
        register_address_space(&new_aspace);
        let new_tlb_state = {
            let image = new_aspace.lock();
            image.tlb_state()
        };
        new_aspace.lock().merge_resident_highwater(old_maxrss_kb);
        let executor = current().clone();
        let executor_scheduler = task_scheduling_snapshot(&executor).ok();
        // `publish_exec_image` is the only production image writer. Exec has
        // already drained the thread group to `owner`, so preventing this
        // thread from being switched out closes the only scheduler race: an
        // on-enter hook can observe either complete publication, but can
        // never run while this task is suspended holding either writer lock.
        let mut exec_committed = self.exec_committed.lock();
        let _switch_guard = kernel_guard::NoPreemptIrqSave::new();
        self.group_leader_identity
            .replace_landlock_domain(thread.landlock_domain());
        let (group_leader, retired_image) = replace_process_image_with_group_handoff(
            &self.image_binding,
            &self.group_leader_identity,
            credential,
            Some(GroupLeaderSignalIdentity::new(
                thread.kernel_tid(),
                thread.signal.clone(),
            )),
            Some(prepared),
            executor_scheduler,
            ProcessImageBinding {
                aspace: new_aspace,
                access_state: new_access_state,
            },
            || {
                *self.image_tlb_state.write() = new_tlb_state;
                self.mempolicy.lock().ranges.clear();
            },
        );
        // Both image writer locks are released. Registry cleanup takes the
        // old mm mutex, whose owner may need this CPU's shootdown IPI to run.
        drop(_switch_guard);
        *exec_committed = true;
        drop(exec_committed);
        unregister_address_space(&old_aspace);
        ExecImageCommit {
            group_leader,
            image: retired_image,
            security,
            credential_lease,
        }
    }

    /// Clones the preallocated real empty files table for final process exit.
    pub(crate) fn exit_fd_table(&self) -> Arc<FdTable> {
        self.exit_fd_table.clone()
    }

    /// Get the top address of the user heap.
    pub fn get_heap_top(&self) -> usize {
        self.mm_layout.read().brk
    }

    pub fn heap_base(&self) -> usize {
        self.mm_layout.read().heap_mapping_base
    }

    pub(crate) fn heap_initial_end(&self) -> usize {
        self.mm_layout.read().heap_mapping_initial_end
    }

    pub(crate) fn set_heap_layout(&self, base: usize) {
        let mut layout = self.mm_layout.write();
        layout.start_brk = base;
        layout.brk = base + crate::config::USER_HEAP_SIZE;
        layout.start_data = base;
        layout.end_data = base;
        layout.heap_mapping_base = base;
        layout.heap_mapping_initial_end = base + crate::config::USER_HEAP_SIZE;
    }

    /// Rebuilds ABI layout from the newly published exec address space. This
    /// reads the same VMA topology that faults and `/proc/<pid>/maps` use;
    /// no loader-side shadow ranges survive an exec handoff.
    pub(crate) fn reset_mm_layout_for_exec(
        &self,
        heap_base: usize,
        stack_pointer: usize,
        saved_auxv: Vec<u8>,
    ) {
        let aspace_handle = self.aspace();
        let aspace = aspace_handle.lock();
        let mut start_code = usize::MAX;
        let mut end_code = 0usize;
        let mut start_data = usize::MAX;
        let mut end_data = 0usize;
        for area in aspace
            .areas()
            .filter(|area| area.flags().contains(MappingFlags::USER))
        {
            if area.flags().contains(MappingFlags::EXECUTE) {
                start_code = start_code.min(area.start().as_usize());
                end_code = end_code.max(area.end().as_usize());
            }
            if area.flags().contains(MappingFlags::WRITE) && area.start().as_usize() < heap_base {
                start_data = start_data.min(area.start().as_usize());
                end_data = end_data.max(area.end().as_usize());
            }
        }
        drop(aspace);
        let mut layout = self.mm_layout.write();
        layout.start_code = (start_code != usize::MAX)
            .then_some(start_code)
            .unwrap_or(0);
        layout.end_code = end_code;
        layout.start_data = (start_data != usize::MAX)
            .then_some(start_data)
            .unwrap_or(heap_base);
        layout.end_data = end_data.max(heap_base);
        layout.start_brk = heap_base;
        layout.brk = heap_base + crate::config::USER_HEAP_SIZE;
        layout.start_stack = stack_pointer;
        layout.arg_start = 0;
        layout.arg_end = 0;
        layout.env_start = 0;
        layout.env_end = 0;
        layout.auxv = saved_auxv;
        layout.heap_mapping_base = heap_base;
        layout.heap_mapping_initial_end = heap_base + crate::config::USER_HEAP_SIZE;
    }

    /// Fallibly snapshots the executable path without allocator work under
    /// the process metadata lock.
    pub(crate) fn try_exe_path(&self) -> AxResult<FsPathBuf> {
        let mut path = Vec::new();
        loop {
            path.clear();
            let required = self.exe_path.read().as_bytes().len();
            if path.capacity() < required {
                path.try_reserve_exact(required)
                    .map_err(|_| AxError::NoMemory)?;
            }
            let current = self.exe_path.read();
            if path.capacity() < current.as_bytes().len() {
                drop(current);
                continue;
            }
            path.extend_from_slice(current.as_bytes());
            return Ok(FsPathBuf::from_vec(path));
        }
    }

    /// Returns the current address-space handle for this process.
    pub fn aspace(&self) -> Arc<Mutex<AddrSpace>> {
        self.image_binding.read().aspace.clone()
    }

    /// Returns the scheduler TLB state without taking the process-image or
    /// address-space locks. The latter may be held by a page-table writer
    /// waiting for a remote shootdown acknowledgement; the former may be held
    /// by the exec publication which this hook must allow to run to completion.
    pub(crate) fn aspace_tlb_state(&self) -> Arc<TlbState> {
        scheduler_tlb_state_snapshot(&self.image_tlb_state)
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

    pub(crate) fn namespace_proxy(&self) -> NamespaceProxy {
        self.namespaces.read().clone()
    }

    pub(crate) fn user_ns(&self) -> Arc<UserNamespace> {
        self.namespaces.read().user()
    }
    pub(crate) fn pid_ns(&self) -> Arc<PidNamespace> {
        self.namespaces.read().pid()
    }
    pub(crate) fn pid_ns_for_children(&self) -> Arc<PidNamespace> {
        self.namespaces.read().pid_for_children()
    }
    pub(crate) fn mount_ns(&self) -> Arc<MountNamespace> {
        self.namespaces.read().mount()
    }
    pub(crate) fn ipc_ns(&self) -> Arc<IpcNamespace> {
        self.namespaces.read().ipc()
    }

    pub(crate) fn register_touched_ipc_namespace(
        &self,
        namespace: Arc<IpcNamespace>,
    ) -> AxResult<()> {
        let mut touched = self.touched_ipc_namespaces.lock();
        if touched.iter().any(|known| Arc::ptr_eq(known, &namespace)) {
            return Ok(());
        }
        touched.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        touched.push(namespace);
        Ok(())
    }

    /// Fallibly snapshots every IPC namespace that owns process-lifetime
    /// objects for this process. A fork into a new current IPC namespace must
    /// still inherit SysV mappings backed by the parent's older namespaces;
    /// consulting only `ipc_ns()` would silently lose those attachments.
    pub(crate) fn touched_ipc_namespaces_snapshot(&self) -> AxResult<Vec<Arc<IpcNamespace>>> {
        let touched = self.touched_ipc_namespaces.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(touched.len())
            .map_err(|_| AxError::NoMemory)?;
        snapshot.extend(touched.iter().cloned());
        Ok(snapshot)
    }

    pub(crate) fn cleanup_touched_ipc_namespaces(&self, pid: Pid) {
        // Exit cannot allocate or keep a SpinNoIrq guard across namespace
        // cleanup (those paths may take sleepable manager locks). Drain the
        // already-reserved ownership vector, then release the IRQ-safe guard
        // before invoking either cleanup operation.
        let namespaces = {
            let mut touched = self.touched_ipc_namespaces.lock();
            core::mem::take(&mut *touched)
        };
        for namespace in namespaces {
            crate::syscall::ipc::cleanup_process_mqueue_notifications_in(&namespace, pid);
            // SysV attachments are retired here, in the exiting task's own
            // context and before `publish_final_process_exit` makes the zombie
            // waitable, exactly as Linux's `exit_shm()` runs from `do_exit()`
            // ahead of `exit_notify()`. This is what makes an IPC_RMID segment
            // unreachable at the moment its last attachment disappears: a
            // parent's `wait()` returning is itself the proof that the exited
            // process owns nothing. Leaving it to the deferred VMA mapping
            // finalizer is not equivalent, because AddrSpace teardown (and its
            // TLB grace) completes after the zombie is published, so `shmat`
            // could still find the removed shmid.
            //
            // Page lifetime is not decided here. Every mapping of the segment
            // holds its own `Arc<SharedPages>` through its `SharedBackend`, and
            // an unmap releases that ownership only after its TLB grace
            // (`AddrSpace::unmap_areas_with_tlb_grace`), so the segment's own
            // reference is never the last one while a mapping or a translation
            // still needs the frames. A record removed here only turns the
            // still-outstanding VMA lease's deferred finalizer into an
            // exact-match no-op, which is the same state an explicit `shmdt`
            // already produces while its lease is outstanding.
            crate::syscall::ipc::clear_proc_shm_in_namespace(&namespace, pid);
        }
    }
    pub(crate) fn net_ns(&self) -> Arc<NetworkNamespace> {
        self.namespaces.read().net()
    }
    pub(crate) fn cgroup_ns(&self) -> Arc<CgroupNamespace> {
        self.namespaces.read().cgroup()
    }
    pub(crate) fn uts_ns(&self) -> Arc<UtsNamespace> {
        self.namespaces.read().uts()
    }
    pub(crate) fn time_ns(&self) -> Arc<TimeNamespace> {
        self.namespaces.read().time()
    }
    pub(crate) fn time_ns_for_children(&self) -> Arc<TimeNamespace> {
        self.namespaces.read().time_for_children()
    }

    pub(crate) fn cgroup_ns_id(&self) -> u64 {
        self.cgroup_ns().id()
    }

    pub(crate) fn prepare_namespace_replacement(
        &self,
        update: impl FnOnce(&mut NamespaceProxy),
    ) -> PreparedNamespaceProxyReplacement {
        let mut replacement = self.namespace_proxy();
        update(&mut replacement);
        PreparedNamespaceProxyReplacement { replacement }
    }

    pub(crate) fn prepare_namespace_proxy_replacement(
        &self,
        replacement: NamespaceProxy,
    ) -> PreparedNamespaceProxyReplacement {
        PreparedNamespaceProxyReplacement { replacement }
    }

    pub(crate) fn prepare_user_namespace_attach(
        &self,
        user_ns: Arc<UserNamespace>,
    ) -> PreparedNamespaceProxyReplacement {
        self.prepare_namespace_replacement(|proxy| proxy.replace_user(user_ns))
    }

    /// Attachment hand-off for the mount topology worker.  The caller plans
    /// and validates topology in `mounts.rs` first, then commits this token
    /// alongside its topology publication.
    pub(crate) fn prepare_mount_namespace_attach(
        &self,
        mount_ns: Arc<MountNamespace>,
    ) -> PreparedNamespaceProxyReplacement {
        self.prepare_namespace_replacement(|proxy| proxy.replace_mount(mount_ns))
    }

    /// Attachment hand-off for SysV/POSIX IPC.  IPC subsystem code owns the
    /// managers; ProcessData owns only the namespace pointer publication.
    pub(crate) fn prepare_ipc_namespace_attach(
        &self,
        ipc_ns: Arc<IpcNamespace>,
    ) -> PreparedNamespaceProxyReplacement {
        self.prepare_namespace_replacement(|proxy| proxy.replace_ipc(ipc_ns))
    }

    pub(crate) fn try_unshared_time_ns(
        &self,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<TimeNamespace>> {
        self.time_ns_for_children().try_fork(owner_user_ns)
    }

    pub(crate) fn replace_time_ns_for_children(&self, new_ns: Arc<TimeNamespace>) {
        let mut namespaces = self.namespaces.write();
        namespaces.replace_time_for_children(new_ns);
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
        self.mm_layout.write().brk = top;
    }

    pub(crate) fn mm_layout(&self) -> ProcessMmLayout {
        self.mm_layout.read().clone()
    }

    /// Snapshots the exec-installed auxiliary vector image.
    pub(crate) fn saved_auxv(&self) -> Vec<u8> {
        self.mm_layout.read().auxv.clone()
    }

    /// Reads `signal_struct::autoreap`.
    pub(crate) fn autoreap(&self) -> bool {
        self.autoreap.load(Ordering::Acquire)
    }

    /// Sets `signal_struct::autoreap` for a child created by `CLONE_AUTOREAP`.
    pub(crate) fn set_autoreap(&self) {
        self.autoreap.store(true, Ordering::Release);
    }

    /// Reads the `signal_struct::timer_create_restore_ids` bit.
    pub(crate) fn timer_restore_ids(&self) -> bool {
        self.timer_restore_ids.load(Ordering::Acquire)
    }

    /// Writes the `signal_struct::timer_create_restore_ids` bit.
    ///
    /// Linux keeps this in `signal_struct`, so it is shared by the whole
    /// thread group and lives until the group's last thread exits: `do_exit()`
    /// calls `exit_itimers()` there, and neither exec nor a separate
    /// `prctl(PR_TIMER_CREATE_RESTORE_IDS_OFF)` is needed to clear it.
    /// The `timer_create(2)` input-id path that consumes the bit is not
    /// implemented yet; see the `PR_TIMER_CREATE_RESTORE_IDS` arm.
    pub(crate) fn set_timer_restore_ids(&self, value: bool) {
        self.timer_restore_ids.store(value, Ordering::Release);
    }

    /// Publishes a fully validated layout after its corresponding VMA/heap
    /// transaction has completed. No caller may mutate individual fields.
    pub(crate) fn replace_mm_layout(&self, layout: ProcessMmLayout) {
        *self.mm_layout.write() = layout;
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
        // PR_MDWE_NO_INHERIT suppresses the complete MDWE state in a newly
        // created mm. CLONE_VM does not call this method and therefore keeps
        // sharing the parent's state as Linux does.
        let parent_mdwe = parent.mdwe.load(Ordering::Acquire);
        self.mdwe.store(
            if parent_mdwe & 0b10 == 0 {
                parent_mdwe
            } else {
                0
            },
            Ordering::Release,
        );
    }

    pub(crate) fn mdwe(&self) -> u8 {
        self.mdwe.load(Ordering::Acquire)
    }

    /// MDWE is monotonic for the lifetime of an mm: a caller can add either
    /// valid bit but can never weaken a previously installed restriction.
    pub(crate) fn set_mdwe(&self, flags: u8) -> AxResult<()> {
        const VALID: u8 = 0b11;
        if flags & !VALID != 0 {
            return Err(AxError::InvalidInput);
        }
        self.mdwe.fetch_or(flags, Ordering::AcqRel);
        Ok(())
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
        // The wait path publishes the reaped state and charges this ledger
        // while holding the same lock.  Keep readers on that linearization
        // boundary so CHILDREN cannot observe the interval between those two
        // operations.
        let _wait_guard = self.wait_lock.lock();
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

    pub(crate) fn begin_usage_transition(&self) {
        self.usage_transition_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn end_usage_transition(&self) {
        self.usage_transition_epoch.fetch_add(1, Ordering::Release);
    }

    /// Records a waited-for child subtree into the process's child ledger.
    pub fn account_waited_child(&self, usage: super::accounting::TaskUsage) {
        self.waited_children_usage.add(usage);
    }

    pub(crate) fn sample_maxrss_kb(&self) -> u64 {
        let image = self.aspace();
        let image = image.lock();
        image.merge_resident_highwater(image.resident_user_bytes() as u64 / 1024)
    }
}

impl ProcessData {
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
        self.mempolicy.lock().bind_range(start, end, policy);
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

    /// Updates the home-node preference for the policy covering an existing
    /// VMA prefix.  An absent VMA policy deliberately remains absent, matching
    /// Linux's `set_mempolicy_home_node()` handling of default-policy VMAs.
    pub fn set_mempolicy_home_node_range(
        &self,
        start: usize,
        size: usize,
        home_node: usize,
    ) -> AxResult<bool> {
        let Some(end) = start.checked_add(size) else {
            return Err(AxError::InvalidInput);
        };
        let mut snapshot = Vec::new();
        loop {
            let required = self.mempolicy.lock().ranges.len();
            if snapshot.capacity() < required {
                snapshot
                    .try_reserve_exact(required - snapshot.capacity())
                    .map_err(|_| AxError::NoMemory)?;
            }
            snapshot.clear();
            {
                let state = self.mempolicy.lock();
                if snapshot.capacity() < state.ranges.len() {
                    continue;
                }
                snapshot.extend(state.ranges.iter().copied());
            }
            let (new_ranges, updated, error) =
                MempolicyState::try_set_home_node_in_range(&snapshot, start, end, home_node)?;
            let mut state = self.mempolicy.lock();
            if state.ranges != snapshot {
                continue;
            }
            state.ranges = new_ranges;
            return error.map_or(Ok(updated), |error| Err(error.into()));
        }
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

    pub(crate) fn lock_ptrace_actions(&self) -> PtraceActionGuard<'_> {
        let guard = self.ptrace_actions.lock();
        #[cfg(test)]
        let probe = PostCommitLockProbe::new(PostCommitLockKind::PtraceAction);
        PtraceActionGuard {
            _guard: guard,
            #[cfg(test)]
            _probe: probe,
        }
    }

    pub fn ptrace_tracer(&self) -> Option<Pid> {
        self.ptrace_ctl
            .lock()
            .active_session()
            .map(|session| session.tracer)
    }

    pub(crate) fn ptrace_active_session(&self) -> Option<PtraceSession> {
        self.ptrace_ctl.lock().active_session()
    }

    /// Atomically snapshots both the exact ptrace generation and Linux's
    /// immutable relationship-time `ptracer_cred`. Exec and other privilege
    /// consumers must not combine `ptrace_tracer()` with a later PID
    /// credential lookup.
    pub(crate) fn ptrace_relationship_snapshot(&self) -> Option<PtraceRelationshipSnapshot> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let relationship = ptrace_ctl.active_relationship();
        drop(ptrace_ctl);
        relationship
    }

    /// Samples the inherited relationship together with its option word and
    /// seize mode. Clone must never splice a relationship from one generation
    /// to options observed after a detach/reattach.
    pub(crate) fn ptrace_clone_snapshot(&self) -> Option<(PtraceRelationshipSnapshot, u32, bool)> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        Some((
            ptrace_ctl.active_relationship()?,
            ptrace_ctl.options,
            ptrace_ctl.seized,
        ))
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
            retired_relationship: None,
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
    /// the exact hook-authorized tasks, actor credential, and target image.
    /// The actor credential guard is acquired before the publication gate;
    /// `relationship_credential` is either that live credential or the
    /// immutable relationship-time credential retained by CLONE_PTRACE. The
    /// fixed order inside is exec gate, image, access security, exact target
    /// credential, ptrace control, then tracer reverse links.
    pub(crate) fn publish_ptrace_relationship(
        &self,
        publication: &PtracePublicationGuard<'_>,
        target: &super::Thread,
        ptracer: &super::Thread,
        authorized_ptracer: &CredentialSnapshotGuard<'_>,
        origin: PtraceRelationshipOrigin,
        relationship_credential: &Arc<Cred>,
        seized: bool,
        initial_options: u32,
        authorized: &ProcessImageAccessSnapshot,
        reverse_link: PreparedPtraceReverseLink<'_>,
    ) -> AxResult<PtraceSession> {
        if !core::ptr::eq(publication.owner, self) {
            return Err(AxError::BadState);
        }
        let tracer = ptracer.proc_data.proc.pid();
        let tracer_kernel_tid = ptracer.kernel_tid();
        if let Some(tracer_owner) = publication.tracer_owner
            && (!core::ptr::eq(tracer_owner, &*ptracer.proc_data)
                || tracer_owner.proc.pid() != tracer
                || !tracer_owner
                    .proc
                    .thread_ids()
                    .any(|tid| tid == tracer_kernel_tid))
        {
            return Err(AxError::NoSuchProcess);
        }
        let ptracer_slot = ptracer.credential_slot();
        if ptracer.exit.load(Ordering::Acquire)
            || !ptracer
                .proc_data
                .proc
                .thread_ids()
                .any(|tid| tid == tracer_kernel_tid)
            || !core::ptr::eq(authorized_ptracer.slot(), &*ptracer_slot)
        {
            return Err(AxError::NoSuchProcess);
        }
        let relationship_owner = match origin {
            PtraceRelationshipOrigin::Attach => ptracer,
            PtraceRelationshipOrigin::Traceme => target,
            PtraceRelationshipOrigin::Inherited => target,
        };
        if origin != PtraceRelationshipOrigin::Inherited {
            let relationship_slot = relationship_owner.credential_slot();
            if !Arc::ptr_eq(relationship_credential, &relationship_slot.current()) {
                return Err(AxError::BadState);
            }
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
        if origin == PtraceRelationshipOrigin::Traceme
            && !Arc::ptr_eq(relationship_credential, &authorized.credential)
        {
            return Err(AxError::OperationNotPermitted);
        }
        if origin == PtraceRelationshipOrigin::Attach
            && !Arc::ptr_eq(relationship_credential, authorized_ptracer.credential())
        {
            return Err(AxError::BadState);
        }
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        let old_generation = ptrace_ctl.generation;
        let Some(session) = ptrace_ctl.try_begin(
            tracer,
            tracer_kernel_tid,
            seized,
            initial_options,
            origin,
            relationship_credential,
        ) else {
            return Err(if ptrace_ctl.active_session().is_some() {
                AxError::OperationNotPermitted
            } else {
                AxError::OutOfRange
            });
        };
        if let Err((error, reverse_link)) = reverse_link.publish(session) {
            let retired_relationship = ptrace_ctl
                .rollback_begin(session, old_generation)
                .expect("new ptrace relationship owns rollback session");
            drop(ptrace_ctl);
            drop(current_credential);
            drop(security);
            drop(image);
            drop(exec_ctl);
            // The preallocated node and reservation token are destroyed only
            // after every publication spin/image guard has been released.
            // The relationship credential follows the same destruction-safe
            // boundary because its free hooks may not run under those gates.
            // The typed `relationship_credential` guard still owns the same
            // Arc until the caller releases the outer publication guard, so
            // this rollback drop also cannot be the final free callback.
            drop(reverse_link);
            drop(retired_relationship);
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
    // Returning the record by value is the rollback contract stated above;
    // boxing it would add an allocation to signal delivery.
    #[allow(clippy::result_large_err)]
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
        job_ctl.ptrace_event = 0;
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
    ) -> Option<(
        ContinueResult,
        Option<PtraceSignalRecord>,
        Option<PtraceRelationshipSnapshot>,
    )> {
        self.resume_ptrace_inner(session, detach, true)
    }

    fn resume_ptrace_inner(
        &self,
        session: PtraceSession,
        detach: bool,
        require_inactive: bool,
    ) -> Option<(
        ContinueResult,
        Option<PtraceSignalRecord>,
        Option<PtraceRelationshipSnapshot>,
    )> {
        let mut pending = self.ptrace_signal.lock();
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return None;
        }

        let mut job_ctl = self.job_ctl.lock();
        if require_inactive && !job_ctl.is_ptrace_inactive_for(session) {
            return None;
        }
        let retired_relationship = detach.then(|| {
            ptrace_ctl
                .clear_session(session)
                .expect("validated ptrace session must clear")
        });

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
        // The caller carries `retired_relationship` past any sleepable outer
        // ptrace-action guard before dropping it. Credential security free
        // hooks may run when this was the final owner.
        Some((result, record, retired_relationship))
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
        job_ctl.ptrace_event = 0;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.continued = false;
        true
    }

    /// Publishes a clone/fork/vfork event for an already traced parent.  The
    /// child PID is retained in the ptrace control word for GETEVENTMSG while
    /// wait status derives its event high byte from the job-control record.
    pub(crate) fn ptrace_event_stop(
        &self,
        session: PtraceSession,
        event: u8,
        message: usize,
    ) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session)
            || (job_ctl.stop_kind == StopKind::Ptrace && job_ctl.state != StopState::Running)
        {
            return false;
        }
        ptrace_ctl.event_message = message;
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = Signo::SIGTRAP as u8;
        job_ctl.ptrace_event = event;
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
        job_ctl.ptrace_event = 0;
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

    pub(crate) fn end_ptrace(&self, session: PtraceSession) -> Option<PtraceRelationshipSnapshot> {
        let (result, record, retired_relationship) =
            self.resume_ptrace_inner(session, true, false)?;
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        self.finish_ptrace_resume(result);
        Some(retired_relationship.expect("end_ptrace always detaches the validated relationship"))
    }

    pub(crate) fn clear_ptrace(&self) -> Option<PtraceRelationshipSnapshot> {
        let (relationship, record) = {
            let mut pending = self.ptrace_signal.lock();
            let mut ptrace_ctl = self.ptrace_ctl.lock();
            let relationship = ptrace_ctl.clear_active();
            (relationship, pending.take())
        };
        if let Some(record) = record {
            super::timer::acknowledge_posix_timer_signal(self, record.info());
            drop(record);
        }
        // The process-exit caller keeps this owner until its outer
        // ptrace-action guard is gone.
        relationship
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
        PtraceReverseLinkDrain {
            next,
            retained: None,
        }
    }

    pub(crate) fn clear_ptrace_tracees_for_task(
        &self,
        tracer_kernel_tid: Pid,
    ) -> PtraceReverseLinkDrain {
        let mut tracees = self.ptrace_tracees.lock();
        let next = tracees.drain_task(tracer_kernel_tid);
        drop(tracees);
        PtraceReverseLinkDrain {
            next,
            retained: None,
        }
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
            || self.cgroup_freeze_requested.load(Ordering::Acquire)
    }

    /// Requests a scheduler-backed cgroup freezer park at every thread's next
    /// interruptible task boundary.  The caller interrupts live members after
    /// this release store; user-return and signal paths then enter the shared
    /// stop wait without exposing job-control state.
    pub(crate) fn request_cgroup_freeze(&self) {
        self.cgroup_freeze_requested.store(true, Ordering::Release);
        self.stop_event.wake();
    }

    /// Releases only the cgroup freezer reason and wakes its parked threads.
    pub(crate) fn thaw_cgroup_freeze(&self) {
        self.cgroup_freeze_requested.store(false, Ordering::Release);
        self.stop_event.wake();
    }

    pub(crate) fn cgroup_freeze_requested(&self) -> bool {
        self.cgroup_freeze_requested.load(Ordering::Acquire)
    }

    pub(crate) fn enter_cgroup_freezer(&self) {
        self.cgroup_frozen_threads.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn leave_cgroup_freezer(&self) {
        let previous = self.cgroup_frozen_threads.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "cgroup freezer thread count underflow");
    }

    /// A process is fully frozen only after every currently live thread has
    /// entered the scheduler's stopped wait.  A process with no live threads
    /// is already quiescent, so exit cannot leave a freezing cgroup stuck.
    pub(crate) fn cgroup_freeze_complete(&self) -> bool {
        self.cgroup_freeze_requested()
            && self.cgroup_frozen_threads.load(Ordering::Acquire)
                >= self.proc.thread_count() as usize
    }

    /// Begins a job-control stop transition.
    pub fn begin_stop(&self, signo: u8) -> bool {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopping;
        job_ctl.stop_signal = signo;
        job_ctl.ptrace_event = 0;
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

    /// Begins a multi-thread exec de-threading phase. An in-flight thread
    /// publication is transient: wait on `exec_event` and retry WouldBlock.
    pub fn begin_exec(&self, owner: Pid) -> AxResult<()> {
        begin_exec_control(&mut self.exec_ctl.lock(), owner)
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
        // A process CPU-clock sleeper pins ProcessData rather than an
        // individual runnable thread.  Group exit closes that accounting
        // domain, so publish the lifecycle edge after releasing exec_ctl.
        super::notify_cpu_clock_sleepers();
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
        self.prepare_initial_thread_admission(ProcessInitialAdmission::Ordinary(publication))
    }

    pub(crate) fn prepare_scoped_initial_thread(
        self: &Arc<Self>,
        publication: ScopedInitialProcessAdmission,
    ) -> AxResult<InitialProcessThreadAdmission> {
        self.prepare_initial_thread_admission(ProcessInitialAdmission::ScopeInit(publication))
    }

    pub(crate) fn prepare_initial_thread_admission(
        self: &Arc<Self>,
        publication: ProcessInitialAdmission,
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
        if release_exec_control_owner(&self.exec_ctl, owner) {
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
        if release_vfork_control_parent(&self.vfork_ctl) {
            self.vfork_event.wake();
        }
    }
}

fn begin_exec_control(exec_ctl: &mut ExecControlState, owner: Pid) -> AxResult<()> {
    if exec_ctl.group_exit {
        return Err(AxError::Interrupted);
    }
    match exec_ctl.owner {
        Some(curr) if curr == owner => Ok(()),
        None if exec_ctl.pending_thread_additions == 0 => {
            exec_ctl.owner = Some(owner);
            Ok(())
        }
        None => Err(AxError::WouldBlock),
        Some(_) => Err(AxError::Interrupted),
    }
}

fn release_exec_control_owner(exec_ctl: &SpinNoIrq<ExecControlState>, owner: Pid) -> bool {
    let mut exec_ctl = exec_ctl.lock();
    if exec_ctl.owner != Some(owner) {
        return false;
    }
    exec_ctl.owner = None;
    true
}

fn release_vfork_control_parent(vfork_ctl: &SpinNoIrq<VforkControlState>) -> bool {
    vfork_ctl.lock().parent_tid.take().is_some()
}

impl Drop for ProcessData {
    fn drop(&mut self) {
        let executable = *self.executable.lock();
        executable::release(executable);
    }
}

#[cfg(test)]
mod tests;
