use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FileNode, FileNodeOps, Filesystem, FilesystemOps, FsName, FsNameBuf, FsPath, Metadata,
    MetadataUpdate, NamedCreateOptions, NodeFlags, NodeOps, NodePermission, NodeType, Reference,
    RenameRequest, StatFs, UnlinkRequest, VfsError, VfsResult, WeakDirEntry, path::MAX_NAME_LEN,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, Pollable};
#[cfg(any(not(test), target_os = "none"))]
use axsync::Mutex;
use hashbrown::{HashMap, HashSet};
use linux_raw_sys::general::CAP_SYS_ADMIN;
use spin::Lazy;
#[cfg(all(test, not(target_os = "none")))]
use spin::Mutex;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{SignalInfo, Signo};

use super::pseudo_stat_fs;
use crate::{
    file::{
        Directory, OpenCredentials, current_file_operation_security_credential,
        current_file_write_credentials, get_typed_file,
    },
    task::{
        AsThread, Cred, Process, ProcessData, get_process_data, get_process_including_zombie,
        ns_capable, send_signal_to_process_data, try_tasks,
    },
};

const CGROUP_SUPER_MAGIC: u32 = 0x0027_e0eb;
const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
const MAX_CGROUP_CHILDREN: usize = 65_536;
/// Bound recursive hierarchy walks and the number of simultaneously held
/// descendant locks. Cgroup state is synthetic and starts empty on every boot,
/// so enforcing this at create/move admission covers every reachable tree.
const MAX_CGROUP_DEPTH: usize = 256;

fn cgroup_control_file_flags() -> NodeFlags {
    NodeFlags::NON_CACHEABLE | NodeFlags::OPEN_CREDENTIAL
}
/// TheKernel does not yet have system-wide task accounting that can serve as
/// a cgroup membership budget. Keep both membership indexes explicitly
/// bounded until that accounting can own a tunable limit.
const MAX_CGROUP_MEMBERSHIPS: usize = 65_536;

#[derive(Clone, Copy)]
enum RegistryValidation {
    ProductionIdentity,
    #[cfg(test)]
    SyntheticLocal,
}

fn try_reserve_cgroup_child_slot(
    children: &mut HashMap<FsNameBuf, Arc<CgroupDir>>,
    limit: usize,
    grows: bool,
) -> VfsResult<()> {
    if grows && children.len() >= limit {
        return Err(VfsError::NoMemory);
    }
    children.try_reserve(1).map_err(|_| VfsError::NoMemory)
}

fn try_owned(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    Ok(result)
}

fn try_owned_name(value: &FsName) -> VfsResult<FsNameBuf> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(value.as_bytes().len())
        .map_err(|_| VfsError::NoMemory)?;
    result.extend_from_slice(value.as_bytes());
    FsNameBuf::from_vec(result).map_err(|_| VfsError::NoMemory)
}

fn try_owned_control_name(value: &str) -> VfsResult<FsNameBuf> {
    try_owned_name(FsName::new(value.as_bytes()))
}

fn try_join_names<'a, I>(names: I) -> VfsResult<String>
where
    I: Iterator<Item = &'a str> + Clone,
{
    let count = names.clone().count();
    let capacity = names
        .clone()
        .try_fold(0usize, |total, name| total.checked_add(name.len()))
        .and_then(|capacity| capacity.checked_add(count.saturating_sub(1)))
        .and_then(|capacity| capacity.checked_add(usize::from(count != 0)))
        .ok_or(VfsError::NoMemory)?;
    let mut out = String::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| VfsError::NoMemory)?;
    for (index, name) in names.enumerate() {
        if index != 0 {
            out.push(' ');
        }
        out.push_str(name);
    }
    if count != 0 {
        out.push('\n');
    }
    Ok(out)
}

const CONTROL_FILES: &[&str] = &[
    "tasks",
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
    "cgroup.events",
    "cgroup.type",
    "cgroup.freeze",
    "cgroup.kill",
    "pids.max",
    "pids.current",
    "pids.events",
    "pids.peak",
    "cpu.uclamp.min",
    "cpu.uclamp.max",
];
/// One global synthetic-inode budget for a cgroup filesystem.  Per-parent
/// child limits alone do not bound a deep tree; this keeps allocator-backed
/// identity bookkeeping finite until cgroup memory accounting exists.
const MAX_CGROUP_INODES: usize = (MAX_CGROUP_CHILDREN + 1) * (CONTROL_FILES.len() + 1);

/// The unified hierarchy deliberately exposes only controllers with complete
/// task-accounting semantics.  In particular, do not advertise cpu, memory,
/// or io merely because a userspace manager probes for them: claiming one
/// would make its policy decisions depend on controls the kernel cannot
/// enforce.
const ALL_CONTROLLERS: &[&str] = &["pids"];
const KNOWN_V1_CONTROLLERS: &[&str] = &[
    "blkio",
    "cpu",
    "cpuacct",
    "cpuset",
    "debug",
    "devices",
    "freezer",
    "hugetlb",
    "memory",
    "misc",
    "net_cls",
    "net_prio",
    "perf_event",
    "pids",
    "rdma",
];

struct PidMembershipRegistry {
    /// Serializes admission and publication. Registry locks are always taken
    /// after this lock, with the global map before per-cgroup member sets.
    operation: Mutex<()>,
    /// A process has one membership in every hierarchy, rather than one
    /// system-wide membership.  In particular a v1 controller mount must not
    /// evict the unified v2 membership.
    by_pid: Mutex<HashMap<Pid, HashMap<CgroupHierarchyKey, CgroupMembership>>>,
    global_limit: usize,
    per_cgroup_limit: usize,
}

/// One publication bit shared by the global PID index and the target cgroup.
///
/// Fork preparation installs both index entries with this bit clear. Readers
/// filter on the bit, while capacity and identity writers still account the
/// reserved entries. The consuming admission commit is therefore one release
/// store: it cannot allocate, fail halfway through, or expose only one index.
struct CgroupMembershipPublication {
    visible: AtomicBool,
}

impl CgroupMembershipPublication {
    const fn new(visible: bool) -> Self {
        Self {
            visible: AtomicBool::new(visible),
        }
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct CgroupMembership {
    target: Weak<CgroupDir>,
    publication: Arc<CgroupMembershipPublication>,
    /// PID numbers are reusable after reaping.  Every map entry is therefore
    /// tied to the stable core Process identity that created it.
    process_identity: Weak<Process>,
    /// Bound only once the corresponding process identity is globally
    /// published.  cgroup.kill retains this exact object instead of resolving
    /// a numeric PID after dropping the membership snapshot.
    process: Weak<ProcessData>,
}

impl CgroupMembership {
    fn is_visible(&self) -> bool {
        self.publication.is_visible()
    }
}

/// Invisible, fully admitted cgroup membership for one fork child.
///
/// The token holds no registry lock while clone constructs the child. Dropping
/// it removes the exact hidden entries from both indexes; consuming it with
/// [`commit`](Self::commit) publishes both through their shared atomic bit.
#[must_use = "dropping a cgroup fork admission rolls back the hidden membership"]
pub(crate) struct CgroupForkAdmission<'a> {
    registry: &'a PidMembershipRegistry,
    child_pid: Pid,
    reservations: Vec<CgroupForkReservation>,
    /// One publication boundary for the whole inherited membership set.  A
    /// clone must never become visible in only a prefix of its hierarchies.
    publication: Option<Arc<CgroupMembershipPublication>>,
    process_identity: Weak<Process>,
    committed: bool,
}

struct CgroupForkReservation {
    hierarchy: CgroupHierarchyKey,
    target: Arc<CgroupDir>,
}

static PID_CGROUPS: Lazy<PidMembershipRegistry> = Lazy::new(|| {
    PidMembershipRegistry::with_limits(MAX_CGROUP_MEMBERSHIPS, MAX_CGROUP_MEMBERSHIPS)
});

/// System-wide scheduler clamp controls exposed through `/proc/sys/kernel`.
///
/// These are one coupled policy tuple, rather than independent atomics: a
/// reader must never observe a minimum greater than the maximum while two
/// sysctl writes are being serialized. Cgroup-v2 effective limits begin with
/// this tuple and then narrow through their ancestor chain.
#[derive(Clone, Copy)]
struct SchedUtilClampControls {
    minimum: u16,
    maximum: u16,
    minimum_rt_default: u16,
}

impl SchedUtilClampControls {
    const MAX: u16 = 1024;

    const fn unrestricted() -> Self {
        Self {
            minimum: 0,
            maximum: Self::MAX,
            minimum_rt_default: Self::MAX,
        }
    }

    fn set_minimum(&mut self, value: u16) -> VfsResult<()> {
        if value > self.maximum {
            return Err(VfsError::InvalidInput);
        }
        self.minimum = value;
        Ok(())
    }

    fn set_maximum(&mut self, value: u16) -> VfsResult<()> {
        if value > Self::MAX || value < self.minimum {
            return Err(VfsError::InvalidInput);
        }
        self.maximum = value;
        Ok(())
    }

