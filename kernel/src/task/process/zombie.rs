//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

/// Terminal scheduler state retained across the live task's exit.
///
/// Linux keeps `p->policy`, `p->prio`, `p->rt_priority`,
/// `p->sched_reset_on_fork` and the uclamp request inside the `task_struct`,
/// which stays allocated until `release_task()`, so `sched_getscheduler`,
/// `sched_getparam` and `sched_rr_get_interval` answer for an unreaped zombie.
/// TheKernel drops its live scheduler entity when the process's last thread
/// exits, so this snapshot carries the same fields across that boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ZombieSchedulerSnapshot {
pub(crate)     class: SchedClass,
pub(crate)     nice: i8,
    /// The real-time priority belongs to the same scheduler transaction as
    /// class, nice, reset-on-fork and version.  It must survive the live task
    /// disappearing so sched_getparam can observe an unreaped zombie exactly
    /// as Linux does.
pub(crate)     rt_priority: u8,
    /// Linux's policy query exposes this flag as part of the returned policy,
    /// including while the group leader is an unreaped zombie.
pub(crate)     reset_on_fork: bool,
    /// Linux's `sched_dl_entity::flags` (`SCHED_DL_FLAGS`), which
    /// `__getparam_dl()` reports through `sched_getattr(2)` for a deadline
    /// task. Retained so a zombie still answers with the bits it held.
pub(crate)     dl_flags: u32,
    /// Raw uclamp request plus per-side ownership retained after the live
    /// scheduler entity disappears.
pub(crate)     uclamp_min: u16,
pub(crate)     uclamp_max: u16,
pub(crate)     uclamp_min_user_defined: bool,
pub(crate)     uclamp_max_user_defined: bool,
pub(crate)     uclamp_effective_min: u16,
pub(crate)     uclamp_effective_max: u16,
    /// The last successfully installed CPU affinity.  The group-leader
    /// identity retains this cell after its live task has exited, matching the
    /// still-addressable unreaped PID lifecycle.
pub(crate)     affinity: AxCpuMask,
    /// Generation of the persistent group-leader binding that owned this
    /// scheduler state. Scheduler commit versions are local to a task, so
    /// they are comparable only within one binding generation.
pub(crate)     identity_epoch: u64,
pub(crate)     version: u64,
}

impl Default for ZombieSchedulerSnapshot {
    fn default() -> Self {
        Self {
            class: SchedClass::Normal,
            nice: 0,
            rt_priority: 0,
            reset_on_fork: false,
            dl_flags: 0,
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

// `reset_on_fork` is deliberately not carried by this conversion: it is
// published by `publish_scheduler_commit()` from the same transaction, and
// `From<SchedState>` is also used for a task's *initial* state, where Linux's
// `sched_reset_on_fork` has not been set yet.
impl From<SchedState> for ZombieSchedulerSnapshot {
    fn from(state: SchedState) -> Self {
        Self {
            class: state.class,
            nice: state.nice,
            rt_priority: state.rt_priority,
            reset_on_fork: false,
            dl_flags: 0,
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

impl ZombieSchedulerSnapshot {
    /// Rebuilds the scheduler state Linux would still read out of the
    /// `task_struct` of an unreaped zombie.
    ///
    /// `nice` and `rt_priority` are mutually exclusive by class, exactly as
    /// they are for a live task, because both are written by the same
    /// scheduler transaction and `EevdfTaskParams::validated()` normalises the
    /// field the class does not use to zero.
pub(crate) const fn state(&self) -> SchedState {
        SchedState {
            class: self.class,
            nice: self.nice,
            rt_priority: self.rt_priority,
        }
    }
}

pub(crate) fn scheduler_version_is_newer_or_equal(candidate: u64, published: u64) -> bool {
    candidate.wrapping_sub(published) < (1_u64 << 63)
}

pub(crate) fn scheduler_publication_matches(
    published_token: u64,
    expected_token: u64,
    commit: TaskSchedulingSnapshot,
    current: Option<TaskSchedulingSnapshot>,
) -> bool {
    published_token == expected_token && current == Some(commit)
}
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
    let pid_ns = process_identity_pid_ns(process);
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
    crate::task::ops::account_released_thread();
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

/// Updates the nice value retained for an authoritative unreaped zombie.
///
/// This is the `set_user_nice()` half of Linux's `set_one_prio()`: Linux
/// resolves a zombie through `find_task_by_vpid()` and writes `p->static_prio`
/// in place, because nothing about a `task_struct`'s scheduling fields is
/// retired before `release_task()`. TheKernel's live scheduler entity is gone,
/// so the retained transaction is what carries the new value to
/// `getpriority(2)`. Like `set_zombie_affinity()`, the reap edge is the
/// serialization point: a reap that wins this lock turns the update into ESRCH
/// rather than mutating a registry entry that is no longer authoritative.
pub(crate) fn set_zombie_nice(process: &Process, nice: i8) -> AxResult<()> {
    ensure_authoritative_zombie(process)?;
    let snapshot = process.zombie_payload().ok_or(AxError::NoSuchProcess)?;
    let owner = snapshot.reap_owner.lock();
    let scheduler = owner
        .as_ref()
        .and_then(|identity| identity.scheduler.as_ref())
        .ok_or(AxError::NoSuchProcess)?;
    scheduler.lock().nice = nice;
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

/// Returns the resource limit an authoritative unreaped zombie retained.
///
/// Linux reads `task_rlimit(p, resource)`, i.e. `p->signal->rlim[resource]`,
/// which `release_task()` is what finally drops
/// (`include/linux/sched/signal.h:758-762`); the durable owner identity keeps
/// the same cell alive for the whole zombie lifetime.
pub(crate) fn zombie_rlimit(process: &Process, resource: u32) -> AxResult<u64> {
    ensure_authoritative_zombie(process)?;
    let limits = process
        .zombie_payload()
        .ok_or(AxError::NoSuchProcess)?
        .reap_owner
        .lock()
        .as_ref()
        .map(|identity| identity.rlimits.clone())
        .ok_or(AxError::NoSuchProcess)?;
    Ok(limits.read()[resource].current)
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
pub(crate) fn retire_group_leader_signal_owner(owner: &GroupLeaderSignalOwner) -> bool {
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
