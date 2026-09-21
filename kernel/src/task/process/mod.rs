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
        PtraceRelationshipOrigin, PtraceRelationshipSnapshot, PtraceSession, StopFilter, StopKind,
        StopReport, StopState, VforkControlState,
    },
    resources::Rlimits,
    security::LandlockDomain,
    signal::PtraceSignalRecord,
    thread::{
        TaskParentNode, TaskParentPublicationGuard, lock_task_parent_publication,
        resolve_retained_task_parent,
    },
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

mod admission;
mod group_leader;
mod identity;
mod image;
mod mempolicy;
mod namespaces;
mod ptrace;
mod session;
mod zombie;

pub(crate) use admission::*;
pub(crate) use group_leader::*;
pub(crate) use identity::*;
pub(crate) use image::*;
pub use mempolicy::Mempolicy;
pub(crate) use mempolicy::*;
pub(crate) use namespaces::*;
pub(crate) use ptrace::*;
pub(crate) use session::*;
pub(crate) use zombie::*;
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
static PROCESS_DOMAIN: Once<ProcessDomain> = Once::new();

/// Initializes the sole kernel-owned process domain before publishing init.
pub(crate) fn init_process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.try_call_once(|| ProcessDomain::try_new().map_err(process_error))
}

/// Returns the process domain after boot initialization.
pub(crate) fn process_domain() -> AxResult<&'static ProcessDomain> {
    PROCESS_DOMAIN.get().ok_or(AxError::BadState)
}
/// [`Process`]-shared data.
pub struct ProcessData {
    /// Immutable resource domain, inherited by fork and retained across exec.
pub(crate)     world: crate::task::WorldId,
    /// The process.
pub(crate)     proc: Arc<Process>,
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
pub(crate)     executable: SpinNoIrq<Option<ExecutableKey>>,
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
    /// Serializes a whole `brk` transaction — limit classification, VMA
    /// growth/shrink, and the final break publication — the way Linux's
    /// `mmap_write_lock` does in `SYSCALL_DEFINE1(brk)`.  Without it two
    /// concurrent brk calls could classify against the same stale break.
    brk_lock: Mutex<()>,
    /// `signal_struct::timer_create_restore_ids`, the CRIU timer-restore mode.
    timer_restore_ids: AtomicBool,
    /// `signal_struct::autoreap`, set only by `clone3(CLONE_AUTOREAP)`.
    ///
    /// The bit belongs to the child, not to its parent: a process created
    /// this way is reaped as it exits and reports nothing, whatever the
    /// parent's `SIGCHLD` disposition is.
    autoreap: AtomicBool,

    /// The resource limits, shared with the durable group-leader identity so
    /// an unreaped zombie still reports the target's own limits.
    pub rlim: Arc<RwLock<Rlimits>>,

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
pub(crate)     signal_pending_event: Arc<PollSet>,

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
pub(crate)     posix_timers: SpinNoIrq<Vec<Option<PosixTimer>>>,
    /// Process-wide `setitimer(2)` state. Real-time alarm actions and CPU-time
    /// charges are serialized here; no thread-local `RefCell` is accessed by
    /// an alarm worker.
pub(crate)     process_itimers: SpinNoIrq<ProcessITimers>,
    /// Monotonic thread-group CPU clock. Writers publish each task-local
    /// user/system interval exactly once; RLIMIT_CPU consumes this lifetime
    /// total, while armed VIRTUAL/PROF timers use the eligible clocks below.
pub(crate)     process_cpu_total_ns: AtomicU64,
    /// Durable fail-closed marker if any process CPU clock saturates instead
    /// of wrapping into a reused accounting domain.
pub(crate)     process_cpu_accounting_overflowed: AtomicBool,
    /// Independently rebased eligible clocks for ITIMER_VIRTUAL/PROF. Even
    /// epochs are stable; odd epochs fence an arm transition while the owner
    /// waits for already-admitted IRQ writers to retire.
pub(crate)     process_itimer_virtual_epoch: AtomicU64,
pub(crate)     process_itimer_virtual_writers: AtomicUsize,
pub(crate)     process_itimer_virtual_clock_ns: AtomicU64,
pub(crate)     process_itimer_prof_epoch: AtomicU64,
pub(crate)     process_itimer_prof_writers: AtomicUsize,
pub(crate)     process_itimer_prof_clock_ns: AtomicU64,
    /// Lock-free fast-path publication for process CPU timers. Accounting
    /// avoids the shared timer lock while neither VIRTUAL nor PROF is armed.
pub(crate)     process_itimer_cpu_armed: AtomicU8,
    /// True while the canonical RLIMIT_CPU soft limit is finite. The timer IRQ
    /// uses this only to request a later task-context policy boundary.
pub(crate)     process_rlimit_cpu_active: AtomicBool,
    /// Standard timer signals awaiting a scheduler-safe task-context drain.
pub(crate)     process_itimer_pending: AtomicU8,
    /// Encoded owner CPU (+1) for the queued process-timer node. This is the
    /// exact wake target when a producer observes an already queued token;
    /// zero means the consumer handoff is currently unowned.
pub(crate)     process_itimer_work_owner_cpu: AtomicUsize,
    /// Intrusive single-consumer work node used to defer process-timer signal
    /// publication out of IRQ-off context-switch accounting.
pub(crate)     process_itimer_work_queued: AtomicBool,
pub(crate)     process_itimer_work_node: ProcessITimerWorkNode,
    /// Preallocated RCU-published reverse subscriptions for foreign encoded
    /// CPU-clock POSIX timers targeting this process.
pub(crate)     foreign_cpu_timer_subscribers: ForeignCpuTimerSubscriberPool,

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
    /// Exact kernel tid whose relationship owns `ptrace_ctl.options`, tagged
    /// with that relationship's publishing generation.
    ///
    /// Linux stores `PT_SUSPEND_SECCOMP` in one tracee's `ptrace` word, so the
    /// process-shared option word alone must never suspend a sibling which was
    /// never attached. The generation tag keeps a value retained by a detached
    /// or rolled-back relationship inert.
    ptrace_suspended_tracee: SpinNoIrq<Option<(u64, Pid)>>,
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
    /// Count of live threads that have not entered `do_exit()`; the
    /// counterpart of Linux `signal->quick_threads`.  Thread admission raises
    /// it and `do_exit()` entry lowers it, so `all_threads_exiting()` is
    /// exactly the count reaching zero.
    quick_threads: AtomicUsize,
    /// Serializes setpgid with successful exec publication; fork starts false.
pub(crate)     exec_committed: Mutex<bool>,
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

#[cfg(test)]
mod pkey_tests;

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
        let rlimits = group_leader_identity.rlimits.clone();
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
            brk_lock: Mutex::new(()),
            // `copy_signal()` allocates a zeroed `signal_struct` and copies
            // neither bit, so every process starts with both off; only
            // `clone3(CLONE_AUTOREAP)` and the timer prctl turn them on.
            timer_restore_ids: AtomicBool::new(false),
            autoreap: AtomicBool::new(false),