    fn set_minimum_rt_default(&mut self, value: u16) -> VfsResult<()> {
        if value > Self::MAX {
            return Err(VfsError::InvalidInput);
        }
        self.minimum_rt_default = value;
        Ok(())
    }
}

static SCHED_UTIL_CLAMP_CONTROLS: Lazy<Mutex<SchedUtilClampControls>> =
    Lazy::new(|| Mutex::new(SchedUtilClampControls::unrestricted()));

/// Every committed system or cgroup clamp policy update advances this
/// seqlock-style generation.  A task records the even generation for which
/// its scheduler-owned effective bounds were last published.  Failed live
/// recomputations intentionally leave that record stale: the next safe user
/// return retries from the durable policy rather than silently losing a
/// successfully committed control-plane write.
static UCLAMP_POLICY_UPDATE: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
static UCLAMP_POLICY_GENERATION: AtomicU64 = AtomicU64::new(2);

/// The newest committed clamp generation for which at least one live task
/// still needs its runqueue transaction retried.  This is a durable handoff
/// to the existing policy worker: a transient owner/migration failure must
/// not turn a successful cgroup write into a task-local dirty bit that is
/// noticed only at a later, unrelated user-return boundary.
///
/// Zero means that no retry is outstanding. Policy generations start even
/// and non-zero, so zero remains an unambiguous empty marker even when the
/// sequence wraps.
static UCLAMP_RECONCILE_PENDING_GENERATION: AtomicU64 = AtomicU64::new(0);

#[inline]
fn begin_uclamp_policy_update() {
    let previous = UCLAMP_POLICY_GENERATION.load(Ordering::Acquire);
    debug_assert_eq!(previous & 1, 0, "uclamp policy writer was not serialized");
    debug_assert_ne!(
        previous, 0,
        "uclamp policy generation used its empty sentinel"
    );
    UCLAMP_POLICY_GENERATION.store(uclamp_policy_write_generation(previous), Ordering::Release);
}

#[inline]
fn finish_uclamp_policy_update() -> u64 {
    let previous = UCLAMP_POLICY_GENERATION.load(Ordering::Acquire);
    debug_assert_ne!(previous & 1, 0, "uclamp policy update did not begin");
    let generation = uclamp_policy_commit_generation(previous);
    UCLAMP_POLICY_GENERATION.store(generation, Ordering::Release);
    generation
}

/// Generation zero is reserved as the empty ticket in the worker handoff.
/// Policy generations are even, so wrap from the largest usable even value
/// directly to two instead of ever publishing that sentinel.
#[inline]
fn uclamp_policy_write_generation(generation: u64) -> u64 {
    debug_assert_eq!(generation & 1, 0);
    debug_assert_ne!(generation, 0);
    generation.wrapping_add(1)
}

#[inline]
fn uclamp_policy_commit_generation(in_progress: u64) -> u64 {
    debug_assert_eq!(in_progress & 1, 1);
    if in_progress == u64::MAX {
        2
    } else {
        in_progress + 1
    }
}

/// Returns whether `candidate` follows `base` in the wrapping generation
/// serial space.  No more than half the u64 space can be outstanding because
/// updates are serialized, making the usual modular comparison unambiguous.
#[inline]
fn uclamp_generation_is_newer(candidate: u64, base: u64) -> bool {
    debug_assert_ne!(candidate, 0);
    debug_assert_ne!(base, 0);
    candidate != base && candidate.wrapping_sub(base) < (1u64 << 63)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UclampReconcileFailure {
    Scheduler,
    RunQueueUnavailable,
    Unsupported,
}

impl UclampReconcileFailure {
    fn from_task_sched(error: axtask::TaskSchedError) -> Option<Self> {
        match error {
            axtask::TaskSchedError::Scheduler(_) => Some(Self::Scheduler),
            axtask::TaskSchedError::RunQueueUnavailable(_) => Some(Self::RunQueueUnavailable),
            axtask::TaskSchedError::Unsupported => Some(Self::Unsupported),
            axtask::TaskSchedError::TaskExited => None,
        }
    }
}

/// Retain the newest pending policy. An older, late retry must never replace
/// a newer cgroup/sysctl commit in the worker handoff.
fn publish_uclamp_reconcile_pending(generation: u64) {
    publish_uclamp_reconcile_pending_to(&UCLAMP_RECONCILE_PENDING_GENERATION, generation);
    crate::deferred_work::wake_policy_worker();
}

fn publish_uclamp_reconcile_pending_to(pending: &AtomicU64, generation: u64) {
    debug_assert_ne!(generation, 0);
    debug_assert_eq!(generation & 1, 0);
    let mut observed = pending.load(Ordering::Acquire);
    loop {
        if observed != 0 && !uclamp_generation_is_newer(generation, observed) {
            return;
        }
        match pending.compare_exchange_weak(
            observed,
            generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(current) => observed = current,
        }
    }
}

/// Claims one pending generation before doing any fallible work.  A publisher
/// that repeats the *same* generation after this exchange observes zero and
/// installs a fresh ticket, so this worker pass can never acknowledge work
/// that arrived after its claim.
fn claim_uclamp_reconcile_pending() -> Option<u64> {
    let generation = UCLAMP_RECONCILE_PENDING_GENERATION.swap(0, Ordering::AcqRel);
    (generation != 0).then_some(generation)
}

/// Exposed to the already-running policy worker. No IPI path calls this;
/// rebuilding a task snapshot and taking runqueue locks are task-context work.
pub(crate) fn uclamp_reconcile_pending() -> bool {
    UCLAMP_RECONCILE_PENDING_GENERATION.load(Ordering::Acquire) != 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UclampReconcileDrain {
    Idle,
    Converged,
    Retry(UclampReconcileFailure),
}

/// Returns the last fully committed policy generation.  The value is always
/// even; readers that need constraints must use the snapshot helpers below.
pub(crate) fn uclamp_policy_generation() -> u64 {
    loop {
        let generation = UCLAMP_POLICY_GENERATION.load(Ordering::Acquire);
        if generation & 1 == 0 {
            return generation;
        }
        core::hint::spin_loop();
    }
}

pub(crate) fn sched_util_clamp_min() -> u16 {
    SCHED_UTIL_CLAMP_CONTROLS.lock().minimum
}

pub(crate) fn sched_util_clamp_max() -> u16 {
    SCHED_UTIL_CLAMP_CONTROLS.lock().maximum
}

pub(crate) fn sched_util_clamp_min_rt_default() -> u16 {
    SCHED_UTIL_CLAMP_CONTROLS.lock().minimum_rt_default
}

pub(crate) fn set_sched_util_clamp_min(value: u16) -> VfsResult<()> {
    let tasks = snapshot_live_uclamp_tasks()?;
    let generation = {
        let _policy = UCLAMP_POLICY_UPDATE.lock();
        let mut controls = SCHED_UTIL_CLAMP_CONTROLS.lock();
        let mut next = *controls;
        next.set_minimum(value)?;
        begin_uclamp_policy_update();
        *controls = next;
        finish_uclamp_policy_update()
    };
    republish_live_uclamp_tasks(tasks, None, generation).map(|_| ())
}

pub(crate) fn set_sched_util_clamp_max(value: u16) -> VfsResult<()> {
    let tasks = snapshot_live_uclamp_tasks()?;
    let generation = {
        let _policy = UCLAMP_POLICY_UPDATE.lock();
        let mut controls = SCHED_UTIL_CLAMP_CONTROLS.lock();
        let mut next = *controls;
        next.set_maximum(value)?;
        begin_uclamp_policy_update();
        *controls = next;
        finish_uclamp_policy_update()
    };
    republish_live_uclamp_tasks(tasks, None, generation).map(|_| ())
}

pub(crate) fn set_sched_util_clamp_min_rt_default(value: u16) -> VfsResult<()> {
    let tasks = snapshot_live_uclamp_tasks()?;
    let generation = {
        let _policy = UCLAMP_POLICY_UPDATE.lock();
        let mut controls = SCHED_UTIL_CLAMP_CONTROLS.lock();
        let mut next = *controls;
        next.set_minimum_rt_default(value)?;
        begin_uclamp_policy_update();
        *controls = next;
        finish_uclamp_policy_update()
    };
    republish_live_uclamp_tasks(tasks, None, generation).map(|_| ())
}

/// Captures task references before changing a control plane.  The following
/// scheduler transactions therefore never carry cgroup/process/address-space
/// locks into a runqueue lock.
fn snapshot_live_uclamp_tasks() -> VfsResult<Vec<axtask::AxTaskRef>> {
    try_tasks().map_err(|_| VfsError::NoMemory)
}

/// Snapshots all non-scheduler policy affecting a process's clamp.  Callers
/// must take this before entering a task runqueue transaction.
pub(crate) fn uclamp_constraints_for_pid(pid: Pid) -> axtask::UclampConstraints {
    uclamp_constraints_for_pid_with_generation(pid).0
}

fn uclamp_constraints_for_pid_with_generation(pid: Pid) -> (axtask::UclampConstraints, u64) {
    loop {
        let before = uclamp_policy_generation();
        let (cgroup_minimum, cgroup_maximum) = cgroup_for_pid(pid)
            .map(|dir| dir.uclamp_effective())
            .unwrap_or((0, 1024));
        let controls = *SCHED_UTIL_CLAMP_CONTROLS.lock();
        let constraints = axtask::UclampConstraints {
            system_minimum: controls.minimum,
            system_maximum: controls.maximum,
            cgroup_minimum,
            cgroup_maximum,
            rt_default_minimum: controls.minimum_rt_default,
        };
        if UCLAMP_POLICY_GENERATION.load(Ordering::Acquire) == before {
            return (constraints, before);
        }
    }
}

/// Snapshot the constraints selected by an invisible fork admission.
///
/// A child is intentionally not yet present in the public PID lookup when
/// this runs, but its target cgroup has already been reserved.  Using that
/// target closes the otherwise-visible first-runnable window for fork and
/// clone3 children (including CLONE_INTO_CGROUP).
pub(crate) fn uclamp_constraints_for_fork_admission(
    admission: &CgroupForkAdmission<'_>,
) -> axtask::UclampConstraints {
    loop {
        let before = uclamp_policy_generation();
        let (cgroup_minimum, cgroup_maximum) = admission
            .reservations
            .iter()
            .find(|reservation| reservation.hierarchy.version == CgroupVersion::V2)
            .map(|reservation| &reservation.target)
            .map(|dir| dir.uclamp_effective())
            .unwrap_or((0, 1024));
        let controls = *SCHED_UTIL_CLAMP_CONTROLS.lock();
        let constraints = axtask::UclampConstraints {
            system_minimum: controls.minimum,
            system_maximum: controls.maximum,
            cgroup_minimum,
            cgroup_maximum,
            rt_default_minimum: controls.minimum_rt_default,
        };
        if UCLAMP_POLICY_GENERATION.load(Ordering::Acquire) == before {
            return constraints;
        }
    }
}

/// Each task commit atomically replaces its effective bounds and its one
/// runqueue multiset entry.  Task exit/migration races do not undo a completed
/// cgroup control transaction.  Instead, the durable policy-worker handoff
/// records the newest failed generation and retries it promptly in ordinary
/// task context.
fn republish_live_uclamp_tasks(
    tasks: Vec<axtask::AxTaskRef>,
    only_process: Option<Pid>,
    committed_generation: u64,
) -> VfsResult<Option<UclampReconcileFailure>> {
    let mut retry = None;
    for task in tasks {
        // Kernel helper/idle tasks have no Linux process or cgroup policy.
        let Some(thread) = task.try_as_thread() else {
            continue;
        };
        let pid = thread.proc_data.proc.pid();
        if only_process.is_some_and(|target| target != pid) {
            continue;
        }
        let (constraints, generation) = uclamp_constraints_for_pid_with_generation(pid);
        match axtask::recompute_task_uclamp(&task, constraints) {
            Ok(commit) => {
                crate::syscall::publish_sched_commit(&task, commit);
                // A later policy commit won the race with this task's
                // recomputation. Keep it dirty for the safe-boundary retry.
                if generation == committed_generation
                    && UCLAMP_POLICY_GENERATION.load(Ordering::Acquire) == generation
                {
                    thread.publish_uclamp_policy_generation(generation);
                } else {
                    // Do not clear an older pending generation when a newer
                    // writer raced this task's transaction. The worker will
                    // re-evaluate against the newer durable policy.
                    publish_uclamp_reconcile_pending(generation);
                }
            }
            Err(error) => {
                // `TaskExited` is the only terminal outcome. Every scheduler
                // mechanism failure is explicitly classified and retained for
                // the policy worker; it must not be silently converted into a
                // user-return-only retry.
                if let Some(kind) = UclampReconcileFailure::from_task_sched(error) {
                    publish_uclamp_reconcile_pending(generation);
                    // The worker reports this at its bounded retry cadence;
                    // do not emit one log line per task per failed pass.
                    retry = Some(match retry {
                        Some(UclampReconcileFailure::Unsupported) => {
                            UclampReconcileFailure::Unsupported
                        }
                        Some(UclampReconcileFailure::RunQueueUnavailable)
                            if kind == UclampReconcileFailure::Scheduler =>
                        {
                            UclampReconcileFailure::RunQueueUnavailable
                        }
                        _ => kind,
                    });
                }
            }
        }
    }
    Ok(retry)
}

/// Performs one policy-worker reconciliation pass for the newest failed
/// generation. This deliberately resnapshots all live tasks: a task may have
/// migrated, exited, or joined the cgroup after the original bounded pass.
///
/// If a retry fails again, [`republish_live_uclamp_tasks`] republishes the
/// same (or newer) generation before this function returns. The worker applies
/// bounded backoff and retries without requiring the affected task to return
/// to user space, receive an unrelated syscall, or wait for a scheduler tick.
pub(crate) fn drain_pending_uclamp_reconcile() -> UclampReconcileDrain {
    let Some(pending) = claim_uclamp_reconcile_pending() else {
        return UclampReconcileDrain::Idle;
    };
    let Ok(tasks) = snapshot_live_uclamp_tasks() else {
        // Claim happened before allocation. Reinstall this exact ticket so a
        // concurrent same-generation publisher and this failure both survive.
        publish_uclamp_reconcile_pending(pending);
        return UclampReconcileDrain::Retry(UclampReconcileFailure::Scheduler);
    };

    match republish_live_uclamp_tasks(tasks, None, pending) {
        Ok(Some(_))
            if {
                let next = UCLAMP_RECONCILE_PENDING_GENERATION.load(Ordering::Acquire);
                next != 0 && uclamp_generation_is_newer(next, pending)
            } =>
        {
            // A newer committed policy owns the next pass.
            UclampReconcileDrain::Converged
        }
        Ok(Some(failure)) => UclampReconcileDrain::Retry(failure),
        Ok(None) => match UCLAMP_RECONCILE_PENDING_GENERATION.load(Ordering::Acquire) {
            0 => UclampReconcileDrain::Converged,
            next if uclamp_generation_is_newer(next, pending) => {
                // A newer policy superseded this pass; reset retry backoff
                // so the new durable constraints publish promptly.
                UclampReconcileDrain::Converged
            }
            _ => {
                // A same-generation publisher arrived after the claim. Its
                // fresh ticket must remain pending and receives the normal
                // bounded retry backoff rather than being silently cleared.
                UclampReconcileDrain::Retry(UclampReconcileFailure::Scheduler)
            }
        },
        Err(_) => {
            // Preserve the claimed generation if the wrapper ever gains a
            // pre-transaction failure mode.
            publish_uclamp_reconcile_pending(pending);
            UclampReconcileDrain::Retry(UclampReconcileFailure::Scheduler)
        }
    }
}

/// Retry a previously failed live-clamp recomputation before returning the
/// current task to user mode.  This is deliberately task context rather than
/// a timer/switch callback: stable runqueue acquisition may redirect while a
/// task migrates, and a control-plane commit must never be dropped merely
/// because that bounded attempt lost the race.
pub(crate) fn reconcile_current_uclamp_if_stale() {
    let task = axtask::current();
    let Some(thread) = task.try_as_thread() else {
        return;
    };
    let pid = thread.proc_data.proc.pid();
    let (constraints, generation) = uclamp_constraints_for_pid_with_generation(pid);
    if thread.uclamp_policy_generation() == generation {
        return;
    }
    match axtask::recompute_task_uclamp(&task, constraints) {
        Ok(commit) => {
            crate::syscall::publish_sched_commit(&task, commit);
            if UCLAMP_POLICY_GENERATION.load(Ordering::Acquire) == generation {
                thread.publish_uclamp_policy_generation(generation);
            } else {
                // A writer won after the local snapshot. Give the policy
                // worker a prompt retry instead of relying on another user
                // return for this task.
                publish_uclamp_reconcile_pending(generation);
            }
        }
        Err(error) => {
            if UclampReconcileFailure::from_task_sched(error).is_some() {
                // User-return is an opportunistic fast path only. Its bounded
                // scheduler attempt must rejoin the durable worker lane on
                // every nonterminal failure.
                publish_uclamp_reconcile_pending(generation);
            }
        }
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum CgroupVersion {
    V1,
    V2,
}

/// One cgroup hierarchy is kernel state, not mount state.  In particular a
/// second cgroup2 mount must never manufacture a second tree (and thereby a
/// second set of limits and memberships).  A mount only selects a root view
/// into this stable hierarchy.
struct CgroupHierarchy {
    version: CgroupVersion,
    controllers: Vec<String>,
    /// The canonical superblock owns the actual nodes.  Views share its VFS
    /// identity/cache through `Filesystem::try_new_view` below.
    filesystem: Filesystem,
    root: Arc<CgroupDir>,
}

/// Opaque cgroup-namespace root.  It deliberately retains the actual stable
/// hierarchy node, rather than a pathname: renames above the namespace root
/// must not change what the namespace sees.
#[derive(Clone)]
pub(crate) struct CgroupNamespaceRoots {
    views: Arc<CgroupNamespaceRootSet>,
}

struct CgroupNamespaceRootSet {
    roots: HashMap<CgroupHierarchyKey, Arc<CgroupDir>>,
}

impl CgroupNamespaceRoots {
    fn try_from_live_hierarchies(
        override_root: Option<(&Arc<CgroupHierarchy>, Arc<CgroupDir>)>,
    ) -> VfsResult<Self> {
        let hierarchies = CGROUP_HIERARCHIES.lock();
        let mut roots = HashMap::new();
        roots
            .try_reserve(hierarchies.len())
            .map_err(|_| VfsError::NoMemory)?;
        for (key, hierarchy) in hierarchies.iter() {
            let root = override_root
                .as_ref()
                .filter(|(overridden, _)| Arc::ptr_eq(overridden, hierarchy))
                .map(|(_, root)| root.clone())
                .unwrap_or_else(|| hierarchy.root.clone());
            roots.insert(key.clone(), root);
        }
        Arc::try_new(CgroupNamespaceRootSet { roots })
            .map(|views| Self { views })
            .map_err(|_| VfsError::NoMemory)
    }

    fn root_for(&self, hierarchy: &Arc<CgroupHierarchy>) -> VfsResult<Arc<CgroupDir>> {
        let key = CgroupHierarchyKey {
            version: hierarchy.version,
            controllers: hierarchy.controllers.clone(),
        };
        // A hierarchy can be admitted after this namespace was created.  It
        // did not exist at snapshot time, so its only non-leaking view is the
        // hierarchy root (there is no creator membership within it yet).
        Ok(self
            .views
            .roots
            .get(&key)
            .cloned()
            .unwrap_or_else(|| hierarchy.root.clone()))
    }

    /// Constructs a mount view rooted at this namespace's stable cgroup
    /// node.  The caller's mount namespace owns placement; this object owns
    /// neither mount topology nor task membership.
    pub(crate) fn try_mount_view(&self) -> VfsResult<Filesystem> {
        let hierarchy = cgroup_v2_hierarchy()?;
        self.try_mount_view_for(&hierarchy)
    }

    fn try_mount_view_for(&self, hierarchy: &Arc<CgroupHierarchy>) -> VfsResult<Filesystem> {
        let root = self.root_for(hierarchy)?;
        CgroupMountView::try_new(hierarchy.clone(), root)?.filesystem()
    }
}

/// A per-mount view, intentionally tiny.  It shares the hierarchy's VFS
/// identity and root-cache lifetime, but its root dentry is the namespace
/// root selected by the mount/namespace transaction.
struct CgroupMountView {
    hierarchy: Arc<CgroupHierarchy>,
    root: DirEntry,
}

impl CgroupMountView {
    fn try_new(hierarchy: Arc<CgroupHierarchy>, root: Arc<CgroupDir>) -> VfsResult<Arc<Self>> {
        let root = root
            .this
            .lock()
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::InvalidInput)?;
        Arc::try_new(Self { hierarchy, root }).map_err(|_| VfsError::NoMemory)
    }

    fn filesystem(self: Arc<Self>) -> VfsResult<Filesystem> {
        let source = self.hierarchy.filesystem.clone();
        Filesystem::try_new_view(self, &source)
    }
}

impl FilesystemOps for CgroupMountView {
    fn name(&self) -> &str {
        self.hierarchy.filesystem.name()
    }

    fn root_dir(&self) -> DirEntry {
        self.root.clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        self.hierarchy.filesystem.stat()
    }

    fn unmount(&self) {
        // The hierarchy is global kernel state.  Dropping one mount view
        // cannot remove its nodes or detach tasks from it.
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct CgroupHierarchyKey {
    version: CgroupVersion,
    controllers: Vec<String>,
}

const MAX_CGROUP_HIERARCHIES: usize = 4;

static CGROUP_HIERARCHIES: Lazy<Mutex<HashMap<CgroupHierarchyKey, Arc<CgroupHierarchy>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn new_cgroup_v1(controllers: Vec<String>) -> VfsResult<Filesystem> {
    let hierarchy = cgroup_hierarchy(CgroupVersion::V1, controllers)?;
    CgroupMountView::try_new(hierarchy.clone(), hierarchy.root.clone())?.filesystem()
}

pub fn new_cgroup_v2() -> VfsResult<Filesystem> {
    let mut controllers = Vec::new();
    controllers
        .try_reserve_exact(ALL_CONTROLLERS.len())
        .map_err(|_| VfsError::NoMemory)?;
    for controller in ALL_CONTROLLERS {
        controllers.push(try_owned(controller)?);
    }
    let hierarchy = cgroup_hierarchy(CgroupVersion::V2, controllers)?;
    CgroupMountView::try_new(hierarchy.clone(), hierarchy.root.clone())?.filesystem()
}

/// Returns the initial cgroup namespace root.  This is a stable node handle,
/// so callers can retain it across mount/unmount and parent directory rename.
pub(crate) fn root_namespace_roots() -> AxResult<CgroupNamespaceRoots> {
    let _ = cgroup_v2_hierarchy()?;
    // This kernel only admits the fully implemented pids v1 controller, but
    // it is still a distinct hierarchy and therefore needs its own stable
    // namespace root before any namespace can be cloned.
    let mut v1_controllers = Vec::new();
    v1_controllers
        .try_reserve_exact(1)
        .map_err(|_| AxError::NoMemory)?;
    v1_controllers.push(try_owned("pids").map_err(AxError::from)?);
    let _ = cgroup_hierarchy(CgroupVersion::V1, v1_controllers)?;
    CgroupNamespaceRoots::try_from_live_hierarchies(None).map_err(AxError::from)
}

/// Captures the cgroup node containing `pid` as a namespace root.  An
/// untracked process sees the initial hierarchy root, matching the Linux
/// fallback for a task outside a delegated subtree.
pub(crate) fn cgroup_namespace_roots_for_pid(pid: Pid) -> AxResult<CgroupNamespaceRoots> {
    let memberships = PID_CGROUPS.memberships(pid);
    if !memberships.is_empty() {
        let hierarchies = CGROUP_HIERARCHIES.lock();
        let mut roots = HashMap::new();
        roots
            .try_reserve(hierarchies.len())
            .map_err(|_| AxError::NoMemory)?;
        for (key, hierarchy) in hierarchies.iter() {
            let root = memberships
                .get(key)
                .cloned()
                .unwrap_or_else(|| hierarchy.root.clone());
            roots.insert(key.clone(), root);
        }
        return Arc::try_new(CgroupNamespaceRootSet { roots })
            .map(|views| CgroupNamespaceRoots { views })
            .map_err(|_| AxError::NoMemory);
    }
    root_namespace_roots()
}

/// Mount a cgroup2 view from an already prepared cgroup namespace root.
/// `mount.rs` will use this during fsopen/fsmount transaction publication.
pub(crate) fn new_cgroup_v2_for_namespace(roots: &CgroupNamespaceRoots) -> VfsResult<Filesystem> {
    roots.try_mount_view()
}

/// Mount a v1 hierarchy through the root selected for this cgroup namespace.
/// A namespace contains a stable root for every live hierarchy, not merely
/// its unified v2 tree.
pub(crate) fn new_cgroup_v1_for_namespace(
    controllers: Vec<String>,
    roots: &CgroupNamespaceRoots,
) -> VfsResult<Filesystem> {
    let hierarchy = cgroup_hierarchy(CgroupVersion::V1, controllers)?;
    roots.try_mount_view_for(&hierarchy)
}

fn cgroup_v2_hierarchy() -> VfsResult<Arc<CgroupHierarchy>> {
    let mut controllers = Vec::new();
    controllers
        .try_reserve_exact(ALL_CONTROLLERS.len())
        .map_err(|_| VfsError::NoMemory)?;
    for controller in ALL_CONTROLLERS {
        controllers.push(try_owned(controller)?);
    }
    cgroup_hierarchy(CgroupVersion::V2, controllers)
}

fn cgroup_hierarchy(
    version: CgroupVersion,
    controllers: Vec<String>,
) -> VfsResult<Arc<CgroupHierarchy>> {
    let key = CgroupHierarchyKey {
        version,
        controllers: controllers.clone(),
    };
    let mut hierarchies = CGROUP_HIERARCHIES.lock();
    if let Some(hierarchy) = hierarchies.get(&key) {
        return Ok(hierarchy.clone());
    }
    if hierarchies.len() >= MAX_CGROUP_HIERARCHIES {
        return Err(VfsError::NoMemory);
    }
    hierarchies.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    let hierarchy = CgroupFs::try_new_hierarchy(version, controllers)?;
    hierarchies.insert(key, hierarchy.clone());
    Ok(hierarchy)
}

fn cgroup_hierarchy_for_dir(dir: &Arc<CgroupDir>) -> AxResult<Arc<CgroupHierarchy>> {
    let hierarchies = CGROUP_HIERARCHIES.lock();
    hierarchies
        .values()
        .find(|hierarchy| Arc::ptr_eq(&hierarchy.root.node.fs, &dir.node.fs))
        .cloned()
        .ok_or(AxError::NotFound)
}

pub fn parse_v1_controllers(source: &str, data: &str) -> AxResult<Vec<String>> {
    let mut controllers = Vec::new();
    for token in source.split(',') {
        let token = token.trim();
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            controllers.push(try_owned(token)?);
        } else if KNOWN_V1_CONTROLLERS.contains(&token) {
            return Err(AxError::NoSuchDevice);
        }
    }
    for token in data.split(',') {
        let token = token.trim();
        if token.is_empty() || is_generic_cgroup_mount_option(token) {
            continue;
        }
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            controllers.push(try_owned(token)?);
        } else if KNOWN_V1_CONTROLLERS.contains(&token) {
            return Err(AxError::NoSuchDevice);
        } else {
            return Err(AxError::InvalidInput);
        }
    }
    if controllers.is_empty() {
        controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        controllers.push(try_owned("pids")?);
    }
    Ok(controllers)
}

fn is_generic_cgroup_mount_option(token: &str) -> bool {
    matches!(
        token,
        "none" | "cgroup" | "rw" | "ro" | "relatime" | "nosuid" | "nodev" | "noexec"
    )
}

pub fn proc_cgroups_snapshot() -> String {
    let mut out = String::from("#subsys_name\thierarchy\tnum_cgroups\tenabled\n");
    for (index, controller) in ALL_CONTROLLERS.iter().enumerate() {
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!("{controller}\t{}\t1\t1\n", index + 1),
        );
    }
    out
}

struct CgroupFs {
    name: &'static str,
    fs_type: u32,
    version: CgroupVersion,
    controllers: Vec<String>,
    namespace: Mutex<()>,
    inodes: Mutex<HashSet<u64>>,
    next_inode: AtomicU64,
    root: Mutex<Option<DirEntry>>,
    root_dir: Mutex<Option<Arc<CgroupDir>>>,
}

impl CgroupFs {
    fn mount(version: CgroupVersion, controllers: Vec<String>) -> VfsResult<Filesystem> {
        let hierarchy = cgroup_hierarchy(version, controllers)?;
        CgroupMountView::try_new(hierarchy.clone(), hierarchy.root.clone())?.filesystem()
    }

    /// Builds the one canonical node tree for a hierarchy.  Callers must
    /// register the returned hierarchy before exposing a mount view, so two
    /// concurrent mounts cannot observe separate roots.
    fn try_new_hierarchy(
        version: CgroupVersion,
        controllers: Vec<String>,
    ) -> VfsResult<Arc<CgroupHierarchy>> {
        let fs = Arc::try_new(Self {
            name: match version {
                CgroupVersion::V1 => "cgroup",
                CgroupVersion::V2 => "cgroup2",
            },
            fs_type: match version {
                CgroupVersion::V1 => CGROUP_SUPER_MAGIC,
                CgroupVersion::V2 => CGROUP2_SUPER_MAGIC,
            },
            version,
            controllers,
            namespace: Mutex::new(()),
            inodes: Mutex::new(HashSet::new()),
            next_inode: AtomicU64::new(1),
            root: Mutex::new(None),
            root_dir: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let filesystem = Filesystem::try_new(fs.clone())?;
        let root_dir = CgroupDir::try_new_root(fs.clone())?;
        let root = DirEntry::try_new_dir(DirNode::new(root_dir.clone()), Reference::root())?;
        root_dir.bind(root.downgrade());
        *fs.root_dir.lock() = Some(root_dir.clone());
        *fs.root.lock() = Some(root.clone());
        Arc::try_new(CgroupHierarchy {
            version,
            controllers: fs.controllers.clone(),
            filesystem,
            root: root_dir,
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn try_alloc_inode(&self) -> VfsResult<u64> {
        let mut inodes = self.inodes.lock();
        if inodes.len() >= MAX_CGROUP_INODES {
            return Err(VfsError::NoMemory);
        }
        inodes.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let ino = self
            .next_inode
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| VfsError::StorageFull)?;
        inodes.insert(ino);
        Ok(ino)
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(&ino);
    }
}

impl FilesystemOps for CgroupFs {
    fn name(&self) -> &str {
        self.name
    }

    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(pseudo_stat_fs(self.fs_type))
    }

    fn unmount(&self) {
        self.root.lock().take();
        self.root_dir.lock().take();
    }
}

struct CgroupNode {
    fs: Arc<CgroupFs>,
    ino: u64,
    metadata: Mutex<Metadata>,
}

impl CgroupNode {
    fn try_new(fs: Arc<CgroupFs>, node_type: NodeType, mode: NodePermission) -> VfsResult<Self> {
        let ino = fs.try_alloc_inode()?;
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 1,
            mode,
            node_type,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now.into(),
            btime: now.into(),
            mtime: now.into(),
            ctime: now.into(),
        };
        Ok(Self {
            fs,
            ino,
            metadata: Mutex::new(metadata),
        })
    }

    fn metadata(&self) -> Metadata {
        self.metadata.lock().clone()
    }

    fn update_metadata(&self, update: MetadataUpdate) {
        let mut metadata = self.metadata.lock();
        let mut status_changed = false;
        if let Some(mode) = update.mode {
            metadata.mode = mode;
            status_changed = true;
        }
        if let Some((uid, gid)) = update.owner {
            metadata.uid = uid;
            metadata.gid = gid;
            status_changed = true;
        }
        if let Some(rdev) = update.rdev {
            metadata.rdev = rdev;
            status_changed = true;
        }
        if let Some(atime) = update.atime {
            metadata.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            metadata.mtime = mtime;
            status_changed = true;
        }
        if let Some(ctime) = update.ctime {
            metadata.ctime = ctime;
        } else if status_changed {
            metadata.ctime = wall_time().into();
        }
    }
}

impl Drop for CgroupNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.ino);
    }
}

struct CgroupDir {
    node: CgroupNode,
    parent: Mutex<Option<Weak<CgroupDir>>>,
    this: Mutex<Option<WeakDirEntry>>,
    namespace_epoch: AtomicU64,
    children: Mutex<HashMap<FsNameBuf, Arc<CgroupDir>>>,
    files: HashMap<FsNameBuf, Arc<CgroupFile>>,
    /// PID entries include invisible fork reservations. Every user-visible
    /// reader filters the shared publication bit; writers count all entries so
    /// a pending fork cannot overbook capacity or let this directory disappear.
    pids: Mutex<HashMap<Pid, Arc<CgroupMembershipPublication>>>,
    pids_max: Mutex<Option<u64>>,
    pids_peak: Mutex<u64>,
    pids_events_limit: Mutex<u64>,
    /// `cgroup.freeze` request for this hierarchy node.  Member processes
    /// park through the task stop wait; `cgroup.events:frozen` is derived
    /// separately and becomes true only once every live member reached it.
    frozen: AtomicBool,
    /// This group's requested limits.  Effective limits are derived through
    /// the parent chain at the point they are consumed by a task.
    uclamp: Mutex<(u16, u16)>,
    subtree_control: Mutex<HashSet<String>>,
}

impl CgroupDir {
    fn try_new_root(fs: Arc<CgroupFs>) -> VfsResult<Arc<Self>> {
        Self::try_new(fs, None)
    }

    fn try_new(fs: Arc<CgroupFs>, parent: Option<Weak<CgroupDir>>) -> VfsResult<Arc<Self>> {
        let mode = NodePermission::from_bits_truncate(0o755);
        let mut files = HashMap::new();
        files
            .try_reserve(CONTROL_FILES.len())
            .map_err(|_| VfsError::NoMemory)?;
        for &name in CONTROL_FILES {
            files.insert(
                try_owned_control_name(name)?,
                CgroupFile::try_new(fs.clone(), name)?,
            );
        }
        let node = CgroupNode::try_new(fs, NodeType::Directory, mode)?;
        let dir = Arc::try_new(Self {
            node,
            parent: Mutex::new(parent),
            this: Mutex::new(None),
            namespace_epoch: AtomicU64::new(0),
            children: Mutex::new(HashMap::new()),
            files,
            pids: Mutex::new(HashMap::new()),
            pids_max: Mutex::new(None),
            pids_peak: Mutex::new(0),
            pids_events_limit: Mutex::new(0),
            frozen: AtomicBool::new(false),
            uclamp: Mutex::new((0, 1024)),
            subtree_control: Mutex::new(HashSet::new()),
        })
        .map_err(|_| VfsError::NoMemory)?;
        dir.bind_control_files();
        Ok(dir)
    }

    fn bind(&self, this: WeakDirEntry) {
        *self.this.lock() = Some(this);
    }

    fn reference(&self, name: &FsName) -> VfsResult<Reference> {
        Ok(Reference::new(
            self.this.lock().as_ref().and_then(WeakDirEntry::upgrade),
            try_owned_name(name)?,
        ))
    }

    fn try_child_entry(&self, name: &FsName, child: Arc<CgroupDir>) -> VfsResult<DirEntry> {
        let entry = DirEntry::try_new_dir(DirNode::new(child.clone()), self.reference(name)?)?;
        child.bind(entry.downgrade());
        Ok(entry)
    }

    fn try_file_entry(&self, name: &FsName, file: Arc<CgroupFile>) -> VfsResult<DirEntry> {
        DirEntry::try_new_file(
            FileNode::new(file),
            NodeType::RegularFile,
            self.reference(name)?,
        )
    }

    fn matches_expected_dir(&self, expected: &DirEntry, actual: &Arc<CgroupDir>) -> bool {
        expected.downcast::<CgroupDir>().is_ok_and(|expected| {
            Arc::ptr_eq(&self.node.fs, &expected.node.fs) && Arc::ptr_eq(&expected, actual)
        })
    }

    fn touch_namespace(&self, now: core::time::Duration) {
        self.node.update_metadata(MetadataUpdate {
            mtime: Some(now.into()),
            ctime: Some(now.into()),
            ..Default::default()
        });
    }

    fn is_same_or_descendant_of(candidate: &Arc<Self>, ancestor: &Arc<Self>) -> bool {
        let mut current = Some(candidate.clone());
        for _ in 0..=MAX_CGROUP_DEPTH {
            let Some(dir) = current else {
                return false;
            };
            if Arc::ptr_eq(&dir, ancestor) {
                return true;
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        // A hierarchy deeper than the admitted bound, or a parent cycle, is
        // malformed. Conservatively reject moves through it.
        true
    }

    fn hierarchy_depth(&self) -> VfsResult<usize> {
        let mut depth = 0usize;
        let mut current = self.parent.lock().as_ref().and_then(Weak::upgrade);
        while let Some(dir) = current {
            depth = depth.checked_add(1).ok_or(VfsError::FilesystemLoop)?;
            if depth > MAX_CGROUP_DEPTH {
                return Err(VfsError::FilesystemLoop);
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        Ok(depth)
    }

    fn subtree_height(&self, remaining: usize) -> VfsResult<usize> {
        let children = self.children.lock();
        if children.is_empty() {
            return Ok(0);
        }
        if remaining == 0 {
            return Err(VfsError::FilesystemLoop);
        }
        let mut height = 0usize;
        for child in children.values() {
            height = height.max(
                child
                    .subtree_height(remaining - 1)?
                    .checked_add(1)
                    .ok_or(VfsError::FilesystemLoop)?,
            );
        }
        Ok(height)
    }

    fn try_live_pids(&self) -> VfsResult<Vec<Pid>> {
        let _operation = PID_CGROUPS.operation.lock();
        self.try_live_pids_with_registry_while_operating(&PID_CGROUPS)
    }

    #[cfg(test)]
    fn try_live_pids_with_registry(&self, registry: &PidMembershipRegistry) -> VfsResult<Vec<Pid>> {
        let _operation = registry.operation.lock();
        let pids = self.pids.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        snapshot.extend(
            pids.iter()
                .map(|(&pid, publication)| (pid, publication.clone())),
        );
        drop(pids);
        Ok(snapshot
            .into_iter()
            .filter_map(|(pid, publication)| {
                registry
                    .synthetic_current_publication_while_operating(pid, &publication)
                    .then_some(pid)
            })
            .collect())
    }

    fn try_live_pids_with_registry_while_operating(
        &self,
        registry: &PidMembershipRegistry,
    ) -> VfsResult<Vec<Pid>> {
        self.try_live_pids_with_validation_while_operating(
            registry,
            RegistryValidation::ProductionIdentity,
        )
    }

    fn try_live_pids_with_validation_while_operating(
        &self,
        registry: &PidMembershipRegistry,
        validation: RegistryValidation,
    ) -> VfsResult<Vec<Pid>> {
        let pids = self.pids.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        snapshot.extend(
            pids.iter()
                .map(|(&pid, publication)| (pid, publication.clone())),
        );
        drop(pids);
        Ok(snapshot
            .into_iter()
            .filter_map(|(pid, publication)| {
                let valid = match validation {
                    RegistryValidation::ProductionIdentity => {
                        registry.current_publication_while_operating(pid, &publication)
                    }
                    #[cfg(test)]
                    RegistryValidation::SyntheticLocal => {
                        registry.synthetic_current_publication_while_operating(pid, &publication)
                    }
                };
                valid.then_some(pid)
            })
            .collect())
    }

    fn recursive_live_pid_count(&self) -> usize {
        let _operation = PID_CGROUPS.operation.lock();
        self.recursive_live_pid_count_while_operating()
    }

    fn recursive_live_pid_count_while_operating(&self) -> usize {
        self.recursive_live_pid_count_with_registry_while_operating(&PID_CGROUPS)
    }

    fn recursive_live_pid_count_with_registry_while_operating(
        &self,
        registry: &PidMembershipRegistry,
    ) -> usize {
        self.recursive_live_pid_count_with_validation_while_operating(
            registry,
            RegistryValidation::ProductionIdentity,
        )
    }

    fn recursive_live_pid_count_with_validation_while_operating(
        &self,
        registry: &PidMembershipRegistry,
        validation: RegistryValidation,
    ) -> usize {
        let local = self
            .try_live_pids_with_validation_while_operating(registry, validation)
            .map_or(0, |pids| pids.len());
        local
            + self
                .children
                .lock()
                .values()
                .map(|child| {
                    child.recursive_live_pid_count_with_validation_while_operating(
                        registry, validation,
                    )
                })
                .sum::<usize>()
    }

    #[cfg(test)]
    fn recursive_live_pid_count_with_registry(&self, registry: &PidMembershipRegistry) -> usize {
        self.try_live_pids_with_registry(registry)
            .map_or(0, |pids| pids.len())
            + self
                .children
                .lock()
                .values()
                .map(|child| child.recursive_live_pid_count_with_registry(registry))
                .sum::<usize>()
    }

    fn append_recursive_live_pids(&self, out: &mut Vec<Pid>, remaining: usize) -> VfsResult<()> {
        let pids = self.try_live_pids_with_registry_while_operating(&PID_CGROUPS)?;
        out.try_reserve(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        out.extend(pids);
        let children = self.children.lock();
        if !children.is_empty() && remaining == 0 {
            return Err(VfsError::FilesystemLoop);
        }
        for child in children.values() {
            child.append_recursive_live_pids(out, remaining - 1)?;
        }
        Ok(())
    }

    fn is_effectively_frozen(&self) -> bool {
        let mut current = Some(self.this_dir().ok());
        for _ in 0..=MAX_CGROUP_DEPTH {
            let Some(Some(dir)) = current else {
                return false;
            };
            if dir.frozen.load(Ordering::Acquire) {
                return true;
            }
            current = Some(dir.parent.lock().as_ref().and_then(Weak::upgrade));
        }
        // A malformed hierarchy must not admit tasks beyond the bounded walk.
        true
    }

    /// Counts both published members and invisible fork admissions. This is
    /// used only for admission/limit decisions, never for cgroup reader output.
    fn recursive_admitted_pid_count(&self) -> usize {
        let local = self.pids.lock().len();
        local
            + self
                .children
                .lock()
                .values()
                .map(|child| child.recursive_admitted_pid_count())
                .sum::<usize>()
    }

    fn update_pids_peak(&self, count: usize) {
        if !self.pids_controller_active() {
            return;
        }
        let mut peak = self.pids_peak.lock();
        *peak = (*peak).max(count as u64);
    }

    fn update_pids_peak_hierarchy_with_registry(
        self: &Arc<Self>,
        registry: &PidMembershipRegistry,
    ) {
        self.update_pids_peak_hierarchy_with_validation(
            registry,
            RegistryValidation::ProductionIdentity,
        )
    }

    fn update_pids_peak_hierarchy_with_validation(
        self: &Arc<Self>,
        registry: &PidMembershipRegistry,
        validation: RegistryValidation,
    ) {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            dir.update_pids_peak(
                dir.recursive_live_pid_count_with_validation_while_operating(registry, validation),
            );
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
    }

    /// Accounts exactly the child owned by one still-hidden fork admission.
    /// Other pending children remain excluded until their own serialized
    /// commit, so peak never gets ahead by more than a child that is about to
    /// be published and can never lag behind pids.current.
    fn update_pids_peak_for_pending_child_with_registry(
        self: &Arc<Self>,
        registry: &PidMembershipRegistry,
    ) {
        self.update_pids_peak_for_pending_child_with_validation(
            registry,
            RegistryValidation::ProductionIdentity,
        )
    }

    fn update_pids_peak_for_pending_child_with_validation(
        self: &Arc<Self>,
        registry: &PidMembershipRegistry,
        validation: RegistryValidation,
    ) {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            dir.update_pids_peak(
                dir.recursive_live_pid_count_with_validation_while_operating(registry, validation)
                    + 1,
            );
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
    }

    fn limiting_dir_for_fork(self: &Arc<Self>) -> Option<Arc<CgroupDir>> {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            if dir.pids_controller_active() {
                let limit = *dir.pids_max.lock();
                if let Some(limit) = limit
                    && dir.recursive_admitted_pid_count() as u64 + 1 > limit
                {
                    return Some(dir);
                }
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        None
    }

    fn has_real_children(&self) -> bool {
        !self.children.lock().is_empty()
    }

    fn pids_controller_active(&self) -> bool {
        match self.node.fs.version {
            CgroupVersion::V1 => self
                .node
                .fs
                .controllers
                .iter()
                .any(|controller| controller == "pids"),
            CgroupVersion::V2 => self
                .parent
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.subtree_control.lock().contains("pids")),
        }
    }

    fn cpu_controller_active(&self) -> bool {
        match self.node.fs.version {
            CgroupVersion::V1 => self
                .node
                .fs
                .controllers
                .iter()
                .any(|controller| controller == "cpu"),
            CgroupVersion::V2 => self
                .parent
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.subtree_control.lock().contains("cpu")),
        }
    }

    fn uclamp_effective(&self) -> (u16, u16) {
        let controls = *SCHED_UTIL_CLAMP_CONTROLS.lock();
        let mut minimum = controls.minimum;
        let mut maximum = controls.maximum;
        let mut current = Some(self.this_dir().ok());
        for _ in 0..=MAX_CGROUP_DEPTH {
            let Some(Some(dir)) = current else { break };
            let (min, max) = *dir.uclamp.lock();
            minimum = minimum.max(min);
            maximum = maximum.min(max);
            current = Some(dir.parent.lock().as_ref().and_then(Weak::upgrade));
        }
        (minimum.min(maximum), maximum)
    }

    fn control_file_visible(&self, name: &str) -> bool {
        match self.node.fs.version {
            CgroupVersion::V1 => {
                matches!(name, "tasks" | "cgroup.procs")
                    || (name.starts_with("pids.") && self.pids_controller_active())
                    || (matches!(name, "cpu.uclamp.min" | "cpu.uclamp.max")
                        && self.cpu_controller_active())
            }
            CgroupVersion::V2 => match name {
                "cgroup.procs"
                | "cgroup.controllers"
                | "cgroup.subtree_control"
                | "cgroup.events"
                | "cgroup.type"
                | "cgroup.freeze" => true,
                "cgroup.kill" => self.parent.lock().is_some(),
                _ if name.starts_with("pids.") => self.pids_controller_active(),
                "cpu.uclamp.min" | "cpu.uclamp.max" => self.cpu_controller_active(),
                _ => false,
            },
        }
    }

    fn reset_pids_controller(&self) {
        *self.pids_max.lock() = None;
        *self.pids_peak.lock() = 0;
        *self.pids_events_limit.lock() = 0;
    }

    fn initialize_pids_controller(&self) {
        self.reset_pids_controller();
        self.update_pids_peak(self.recursive_live_pid_count_while_operating());
    }

    fn v2_has_enabled_child_controllers(&self) -> bool {
        self.node.fs.version == CgroupVersion::V2
            && self.parent.lock().is_some()
            && !self.subtree_control.lock().is_empty()
    }

    fn attach_pid(&self, pid: Pid) -> VfsResult<()> {
        let pid = if pid == 0 {
            axtask::current().as_thread().proc_data.proc.pid()
        } else {
            pid
        };
        if self.v2_has_enabled_child_controllers() {
            return Err(VfsError::ResourceBusy);
        }
        let target = get_process_data(pid).map_err(|_| VfsError::NotFound)?;
        let credentials = current_file_write_credentials().ok_or(VfsError::Io)?;
        let actor_cred = current_file_operation_security_credential().ok_or(VfsError::Io)?;
        if !can_migrate_from_open_cgroup_namespace(&credentials, &actor_cred) {
            return Err(VfsError::NotFound);
        }
        // cgroup.procs is process-directed; sample the persistent Linux group
        // leader credential binding once, even if the original leader exited.
        let target_cred = target.group_leader_cred();
        if !can_migrate_with_credentials(&credentials, &actor_cred, &target_cred) {
            return Err(VfsError::PermissionDenied);
        }
        // Reserve all task references before the membership commit.  Applying
        // their scheduler transactions after `try_attach` never nests the
        // registry's operation lock under a runqueue lock.
        let tasks = snapshot_live_uclamp_tasks()?;
        let this = self.this_dir()?;
        PID_CGROUPS.try_attach_process(&this, pid, &target)?;
        reconcile_process_freeze_after_migration(pid)?;
        republish_live_uclamp_tasks(tasks, Some(pid), uclamp_policy_generation()).map(|_| ())
    }

    fn kill_attached_recursive(&self) -> VfsResult<()> {
        // Capture strong ProcessData references in one membership epoch.  Do
        // not resolve numeric PIDs after dropping it: a concurrent exit and
        // PID reuse must never redirect cgroup.kill at an unrelated process.
        let processes = {
            let _operation = PID_CGROUPS.operation.lock();
            let mut pids = Vec::new();
            self.append_recursive_live_pids(&mut pids, MAX_CGROUP_DEPTH)?;
            let mut processes = Vec::new();
            processes
                .try_reserve_exact(pids.len())
                .map_err(|_| VfsError::NoMemory)?;
            for pid in pids {
                if let Some(process) = PID_CGROUPS.process_while_operating(pid) {
                    processes.push(process);
                }
            }
            processes
        };
        for process in processes {
            // A process can exit after the snapshot; that is the successful
            // concurrent outcome for cgroup.kill.  The retained ProcessData
            // still names that exact process identity, never a recycled PID.
            let _ =
                send_signal_to_process_data(&process, Some(SignalInfo::new_kernel(Signo::SIGKILL)));
        }
        Ok(())
    }

    fn tasks_text(&self) -> VfsResult<String> {
        let _operation = PID_CGROUPS.operation.lock();
        self.tasks_text_while_operating()
    }

    fn tasks_text_while_operating(&self) -> VfsResult<String> {
        self.tasks_text_with_registry_while_operating(&PID_CGROUPS)
    }

    #[cfg(test)]
    fn tasks_text_with_registry(&self, registry: &PidMembershipRegistry) -> VfsResult<String> {
        let _operation = registry.operation.lock();
        let pids = self.pids.lock();
        let mut out = String::new();
        out.try_reserve_exact(pids.len().saturating_mul(22))
            .map_err(|_| VfsError::NoMemory)?;
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        snapshot.extend(
            pids.iter()
                .map(|(&pid, publication)| (pid, publication.clone())),
        );
        drop(pids);
        for pid in snapshot.into_iter().filter_map(|(pid, publication)| {
            registry
                .synthetic_current_publication_while_operating(pid, &publication)
                .then_some(pid)
        }) {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{pid}\n"));
        }
        Ok(out)
    }

    fn tasks_text_with_registry_while_operating(
        &self,
        registry: &PidMembershipRegistry,
    ) -> VfsResult<String> {
        let pids = self.pids.lock();
        let mut out = String::new();
        out.try_reserve_exact(pids.len().saturating_mul(22))
            .map_err(|_| VfsError::NoMemory)?;
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        snapshot.extend(
            pids.iter()
                .map(|(&pid, publication)| (pid, publication.clone())),
        );
        drop(pids);
        for pid in snapshot.into_iter().filter_map(|(pid, publication)| {
            registry
                .current_publication_while_operating(pid, &publication)
                .then_some(pid)
        }) {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{pid}\n"));
        }
        Ok(out)
    }

    fn subtree_control_text(&self) -> VfsResult<String> {
        let control = self.subtree_control.lock();
        try_join_names(control.iter().map(String::as_str))
    }

    fn controller_available(&self, name: &str) -> bool {
        if self.node.fs.version == CgroupVersion::V1 {
            return false;
        }
        if let Some(parent) = self.parent.lock().as_ref().and_then(Weak::upgrade) {
            return parent.subtree_control.lock().contains(name);
        }
        self.node
            .fs
            .controllers
            .iter()
            .any(|controller| controller == name)
    }

    fn controllers_text(&self) -> VfsResult<String> {
        if self.node.fs.version == CgroupVersion::V1 {
            return Ok(String::new());
        }
        if let Some(parent) = self.parent.lock().as_ref().and_then(Weak::upgrade) {
            let control = parent.subtree_control.lock();
            return try_join_names(control.iter().map(String::as_str));
        }
        try_join_names(self.node.fs.controllers.iter().map(String::as_str))
    }

    fn events_text(&self) -> String {
        format!(
            "populated {}\nfrozen {}\n",
            u8::from(self.recursive_live_pid_count_while_operating() != 0),
            u8::from(self.recursive_frozen())
        )
    }

    #[cfg(test)]
    fn events_text_with_registry(&self, registry: &PidMembershipRegistry) -> String {
        format!(
            "populated {}\nfrozen {}\n",
            u8::from(self.recursive_live_pid_count_with_registry(registry) != 0),
            u8::from(self.recursive_frozen())
        )
    }

    fn freeze_text(&self) -> String {
        format!("{}\n", u8::from(self.frozen.load(Ordering::Acquire)))
    }

    fn set_frozen(&self, data: &[u8]) -> VfsResult<()> {
        let value = match core::str::from_utf8(data)
            .map_err(|_| VfsError::InvalidInput)?
            .trim()
        {
            "0" => false,
            "1" => true,
            _ => return Err(VfsError::InvalidInput),
        };
        let _operation = PID_CGROUPS.operation.lock();
        // Allocate every bounded snapshot before publishing the new freezer
        // state.  A failed write must not strand a hierarchy in an advertised
        // frozen state without having driven its member tasks.
        let pids = self.recursive_live_pids_under_operation()?;
        let tasks = try_tasks().map_err(|_| VfsError::NoMemory)?;
        self.frozen.store(value, Ordering::Release);
        self.apply_freeze_state(value, pids, tasks);
        Ok(())
    }

    fn recursive_frozen(&self) -> bool {
        if !self.is_effectively_frozen() {
            return false;
        }
        let Ok(pids) = self.recursive_live_pids_under_operation() else {
            return false;
        };
        pids.into_iter()
            .all(|pid| get_process_data(pid).is_ok_and(|process| process.cgroup_freeze_complete()))
    }

    /// Caller holds `PID_CGROUPS.operation` when it needs a membership epoch.
    fn recursive_live_pids_under_operation(&self) -> VfsResult<Vec<Pid>> {
        let mut pids = Vec::new();
        self.append_recursive_live_pids(&mut pids, MAX_CGROUP_DEPTH)?;
        Ok(pids)
    }

    fn apply_freeze_state(&self, freeze: bool, pids: Vec<Pid>, tasks: Vec<axtask::AxTaskRef>) {
        for pid in &pids {
            let Ok(process) = get_process_data(*pid) else {
                continue;
            };
            if freeze {
                process.request_cgroup_freeze();
            } else if !PID_CGROUPS
                .get_v2_while_operating(*pid, &process.proc)
                .is_some_and(|dir| dir.is_effectively_frozen())
            {
                // A nested cgroup can retain an independent freezer request;
                // thawing an ancestor must not release it.
                process.thaw_cgroup_freeze();
            }
        }
        if freeze {
            for task in tasks {
                let Some(thread) = task.try_as_thread() else {
                    continue;
                };
                if pids.contains(&thread.proc_data.proc.pid()) {
                    task.interrupt();
                }
            }
        }
    }

    fn pids_max_text(&self) -> String {
        match *self.pids_max.lock() {
            Some(limit) => format!("{limit}\n"),
            None => "max\n".to_string(),
        }
    }

    fn set_pids_max(&self, data: &[u8]) -> VfsResult<()> {
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        let text = text.trim();
        let value = if text == "max" {
            None
        } else {
            if text.starts_with('-') {
                return Err(VfsError::InvalidInput);
            }
            Some(text.parse::<u64>().map_err(|_| VfsError::InvalidInput)?)
        };
        *self.pids_max.lock() = value;
        Ok(())
    }

    fn uclamp_text(&self, minimum: bool) -> String {
        let (min, max) = *self.uclamp.lock();
        format!("{}\n", if minimum { min } else { max })
    }

    fn set_uclamp(&self, minimum: bool, data: &[u8]) -> VfsResult<u64> {
        let value = core::str::from_utf8(data)
            .map_err(|_| VfsError::InvalidInput)?
            .trim()
            .parse::<u16>()
            .map_err(|_| VfsError::InvalidInput)?;
        if value > 1024 {
            return Err(VfsError::InvalidInput);
        }
        let _policy = UCLAMP_POLICY_UPDATE.lock();
        let mut clamp = self.uclamp.lock();
        let mut next = *clamp;
        if (minimum && value > next.1) || (!minimum && value < next.0) {
            return Err(VfsError::InvalidInput);
        }
        if minimum {
            next.0 = value;
        } else {
            next.1 = value;
        }
        begin_uclamp_policy_update();
        *clamp = next;
        Ok(finish_uclamp_policy_update())
    }

    fn child_has_subtree_controller(&self, name: &str) -> bool {
        self.children
            .lock()
            .values()
            .any(|child| child.subtree_control.lock().contains(name))
    }

    fn update_subtree_control(&self, data: &[u8]) -> VfsResult<()> {
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        // Availability validation and the eventual controller reset belong to
        // the same membership operation. Otherwise a parent can disable pids
        // after a child validates `+pids` but before the child publishes it.
        let _operation = PID_CGROUPS.operation.lock();
        let mut pids_state = None;
        for token in text.split_ascii_whitespace() {
            if token.len() < 2 {
                return Err(VfsError::InvalidInput);
            }
            let (op, name) = token.split_at(1);
            match op {
                "+" => {
                    if !self.controller_available(name) {
                        return Err(VfsError::NotFound);
                    }
                    if name != "pids" {
                        return Err(VfsError::OperationNotSupported);
                    }
                    pids_state = Some(true);
                }
                "-" => {
                    if name != "pids" {
                        return Err(VfsError::OperationNotSupported);
                    }
                    pids_state = Some(false);
                }
                _ => return Err(VfsError::InvalidInput),
            }
        }

        if pids_state.is_none() {
            return Ok(());
        }
        let prepared_name = pids_state
            .filter(|enabled| *enabled)
            .map(|_| try_owned("pids"))
            .transpose()?;
        // Membership publication, topology changes, controller reset, and
        // pids.current/pids.peak reads share this operation domain. In
        // particular, a pending fork cannot publish across a controller reset
        // or observe an ancestor chain that is concurrently being renamed.
        let mut control = self.subtree_control.lock();
        let was_enabled = control.contains("pids");
        if pids_state == Some(true)
            && self.node.fs.version == CgroupVersion::V2
            && self.parent.lock().is_some()
            && !self.pids.lock().is_empty()
        {
            return Err(VfsError::ResourceBusy);
        }
        if pids_state == Some(false) && self.child_has_subtree_controller("pids") {
            return Err(VfsError::ResourceBusy);
        }
        if pids_state == Some(true) && !was_enabled {
            control.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            control.insert(prepared_name.ok_or(VfsError::Io)?);
        }
        if pids_state == Some(false) && was_enabled {
            control.remove("pids");
        }
        drop(control);
        for child in self.children.lock().values() {
            if pids_state == Some(true) && !was_enabled {
                child.initialize_pids_controller();
            } else if pids_state == Some(false) && was_enabled {
                child.reset_pids_controller();
            }
        }
        Ok(())
    }
}

fn same_membership_mapping(lhs: Option<&CgroupMembership>, rhs: Option<&CgroupMembership>) -> bool {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => {
            Weak::ptr_eq(&lhs.target, &rhs.target)
                && Arc::ptr_eq(&lhs.publication, &rhs.publication)
        }
        (None, None) => true,
        _ => false,
    }
}

fn membership_matches_process(membership: &CgroupMembership, process: &Arc<Process>) -> bool {
    membership
        .process_identity
        .upgrade()
        .is_some_and(|identity| Arc::ptr_eq(&identity, process))
}

fn hierarchy_key_for_dir(dir: &Arc<CgroupDir>) -> AxResult<CgroupHierarchyKey> {
    Ok(CgroupHierarchyKey {
        version: dir.node.fs.version,
        controllers: dir.node.fs.controllers.clone(),
    })
}

impl PidMembershipRegistry {
    fn with_limits(global_limit: usize, per_cgroup_limit: usize) -> Self {
        Self {
            operation: Mutex::new(()),
            by_pid: Mutex::new(HashMap::new()),
            global_limit,
            per_cgroup_limit,
        }
    }

    fn try_attach(&self, target: &Arc<CgroupDir>, pid: Pid, charge_fork: bool) -> AxResult<bool> {
        let _operation = self.operation.lock();
        let result = self.try_attach_locked(target, pid, charge_fork);
        #[cfg(test)]
        if result.is_ok() {
            target.update_pids_peak_hierarchy_with_validation(
                self,
                RegistryValidation::SyntheticLocal,
            );
        }
        result
    }

    fn try_attach_process(
        &self,
        target: &Arc<CgroupDir>,
        pid: Pid,
        process: &Arc<ProcessData>,
    ) -> AxResult<bool> {
        let _operation = self.operation.lock();
        self.purge_reused_pid_while_operating(pid, &process.proc);
        // Keep membership publication in the same policy-generation window
        // as a clamp-file write. A concurrent policy snapshot therefore sees
        // either the old membership and old generation or the new pair, never
        // a new cgroup with an already-clean old task marker.
        let _policy = UCLAMP_POLICY_UPDATE.lock();
        begin_uclamp_policy_update();
        let result = self.try_attach_locked(target, pid, false);
        let result = match result {
            Ok(changed) => {
                self.bind_process_while_operating(pid, process);
                Ok(changed)
            }
            Err(error) => Err(error),
        };
        let _generation = finish_uclamp_policy_update();
        result
    }

    fn prepare_charge_from_for_child(
        &self,
        parent_pid: Pid,
        child_pid: Pid,
        child: &Arc<Process>,
    ) -> AxResult<CgroupForkAdmission<'_>> {
        let _operation = self.operation.lock();
        self.purge_reused_pid_while_operating(child_pid, child);
        let parent = get_process_including_zombie(parent_pid)?;
        self.purge_reused_pid_while_operating(parent_pid, &parent);
        let inherited = self.visible_targets_locked(parent_pid, &parent)?;
        self.prepare_fork_set_locked(inherited, child_pid, Some(child))
    }

    fn prepare_fork_attach_from(
        &self,
        _parent_pid: Pid,
        target: &Arc<CgroupDir>,
        child_pid: Pid,
        child: &Arc<Process>,
    ) -> AxResult<CgroupForkAdmission<'_>> {
        let _operation = self.operation.lock();
        self.purge_reused_pid_while_operating(child_pid, child);
        let parent = get_process_including_zombie(_parent_pid)?;
        self.purge_reused_pid_while_operating(_parent_pid, &parent);
        let target_key = hierarchy_key_for_dir(target)?;
        if target_key.version != CgroupVersion::V2 {
            return Err(AxError::InvalidInput);
        }
        let mut inherited = self.visible_targets_locked(_parent_pid, &parent)?;
        inherited.retain(|(key, _)| *key != target_key);
        inherited.push((target_key, target.clone()));
        self.prepare_fork_set_locked(inherited, child_pid, Some(child))
    }

    fn visible_targets_locked(
        &self,
        pid: Pid,
        process: &Arc<Process>,
    ) -> AxResult<Vec<(CgroupHierarchyKey, Arc<CgroupDir>)>> {
        let mut by_pid = self.by_pid.lock();
        let Some(memberships) = by_pid.get(&pid).cloned() else {
            return Ok(Vec::new());
        };
        let mut targets = Vec::new();
        targets
            .try_reserve(memberships.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut stale = false;
        for (key, membership) in memberships {
            if !membership_matches_process(&membership, process) {
                stale = true;
                continue;
            }
            if !membership.is_visible() {
                return Err(AxError::ResourceBusy);
            }
            if let Some(target) = membership.target.upgrade() {
                targets.push((key, target));
            } else {
                stale = true;
            }
        }
        if stale {
            // A stale target in one hierarchy must not erase the source
            // membership inherited from every other hierarchy.
            if let Some(memberships) = by_pid.get_mut(&pid) {
                memberships.retain(|_, membership| {
                    membership_matches_process(membership, process)
                        && membership.target.strong_count() != 0
                });
                if memberships.is_empty() {
                    by_pid.remove(&pid);
                }
            }
        }
        Ok(targets)
    }

    #[cfg(test)]
    fn visible_targets_without_identity_locked(
        &self,
        pid: Pid,
    ) -> AxResult<Vec<(CgroupHierarchyKey, Arc<CgroupDir>)>> {
        let by_pid = self.by_pid.lock();
        let Some(memberships) = by_pid.get(&pid) else {
            return Ok(Vec::new());
        };
        let mut targets = Vec::new();
        targets
            .try_reserve(memberships.len())
            .map_err(|_| AxError::NoMemory)?;
        for (key, membership) in memberships {
            if !membership.is_visible() {
                return Err(AxError::ResourceBusy);
            }
            if let Some(target) = membership.target.upgrade() {
                targets.push((key.clone(), target));
            }
        }
        Ok(targets)
    }

    #[cfg(test)]
    fn prepare_charge_from(
        &self,
        parent_pid: Pid,
        child_pid: Pid,
    ) -> AxResult<CgroupForkAdmission<'_>> {
        let _operation = self.operation.lock();
        let inherited = self.visible_targets_without_identity_locked(parent_pid)?;
        self.prepare_fork_set_locked(inherited, child_pid, None)
    }

    fn prepare_fork_set_locked(
        &self,
        targets: Vec<(CgroupHierarchyKey, Arc<CgroupDir>)>,
        child_pid: Pid,
        child_process: Option<&Arc<Process>>,
    ) -> AxResult<CgroupForkAdmission<'_>> {
        if targets.is_empty() {
            return Ok(CgroupForkAdmission::untracked(
                self,
                child_pid,
                child_process,
            ));
        }
        for (_, target) in &targets {
            if target.v2_has_enabled_child_controllers()
                || (target.node.fs.version == CgroupVersion::V2 && target.is_effectively_frozen())
            {
                return Err(AxError::ResourceBusy);
            }
            if let Some(limiting) = target.limiting_dir_for_fork() {
                let mut events = limiting.pids_events_limit.lock();
                *events = events.checked_add(1).ok_or(AxError::BadState)?;
                return Err(AxError::WouldBlock);
            }
        }
        let mut by_pid = self.by_pid.lock();
        if by_pid.contains_key(&child_pid) {
            return Err(AxError::AlreadyExists);
        }
        let membership_count = by_pid
            .values()
            .try_fold(0usize, |count, memberships| {
                count.checked_add(memberships.len())
            })
            .ok_or(AxError::NoMemory)?;
        if membership_count
            .checked_add(targets.len())
            .ok_or(AxError::NoMemory)?
            > self.global_limit
        {
            return Err(AxError::NoMemory);
        }
        by_pid.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        let mut child = HashMap::new();
        child
            .try_reserve(targets.len())
            .map_err(|_| AxError::NoMemory)?;
        let mut reservations = Vec::new();
        reservations
            .try_reserve(targets.len())
            .map_err(|_| AxError::NoMemory)?;
        // Finish every fallible allocation and capacity check before either
        // index is changed.  A multi-hierarchy fork is all-or-nothing.
        for (_, target) in &targets {
            let mut pids = target.pids.lock();
            if pids.contains_key(&child_pid) {
                return Err(AxError::AlreadyExists);
            }
            if pids.len() >= self.per_cgroup_limit {
                return Err(AxError::NoMemory);
            }
            pids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        }
        let mut prepared = Vec::new();
        prepared
            .try_reserve(targets.len())
            .map_err(|_| AxError::NoMemory)?;
        let publication =
            Arc::try_new(CgroupMembershipPublication::new(false)).map_err(|_| AxError::NoMemory)?;
        for (key, target) in targets {
            prepared.push((key, target, publication.clone()));
        }
        for (key, target, publication) in prepared {
            let mut pids = target.pids.lock();
            pids.insert(child_pid, publication.clone());
            drop(pids);
            child.insert(
                key.clone(),
                CgroupMembership {
                    target: Arc::downgrade(&target),
                    publication: publication.clone(),
                    process_identity: child_process
                        .map(|process| Arc::downgrade(process))
                        .unwrap_or_else(Weak::new),
                    process: Weak::new(),
                },
            );
            reservations.push(CgroupForkReservation {
                hierarchy: key,
                target,
            });
        }
        by_pid.insert(child_pid, child);
        Ok(CgroupForkAdmission {
            registry: self,
            child_pid,
            reservations,
            publication: Some(publication),
            process_identity: child_process
                .map(|process| Arc::downgrade(process))
                .unwrap_or_else(Weak::new),
            committed: false,
        })
    }

    /// Reserves both indexes before changing either one. The operation lock
    /// prevents another writer from consuming those reservations; publication
    /// then holds the global map and every affected member set, so readers can
    /// observe only the old state or the fully committed new state.
    fn try_attach_locked(
        &self,
        target: &Arc<CgroupDir>,
        pid: Pid,
        charge_fork: bool,
    ) -> AxResult<bool> {
        if target.v2_has_enabled_child_controllers() {
            return Err(AxError::ResourceBusy);
        }
        if charge_fork
            && target.node.fs.version == CgroupVersion::V2
            && target.is_effectively_frozen()
        {
            // A fork admission is published before its child has a task
            // object that can enter the freezer wait.  Reject it rather than
            // exposing an unfrozen child.  Existing-process migration is
            // allowed and reconciled to the freezer immediately after commit.
            return Err(AxError::ResourceBusy);
        }
        let hierarchy = hierarchy_key_for_dir(target)?;
        let mut by_pid = self.by_pid.lock();
        by_pid.retain(|_, memberships| {
            memberships.retain(|_, membership| {
                !membership.is_visible() || membership.target.strong_count() != 0
            });
            !memberships.is_empty()
        });
        let old_mapping = by_pid
            .get(&pid)
            .and_then(|memberships| memberships.get(&hierarchy))
            .cloned();
        if old_mapping
            .as_ref()
            .is_some_and(|mapping| !mapping.is_visible())
        {
            return Err(AxError::ResourceBusy);
        }
        let target_weak = Arc::downgrade(target);
        {
            let target_pids = target.pids.lock();
            if let Some(existing) = target_pids.get(&pid) {
                if !existing.is_visible() {
                    return Err(AxError::ResourceBusy);
                }
                if old_mapping.as_ref().is_some_and(|old| {
                    Weak::ptr_eq(&old.target, &target_weak)
                        && Arc::ptr_eq(&old.publication, existing)
                }) {
                    return Ok(false);
                }
                return Err(AxError::BadState);
            }
        }

        if charge_fork && let Some(limiting) = target.limiting_dir_for_fork() {
            let mut events = limiting.pids_events_limit.lock();
            *events = events.checked_add(1).ok_or(AxError::BadState)?;
            return Err(AxError::WouldBlock);
        }
        let mut target_pids = target.pids.lock();
        if target_pids.len() >= self.per_cgroup_limit {
            return Err(AxError::NoMemory);
        }
        target_pids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        if old_mapping.is_none() {
            let membership_count = by_pid
                .values()
                .try_fold(0usize, |count, memberships| {
                    count.checked_add(memberships.len())
                })
                .ok_or(AxError::NoMemory)?;
            if membership_count >= self.global_limit {
                return Err(AxError::NoMemory);
            }
            if let Some(memberships) = by_pid.get_mut(&pid) {
                memberships.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            } else {
                by_pid.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            }
        }

        if !same_membership_mapping(
            by_pid
                .get(&pid)
                .and_then(|memberships| memberships.get(&hierarchy)),
            old_mapping.as_ref(),
        ) {
            return Err(AxError::Io);
        }
        let old_dir = old_mapping
            .as_ref()
            .and_then(|old| old.target.upgrade())
            .filter(|old| !Arc::ptr_eq(old, target));
        let publication =
            Arc::try_new(CgroupMembershipPublication::new(true)).map_err(|_| AxError::NoMemory)?;
        let replacement = CgroupMembership {
            target: target_weak,
            publication: publication.clone(),
            process_identity: old_mapping
                .as_ref()
                .map(|membership| membership.process_identity.clone())
                .unwrap_or_else(Weak::new),
            process: old_mapping
                .as_ref()
                .map(|membership| membership.process.clone())
                .unwrap_or_else(Weak::new),
        };

        let mut removed_old_publication = None;
        if let Some(old_dir) = old_dir {
            let mut old_pids = old_dir.pids.lock();
            let Some(old_publication) = old_mapping.as_ref().map(|old| &old.publication) else {
                return Err(AxError::Io);
            };
            if !old_pids
                .get(&pid)
                .is_some_and(|current| Arc::ptr_eq(current, old_publication))
            {
                return Err(AxError::Io);
            }
            removed_old_publication = old_pids.remove(&pid);
            let inserted_target = target_pids.insert(pid, publication.clone());
            if inserted_target.is_some() {
                if let Some(old_publication) = removed_old_publication.take() {
                    old_pids.insert(pid, old_publication);
                }
                return Err(AxError::BadState);
            }
            let replaced = by_pid
                .get_mut(&pid)
                .and_then(|memberships| memberships.insert(hierarchy.clone(), replacement.clone()));
            if !same_membership_mapping(replaced.as_ref(), old_mapping.as_ref()) {
                if let Some(old_mapping) = old_mapping {
                    by_pid
                        .get_mut(&pid)
                        .expect("membership map disappeared")
                        .insert(hierarchy.clone(), old_mapping);
                } else {
                    by_pid
                        .get_mut(&pid)
                        .expect("membership map disappeared")
                        .remove(&hierarchy);
                }
                target_pids.remove(&pid);
                if let Some(old_publication) = removed_old_publication.take() {
                    old_pids.insert(pid, old_publication);
                }
                return Err(AxError::Io);
            }
        } else {
            if target_pids.insert(pid, publication.clone()).is_some() {
                return Err(AxError::BadState);
            }
            let replaced = by_pid
                .entry(pid)
                .or_insert_with(HashMap::new)
                .insert(hierarchy.clone(), replacement);
            if !same_membership_mapping(replaced.as_ref(), old_mapping.as_ref()) {
                if let Some(old_mapping) = old_mapping {
                    by_pid
                        .get_mut(&pid)
                        .expect("membership map disappeared")
                        .insert(hierarchy.clone(), old_mapping);
                } else {
                    by_pid
                        .get_mut(&pid)
                        .expect("membership map disappeared")
                        .remove(&hierarchy);
                }
                target_pids.remove(&pid);
                return Err(AxError::Io);
            }
        }

        drop(target_pids);
        drop(by_pid);
        drop(removed_old_publication);
        target.update_pids_peak_hierarchy_with_registry(self);
        // Publish the perf software source only after both membership indexes
        // committed.  A failed/rolled-back move must never produce a cgroup
        // switch record.  The event is task-local, so find the exact live
        // thread rather than charging an unrelated CPU context.
        if let Ok(tasks) = try_tasks() {
            for task in tasks {
                if task
                    .try_as_thread()
                    .is_some_and(|thread| thread.proc_data.proc.pid() == pid)
                {
                    if let Some(cpu) = task.as_thread().perf_last_cpu_for_reconcile() {
                        crate::file::PerfGroup::cgroup_membership_changed(task.id().as_u64(), cpu);
                    }
                    task.as_thread()
                        .perf_emit_dynamic(crate::file::PerfEvent::Software(
                            crate::file::SoftwareEvent::CgroupSwitches,
                        ));
                }
            }
        }
        Ok(true)
    }

    fn detach(&self, pid: Pid, process: &Arc<Process>) {
        let _operation = self.operation.lock();
        let mut by_pid = self.by_pid.lock();
        let Some(mut currents) = by_pid.remove(&pid) else {
            return;
        };
        currents.retain(|_, current| {
            if !membership_matches_process(current, process) {
                return true;
            }
            if current.is_visible()
                && let Some(dir) = current.target.upgrade()
            {
                let mut pids = dir.pids.lock();
                if pids
                    .get(&pid)
                    .is_some_and(|publication| Arc::ptr_eq(publication, &current.publication))
                {
                    pids.remove(&pid);
                } else {
                    error!("cgroup PID {pid} lost its exact target membership during detach");
                }
            }
            false
        });
        if !currents.is_empty() {
            by_pid.insert(pid, currents);
        }
        drop(by_pid);
    }

    fn get_v2(&self, pid: Pid, process: &Arc<Process>) -> Option<Arc<CgroupDir>> {
        let _operation = self.operation.lock();
        self.purge_reused_pid_while_operating(pid, process);
        let mut by_pid = self.by_pid.lock();
        let mapped = by_pid
            .get(&pid)?
            .iter()
            .find(|(key, _)| key.version == CgroupVersion::V2)
            .map(|(_, membership)| membership.clone())?;
        if !mapped.is_visible() {
            return None;
        }
        let Some(dir) = mapped.target.upgrade() else {
            if let Some(memberships) = by_pid.get_mut(&pid) {
                memberships.retain(|_, membership| membership.target.strong_count() != 0);
            }
            return None;
        };
        Some(dir)
    }

    #[cfg(test)]
    fn get(&self, pid: Pid) -> Option<Arc<CgroupDir>> {
        let process = get_process_including_zombie(pid).ok()?;
        self.get_v2(pid, &process)
    }

    /// Caller holds `operation`; unlike [`get`](Self::get), this does not
    /// mutate stale entries while a freezer transition is using a membership
    /// snapshot as its publication boundary.
    fn get_v2_while_operating(&self, pid: Pid, process: &Arc<Process>) -> Option<Arc<CgroupDir>> {
        let by_pid = self.by_pid.lock();
        let mapped = by_pid
            .get(&pid)?
            .iter()
            .find(|(key, membership)| {
                key.version == CgroupVersion::V2 && membership_matches_process(membership, process)
            })
            .map(|(_, membership)| membership.clone())?;
        if !mapped.is_visible() {
            return None;
        }
        mapped.target.upgrade()
    }

    fn bind_process_while_operating(&self, pid: Pid, process: &Arc<ProcessData>) {
        let mut by_pid = self.by_pid.lock();
        let Some(memberships) = by_pid.get_mut(&pid) else {
            return;
        };
        for membership in memberships.values_mut() {
            if membership.process_identity.upgrade().is_none()
                || membership_matches_process(membership, &process.proc)
            {
                membership.process_identity = Arc::downgrade(&process.proc);
                membership.process = Arc::downgrade(process);
            }
        }
    }

    /// The PID namespace may have released a reaped PID before a delayed
    /// cgroup cleanup runs.  Remove only memberships whose retained core
    /// identity is not this live process, leaving a reused PID's new entries
    /// untouched by the old lifecycle edge.
    fn purge_reused_pid_while_operating(&self, pid: Pid, process: &Arc<Process>) {
        let mut by_pid = self.by_pid.lock();
        let Some(mut memberships) = by_pid.remove(&pid) else {
            return;
        };
        memberships.retain(|_, membership| {
            if membership_matches_process(membership, process) {
                return true;
            }
            if let Some(target) = membership.target.upgrade() {
                let mut pids = target.pids.lock();
                if pids
                    .get(&pid)
                    .is_some_and(|publication| Arc::ptr_eq(publication, &membership.publication))
                {
                    pids.remove(&pid);
                }
            }
            false
        });
        if !memberships.is_empty() {
            by_pid.insert(pid, memberships);
        }
    }

    fn process_while_operating(&self, pid: Pid) -> Option<Arc<ProcessData>> {
        let process = get_process_including_zombie(pid).ok()?;
        let by_pid = self.by_pid.lock();
        let membership = by_pid.get(&pid)?.values().find(|membership| {
            membership.is_visible() && membership_matches_process(membership, &process)
        })?;
        membership
            .is_visible()
            .then(|| {
                membership
                    .process
                    .upgrade()
                    .filter(|data| Arc::ptr_eq(&data.proc, &process))
            })
            .flatten()
    }

    fn memberships(&self, pid: Pid) -> HashMap<CgroupHierarchyKey, Arc<CgroupDir>> {
        let Ok(process) = get_process_including_zombie(pid) else {
            return HashMap::new();
        };
        let _operation = self.operation.lock();
        self.purge_reused_pid_while_operating(pid, &process);
        self.memberships_for_process_while_operating(pid, &process)
    }

    /// Queries a retained historical process identity, as used by an already
    /// opened procfs file.  This must only filter: after PID reuse the
    /// supplied Process is intentionally no longer the current owner, and
    /// must not be allowed to purge that owner's memberships.
    fn memberships_for_process(
        &self,
        pid: Pid,
        process: &Arc<Process>,
    ) -> HashMap<CgroupHierarchyKey, Arc<CgroupDir>> {
        let _operation = self.operation.lock();
        self.memberships_for_process_while_operating(pid, process)
    }

    fn memberships_for_process_while_operating(
        &self,
        pid: Pid,
        process: &Arc<Process>,
    ) -> HashMap<CgroupHierarchyKey, Arc<CgroupDir>> {
        self.by_pid
            .lock()
            .get(&pid)
            .map(|memberships| {
                memberships
                    .iter()
                    .filter_map(|(key, membership)| {
                        (membership.is_visible() && membership_matches_process(membership, process))
                            .then(|| membership.target.upgrade())
                            .flatten()
                            .map(|target| (key.clone(), target))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn current_publication(
        &self,
        pid: Pid,
        publication: &Arc<CgroupMembershipPublication>,
    ) -> bool {
        let _operation = self.operation.lock();
        self.current_publication_while_operating(pid, publication)
    }

    /// Caller holds `operation`. This is deliberately read-only: directory
    /// readers can safely use it while holding their own member-set snapshot
    /// without recursively acquiring the registry mutex.
    fn current_publication_while_operating(
        &self,
        pid: Pid,
        publication: &Arc<CgroupMembershipPublication>,
    ) -> bool {
        if !publication.is_visible() {
            return false;
        }
        let Ok(process) = get_process_including_zombie(pid) else {
            return false;
        };
        self.by_pid.lock().get(&pid).is_some_and(|memberships| {
            memberships.values().any(|membership| {
                membership.is_visible()
                    && Arc::ptr_eq(&membership.publication, publication)
                    && membership_matches_process(membership, &process)
            })
        })
    }

    /// Local registry unit tests model PIDs without installing them in the
    /// global process table.  Keep their reader context explicit: it checks
    /// the same publication/index identity relation, but deliberately has no
    /// global-PID liveness lookup. Production readers never call this.
    #[cfg(test)]
    fn synthetic_current_publication_while_operating(
        &self,
        pid: Pid,
        publication: &Arc<CgroupMembershipPublication>,
    ) -> bool {
        publication.is_visible()
            && self.by_pid.lock().get(&pid).is_some_and(|memberships| {
                memberships.values().any(|membership| {
                    membership.is_visible() && Arc::ptr_eq(&membership.publication, publication)
                })
            })
    }
}

impl<'a> CgroupForkAdmission<'a> {
    fn untracked(
        registry: &'a PidMembershipRegistry,
        child_pid: Pid,
        child: Option<&Arc<Process>>,
    ) -> Self {
        Self {
            registry,
            child_pid,
            reservations: Vec::new(),
            publication: None,
            process_identity: child
                .map(|process| Arc::downgrade(process))
                .unwrap_or_else(Weak::new),
            committed: false,
        }
    }

    /// Makes the already installed pair of hidden index entries visible.
    /// The clone path has already published the child task identity but has
    /// not made it runnable, so a freezer request observed here can be bound
    /// to the exact child before its first user return.
    pub(crate) fn commit(mut self) {
        let _operation = self.registry.operation.lock();
        for reservation in &self.reservations {
            let target = &reservation.target;
            let frozen =
                target.node.fs.version == CgroupVersion::V2 && target.is_effectively_frozen();
            if frozen {
                let process = get_process_data(self.child_pid).unwrap_or_else(|_| {
                    panic!(
                        "published clone child {} disappeared before cgroup freezer commit",
                        self.child_pid
                    )
                });
                self.registry
                    .bind_process_while_operating(self.child_pid, &process);
                // A freezer can start after prepare_fork_attach_locked()
                // installed the hidden reservation.  Request the exact child
                // park before releasing its membership publication; clone has
                // not yet handed this task to a run queue.
                process.request_cgroup_freeze();
            } else if let Ok(process) = get_process_data(self.child_pid) {
                self.registry
                    .bind_process_while_operating(self.child_pid, &process);
            }
            #[cfg(test)]
            target.update_pids_peak_for_pending_child_with_validation(
                self.registry,
                RegistryValidation::SyntheticLocal,
            );
            #[cfg(not(test))]
            target.update_pids_peak_for_pending_child_with_registry(self.registry);
        }
        if let Some(publication) = self.publication.as_ref() {
            publication.visible.store(true, Ordering::Release);
        }
        self.committed = true;
    }
}

impl Drop for CgroupForkAdmission<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _operation = self.registry.operation.lock();
        let mut by_pid = self.registry.by_pid.lock();
        let Some(memberships) = by_pid.get(&self.child_pid) else {
            return;
        };
        let Some(publication) = self.publication.as_ref() else {
            return;
        };
        for reservation in &self.reservations {
            let target_weak = Arc::downgrade(&reservation.target);
            let exact_global = memberships
                .get(&reservation.hierarchy)
                .is_some_and(|membership| {
                    Arc::ptr_eq(&membership.publication, publication)
                        && Weak::ptr_eq(&membership.target, &target_weak)
                        && Weak::ptr_eq(&membership.process_identity, &self.process_identity)
                });
            let mut pids = reservation.target.pids.lock();
            let exact_target = pids
                .get(&self.child_pid)
                .is_some_and(|current| Arc::ptr_eq(current, publication));
            if !exact_global || !exact_target {
                error!(
                    "cgroup fork admission for PID {} lost an exact hidden reservation",
                    self.child_pid
                );
                return;
            }
            pids.remove(&self.child_pid);
        }
        let removed_global = by_pid.remove(&self.child_pid);
        drop(by_pid);
        drop(removed_global);
    }
}

fn can_migrate_with_credentials(
    credentials: &OpenCredentials,
    actor_cred: &Cred,
    target_cred: &Cred,
) -> bool {
    let target_ids = target_cred.ids();
    ns_capable(actor_cred, target_cred.user_ns(), CAP_SYS_ADMIN)
        || [
            credentials.uid,
            credentials.euid,
            credentials.suid,
            credentials.fsuid,
        ]
        .into_iter()
        .any(|uid| uid == target_ids.ruid || uid == target_ids.euid || uid == target_ids.suid)
}

fn can_migrate_from_open_cgroup_namespace(
    credentials: &OpenCredentials,
    actor_cred: &Cred,
) -> bool {
    let current_ns = axtask::current().as_thread().cgroup_ns();
    credentials.cgroup_ns_id == current_ns.id()
        || ns_capable(actor_cred, current_ns.owner_user_ns(), CAP_SYS_ADMIN)
}

fn detach_mapped_pid(process: &Arc<Process>) {
    PID_CGROUPS.detach(process.pid(), process);
}

/// Keeps process-owned parking state aligned with the membership commit.
/// Moving out of a frozen cgroup must wake the task; moving into one is
/// rejected during admission, but this defensive branch also covers future
/// kernel-only membership publishers.
fn reconcile_process_freeze_after_migration(pid: Pid) -> VfsResult<()> {
    let Ok(process) = get_process_data(pid) else {
        return Ok(());
    };
    if cgroup_for_pid(pid).is_some_and(|dir| dir.is_effectively_frozen()) {
        process.request_cgroup_freeze();
        for task in try_tasks().map_err(|_| VfsError::NoMemory)? {
            if task
                .try_as_thread()
                .is_some_and(|thread| thread.proc_data.proc.pid() == pid)
            {
                task.interrupt();
            }
        }
    } else {
        process.thaw_cgroup_freeze();
    }
    Ok(())
}

pub(crate) fn detach_process(process: &Arc<Process>) {
    detach_mapped_pid(process);
}

fn cgroup_for_pid(pid: Pid) -> Option<Arc<CgroupDir>> {
    let process = get_process_including_zombie(pid).ok()?;
    PID_CGROUPS.get_v2(pid, &process)
}

fn cgroup_ancestor_chain(dir: Arc<CgroupDir>) -> Vec<Arc<CgroupDir>> {
    let mut chain = Vec::new();
    let mut current = Some(dir);
    while let Some(dir) = current {
        current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        chain.push(dir);
    }
    chain
}

fn cgroup_dir_name(dir: &CgroupDir) -> Vec<u8> {
    dir.this
        .lock()
        .as_ref()
        .and_then(WeakDirEntry::upgrade)
        .map(|entry| entry.name().as_bytes().to_vec())
        .unwrap_or_default()
}

/// Render an actual hierarchy node relative to a cgroup namespace root.
/// This deliberately walks stable node parent links, never a dentry's global
/// absolute path.  A node outside the view is represented with `..` segments
/// rather than leaking its host-visible ancestor pathname.
fn cgroup_relative_path(dir: Arc<CgroupDir>, view_root: Arc<CgroupDir>) -> Vec<u8> {
    let dir_chain = cgroup_ancestor_chain(dir);
    let root_chain = cgroup_ancestor_chain(view_root);
    let mut dir_index = dir_chain.len();
    let mut root_index = root_chain.len();
    while dir_index != 0
        && root_index != 0
        && Arc::ptr_eq(&dir_chain[dir_index - 1], &root_chain[root_index - 1])
    {
        dir_index -= 1;
        root_index -= 1;
    }

    let mut path = Vec::new();
    path.push(b'/');
    for _ in 0..root_index {
        path.extend_from_slice(b"../");
    }
    for node in dir_chain[..dir_index].iter().rev() {
        let name = cgroup_dir_name(node);
        if !name.is_empty() {
            path.extend_from_slice(&name);
            path.push(b'/');
        }
    }
    if path.len() > 1 && path.last() == Some(&b'/') {
        path.pop();
    }
    path
}

fn cgroup_hierarchy_id(controllers: &[String]) -> usize {
    controllers
        .first()
        .and_then(|controller| ALL_CONTROLLERS.iter().position(|it| *it == controller))
        .map_or(1, |index| index + 1)
}

pub(crate) fn proc_cgroup_membership(
    pid: Pid,
    process: &Arc<Process>,
    roots: &CgroupNamespaceRoots,
) -> Vec<u8> {
    let memberships = PID_CGROUPS.memberships_for_process(pid, process);
    if memberships.is_empty() {
        return b"0::/\n".to_vec();
    }
    let mut output = Vec::new();
    for (key, dir) in memberships {
        let hierarchy = match cgroup_hierarchy_for_dir(&dir) {
            Ok(hierarchy) => hierarchy,
            Err(_) => continue,
        };
        let view_root = match roots.root_for(&hierarchy) {
            Ok(root) => root,
            Err(_) => continue,
        };
        let path = cgroup_relative_path(dir.clone(), view_root);
        match key.version {
            CgroupVersion::V2 => output.extend_from_slice(b"0::"),
            CgroupVersion::V1 => {
                let id = cgroup_hierarchy_id(&key.controllers);
                output.extend_from_slice(format!("{id}:{}:", key.controllers.join(",")).as_bytes());
            }
        }
        output.extend_from_slice(&path);
        output.push(b'\n');
    }
    if output.is_empty() {
        b"0::/\n".to_vec()
    } else {
        output
    }
}

pub(crate) fn proc_cpuset_membership(
    pid: Pid,
    process: &Arc<Process>,
    roots: &CgroupNamespaceRoots,
) -> Vec<u8> {
    PID_CGROUPS
        .memberships_for_process(pid, process)
        .into_iter()
        .find(|(key, _)| {
            key.controllers
                .iter()
                .any(|controller| controller == "cpuset")
        })
        .or_else(|| {
            PID_CGROUPS
                .memberships_for_process(pid, process)
                .into_iter()
                .find(|(key, _)| key.version == CgroupVersion::V2)
        })
        .and_then(|(_, dir)| {
            let hierarchy = cgroup_hierarchy_for_dir(&dir).ok()?;
            let root = roots.root_for(&hierarchy).ok()?;
            let mut path = cgroup_relative_path(dir, root);
            path.push(b'\n');
            Some(path)
        })
        .unwrap_or_else(|| b"/\n".to_vec())
}

/// Stable cgroup identity for perf's `PERF_FLAG_PID_CGROUP` target.  The
/// inode is allocated with the directory and survives rename; an arbitrary
/// directory FD is rejected instead of becoming a root-cgroup alias.
pub(crate) fn perf_cgroup_fd_identity(cgroup_fd: i32) -> AxResult<u64> {
    let directory = get_typed_file::<Directory>(cgroup_fd)?;
    let dir = directory
        .inner()
        .entry()
        .downcast::<CgroupDir>()
        .map_err(|_| AxError::InvalidInput)?;
    (dir.node.fs.version == CgroupVersion::V2)
        .then_some(dir.node.ino)
        .ok_or(AxError::InvalidInput)
}

/// Membership predicate used by the CPU perf scheduler before entering a
/// cgroup-target group.  A disappeared process/membership is false and does
/// not resurrect cgroup state.
pub(crate) fn perf_cgroup_contains(pid: Pid, identity: u64) -> bool {
    cgroup_for_pid(pid).is_some_and(|dir| dir.node.ino == identity)
}

/// Resolves and retains a cgroup-v2 directory for a BPF link.  The returned
/// directory is the target lifetime anchor; an inode number alone would let
/// an unlinked/recycled synthetic cgroup accidentally receive a stale link.
pub(crate) fn bpf_cgroup_fd_target(cgroup_fd: i32) -> AxResult<(u64, Arc<Directory>)> {
    let directory = get_typed_file::<Directory>(cgroup_fd)?;
    let dir = directory
        .inner()
        .entry()
        .downcast::<CgroupDir>()
        .map_err(|_| AxError::InvalidInput)?;
    if dir.node.fs.version != CgroupVersion::V2 {
        return Err(AxError::InvalidInput);
    }
    Ok((dir.node.ino, directory.clone_object()))
}

/// Returns a stable root-to-leaf cgroup-v2 ancestry snapshot for a retained
/// cgroup directory.  BPF attach admission and packet dispatch share this
/// exact walker: hierarchy policy is never reconstructed from a pathname
/// (which may be renamed while a link is alive).
pub(crate) fn bpf_cgroup_hierarchy(directory: &Directory) -> AxResult<Vec<u64>> {
    let directory = directory
        .inner()
        .entry()
        .downcast::<CgroupDir>()
        .map_err(|_| AxError::InvalidInput)?;
    if directory.node.fs.version != CgroupVersion::V2 {
        return Err(AxError::InvalidInput);
    }
    let mut hierarchy = Vec::new();
    hierarchy
        .try_reserve(MAX_CGROUP_DEPTH + 1)
        .map_err(|_| AxError::NoMemory)?;
    let mut current = directory;
    for _ in 0..=MAX_CGROUP_DEPTH {
        hierarchy.push(current.node.ino);
        let Some(parent) = current.parent.lock().as_ref().and_then(Weak::upgrade) else {
            hierarchy.reverse();
            return Ok(hierarchy);
        };
        current = parent;
    }
    Err(AxError::BadState)
}

/// Root-to-leaf membership ancestry for a packet executing in task context.
/// Non-task packet contexts intentionally return an empty hierarchy: cgroup
/// BPF cannot be attributed there and must not inherit an arbitrary creator's
/// cgroup policy.
pub(crate) fn bpf_current_cgroup_hierarchy() -> AxResult<Vec<u64>> {
    let Some(task) = axtask::current_may_uninit() else {
        return Ok(Vec::new());
    };
    let Some(thread) = task.try_as_thread() else {
        return Ok(Vec::new());
    };
    let Some(mut current) = cgroup_for_pid(thread.proc_data.proc.pid()) else {
        return Ok(Vec::new());
    };
    let mut hierarchy = Vec::new();
    hierarchy
        .try_reserve(MAX_CGROUP_DEPTH + 1)
        .map_err(|_| AxError::NoMemory)?;
    for _ in 0..=MAX_CGROUP_DEPTH {
        hierarchy.push(current.node.ino);
        let Some(parent) = current.parent.lock().as_ref().and_then(Weak::upgrade) else {
            hierarchy.reverse();
            return Ok(hierarchy);
        };
        current = parent;
    }
    Err(AxError::BadState)
}

/// Packet hooks may run from a task that is distinct from the creator of a
/// link.  Attribute the event through the live process membership index; a
/// worker/IRQ context with no Linux thread simply does not match cgroup BPF.
pub(crate) fn bpf_current_in_cgroup(identity: u64) -> bool {
    bpf_current_cgroup_hierarchy()
        .map(|hierarchy| hierarchy.contains(&identity))
        .unwrap_or(false)
}

/// Prepares an invisible fork-child membership inherited from `parent_pid`.
///
/// Clone should retain the returned token through every fallible construction
/// step, then call [`CgroupForkAdmission::commit`] in its final publication
/// phase. Dropping the token precisely removes both hidden reservations.
pub(crate) fn prepare_fork_charge(
    parent_pid: Pid,
    child_pid: Pid,
    child: &Arc<Process>,
) -> AxResult<CgroupForkAdmission<'static>> {
    PID_CGROUPS.prepare_charge_from_for_child(parent_pid, child_pid, child)
}

/// Prepares an invisible fork-child membership in the cgroup named by an fd.
pub(crate) fn prepare_fork_charge_into(
    parent_pid: Pid,
    cgroup_fd: i32,
    child_pid: Pid,
    child: &Arc<Process>,
) -> AxResult<CgroupForkAdmission<'static>> {
    let dir_file = get_typed_file::<Directory>(cgroup_fd)?;
    let dir = dir_file
        .inner()
        .entry()
        .downcast::<CgroupDir>()
        .map_err(|_| AxError::InvalidInput)?;
    if dir.node.fs.version != CgroupVersion::V2 {
        return Err(AxError::InvalidInput);
    }
    if dir.v2_has_enabled_child_controllers() {
        return Err(AxError::ResourceBusy);
    }
    // Keep the source PID explicit: a CLONE_INTO_CGROUP admission replaces
    // only this v2 hierarchy while retaining memberships inherited from the
    // source in every other hierarchy.  The multi-hierarchy registry owns
    // that atomic reservation set.
    PID_CGROUPS.prepare_fork_attach_from(parent_pid, &dir, child_pid, child)
}

impl NodeOps for CgroupDir {
    fn inode(&self) -> u64 {
        self.node.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(self.node.metadata())
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.node.update_metadata(update);
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.fs.as_ref()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl DirNodeOps for CgroupDir {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        node_type == NodeType::Directory
    }

    fn supports_rmdir(&self) -> bool {
        true
    }

    fn supports_rename(&self) -> bool {
        self.node.fs.version == CgroupVersion::V1
    }

    fn namespace_epoch(&self) -> u64 {
        self.namespace_epoch.load(Ordering::Acquire)
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let parent_ino = self
            .parent
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .map_or(self.node.ino, |parent| parent.node.ino);
        let mut position = 0_u64;
        let mut count = 0;
        let mut emit = |name: &FsName, ino: u64, node_type: NodeType| {
            let current = position;
            position = position.saturating_add(1);
            if current < offset {
                return true;
            }
            if !sink.accept(name, ino, node_type, position) {
                return false;
            }
            count += 1;
            true
        };
        if !emit(FsName::new(b"."), self.node.ino, NodeType::Directory)
            || !emit(FsName::new(b".."), parent_ino, NodeType::Directory)
        {
            return Ok(count);
        }
        for (name, dir) in self.children.lock().iter() {
            if !emit(name, dir.node.ino, NodeType::Directory) {
                return Ok(count);
            }
        }
        for (name, file) in &self.files {
            if self.control_file_visible(file.name)
                && !emit(name, file.node.ino, NodeType::RegularFile)
            {
                return Ok(count);
            }
        }
        Ok(count)
    }

    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        if let Some(child) = self.children.lock().get(name).cloned() {
            return self.try_child_entry(name, child);
        }
        if let Some(file) = self.files.get(name).cloned()
            && self.control_file_visible(file.name)
        {
            return self.try_file_entry(name, file);
        }
        Err(VfsError::NotFound)
    }

    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if name.as_bytes().contains(&b'\n') {
            return Err(VfsError::InvalidInput);
        }
        let _namespace = self.node.fs.namespace.lock();
        let mut children = self.children.lock();
        if let Some(child) = children.get(name).cloned() {
            if disposition == CreateDisposition::Exclusive {
                return Err(VfsError::AlreadyExists);
            }
            return Ok(CreateOutcome {
                entry: self.try_child_entry(name, child)?,
                created: false,
            });
        }
        if self.files.contains_key(name) {
            if disposition == CreateDisposition::Exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let control_name =
                core::str::from_utf8(name.as_bytes()).map_err(|_| VfsError::InvalidInput)?;
            let file = self
                .control_file_visible(control_name)
                .then(|| self.files.get(name).cloned())
                .flatten()
                .ok_or(VfsError::NotFound)?;
            return Ok(CreateOutcome {
                entry: self.try_file_entry(name, file)?,
                created: false,
            });
        }
        if options.node_type != NodeType::Directory || options.rdev.is_some() {
            return Err(VfsError::OperationNotPermitted);
        }
        if self.hierarchy_depth()? >= MAX_CGROUP_DEPTH {
            return Err(VfsError::FilesystemLoop);
        }
        try_reserve_cgroup_child_slot(&mut children, MAX_CGROUP_CHILDREN, true)?;
        let owned_name = try_owned_name(name)?;
        let child = Self::try_new(
            self.node.fs.clone(),
            Some(Arc::downgrade(&self.this_dir()?)),
        )?;
        child.node.update_metadata(MetadataUpdate {
            mode: Some(options.permission),
            owner: options.owner,
            ..Default::default()
        });
        let entry = self.try_child_entry(name, child.clone())?;
        options.install_initial_data(&entry)?;
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        children.insert(owned_name, child.clone());
        let now = wall_time();
        drop(children);
        self.touch_namespace(now);
        Ok(CreateOutcome {
            entry,
            created: true,
        })
    }

    fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let _namespace = self.node.fs.namespace.lock();
        // Fork admission uses the same operation before installing a hidden
        // member. Holding it from the emptiness check through namespace
        // removal prevents an admission from targeting a just-detached
        // cgroup between those two steps.
        let _operation = PID_CGROUPS.operation.lock();
        if self.files.contains_key(request.name) {
            return Err(VfsError::OperationNotPermitted);
        }
        let mut children = self.children.lock();
        let Some(child) = children.get(request.name).cloned() else {
            return Err(VfsError::NotFound);
        };
        if request
            .expected
            .is_some_and(|expected| !self.matches_expected_dir(expected, &child))
        {
            return Err(VfsError::NotFound);
        }
        if !request.is_dir {
            return Err(VfsError::IsADirectory);
        }
        if child.has_real_children() {
            return Err(VfsError::DirectoryNotEmpty);
        }
        if !child.pids.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        children.remove(request.name);
        let now = wall_time();
        drop(children);
        self.touch_namespace(now);
        Ok(())
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_dir = request.dst_dir.downcast::<Self>()?;
        if !Arc::ptr_eq(&self.node.fs, &dst_dir.node.fs) {
            return Err(VfsError::CrossesDevices);
        }
        if self.node.fs.version != CgroupVersion::V1 {
            return Err(VfsError::OperationNotPermitted);
        }
        if request.dst_name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if request.dst_name.as_bytes().contains(&b'\n') {
            return Err(VfsError::InvalidInput);
        }
        let _namespace = self.node.fs.namespace.lock();
        // Keep the parent chain stable while fork publication accounts
        // pids.peak. The lock order is namespace -> membership operation ->
        // directory/member locks; membership paths never acquire namespace.
        let _operation = PID_CGROUPS.operation.lock();
        if self.files.contains_key(request.src_name) || dst_dir.files.contains_key(request.dst_name)
        {
            return Err(VfsError::OperationNotPermitted);
        }
        let same_parent = core::ptr::eq(self, Arc::as_ref(&dst_dir));

        if same_parent {
            let mut children = self.children.lock();
            let child = children
                .get(request.src_name)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if !self.matches_expected_dir(request.src, &child) {
                return Err(VfsError::NotFound);
            }
            let dst = children.get(request.dst_name).cloned();
            match (request.dst, dst.as_ref()) {
                (None, None) => {}
                (Some(expected), Some(actual)) if self.matches_expected_dir(expected, actual) => {}
                _ => return Err(VfsError::NotFound),
            }
            if dst.as_ref().is_some_and(|dst| Arc::ptr_eq(&child, dst)) {
                return Ok(());
            }
            if dst.is_some() {
                return Err(VfsError::AlreadyExists);
            }

            try_reserve_cgroup_child_slot(&mut children, MAX_CGROUP_CHILDREN, false)?;
            let dst_name = try_owned_name(request.dst_name)?;
            self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            children.remove(request.src_name);
            children.insert(dst_name, child.clone());
            let now = wall_time();
            drop(children);
            child.node.update_metadata(MetadataUpdate {
                ctime: Some(now.into()),
                ..Default::default()
            });
            self.touch_namespace(now);
            return Ok(());
        }

        let src_dir = self.this_dir()?;
        let src_is_ancestor = Self::is_same_or_descendant_of(&dst_dir, &src_dir);
        let dst_is_ancestor = Self::is_same_or_descendant_of(&src_dir, &dst_dir);
        let lock_src_first = if src_is_ancestor {
            true
        } else if dst_is_ancestor {
            false
        } else {
            (Arc::as_ptr(&src_dir).cast::<()>() as usize)
                < Arc::as_ptr(&dst_dir).cast::<()>() as usize
        };
        let commit = |src_children: &mut HashMap<FsNameBuf, Arc<CgroupDir>>,
                      dst_children: &mut HashMap<FsNameBuf, Arc<CgroupDir>>|
         -> VfsResult<(Arc<CgroupDir>, bool)> {
            let child = src_children
                .get(request.src_name)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if !self.matches_expected_dir(request.src, &child) {
                return Err(VfsError::NotFound);
            }
            let dst = dst_children.get(request.dst_name).cloned();
            match (request.dst, dst.as_ref()) {
                (None, None) => {}
                (Some(expected), Some(actual)) if self.matches_expected_dir(expected, actual) => {}
                _ => return Err(VfsError::NotFound),
            }
            if dst.as_ref().is_some_and(|dst| Arc::ptr_eq(&child, dst)) {
                return Ok((child, false));
            }
            if dst.is_some() {
                return Err(VfsError::AlreadyExists);
            }
            if Self::is_same_or_descendant_of(&dst_dir, &child) {
                return Err(VfsError::InvalidInput);
            }
            let target_depth = dst_dir.hierarchy_depth()?;
            let subtree_height = child.subtree_height(MAX_CGROUP_DEPTH)?;
            if target_depth
                .checked_add(1)
                .and_then(|depth| depth.checked_add(subtree_height))
                .is_none_or(|depth| depth > MAX_CGROUP_DEPTH)
            {
                return Err(VfsError::FilesystemLoop);
            }

            try_reserve_cgroup_child_slot(dst_children, MAX_CGROUP_CHILDREN, true)?;
            let dst_name = try_owned_name(request.dst_name)?;
            let new_parent = Arc::downgrade(&dst_dir);
            self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            dst_dir.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            src_children.remove(request.src_name);
            dst_children.insert(dst_name, child.clone());
            *child.parent.lock() = Some(new_parent);
            Ok((child, true))
        };
        let (child, changed) = if lock_src_first {
            let mut src_children = self.children.lock();
            let mut dst_children = dst_dir.children.lock();
            commit(&mut src_children, &mut dst_children)?
        } else {
            let mut dst_children = dst_dir.children.lock();
            let mut src_children = self.children.lock();
            commit(&mut src_children, &mut dst_children)?
        };
        if !changed {
            return Ok(());
        }
        let now = wall_time();
        child.node.update_metadata(MetadataUpdate {
            ctime: Some(now.into()),
            ..Default::default()
        });
        self.touch_namespace(now);
        dst_dir.touch_namespace(now);
        child.update_pids_peak_hierarchy_with_registry(&PID_CGROUPS);
        Ok(())
    }

    fn is_cacheable(&self) -> bool {
        true
    }
}

impl CgroupDir {
    fn this_dir(&self) -> VfsResult<Arc<CgroupDir>> {
        self.this
            .lock()
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::InvalidInput)?
            .downcast::<CgroupDir>()
    }
}