            rlim: rlimits,

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
            ptrace_suspended_tracee: SpinNoIrq::new(None),
            ptrace_actions: Mutex::new(()),
            ptrace_signal: Mutex::new(None),
            ptrace_tracees: SpinNoIrq::new(PtraceReverseLinks::default()),
            exec_ctl: SpinNoIrq::new(ExecControlState::default()),
            quick_threads: AtomicUsize::new(0),
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

    /// Whether every live thread of this process has entered `do_exit()`.
    ///
    /// This is the local counterpart of Linux `signal->quick_threads == 0`,
    /// which `synchronize_group_exit()` turns into `SIGNAL_GROUP_EXIT` as the
    /// very first act of `do_exit()`.  A thread created afterwards raises the
    /// live count again, just as a new `copy_process()` raises `quick_threads`.
pub(crate) fn all_threads_exiting(&self) -> bool {
        self.quick_threads.load(Ordering::Acquire) == 0
    }

    /// Returns whether Linux `mm/oom_kill.c:__task_will_free_mem()` would
    /// accept this process as an OOM-reap victim.
    ///
    /// A single-threaded task that called `exit(2)` rather than
    /// `exit_group(2)` is therefore already dying as far as the predicate is
    /// concerned, long before it becomes a zombie.  The remaining conjuncts of
    /// Linux's `task_will_free_mem()` live in the syscall: the mm owner is
    /// resolved by [`crate::syscall::mm::process_mrelease_has_live_mm_thread`],
    /// `mm_users > 1` becomes the live `CLONE_VM` sharer scan, and `MMF_OOM_SKIP`
    /// has no counterpart because this kernel's reaper never runs `exit_mmap`.
pub(crate) fn oom_reap_eligible(&self) -> bool {
        tk_linux_mm::task_dying(tk_linux_mm::FreeMemFacts {
            group_exit: self.group_exit_in_progress() || self.all_threads_exiting(),
            thread_group_empty: self.target_thread_group_empty(),
            pf_exiting: self.all_threads_exiting(),
            ..Default::default()
        })
    }

    /// Linux `thread_group_empty(task)` for the task a process pidfd
    /// addresses: the group leader.  The leader's `thread_group` list is
    /// empty exactly when no sibling thread is still live — a live leader at
    /// a live count of one, or an already detached (zombie) leader once the
    /// live count reaches zero.
    fn target_thread_group_empty(&self) -> bool {
        self.proc.has_only_thread(self.proc.pid()) || self.proc.thread_count() == 0
    }

    /// Records the admission of one live thread, the local counterpart of
    /// `copy_process()` raising `quick_threads`.
pub(crate) fn note_thread_admitted(&self) {
        self.quick_threads.fetch_add(1, Ordering::AcqRel);
    }

    /// Records that one thread of this process has entered `do_exit()`,
    /// lowering the not-yet-exiting live count exactly like Linux's
    /// `quick_threads` decrement in `synchronize_group_exit()`.
    pub(crate) fn note_thread_exit_started(&self) -> bool {
        let previous = self.quick_threads.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous != 0, "quick_threads underflow");
        previous == 1
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