struct CgroupFile {
    node: CgroupNode,
    name: &'static str,
    dir: Mutex<Option<Weak<CgroupDir>>>,
}

impl CgroupFile {
    fn try_new(fs: Arc<CgroupFs>, name: &'static str) -> VfsResult<Arc<Self>> {
        let mode = NodePermission::from_bits_truncate(match name {
            "cgroup.kill" => 0o200,
            _ if is_read_only_control_file(name) => 0o444,
            _ => 0o644,
        });
        let node = CgroupNode::try_new(fs, NodeType::RegularFile, mode)?;
        Arc::try_new(Self {
            node,
            name,
            dir: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn bind_dir(&self, dir: &Arc<CgroupDir>) {
        let mut slot = self.dir.lock();
        if slot.is_none() {
            *slot = Some(Arc::downgrade(dir));
        }
    }

    fn dir(&self) -> VfsResult<Arc<CgroupDir>> {
        self.dir
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::InvalidInput)
    }

    fn read_text(&self) -> VfsResult<String> {
        let dir = self.dir()?;
        // Whole-file snapshots linearize with membership publication,
        // migration, parent rename, and controller reset. This prevents a
        // reader from observing pids.current after the member becomes visible
        // but pids.peak before its monotonic update.
        let _operation = PID_CGROUPS.operation.lock();
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        Ok(match self.name {
            "tasks" | "cgroup.procs" => dir.tasks_text_while_operating()?,
            "cgroup.controllers" => dir.controllers_text()?,
            "cgroup.subtree_control" => dir.subtree_control_text()?,
            "cgroup.events" => dir.events_text(),
            "cgroup.type" => "domain\n".to_string(),
            "cgroup.freeze" => dir.freeze_text(),
            "cgroup.kill" => return Err(VfsError::BadFileDescriptor),
            "pids.max" => dir.pids_max_text(),
            "pids.current" => format!("{}\n", dir.recursive_live_pid_count_while_operating()),
            "pids.events" => format!("max {}\n", *dir.pids_events_limit.lock()),
            "pids.peak" => format!("{}\n", *dir.pids_peak.lock()),
            "cpu.uclamp.min" => dir.uclamp_text(true),
            "cpu.uclamp.max" => dir.uclamp_text(false),
            _ => return Err(VfsError::NotFound),
        })
    }

    fn write_text(&self, data: &[u8]) -> VfsResult<()> {
        let dir = self.dir()?;
        if self.name == "pids.max" {
            // File visibility and the new limit must be one operation with
            // controller reset and fork admission. A stale open control file
            // cannot recreate a limit after its parent disabled pids.
            let _operation = PID_CGROUPS.operation.lock();
            if !dir.control_file_visible(self.name) {
                return Err(VfsError::NotFound);
            }
            return dir.set_pids_max(data);
        }
        if self.name == "cgroup.freeze" {
            // Admission, migration, and freezer state share one operation
            // domain, so no child can appear in a frozen hierarchy between
            // this write and the next cgroup.procs/clone admission.
            return dir.set_frozen(data);
        }
        if matches!(self.name, "cpu.uclamp.min" | "cpu.uclamp.max") {
            let tasks = snapshot_live_uclamp_tasks()?;
            let _operation = PID_CGROUPS.operation.lock();
            if !dir.control_file_visible(self.name) {
                return Err(VfsError::NotFound);
            }
            let generation = dir.set_uclamp(self.name == "cpu.uclamp.min", data)?;
            drop(_operation);
            return republish_live_uclamp_tasks(tasks, None, generation).map(|_| ());
        }
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        match self.name {
            "tasks" | "cgroup.procs" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                let pid = text
                    .trim()
                    .parse::<Pid>()
                    .map_err(|_| VfsError::InvalidInput)?;
                dir.attach_pid(pid)
            }
            "cgroup.kill" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                if text.trim() != "1" {
                    return Err(VfsError::InvalidInput);
                }
                dir.kill_attached_recursive()
            }
            "cgroup.subtree_control" => dir.update_subtree_control(data),
            "cgroup.controllers" | "cgroup.events" | "cgroup.type" | "pids.current"
            | "pids.events" | "pids.peak" => Err(VfsError::BadFileDescriptor),
            _ => Err(VfsError::NotFound),
        }
    }
}

fn is_read_only_control_file(name: &str) -> bool {
    matches!(
        name,
        "cgroup.controllers"
            | "cgroup.events"
            | "cgroup.type"
            | "pids.current"
            | "pids.events"
            | "pids.peak"
    )
}

impl NodeOps for CgroupFile {
    fn inode(&self) -> u64 {
        self.node.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let dir = self.dir()?;
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        let mut metadata = self.node.metadata();
        metadata.size = self.read_text().map_or(0, |text| text.len() as u64);
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let dir = self.dir()?;
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        self.node.update_metadata(update);
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.fs.as_ref()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        cgroup_control_file_flags()
    }
}

impl FileNodeOps for CgroupFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.read_text()?;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data.as_bytes()[offset as usize..];
        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.write_text(buf)?;
        Ok(buf.len())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.write_text(buf)?;
        Ok((buf.len(), 0))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        if len == 0 {
            return Ok(());
        }
        Err(VfsError::InvalidInput)
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

impl Pollable for CgroupFile {
    fn poll(&self) -> IoEvents {
        if self.name == "cgroup.kill" {
            IoEvents::WRITABLE
        } else if is_read_only_control_file(self.name) {
            IoEvents::READABLE
        } else {
            IoEvents::READABLE | IoEvents::WRITABLE
        }
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl CgroupDir {
    fn bind_control_files(self: &Arc<Self>) {
        for file in self.files.values() {
            file.bind_dir(self);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use axfs_ng_vfs::Timestamp;

    use super::*;

    #[test]
    fn scheduler_clamp_controls_keep_minimum_and_maximum_ordered() {
        let mut controls = SchedUtilClampControls::unrestricted();
        controls.set_minimum(400).unwrap();
        assert_eq!(controls.set_maximum(399), Err(VfsError::InvalidInput));
        controls.set_maximum(800).unwrap();
        controls.set_minimum_rt_default(1024).unwrap();
        assert_eq!(
            controls.set_minimum_rt_default(SchedUtilClampControls::MAX + 1),
            Err(VfsError::InvalidInput)
        );
    }

    #[test]
    fn effective_uclamp_composes_hierarchy_system_rt_and_explicit_sides() {
        let constraints = axtask::UclampConstraints {
            system_minimum: 200,
            system_maximum: 900,
            // This is the already-folded parent/child cgroup result.
            cgroup_minimum: 400,
            cgroup_maximum: 700,
            rt_default_minimum: 600,
        };
        let default = axtask::UclampRequest::unrestricted();
        assert_eq!(
            constraints.effective(default, axtask::SchedClass::Normal),
            axtask::UtilizationBounds::new(400, 700).unwrap()
        );
        assert_eq!(
            constraints.effective(default, axtask::SchedClass::Fifo),
            axtask::UtilizationBounds::new(600, 700).unwrap()
        );
        let explicit = axtask::UclampRequest {
            minimum: 800,
            maximum: 950,
            minimum_user_defined: true,
            maximum_user_defined: true,
        };
        // Explicit task requests survive class changes but are constrained by
        // the enclosing policy instead of escaping it.
        assert_eq!(
            constraints.effective(explicit, axtask::SchedClass::Normal),
            axtask::UtilizationBounds::new(700, 700).unwrap()
        );
    }

    #[test]
    fn repeated_policy_resolution_has_no_clamp_drift() {
        let constraints = axtask::UclampConstraints {
            system_minimum: 128,
            system_maximum: 896,
            cgroup_minimum: 256,
            cgroup_maximum: 768,
            rt_default_minimum: 512,
        };
        let request = axtask::UclampRequest {
            minimum: 640,
            maximum: 704,
            minimum_user_defined: true,
            maximum_user_defined: true,
        };
        let expected = constraints.effective(request, axtask::SchedClass::Normal);
        for _ in 0..1_000 {
            assert_eq!(
                constraints.effective(request, axtask::SchedClass::Normal),
                expected
            );
        }
    }

    #[test]
    fn uclamp_reconcile_failure_classifies_nonterminal_scheduler_outcomes() {
        assert_eq!(
            UclampReconcileFailure::from_task_sched(axtask::TaskSchedError::Unsupported),
            Some(UclampReconcileFailure::Unsupported)
        );
        assert_eq!(
            UclampReconcileFailure::from_task_sched(axtask::TaskSchedError::RunQueueUnavailable(3)),
            Some(UclampReconcileFailure::RunQueueUnavailable)
        );
        assert_eq!(
            UclampReconcileFailure::from_task_sched(axtask::TaskSchedError::TaskExited),
            None
        );
    }

    #[test]
    fn uclamp_reconcile_claim_preserves_same_generation_republish_race() {
        let pending = AtomicU64::new(0);
        publish_uclamp_reconcile_pending_to(&pending, 8);
        publish_uclamp_reconcile_pending_to(&pending, 6);
        assert_eq!(pending.load(Ordering::Acquire), 8);

        // Claim must precede snapshot/allocation. A concurrent publisher of
        // the same generation gets a new ticket instead of being cleared by
        // the old pass after it returns.
        assert_eq!(pending.swap(0, Ordering::AcqRel), 8);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        publish_uclamp_reconcile_pending_to(&pending, 8);
        assert_eq!(pending.load(Ordering::Acquire), 8);

        // A newer commit always supersedes the old retry ticket.
        publish_uclamp_reconcile_pending_to(&pending, 10);
        assert_eq!(pending.load(Ordering::Acquire), 10);
    }

    #[test]
    fn uclamp_generation_wrap_skips_empty_pending_sentinel() {
        let final_generation = u64::MAX - 1;
        let in_progress = uclamp_policy_write_generation(final_generation);
        assert_eq!(in_progress, u64::MAX);
        assert_eq!(uclamp_policy_commit_generation(in_progress), 2);
    }

    #[test]
    fn uclamp_serial_comparison_handles_max_to_one() {
        // The serial predicate itself is general modular arithmetic. Live
        // policy generations remain even and therefore use MAX -> 2 above.
        assert!(uclamp_generation_is_newer(1, u64::MAX));
        assert!(!uclamp_generation_is_newer(u64::MAX, 1));
    }

    #[test]
    fn uclamp_pending_serial_order_survives_generation_wrap() {
        let stale_high = u64::MAX - 1;
        let new_low = 2;
        assert!(uclamp_generation_is_newer(new_low, stale_high));
        assert!(!uclamp_generation_is_newer(stale_high, new_low));

        let pending = AtomicU64::new(0);
        publish_uclamp_reconcile_pending_to(&pending, stale_high);
        publish_uclamp_reconcile_pending_to(&pending, new_low);
        assert_eq!(pending.load(Ordering::Acquire), new_low);

        // A late retry for the pre-wrap policy cannot replace the newer
        // post-wrap ticket.
        publish_uclamp_reconcile_pending_to(&pending, stale_high);
        assert_eq!(pending.load(Ordering::Acquire), new_low);
    }

    #[test]
    fn uclamp_same_generation_republish_survives_wrap_boundary() {
        let pending = AtomicU64::new(0);
        let generation = 2;
        publish_uclamp_reconcile_pending_to(&pending, generation);
        assert_eq!(pending.swap(0, Ordering::AcqRel), generation);
        publish_uclamp_reconcile_pending_to(&pending, generation);
        assert_eq!(pending.load(Ordering::Acquire), generation);
    }

    #[test]
    fn uclamp_user_return_failures_rejoin_the_worker_lane() {
        for error in [
            axtask::TaskSchedError::Unsupported,
            axtask::TaskSchedError::RunQueueUnavailable(0),
        ] {
            assert!(UclampReconcileFailure::from_task_sched(error).is_some());
        }
        assert!(
            UclampReconcileFailure::from_task_sched(axtask::TaskSchedError::TaskExited).is_none()
        );
    }

    fn test_cgroup_dir() -> Arc<CgroupDir> {
        let fs = Arc::new(CgroupFs {
            name: "test-cgroup",
            fs_type: CGROUP_SUPER_MAGIC,
            version: CgroupVersion::V1,
            controllers: Vec::from(["pids".to_string()]),
            namespace: Mutex::new(()),
            inodes: Mutex::new(HashSet::new()),
            next_inode: AtomicU64::new(1),
            root: Mutex::new(None),
            root_dir: Mutex::new(None),
        });
        CgroupDir::try_new_root(fs).unwrap()
    }

    fn test_cgroup_fs() -> Filesystem {
        CgroupFs::mount(CgroupVersion::V1, Vec::from(["pids".to_string()])).unwrap()
    }

    fn metadata_state(entry: &DirEntry) -> (u64, Timestamp, Timestamp, Timestamp, Timestamp) {
        let metadata = entry.metadata().unwrap();
        (
            metadata.nlink,
            metadata.atime,
            metadata.btime,
            metadata.mtime,
            metadata.ctime,
        )
    }

    fn install_rename_timestamp_sentinels(parents: &[&DirEntry], source: &DirEntry) {
        let sentinel = Timestamp::from(core::time::Duration::MAX);
        for parent in parents {
            parent
                .update_metadata(MetadataUpdate {
                    mtime: Some(sentinel),
                    ctime: Some(sentinel),
                    ..Default::default()
                })
                .unwrap();
        }
        source
            .update_metadata(MetadataUpdate {
                ctime: Some(sentinel),
                ..Default::default()
            })
            .unwrap();
    }

    fn maps_to(registry: &PidMembershipRegistry, pid: Pid, expected: &Arc<CgroupDir>) -> bool {
        let key = hierarchy_key_for_dir(expected).unwrap();
        registry
            .by_pid
            .lock()
            .get(&pid)
            .and_then(|memberships| memberships.get(&key))
            .filter(|membership| membership.is_visible())
            .and_then(|membership| membership.target.upgrade())
            .is_some_and(|actual| Arc::ptr_eq(&actual, expected))
    }

    #[test]
    fn namespace_owner_cgroup_control_files_freeze_open_credential() {
        assert!(cgroup_control_file_flags().contains(NodeFlags::OPEN_CREDENTIAL));
    }

    #[test]
    fn v1_controller_parser_rejects_unimplemented_controllers() {
        assert_eq!(
            parse_v1_controllers("none", "pids").unwrap(),
            ["pids".to_string()]
        );
        assert_eq!(
            parse_v1_controllers("memory", "").unwrap_err(),
            AxError::NoSuchDevice
        );
        assert_eq!(
            parse_v1_controllers("none", "pids,memory").unwrap_err(),
            AxError::NoSuchDevice
        );
        assert_eq!(
            parse_v1_controllers("none", "unknown").unwrap_err(),
            AxError::InvalidInput
        );
    }

    #[test]
    fn v2_advertises_only_pids_and_exposes_domain_liveness_controls() {
        let fs = new_cgroup_v2().unwrap();
        let root = fs.root_dir();
        let dir = root.downcast::<CgroupDir>().unwrap();

        assert_eq!(dir.controllers_text().unwrap(), "pids\n");
        assert_eq!(dir.events_text(), "populated 0\nfrozen 0\n");
        assert_eq!(dir.freeze_text(), "0\n");
        assert!(dir.control_file_visible("cgroup.type"));
        assert!(!dir.control_file_visible("cpu.uclamp.min"));
        assert!(!dir.controller_available("cpu"));
    }

    #[test]
    fn v2_freeze_rejects_fork_and_allows_existing_process_migration() {
        let fs = new_cgroup_v2().unwrap();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let child = root_dir
            .create(
                FsName::new(b"frozen"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let child = child.downcast::<CgroupDir>().unwrap();
        let registry = PidMembershipRegistry::with_limits(4, 4);

        child.set_frozen(b"1\n").unwrap();
        assert_eq!(registry.try_attach(&child, 101, false), Ok(true));
        assert_eq!(
            registry.prepare_charge_from(101, 202).err(),
            Some(AxError::ResourceBusy)
        );
        child.set_frozen(b"0\n").unwrap();
        assert_eq!(
            child.events_text_with_registry(&registry),
            "populated 1\nfrozen 0\n"
        );
    }

    #[test]
    fn membership_publish_updates_both_indexes() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();

        assert_eq!(registry.try_attach(&target, 101, false), Ok(true));
        assert!(target.pids.lock().contains_key(&101));
        assert!(maps_to(&registry, 101, &target));
        assert_eq!(*target.pids_peak.lock(), 1);
    }

    #[test]
    fn fork_admission_stays_invisible_to_readers_until_one_commit() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();
        let admission = registry.prepare_charge_from(101, 202).unwrap();

        // Both capacity/identity slots exist, but cgroup.procs, cgroup.kill,
        // pids.current, and the reverse PID lookup all use these filtered
        // reader paths and must still observe only the parent.
        assert_eq!(registry.by_pid.lock().len(), 2);
        assert_eq!(target.pids.lock().len(), 2);
        assert!(registry.get(202).is_none());
        assert_eq!(
            target.try_live_pids_with_registry(&registry).unwrap(),
            [101]
        );
        assert_eq!(target.tasks_text_with_registry(&registry).unwrap(), "101\n");
        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 1);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..256 {
                    assert!(registry.get(202).is_none());
                    assert_eq!(
                        target.try_live_pids_with_registry(&registry).unwrap(),
                        [101]
                    );
                    assert!(
                        !target
                            .tasks_text_with_registry(&registry)
                            .unwrap()
                            .contains("202")
                    );
                    std::thread::yield_now();
                }
            });
        });