    /// Serializes one `brk` transaction against all others on this mm.
pub(crate) fn brk_lock(&self) -> &Mutex<()> {
        &self.brk_lock
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
pub(crate) fn ptrace_clone_snapshot(
        &self,
        kernel_tid: Pid,
    ) -> Option<(PtraceRelationshipSnapshot, u32, bool)> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let mut options = ptrace_ctl.options;
        // A sibling must not inherit another thread's seccomp suspension into
        // its child. Linux copies current->ptrace, not the group's options.
        if !ptrace_seccomp_suspended_for_tid(&ptrace_ctl, &self.ptrace_suspended_tracee, kernel_tid)
        {
            options &= !tk_linux_process::ptrace_options::SUSPEND_SECCOMP;
        }
        Some((
            ptrace_ctl.active_relationship()?,
            options,
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

    /// Reports whether the tracer of one exact relationship asked for
    /// `PTRACE_O_EXITKILL`.
    ///
    /// Linux stores the option as `PT_EXITKILL` in the *tracee's* `ptrace` word
    /// and `exit_ptrace()` tests it while the relationship is still published:
    ///
    /// ```c
    /// 	list_for_each_entry_safe(p, n, &tracer->ptraced, ptrace_entry) {
    /// 		if (unlikely(p->ptrace & PT_EXITKILL))
    /// 			send_sig_info(SIGKILL, SEND_SIG_PRIV, p);
    /// ```
    ///
    /// Sampling under the same guard that retires the relationship keeps a
    /// detach/reattach by the same numeric tracer from making the new
    /// relationship inherit the old request.
pub(crate) fn ptrace_exitkill_requested(&self, session: PtraceSession) -> bool {
        let ptrace_ctl = self.ptrace_ctl.lock();
        ptrace_ctl.active_session() == Some(session)
            && ptrace_ctl.options & tk_linux_process::ptrace_options::EXITKILL != 0
    }

    /// Reports whether the active relationship's tracer suspended this exact
    /// task's seccomp policy with `PTRACE_O_SUSPEND_SECCOMP`.
    ///
    /// Linux keeps the option in the *tracee's* `ptrace` word as
    /// `PT_SUSPEND_SECCOMP`, and `__secure_computing()` tests it before it
    /// looks at the seccomp mode at all (kernel/seccomp.c):
    ///
    /// ```c
    /// 	if (IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) &&
    /// 	    unlikely(current->ptrace & PT_SUSPEND_SECCOMP))
    /// 		return 0;
    /// ```
    ///
    /// TheKernel publishes one option word per thread group, so the exact
    /// traced kernel tid recorded at publication is part of the query. A
    /// detach resumes enforcement because `clear_session()` zeroes `options`
    /// and the retirement paths clear the tag with it.
pub(crate) fn ptrace_seccomp_suspended_for(&self, kernel_tid: Pid) -> bool {
        ptrace_seccomp_suspended_for_tid(
            &self.ptrace_ctl.lock(),
            &self.ptrace_suspended_tracee,
            kernel_tid,
        )
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
        // The option word and the exact task it describes publish under one
        // ptrace-control critical section, so `__secure_computing()` can never
        // read a `SUSPEND_SECCOMP` word whose traced tid is not this one.
        record_ptrace_suspended_tracee(&self.ptrace_suspended_tracee, session, target.kernel_tid());
        if let Err((error, reverse_link)) = reverse_link.publish(session) {
            let retired_relationship = ptrace_ctl
                .rollback_begin(session, old_generation)
                .expect("new ptrace relationship owns rollback session");
            *self.ptrace_suspended_tracee.lock() = None;
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
            let retired = ptrace_ctl
                .clear_session(session)
                .expect("validated ptrace session must clear");
            *self.ptrace_suspended_tracee.lock() = None;
            retired
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
            *self.ptrace_suspended_tracee.lock() = None;
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

    /// Peeks at the stopped status without consuming it (for WNOWAIT).
    ///
    /// `filter` carries the waiter's own identity, because
    /// `wait_consider_task()` decides stop visibility per waiter rather than
    /// once per task (`StopFilter`).
pub(crate) fn peek_stop_status(&self, filter: StopFilter) -> Option<StopReport> {
        let job_ctl = self.job_ctl.lock();
        job_ctl.stop_report_for(filter)
    }

    /// Claims one already-selected stop report so a waiter can complete
    /// userspace copies first.
    ///
    /// The report itself identifies the stop, so a stop that was replaced by a
    /// later relationship — or one another thread of the group already
    /// reported — is not consumed by a stale waiter.
pub(crate) fn claim_stop_status(&self, report: StopReport) -> Option<StopReport> {
        let mut job_ctl = self.job_ctl.lock();
        if job_ctl.stop_reported {
            return None;
        }
        let current = job_ctl.current_stop_report()?;
        if current != report {
            return None;
        }
        job_ctl.stop_reported = true;
        Some(current)
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