        admission.commit();
        assert!(maps_to(&registry, 202, &target));
        let mut visible = target.try_live_pids_with_registry(&registry).unwrap();
        visible.sort_unstable();
        assert_eq!(visible, [101, 202]);
        assert!(
            target
                .tasks_text_with_registry(&registry)
                .unwrap()
                .contains("202\n")
        );
        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 2);
        assert_eq!(*target.pids_peak.lock(), 2);
    }

    #[test]
    fn dropped_fork_admission_refunds_exact_hidden_slots_and_capacity() {
        let registry = PidMembershipRegistry::with_limits(2, 2);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();

        let first = registry.prepare_charge_from(101, 202).unwrap();
        assert_eq!(
            registry.prepare_charge_from(101, 303).err(),
            Some(AxError::NoMemory)
        );
        drop(first);

        assert!(!registry.by_pid.lock().contains_key(&202));
        assert!(!target.pids.lock().contains_key(&202));
        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 1);
        assert_eq!(*target.pids_peak.lock(), 1);

        registry.prepare_charge_from(101, 303).unwrap().commit();
        assert!(maps_to(&registry, 303, &target));
        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 2);
    }

    #[test]
    fn fork_drop_keeps_both_indexes_when_target_identity_changes() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();
        let admission = registry.prepare_charge_from(101, 202).unwrap();

        target
            .pids
            .lock()
            .insert(202, Arc::new(CgroupMembershipPublication::new(false)));
        drop(admission);

        assert!(registry.by_pid.lock().contains_key(&202));
        assert!(target.pids.lock().contains_key(&202));
        assert_eq!(registry.by_pid.lock().len(), 2);
        assert_eq!(target.pids.lock().len(), 2);
    }

    #[test]
    fn fork_drop_keeps_both_indexes_when_global_identity_changes() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();
        let admission = registry.prepare_charge_from(101, 202).unwrap();

        let key = hierarchy_key_for_dir(&target).unwrap();
        registry
            .by_pid
            .lock()
            .get_mut(&202)
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .publication = Arc::new(CgroupMembershipPublication::new(false));
        drop(admission);

        assert!(registry.by_pid.lock().contains_key(&202));
        assert!(target.pids.lock().contains_key(&202));
        assert_eq!(registry.by_pid.lock().len(), 2);
        assert_eq!(target.pids.lock().len(), 2);
    }

    #[test]
    fn pending_fork_is_charged_to_pids_max_without_becoming_visible() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();
        *target.pids_max.lock() = Some(2);

        let pending = registry.prepare_charge_from(101, 202).unwrap();
        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 1);
        assert_eq!(
            registry.prepare_charge_from(101, 303).err(),
            Some(AxError::WouldBlock)
        );
        assert_eq!(*target.pids_events_limit.lock(), 1);

        drop(pending);
        registry.prepare_charge_from(101, 303).unwrap().commit();
        assert!(maps_to(&registry, 303, &target));
    }

    #[test]
    fn concurrent_fork_commits_keep_peak_at_or_above_current() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();
        registry.try_attach(&target, 101, false).unwrap();
        let first = registry.prepare_charge_from(101, 202).unwrap();
        let second = registry.prepare_charge_from(101, 303).unwrap();
        let barrier = std::sync::Barrier::new(3);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                first.commit();
            });
            scope.spawn(|| {
                barrier.wait();
                second.commit();
            });
            barrier.wait();
        });

        assert_eq!(target.recursive_live_pid_count_with_registry(&registry), 3);
        assert_eq!(*target.pids_peak.lock(), 3);
    }

    #[test]
    fn target_limit_failure_preserves_old_membership() {
        let registry = PidMembershipRegistry::with_limits(4, 1);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();
        registry.try_attach(&target, 202, false).unwrap();

        assert_eq!(
            registry.try_attach(&target, 101, false),
            Err(AxError::NoMemory)
        );
        assert!(old.pids.lock().contains_key(&101));
        assert!(!target.pids.lock().contains_key(&101));
        assert!(target.pids.lock().contains_key(&202));
        assert!(maps_to(&registry, 101, &old));
        assert!(maps_to(&registry, 202, &target));
    }

    #[test]
    fn global_limit_failure_does_not_publish_target_membership() {
        let registry = PidMembershipRegistry::with_limits(1, 4);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();

        assert_eq!(
            registry.try_attach(&target, 202, false),
            Err(AxError::NoMemory)
        );
        assert!(!target.pids.lock().contains_key(&202));
        assert!(maps_to(&registry, 101, &old));
        assert_eq!(registry.by_pid.lock().len(), 1);
    }

    #[test]
    fn migration_is_atomic_and_same_target_attach_is_idempotent() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();

        assert_eq!(registry.try_attach(&target, 101, false), Ok(true));
        assert!(!old.pids.lock().contains_key(&101));
        assert!(target.pids.lock().contains_key(&101));
        assert!(maps_to(&registry, 101, &target));
        assert_eq!(registry.by_pid.lock().len(), 1);

        assert_eq!(registry.try_attach(&target, 101, false), Ok(false));
        assert_eq!(target.pids.lock().len(), 1);
        assert_eq!(registry.by_pid.lock().len(), 1);
    }

    #[test]
    fn fork_admission_failure_does_not_publish_or_update_counters() {
        let registry = PidMembershipRegistry::with_limits(0, 1);
        let target = test_cgroup_dir();

        assert_eq!(
            registry.try_attach(&target, 101, true),
            Err(AxError::NoMemory)
        );
        assert!(target.pids.lock().is_empty());
        assert!(registry.by_pid.lock().is_empty());
        assert_eq!(*target.pids_peak.lock(), 0);
        assert_eq!(*target.pids_events_limit.lock(), 0);
    }

    #[test]
    fn pids_max_rejection_increments_limit_event_once() {
        let registry = PidMembershipRegistry::with_limits(1, 1);
        let target = test_cgroup_dir();
        *target.pids_max.lock() = Some(0);

        assert_eq!(
            registry.try_attach(&target, 101, true),
            Err(AxError::WouldBlock)
        );
        assert!(target.pids.lock().is_empty());
        assert!(registry.by_pid.lock().is_empty());
        assert_eq!(*target.pids_peak.lock(), 0);
        assert_eq!(*target.pids_events_limit.lock(), 1);
    }

    #[test]
    fn child_slot_limit_rejects_growth_but_allows_same_map_rename_admission() {
        let mut children = HashMap::new();
        let child = test_cgroup_dir();
        try_reserve_cgroup_child_slot(&mut children, 1, true).unwrap();
        children.insert(FsNameBuf::from_vec(b"child".to_vec()).unwrap(), child);

        assert_eq!(
            try_reserve_cgroup_child_slot(&mut children, 1, true),
            Err(VfsError::NoMemory)
        );
        assert_eq!(
            try_reserve_cgroup_child_slot(&mut children, 1, false),
            Ok(())
        );
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn cgroup_rename_preserves_identity_and_updates_cross_parent_membership() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let mode = NodePermission::from_bits_truncate(0o755);
        let src_parent = root_dir
            .create(FsName::new(b"src-parent"), NodeType::Directory, mode)
            .unwrap();
        let dst_parent = root_dir
            .create(FsName::new(b"dst-parent"), NodeType::Directory, mode)
            .unwrap();
        let src_dir = src_parent.as_dir().unwrap();
        let dst_dir = dst_parent.as_dir().unwrap();
        let child = src_dir
            .create(FsName::new(b"child"), NodeType::Directory, mode)
            .unwrap();
        let wrong = src_dir
            .create(FsName::new(b"wrong"), NodeType::Directory, mode)
            .unwrap();
        let src_backend = src_parent.downcast::<CgroupDir>().unwrap();
        let dst_backend = dst_parent.downcast::<CgroupDir>().unwrap();
        let src_epoch = src_backend.namespace_epoch();
        let dst_epoch = dst_backend.namespace_epoch();

        assert_eq!(
            src_dir
                .rename(
                    FsName::new(b"child"),
                    &wrong,
                    dst_dir,
                    FsName::new(b"moved"),
                    None
                )
                .unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(src_backend.namespace_epoch(), src_epoch);
        assert_eq!(dst_backend.namespace_epoch(), dst_epoch);
        assert_eq!(
            src_dir.lookup(FsName::new(b"child")).unwrap().inode(),
            child.inode()
        );
        assert_eq!(
            dst_dir.lookup(FsName::new(b"moved")).unwrap_err(),
            VfsError::NotFound
        );

        src_dir
            .rename(
                FsName::new(b"child"),
                &child,
                dst_dir,
                FsName::new(b"moved"),
                None,
            )
            .unwrap();
        assert_eq!(
            src_dir.lookup(FsName::new(b"child")).unwrap_err(),
            VfsError::NotFound
        );
        let moved = dst_dir.lookup(FsName::new(b"moved")).unwrap();
        assert_eq!(moved.inode(), child.inode());
        let moved_backend = moved.downcast::<CgroupDir>().unwrap();
        let parent = moved_backend
            .parent
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .unwrap();
        assert!(Arc::ptr_eq(&parent, &dst_backend));
        assert_eq!(src_backend.namespace_epoch(), src_epoch + 1);
        assert_eq!(dst_backend.namespace_epoch(), dst_epoch + 1);

        let no_op_epoch = dst_backend.namespace_epoch();
        dst_dir
            .rename(
                FsName::new(b"moved"),
                &moved,
                dst_dir,
                FsName::new(b"moved"),
                Some(&moved),
            )
            .unwrap();
        assert_eq!(dst_backend.namespace_epoch(), no_op_epoch);
        assert_eq!(
            dst_dir.lookup(FsName::new(b"moved")).unwrap().inode(),
            child.inode()
        );
    }

    #[test]
    fn cgroup_v1_rename_uses_one_timestamp_for_source_and_parents() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let mode = NodePermission::from_bits_truncate(0o755);
        let old_parent = root_dir
            .create(FsName::new(b"old-parent"), NodeType::Directory, mode)
            .unwrap();
        let new_parent = root_dir
            .create(FsName::new(b"new-parent"), NodeType::Directory, mode)
            .unwrap();
        let old_dir = old_parent.as_dir().unwrap();
        let new_dir = new_parent.as_dir().unwrap();
        let source = old_dir
            .create(FsName::new(b"source"), NodeType::Directory, mode)
            .unwrap();
        install_rename_timestamp_sentinels(&[&old_parent, &new_parent], &source);

        old_dir
            .rename(
                FsName::new(b"source"),
                &source,
                new_dir,
                FsName::new(b"renamed"),
                None,
            )
            .unwrap();

        let source_metadata = source.metadata().unwrap();
        let old_parent_metadata = old_parent.metadata().unwrap();
        let new_parent_metadata = new_parent.metadata().unwrap();
        assert_ne!(
            source_metadata.ctime,
            Timestamp::from(core::time::Duration::MAX)
        );
        assert_eq!(old_parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(old_parent_metadata.ctime, source_metadata.ctime);
        assert_eq!(new_parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(new_parent_metadata.ctime, source_metadata.ctime);
    }

    #[test]
    fn cgroup_v1_same_parent_rename_touches_source_and_parent_together() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let source = root_dir
            .create(
                FsName::new(b"source"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        install_rename_timestamp_sentinels(&[&root], &source);

        root_dir
            .rename(
                FsName::new(b"source"),
                &source,
                root_dir,
                FsName::new(b"renamed"),
                None,
            )
            .unwrap();

        let source_metadata = source.metadata().unwrap();
        let parent_metadata = root.metadata().unwrap();
        assert_ne!(
            source_metadata.ctime,
            Timestamp::from(core::time::Duration::MAX)
        );
        assert_eq!(parent_metadata.mtime, source_metadata.ctime);
        assert_eq!(parent_metadata.ctime, source_metadata.ctime);
    }

    #[test]
    fn failed_and_unsupported_cgroup_rename_preserve_metadata() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let mode = NodePermission::from_bits_truncate(0o755);
        let source = root_dir
            .create(FsName::new(b"source"), NodeType::Directory, mode)
            .unwrap();
        let victim = root_dir
            .create(FsName::new(b"victim"), NodeType::Directory, mode)
            .unwrap();
        install_rename_timestamp_sentinels(&[&root], &source);
        victim
            .update_metadata(MetadataUpdate {
                ctime: Some(Timestamp::from(core::time::Duration::MAX)),
                ..Default::default()
            })
            .unwrap();
        let parent_before = metadata_state(&root);
        let source_before = metadata_state(&source);
        let victim_before = metadata_state(&victim);

        assert_eq!(
            root_dir
                .rename(
                    FsName::new(b"source"),
                    &source,
                    root_dir,
                    FsName::new(b"victim"),
                    Some(&victim)
                )
                .unwrap_err(),
            VfsError::AlreadyExists
        );
        assert_eq!(metadata_state(&root), parent_before);
        assert_eq!(metadata_state(&source), source_before);
        assert_eq!(metadata_state(&victim), victim_before);

        let v2 = new_cgroup_v2().unwrap();
        let v2_root = v2.root_dir();
        let v2_root_dir = v2_root.as_dir().unwrap();
        let v2_source = v2_root_dir
            .create(FsName::new(b"source"), NodeType::Directory, mode)
            .unwrap();
        install_rename_timestamp_sentinels(&[&v2_root], &v2_source);
        let v2_parent_before = metadata_state(&v2_root);
        let v2_source_before = metadata_state(&v2_source);

        assert_eq!(
            v2_root_dir
                .rename(
                    FsName::new(b"source"),
                    &v2_source,
                    v2_root_dir,
                    FsName::new(b"renamed"),
                    None
                )
                .unwrap_err(),
            VfsError::OperationNotPermitted
        );
        assert_eq!(metadata_state(&v2_root), v2_parent_before);
        assert_eq!(metadata_state(&v2_source), v2_source_before);
    }

    #[test]
    fn cgroup_unlink_rejects_a_hidden_fork_membership() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let child = root_dir
            .create(
                FsName::new(b"child"),
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let child_backend = child.downcast::<CgroupDir>().unwrap();
        child_backend
            .pids
            .lock()
            .insert(202, Arc::new(CgroupMembershipPublication::new(false)));

        assert_eq!(
            root_dir.unlink(FsName::new(b"child"), true).unwrap_err(),
            VfsError::ResourceBusy
        );
        assert_eq!(
            root_dir.lookup(FsName::new(b"child")).unwrap().inode(),
            child.inode()
        );
    }

    #[test]
    fn cgroup_mutation_capabilities_match_versioned_backends() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();

        assert!(!root_dir.supports_unlink());
        assert!(root_dir.supports_rmdir());
        assert!(root_dir.supports_rename());
        assert!(root_dir.supports_named_create(NodeType::Directory));
        for node_type in [
            NodeType::Unknown,
            NodeType::Fifo,
            NodeType::CharacterDevice,
            NodeType::BlockDevice,
            NodeType::RegularFile,
            NodeType::Symlink,
            NodeType::Socket,
        ] {
            assert!(!root_dir.supports_named_create(node_type));
        }
        assert!(!root_dir.supports_symlink());

        let v2 = new_cgroup_v2().unwrap();
        let v2_root = v2.root_dir();
        let v2_root_dir = v2_root.as_dir().unwrap();
        assert!(!v2_root_dir.supports_unlink());
        assert!(v2_root_dir.supports_rmdir());
        assert!(!v2_root_dir.supports_rename());
        assert!(v2_root_dir.supports_named_create(NodeType::Directory));
        assert!(!v2_root_dir.supports_named_create(NodeType::RegularFile));
        assert!(!v2_root_dir.supports_symlink());
    }
}
