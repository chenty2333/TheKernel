use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::TimeValue;
use axtask::{
    AxCpuMask, AxTaskRef, DeadlineConfig, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN,
    SchedClass, SchedState, TaskSchedAttrUpdate, TaskSchedError, current,
    future::{BlockOnError, Interrupted, block_on, interruptible},
    sched_state, scheduler_attr_snapshot, scheduler_state_snapshot, set_sched_state_versioned,
    set_sched_state_versioned_with_reset, set_task_affinity, set_task_nice as update_task_nice,
    update_sched_attr_versioned,
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CAP_SYS_ADMIN, CAP_SYS_NICE, CLOCK_BOOTTIME, CLOCK_MONOTONIC,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID, PRIO_PGRP, PRIO_PROCESS,
    PRIO_USER, RLIMIT_NICE, RLIMIT_RTPRIO, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO,
    SCHED_FLAG_KEEP_PARAMS, SCHED_FLAG_KEEP_POLICY, SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE,
    SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, TIMER_ABSTIME, timespec,
};
use thekernel_linux_process_adapter::{Pid, ProcessError};
use thekernel_linux_signal::{DefaultSignalAction, SignalDisposition, Signo};
use thekernel_linux_usercopy::{
    UserMemory, UserMemoryContext, VmMutPtr, VmPtr, vm_load, vm_write_slice,
};

use crate::{
    mm::map_usercopy_error,
    task::{
        AlarmClock, AsThread, Cred, PidNamespace, ProcStateHint, Process, Thread,
        get_process_including_zombie, get_task, get_visible_task, has_pending_syscall_signal,
        ns_capable, prepare_clock_sleep, process_domain,
        security::{
            SchedulerSecurityOperation, SecuritySchedulerContext, SecurityTaskGetSchedulerContext,
            dispatch_scheduler, dispatch_task_getscheduler,
        },
        try_tasks, with_proc_state_hint, zombie_ioprio, zombie_pid_ns, zombie_scheduler_state,
    },
    time::TimeValueLike,
};

const SUPPORTED_SCHED_ATTR_FLAGS: u64 = SCHED_FLAG_RESET_ON_FORK as u64;
const SCHED_ATTR_INPUT_FLAGS: u64 =
    SUPPORTED_SCHED_ATTR_FLAGS | SCHED_FLAG_KEEP_POLICY as u64 | SCHED_FLAG_KEEP_PARAMS as u64;
const SCHED_FLAG_RECLAIM: u64 = 0x02;
const SCHED_FLAG_DL_OVERRUN: u64 = 0x04;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
const SCHED_FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
const SCHED_ATTR_KNOWN_FLAGS: u64 = SCHED_ATTR_INPUT_FLAGS
    | SCHED_FLAG_RECLAIM
    | SCHED_FLAG_DL_OVERRUN
    | SCHED_FLAG_UTIL_CLAMP_MIN
    | SCHED_FLAG_UTIL_CLAMP_MAX;
const SCHED_ATTR_SIZE_VER0: usize = 48;
const SCHED_ATTR_MAX_SIZE: usize = 4096;
const SCHED_RR_TIMESLICE_MS_DEFAULT: u32 = {
    let ms = (RR_TIMESLICE_TICKS * 1000) / axconfig::TICKS_PER_SEC;
    if ms == 0 { 1 } else { ms as u32 }
};
static SCHED_RR_TIMESLICE_MS: AtomicU32 = AtomicU32::new(SCHED_RR_TIMESLICE_MS_DEFAULT);
#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct SchedParam {
    sched_priority: i32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
pub(crate) struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

fn sched_target(pid: i32) -> AxResult<AxTaskRef> {
    if pid < 0 {
        return Err(AxError::InvalidInput);
    }
    if pid == 0 {
        Ok(current().clone())
    } else {
        get_task(pid as Pid)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SchedSetAbi {
    Legacy,
    Attr,
}

fn sched_class_for_set(policy: u32, abi: SchedSetAbi) -> AxResult<SchedClass> {
    match policy {
        SCHED_NORMAL => Ok(SchedClass::Normal),
        SCHED_BATCH => Ok(SchedClass::Batch),
        SCHED_IDLE => Ok(SchedClass::Idle),
        SCHED_FIFO => Ok(SchedClass::Fifo),
        SCHED_RR => Ok(SchedClass::RoundRobin),
        // The legacy ABI cannot carry the runtime/deadline/period tuple, so
        // Linux rejects SCHED_DEADLINE through sched_setscheduler(2) as an
        // invalid request. sched_setattr(2) can express the request, but this
        // kernel has no EDF/CBS class or bandwidth admission yet; report that
        // known capability as unsupported instead of running it as EEVDF.
        SCHED_DEADLINE => match abi {
            SchedSetAbi::Legacy => Err(AxError::InvalidInput),
            SchedSetAbi::Attr => Ok(SchedClass::Deadline),
        },
        _ => Err(AxError::InvalidInput),
    }
}

struct SchedulerAuthoritySnapshot {
    actor: Arc<Cred>,
    target: Arc<Cred>,
}

impl SchedulerAuthoritySnapshot {
    fn new(actor: Arc<Cred>, target: Arc<Cred>) -> Self {
        Self { actor, target }
    }

    fn authorize(&self, operation: SchedulerSecurityOperation) -> AxResult<()> {
        dispatch_scheduler(&SecuritySchedulerContext::new(
            &self.actor,
            &self.target,
            operation,
        ))
    }
}

fn scheduler_actor_snapshot() -> (AxTaskRef, Arc<Cred>) {
    let actor = current();
    let credential = actor.as_thread().current_cred();
    (actor.clone(), credential)
}

fn scheduler_target_credential(
    actor_task: &AxTaskRef,
    actor_cred: &Arc<Cred>,
    task: &AxTaskRef,
) -> AxResult<Arc<Cred>> {
    if Arc::ptr_eq(actor_task, task) {
        return Ok(actor_cred.clone());
    }
    let target_thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    Ok(target_thread.current_cred())
}

fn scheduler_authority_snapshot(task: &AxTaskRef) -> AxResult<SchedulerAuthoritySnapshot> {
    let (actor_task, actor_cred) = scheduler_actor_snapshot();
    let target_cred = scheduler_target_credential(&actor_task, &actor_cred, task)?;
    Ok(SchedulerAuthoritySnapshot::new(actor_cred, target_cred))
}

fn validate_static_priority(priority: i32) -> AxResult<u8> {
    if priority == 0 {
        Ok(0)
    } else {
        Err(AxError::InvalidInput)
    }
}

fn validate_rt_priority(priority: i32) -> AxResult<u8> {
    if (RT_PRIORITY_MIN as i32..=RT_PRIORITY_MAX as i32).contains(&priority) {
        Ok(priority as u8)
    } else {
        Err(AxError::InvalidInput)
    }
}

fn validate_nice(nice: i32) -> AxResult<i8> {
    if (-20..=19).contains(&nice) {
        Ok(nice as i8)
    } else {
        Err(AxError::InvalidInput)
    }
}

fn validate_deadline_parameters(attr: &SchedAttr) -> AxResult<()> {
    if attr.sched_priority != 0
        || attr.sched_runtime == 0
        || attr.sched_deadline == 0
        || attr.sched_runtime > attr.sched_deadline
        || (attr.sched_period != 0 && attr.sched_deadline > attr.sched_period)
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn sched_reset_on_fork(task: &AxTaskRef) -> AxResult<bool> {
    scheduler_state_snapshot(task)
        .map(|commit| commit.reset_on_fork)
        .map_err(map_sched_error)
}

fn linux_policy_from_state(state: SchedState, reset_on_fork: bool) -> i32 {
    linux_policy_from_parts(state, reset_on_fork)
}

fn linux_policy_from_parts(state: SchedState, reset_on_fork: bool) -> i32 {
    linux_policy_from_class(state.class, reset_on_fork)
}

fn linux_policy_from_class(class: SchedClass, reset_on_fork: bool) -> i32 {
    let base = match class {
        SchedClass::Normal => SCHED_NORMAL as i32,
        SchedClass::Batch => SCHED_BATCH as i32,
        SchedClass::Idle => SCHED_IDLE as i32,
        SchedClass::Fifo => SCHED_FIFO as i32,
        SchedClass::RoundRobin => SCHED_RR as i32,
        SchedClass::Deadline => SCHED_DEADLINE as i32,
    };

    if reset_on_fork {
        base | SCHED_RESET_ON_FORK as i32
    } else {
        base
    }
}

fn state_static_priority(state: SchedState) -> i32 {
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => state.rt_priority as i32,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle | SchedClass::Deadline => 0,
    }
}

fn state_nice(state: SchedState) -> i32 {
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => 0,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => state.nice as i32,
        SchedClass::Deadline => 0,
    }
}

fn write_sched_attr_kernel_size<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const SchedAttr,
) -> AxResult<()> {
    let size_ptr = attr as *mut u32;
    VmMutPtr::vm_write(size_ptr, memory, size_of::<SchedAttr>() as u32)
        .map_err(map_usercopy_error)?;
    Ok(())
}

fn read_sched_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    attr: *const SchedAttr,
) -> AxResult<SchedAttr> {
    let mut attr_size =
        VmPtr::vm_read(attr.cast::<u32>(), memory).map_err(map_usercopy_error)? as usize;
    if attr_size == 0 {
        attr_size = SCHED_ATTR_SIZE_VER0;
    }
    if !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX_SIZE).contains(&attr_size) {
        // Linux returns E2BIG after a readable size even if this best-effort
        // write-back lands on a read-only mapping.
        let _ = write_sched_attr_kernel_size(memory, attr);
        return Err(LinuxError::E2BIG.into());
    }

    let mut out = SchedAttr::default();
    let copy_size = attr_size.min(size_of::<SchedAttr>());
    let src = vm_load(memory, attr.cast::<u8>(), copy_size).map_err(map_usercopy_error)?;
    let dst = unsafe {
        core::slice::from_raw_parts_mut((&mut out as *mut SchedAttr).cast::<u8>(), copy_size)
    };
    dst.copy_from_slice(&src);

    if attr_size > size_of::<SchedAttr>() {
        let extra = vm_load(
            memory,
            attr.cast::<u8>().wrapping_add(size_of::<SchedAttr>()),
            attr_size - size_of::<SchedAttr>(),
        )
        .map_err(map_usercopy_error)?;
        if extra.iter().any(|byte| *byte != 0) {
            let _ = write_sched_attr_kernel_size(memory, attr);
            return Err(LinuxError::E2BIG.into());
        }
    }

    out.sched_nice = out.sched_nice.clamp(-20, 19);
    Ok(out)
}

fn raw_priority_from_nice(nice: i8) -> isize {
    20 - nice as isize
}

fn fair_runtime_ns(class: SchedClass) -> u64 {
    let ticks = match class {
        SchedClass::Normal => axtask::EEVDF_PROFILE.normal_target_ticks,
        SchedClass::Batch => axtask::EEVDF_PROFILE.batch_target_ticks,
        SchedClass::Idle => axtask::EEVDF_PROFILE.idle_target_ticks,
        SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline => return 0,
    };
    ticks
        .saturating_mul(1_000_000_000)
        .checked_div(axconfig::TICKS_PER_SEC as u128)
        .unwrap_or(0)
        .min(u64::MAX as u128) as u64
}

fn rr_interval_for_state(state: SchedState) -> Duration {
    if matches!(state.class, SchedClass::RoundRobin) {
        Duration::from_millis(sched_rr_timeslice_ms() as u64)
    } else {
        Duration::ZERO
    }
}

pub fn sched_rr_timeslice_ms() -> u32 {
    SCHED_RR_TIMESLICE_MS.load(Ordering::Relaxed)
}

pub fn set_sched_rr_timeslice_ms(value: i32) {
    let value = if value <= 0 {
        SCHED_RR_TIMESLICE_MS_DEFAULT
    } else {
        value as u32
    };
    SCHED_RR_TIMESLICE_MS.store(value, Ordering::Relaxed);
}

fn apply_sched_state(task: &AxTaskRef, state: SchedState) -> AxResult<isize> {
    let commit = set_sched_state_versioned(task, state).map_err(|error| match error {
        TaskSchedError::Unsupported => AxError::OperationNotSupported,
        TaskSchedError::TaskExited => AxError::NoSuchProcess,
        TaskSchedError::RunQueueUnavailable(_) => AxError::BadState,
        TaskSchedError::Scheduler(_) => AxError::InvalidInput,
    })?;
    publish_sched_commit(task, commit);
    Ok(0)
}

fn publish_sched_commit(task: &AxTaskRef, commit: axtask::TaskSchedCommit) {
    if let Some(thread) = task.try_as_thread()
        && let Some(token) = thread
            .proc_data
            .scheduler_publication_token(thread.kernel_tid())
    {
        let tid = thread.kernel_tid();
        let thread = task.as_thread();
        thread
            .proc_data
            .publish_scheduler_state(tid, token, task, commit);
    }
}

fn apply_sched_state_with_reset(
    task: &AxTaskRef,
    state: SchedState,
    reset_on_fork: bool,
) -> AxResult<isize> {
    let commit = set_sched_state_versioned_with_reset(task, state, reset_on_fork)
        .map_err(map_sched_error)?;
    publish_sched_commit(task, commit);
    Ok(0)
}

fn process_error(error: ProcessError) -> AxError {
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

fn map_sched_error(error: TaskSchedError) -> AxError {
    match error {
        TaskSchedError::Unsupported => AxError::OperationNotSupported,
        TaskSchedError::TaskExited => AxError::NoSuchProcess,
        TaskSchedError::RunQueueUnavailable(_) => AxError::BadState,
        TaskSchedError::Scheduler(_) => AxError::InvalidInput,
    }
}

fn update_sched_policy(task: &AxTaskRef, policy: i32, priority: i32) -> AxResult<isize> {
    let reset_on_fork = policy & SCHED_RESET_ON_FORK as i32 != 0;
    let class = sched_class_for_set(
        (policy & !(SCHED_RESET_ON_FORK as i32)) as u32,
        SchedSetAbi::Legacy,
    )?;
    let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let old_state = sched_state(task);
    let requested_rt_priority = match class {
        SchedClass::Fifo | SchedClass::RoundRobin => validate_rt_priority(priority)?,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            validate_static_priority(priority)?
        }
        SchedClass::Deadline => return Err(AxError::InvalidInput),
    };
    let authority = scheduler_authority_snapshot(task)?;
    authority.authorize(SchedulerSecurityOperation::SetPolicy {
        policy_changed: old_state.class != class,
        current_realtime: matches!(
            old_state.class,
            SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline
        ),
        current_rt_priority: old_state.rt_priority,
        requested_realtime: matches!(class, SchedClass::Fifo | SchedClass::RoundRobin),
        requested_rt_priority,
        rlimit_rtprio: thread.proc_data.rlim.read()[RLIMIT_RTPRIO].current,
        current_idle: matches!(old_state.class, SchedClass::Idle),
        requested_idle: matches!(class, SchedClass::Idle),
        current_nice: old_state.nice,
        requested_nice: old_state.nice,
        rlimit_nice: thread.proc_data.rlim.read()[RLIMIT_NICE].current,
        current_reset_on_fork: sched_reset_on_fork(task)?,
        reset_on_fork,
    })?;
    let mut state = sched_state(task);
    state.class = class;
    match class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            state.rt_priority = validate_rt_priority(priority)?;
            state.nice = 0;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            state.rt_priority = validate_static_priority(priority)?;
            if matches!(class, SchedClass::Idle) {
                state.nice = 19;
            }
        }
        SchedClass::Deadline => return Err(AxError::InvalidInput),
    }
    apply_sched_state_with_reset(task, state, reset_on_fork)
}

fn update_sched_param(task: &AxTaskRef, priority: i32) -> AxResult<isize> {
    let mut state = sched_state(task);
    let realtime = matches!(state.class, SchedClass::Fifo | SchedClass::RoundRobin);
    let new_rt_priority = if realtime {
        validate_rt_priority(priority)?
    } else {
        validate_static_priority(priority)?;
        0
    };
    let rlimit_rtprio = task.as_thread().proc_data.rlim.read()[RLIMIT_RTPRIO].current;
    scheduler_authority_snapshot(task)?.authorize(SchedulerSecurityOperation::SetParam {
        realtime,
        old_rt_priority: state.rt_priority,
        new_rt_priority,
        rlimit_rtprio,
    })?;
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            state.rt_priority = new_rt_priority;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            state.rt_priority = 0;
        }
        SchedClass::Deadline => return Err(AxError::InvalidInput),
    }
    apply_sched_state(task, state)
}

fn linux_priority_bounds(policy: i32) -> AxResult<(isize, isize)> {
    match policy as u32 {
        SCHED_FIFO | SCHED_RR => Ok((RT_PRIORITY_MIN as isize, RT_PRIORITY_MAX as isize)),
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok((0, 0)),
        _ => Err(AxError::InvalidInput),
    }
}

fn record_lower_nice(best: &mut Option<i8>, nice: i8) {
    *best = Some(best.map_or(nice, |current| current.min(nice)));
}

fn getpriority_matches_real_uid<T: Eq>(real_uid: &T, requested_uid: &T) -> bool {
    real_uid == requested_uid
}

fn clamp_nice(prio: i32) -> i8 {
    prio.clamp(-20, 19) as i8
}

struct SchedulerNiceTarget {
    task: AxTaskRef,
    credential: Arc<Cred>,
}

fn scheduler_nice_target(
    actor_task: &AxTaskRef,
    actor_cred: &Arc<Cred>,
    task: AxTaskRef,
) -> AxResult<SchedulerNiceTarget> {
    let credential = scheduler_target_credential(actor_task, actor_cred, &task)?;
    Ok(SchedulerNiceTarget { task, credential })
}

fn set_task_nice(
    actor_task: &AxTaskRef,
    actor_cred: &Arc<Cred>,
    target: &SchedulerNiceTarget,
    new_nice: i8,
) -> AxResult<()> {
    let task = &target.task;
    task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let rlimit_nice = actor_task.as_thread().proc_data.rlim.read()[RLIMIT_NICE].current;
    let current_nice = sched_state(task).nice;
    SchedulerAuthoritySnapshot::new(actor_cred.clone(), target.credential.clone()).authorize(
        SchedulerSecurityOperation::SetNice {
            current_nice,
            requested_nice: new_nice,
            rlimit_nice,
        },
    )?;
    let commit = update_task_nice(task, new_nice).map_err(|error| match error {
        TaskSchedError::Unsupported => AxError::OperationNotSupported,
        TaskSchedError::TaskExited => AxError::NoSuchProcess,
        TaskSchedError::RunQueueUnavailable(_) => AxError::BadState,
        TaskSchedError::Scheduler(_) => AxError::InvalidInput,
    })?;
    if let Some(thread) = task.try_as_thread()
        && let Some(token) = thread
            .proc_data
            .scheduler_publication_token(thread.kernel_tid())
    {
        let tid = thread.kernel_tid();
        let thread = task.as_thread();
        thread
            .proc_data
            .publish_scheduler_state(tid, token, task, commit);
    }
    Ok(())
}

pub fn sys_sched_yield() -> AxResult<isize> {
    axtask::sched_yield();
    Ok(0)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ClockSleepOutcome {
    Completed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ClockSleepInterruptDisposition {
    Retry,
    Return,
}

/// A default SIGCONT resumes a stopped task but has no userspace signal
/// handler to interrupt a sleep.  Other default actions, and every handler,
/// retain the ordinary Linux EINTR boundary.
fn clock_sleep_signal_is_visible(
    signo: Signo,
    blocked: bool,
    disposition: SignalDisposition,
) -> bool {
    if blocked {
        return false;
    }
    match disposition {
        SignalDisposition::Ignore => false,
        SignalDisposition::Handler(_) => true,
        SignalDisposition::Default if signo == Signo::SIGCONT => false,
        SignalDisposition::Default => {
            !matches!(signo.default_action(), DefaultSignalAction::Ignore)
        }
    }
}

/// Refines the common task signal predicate for clock sleeps.  The shared
/// predicate intentionally treats default SIGCONT as syscall-visible; for a
/// sleep it is only a continuation wake, so inspect the exact pending set to
/// avoid exposing EINTR while still preserving any other pending signal.
fn has_pending_clock_sleep_signal(thr: &Thread) -> bool {
    if !has_pending_syscall_signal(thr) {
        return false;
    }

    let pending = thr.signal.pending();
    let blocked = thr.signal.blocked();
    for raw in 1..=64u8 {
        let Some(signo) = Signo::from_repr(raw) else {
            continue;
        };
        if pending.has(signo)
            && clock_sleep_signal_is_visible(
                signo,
                blocked.has(signo),
                thr.proc_data.signal.action(signo).disposition,
            )
        {
            return true;
        }
    }
    false
}

/// Classifies the task-interrupt wake that follows an interruptible clock
/// sleep.  `TaskInner::interrupt` is deliberately a single wake channel: it
/// is also used for exec/exit handoff and other internal state changes, not
/// only for a signal which Linux exposes as EINTR.  Keep the ABI decision
/// tied to the durable task predicates instead of treating the wake bit as a
/// signal by itself.
fn classify_clock_sleep_interrupt(
    pending_exit: bool,
    pending_signal: bool,
    should_exit_for_exec: bool,
    should_wait_for_stop: bool,
) -> ClockSleepInterruptDisposition {
    if pending_exit || pending_signal || should_exit_for_exec || should_wait_for_stop {
        ClockSleepInterruptDisposition::Return
    } else {
        ClockSleepInterruptDisposition::Retry
    }
}

fn current_clock_sleep_interrupt_disposition() -> ClockSleepInterruptDisposition {
    let curr = current();
    let thr = curr.as_thread();
    classify_clock_sleep_interrupt(
        thr.pending_exit(),
        has_pending_clock_sleep_signal(thr),
        thr.proc_data.should_exit_for_exec(thr.tid()),
        thr.proc_data.should_wait_for_stop(),
    )
}

fn flatten_clock_sleep_result(
    result: Result<Result<(), Interrupted>, BlockOnError>,
) -> AxResult<ClockSleepOutcome> {
    match result {
        Err(error) => Err(error.into()),
        Ok(Err(_)) => Ok(ClockSleepOutcome::Interrupted),
        Ok(Ok(())) => Ok(ClockSleepOutcome::Completed),
    }
}

fn sleep_relative(dur: TimeValue) -> AxResult<TimeValue> {
    debug!("sleep_impl <= {dur:?}");

    if dur.is_zero() {
        return Ok(dur);
    }
    let start = AlarmClock::Monotonic.now();
    let deadline = start.checked_add(dur).unwrap_or(Duration::MAX);
    loop {
        let mut sleeper = prepare_clock_sleep(AlarmClock::Monotonic, deadline)?;

        // We detect EINTR manually if the slept time is not enough.  The
        // task interrupt is only a wake hint; retry while no visible signal,
        // exec handoff, or terminal exit predicate owns the interruption.
        let result = with_proc_state_hint(ProcStateHint::Interruptible, || {
            block_on(interruptible(&mut sleeper))
        });
        match flatten_clock_sleep_result(result)? {
            ClockSleepOutcome::Completed => break,
            ClockSleepOutcome::Interrupted
                if matches!(
                    current_clock_sleep_interrupt_disposition(),
                    ClockSleepInterruptDisposition::Retry
                ) => {}
            ClockSleepOutcome::Interrupted => break,
        }
    }

    Ok(AlarmClock::Monotonic.now() - start)
}

fn sleep_absolute(clock: AlarmClock, deadline: TimeValue) -> AxResult<ClockSleepOutcome> {
    debug!("sleep_absolute <= clock: {clock:?}, deadline: {deadline:?}");

    loop {
        let mut sleeper = prepare_clock_sleep(clock, deadline)?;
        let result = with_proc_state_hint(ProcStateHint::Interruptible, || {
            block_on(interruptible(&mut sleeper))
        });
        match flatten_clock_sleep_result(result)? {
            ClockSleepOutcome::Completed => return Ok(ClockSleepOutcome::Completed),
            ClockSleepOutcome::Interrupted
                if matches!(
                    current_clock_sleep_interrupt_disposition(),
                    ClockSleepInterruptDisposition::Retry
                ) => {}
            ClockSleepOutcome::Interrupted => return Ok(ClockSleepOutcome::Interrupted),
        }
    }
}

fn clock_nanosleep_is_absolute(flags: u32) -> AxResult<bool> {
    if flags & !TIMER_ABSTIME != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(flags & TIMER_ABSTIME != 0)
}

fn finish_absolute_clock_sleep(
    outcome: ClockSleepOutcome,
    now: TimeValue,
    deadline: TimeValue,
) -> AxResult<isize> {
    match outcome {
        // Once the timer owns completion, a later backward wall-clock step
        // must not turn that completed operation into EINTR.
        ClockSleepOutcome::Completed => Ok(0),
        // An interrupt can race with a forward wall-clock step before the
        // periodic timer callback drains the slot.  In that case the absolute
        // deadline already owns completion even though the future was still
        // pending at the interrupt boundary.
        ClockSleepOutcome::Interrupted if now >= deadline => Ok(0),
        ClockSleepOutcome::Interrupted => Err(AxError::Interrupted),
    }
}

fn remaining_relative_sleep(req: TimeValue, actual: TimeValue) -> Option<TimeValue> {
    if actual < req {
        Some(req - actual)
    } else {
        None
    }
}

/// Sleep some nanoseconds
pub fn sys_nanosleep<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    req: *const timespec,
    rem: *mut timespec,
) -> AxResult<isize> {
    // FIXME: AnyBitPattern
    let req = unsafe {
        req.vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    }
    .try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    let actual = sleep_relative(req)?;

    if let Some(diff) = remaining_relative_sleep(req, actual) {
        debug!("sys_nanosleep => rem: {diff:?}");
        if !rem.is_null() {
            unsafe {
                VmMutPtr::vm_write_unchecked(rem, memory, timespec::from_time_value(diff))
                    .map_err(map_usercopy_error)?;
            }
        }
        Err(AxError::Interrupted)
    } else {
        Ok(0)
    }
}

pub fn sys_clock_nanosleep<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: *const timespec,
    rem: *mut timespec,
) -> AxResult<isize> {
    let absolute = clock_nanosleep_is_absolute(flags)?;
    let clock = match clock_id as u32 {
        CLOCK_REALTIME => AlarmClock::Realtime,
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => AlarmClock::Monotonic,
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            return Err(AxError::OperationNotSupported);
        }
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(AxError::InvalidInput);
        }
    };

    let req = unsafe {
        req.vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    }
    .try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    if absolute {
        let deadline = match clock_id as u32 {
            CLOCK_MONOTONIC => current()
                .as_thread()
                .proc_data
                .time_ns()
                .host_monotonic_deadline(req),
            CLOCK_BOOTTIME => current()
                .as_thread()
                .proc_data
                .time_ns()
                .host_boottime_deadline(req),
            _ => req,
        };
        let outcome = sleep_absolute(clock, deadline)?;
        finish_absolute_clock_sleep(outcome, clock.now(), deadline)
    } else {
        let actual = sleep_relative(req)?;

        if let Some(diff) = remaining_relative_sleep(req, actual) {
            debug!("sys_clock_nanosleep => rem: {diff:?}");
            if !rem.is_null() {
                unsafe {
                    VmMutPtr::vm_write_unchecked(rem, memory, timespec::from_time_value(diff))
                        .map_err(map_usercopy_error)?;
                }
            }
            Err(AxError::Interrupted)
        } else {
            Ok(0)
        }
    }
}

pub fn sys_sched_getaffinity<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    cpusetsize: usize,
    user_mask: *mut u8,
) -> AxResult<isize> {
    let cpu_count = axhal::cpu_num().max(1);
    let kernel_mask_bytes = linux_cpumask_bytes(cpu_count);
    if cpusetsize < kernel_mask_bytes {
        return Err(AxError::InvalidInput);
    }

    let mask = sched_target(pid)?.cpumask();
    let mut mask_bytes = Vec::new();
    mask_bytes
        .try_reserve_exact(kernel_mask_bytes)
        .map_err(|_| AxError::NoMemory)?;
    mask_bytes.resize(kernel_mask_bytes, 0);
    for cpu in 0..cpu_count {
        if mask.get(cpu) {
            mask_bytes[cpu / u8::BITS as usize] |= 1 << (cpu % u8::BITS as usize);
        }
    }

    vm_write_slice(memory, user_mask, &mask_bytes).map_err(map_usercopy_error)?;

    Ok(kernel_mask_bytes as _)
}

fn linux_cpumask_bytes(cpu_count: usize) -> usize {
    cpu_count.max(1).div_ceil(usize::BITS as usize) * size_of::<usize>()
}

pub fn sys_sched_setaffinity<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    cpusetsize: usize,
    user_mask: *const u8,
) -> AxResult<isize> {
    let cpu_count = axhal::cpu_num().max(1);
    let kernel_mask_bytes = linux_cpumask_bytes(cpu_count);
    if cpusetsize < kernel_mask_bytes {
        return Err(AxError::InvalidInput);
    }

    let user_mask = vm_load(memory, user_mask, kernel_mask_bytes).map_err(map_usercopy_error)?;
    let mut cpu_mask = AxCpuMask::new();

    for i in 0..cpu_count {
        if user_mask[i / 8] & (1 << (i % 8)) != 0 {
            cpu_mask.set(i, true);
        }
    }

    let task = sched_target(pid)?;
    scheduler_authority_snapshot(&task)?.authorize(SchedulerSecurityOperation::SetAffinity)?;

    if pid == 0 {
        axtask::set_current_affinity(cpu_mask)?;
    } else {
        set_task_affinity(&task, cpu_mask)?;
    }

    Ok(0)
}

pub fn sys_getcpu<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    cpu: *mut u32,
    node: *mut u32,
) -> AxResult<isize> {
    // Linux reports the CPU on which this call is actually executing.  A
    // concurrent affinity change may have published a restrictive mask before
    // the target reaches its migration safe point; reporting an allowed-but-
    // fictional CPU during that window would expose shadow scheduler state.
    let cpu_id = axhal::percpu::this_cpu_id();
    if !cpu.is_null() {
        VmMutPtr::vm_write(cpu, memory, cpu_id as u32).map_err(map_usercopy_error)?;
    }
    if !node.is_null() {
        VmMutPtr::vm_write(node, memory, 0).map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_sched_getscheduler(pid: i32) -> AxResult<isize> {
    let task = sched_target(pid)?;
    Ok(linux_policy_from_state(sched_state(&task), sched_reset_on_fork(&task)?) as isize)
}

pub fn sys_sched_setparam<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    param: *const SchedParam,
) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let priority = unsafe {
        param
            .vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    }
    .sched_priority;
    let task = sched_target(pid)?;
    update_sched_param(&task, priority)
}

pub fn sys_sched_setscheduler<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    policy: i32,
    param: *const SchedParam,
) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let priority = unsafe {
        param
            .vm_read_uninit(memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    }
    .sched_priority;
    let task = sched_target(pid)?;
    update_sched_policy(&task, policy, priority)
}

pub fn sys_sched_getparam<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    param: *mut SchedParam,
) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let task = sched_target(pid)?;
    // `SchedParam` is a complete `repr(C)` value containing only its i32
    // priority field, so its initialized representation is safe to copy out
    // through the explicitly bound user-memory context.
    unsafe {
        VmMutPtr::vm_write_unchecked(
            param,
            memory,
            SchedParam {
                sched_priority: state_static_priority(sched_state(&task)),
            },
        )
        .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_sched_get_priority_max(policy: i32) -> AxResult<isize> {
    Ok(linux_priority_bounds(policy)?.1)
}

pub fn sys_sched_get_priority_min(policy: i32) -> AxResult<isize> {
    Ok(linux_priority_bounds(policy)?.0)
}

pub fn sys_sched_rr_get_interval<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    interval: *mut timespec,
) -> AxResult<isize> {
    let task = sched_target(pid)?;
    unsafe {
        VmMutPtr::vm_write_unchecked(
            interval,
            memory,
            timespec::from_time_value(rr_interval_for_state(sched_state(&task))),
        )
        .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_sched_setattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    attr: *const SchedAttr,
    flags: u32,
) -> AxResult<isize> {
    if attr.is_null() || pid < 0 || flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let attr = read_sched_attr(memory, attr)?;
    if (attr.sched_policy as i32) < 0 {
        return Err(AxError::InvalidInput);
    }
    let keep_policy = attr.sched_flags & SCHED_FLAG_KEEP_POLICY as u64 != 0;
    let keep_params = attr.sched_flags & SCHED_FLAG_KEEP_PARAMS as u64 != 0;
    let task = if pid == 0 {
        current().clone()
    } else {
        let caller_pid_ns = current().as_thread().proc_data.pid_ns();
        visible_live_task_for_getpriority(pid as Pid, &caller_pid_ns)
            .filter(|task| !task.as_thread().pending_exit())
            .ok_or(AxError::NoSuchProcess)?
    };
    if attr.sched_flags & !SCHED_ATTR_KNOWN_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.sched_flags & (SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX) != 0
        && (attr.size as usize) < size_of::<SchedAttr>()
    {
        return Err(AxError::InvalidInput);
    }
    let requested_class = if keep_policy {
        None
    } else {
        if attr.sched_policy == SCHED_DEADLINE {
            validate_deadline_parameters(&attr)?;
        }
        Some(sched_class_for_set(attr.sched_policy, SchedSetAbi::Attr)?)
    };
    let reset_on_fork = attr.sched_flags & SUPPORTED_SCHED_ATTR_FLAGS != 0;
    let (actor_task, actor_cred) = scheduler_actor_snapshot();
    let result = update_sched_attr_versioned(&task, |old| {
        let class = requested_class.unwrap_or(old.state.class);
        let target_cred = scheduler_target_credential(&actor_task, &actor_cred, &task)?;
        let target_thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
        let rlimit_rtprio = target_thread.proc_data.rlim.read()[RLIMIT_RTPRIO].current;
        let rlimit_nice = target_thread.proc_data.rlim.read()[RLIMIT_NICE].current;
        let authority = SchedulerAuthoritySnapshot::new(actor_cred.clone(), target_cred);
        let mut state = old.state;
        let old_class = state.class;
        let old_nice = state.nice;
        let old_rt_priority = state.rt_priority;
        state.class = class;
        // SCHED_DEADLINE admission is never relaxed by RLIMIT_RTPRIO: Linux
        // requires CAP_SYS_NICE both to enter it and to change its parameters.
        if matches!(class, SchedClass::Deadline)
            && !ns_capable(&actor_cred, target_cred.user_ns(), CAP_SYS_NICE)
        {
            return Err(AxError::OperationNotPermitted);
        }
        match class {
            SchedClass::Fifo | SchedClass::RoundRobin => {
                state.rt_priority = if keep_params {
                    validate_rt_priority(old_rt_priority as i32)?
                } else {
                    if attr.sched_runtime != 0 || attr.sched_deadline != 0 || attr.sched_period != 0
                    {
                        return Err(AxError::InvalidInput);
                    }
                    validate_rt_priority(attr.sched_priority as i32)?
                };
            }
            SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
                if keep_params {
                    validate_static_priority(0)?;
                } else {
                    if attr.sched_runtime != 0 {
                        return Err(AxError::OperationNotSupported);
                    }
                    if attr.sched_deadline != 0 || attr.sched_period != 0 {
                        return Err(AxError::InvalidInput);
                    }
                    validate_static_priority(attr.sched_priority as i32)?;
                }
                state.rt_priority = 0;
                state.nice = if keep_params {
                    old_nice
                } else {
                    validate_nice(attr.sched_nice)?
                };
            }
            SchedClass::Deadline => {
                let deadline = if keep_params {
                    old.deadline
                } else {
                    validate_deadline_parameters(&attr)?;
                    DeadlineConfig {
                        runtime_ns: attr.sched_runtime,
                        deadline_ns: attr.sched_deadline,
                        period_ns: attr.sched_period,
                    }
                };
                if deadline.normalized_linux().is_none() {
                    return Err(AxError::InvalidInput);
                }
                state.rt_priority = 0;
                state.nice = 0;
            }
        }
        authority.authorize(SchedulerSecurityOperation::SetPolicy {
            policy_changed: old_class != class,
            current_realtime: matches!(
                old_class,
                SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline
            ),
            current_rt_priority: old_rt_priority,
            requested_realtime: matches!(
                class,
                SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline
            ),
            requested_rt_priority: state.rt_priority,
            rlimit_rtprio,
            current_idle: matches!(old_class, SchedClass::Idle),
            requested_idle: matches!(class, SchedClass::Idle),
            current_nice: old_nice,
            requested_nice: state.nice,
            rlimit_nice,
            current_reset_on_fork: old.reset_on_fork,
            reset_on_fork,
        })?;
        let util_min = if attr.sched_flags & SCHED_FLAG_UTIL_CLAMP_MIN != 0 {
            attr.sched_util_min
        } else {
            old.util_min
        };
        let util_max = if attr.sched_flags & SCHED_FLAG_UTIL_CLAMP_MAX != 0 {
            attr.sched_util_max
        } else {
            old.util_max
        };
        if util_min > util_max || util_max > 1024 {
            return Err(AxError::InvalidInput);
        }
        let fair_runtime_ns = match class {
            SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
                if keep_params {
                    old.fair_runtime_ns
                } else {
                    0
                }
            }
            SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline => 0,
        };
        let deadline = if matches!(class, SchedClass::Deadline) {
            if keep_params {
                old.deadline
            } else {
                DeadlineConfig {
                    runtime_ns: attr.sched_runtime,
                    deadline_ns: attr.sched_deadline,
                    period_ns: attr.sched_period,
                }
            }
        } else {
            DeadlineConfig::default()
        };
        Ok::<_, AxError>(TaskSchedAttrUpdate {
            state,
            reset_on_fork,
            fair_runtime_ns,
            deadline,
            util_min,
            util_max,
        })
    });
    match result {
        Ok(Ok(commit)) => {
            publish_sched_commit(&task, commit);
            Ok(0)
        }
        Ok(Err(error)) => Err(error),
        Err(error) => Err(map_sched_error(error)),
    }
}

enum SchedGetAttrTarget {
    Live(AxTaskRef),
    Zombie(Arc<Process>),
}

impl SchedGetAttrTarget {
    fn credential(&self) -> AxResult<Arc<Cred>> {
        match self {
            Self::Live(task) => task
                .try_as_thread()
                .ok_or(AxError::NoSuchProcess)
                .map(|thread| thread.current_cred()),
            Self::Zombie(process) => process
                .zombie_payload()
                .ok_or(AxError::NoSuchProcess)
                .map(|snapshot| snapshot.credential.clone()),
        }
    }

    fn snapshot(&self) -> AxResult<(SchedState, bool, u64, u64, u64, u64, u32, u32)> {
        match self {
            Self::Live(task) => scheduler_attr_snapshot(task)
                .map(|snapshot| {
                    (
                        snapshot.commit.state,
                        snapshot.commit.reset_on_fork,
                        snapshot.fair_runtime_ns,
                        snapshot.deadline_runtime_ns,
                        snapshot.deadline_ns,
                        snapshot.period_ns,
                        snapshot.util_min,
                        snapshot.util_max,
                    )
                })
                .map_err(|error| match error {
                    TaskSchedError::TaskExited => AxError::NoSuchProcess,
                    TaskSchedError::Unsupported => AxError::OperationNotSupported,
                    TaskSchedError::RunQueueUnavailable(_) | TaskSchedError::Scheduler(_) => {
                        AxError::NoSuchProcess
                    }
                }),
            Self::Zombie(process) => {
                let snapshot = zombie_scheduler_state(process)?;
                Ok((
                    SchedState {
                        class: snapshot.class,
                        nice: snapshot.nice,
                        rt_priority: snapshot.rt_priority,
                    },
                    snapshot.reset_on_fork,
                    fair_runtime_ns(snapshot.class),
                    0,
                    0,
                    0,
                    0,
                    0,
                ))
            }
        }
    }
}

pub fn sys_sched_getattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: i32,
    attr: *mut SchedAttr,
    size: u32,
    flags: u32,
) -> AxResult<isize> {
    let out_size = size as usize;
    if attr.is_null()
        || pid < 0
        || !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX_SIZE).contains(&out_size)
        || flags != 0
    {
        return Err(AxError::InvalidInput);
    }

    let target = if pid == 0 {
        SchedGetAttrTarget::Live(current().clone())
    } else {
        let caller_pid_ns = current().as_thread().proc_data.pid_ns();
        if let Some(task) = visible_live_task_for_getpriority(pid as Pid, &caller_pid_ns) {
            SchedGetAttrTarget::Live(task)
        } else {
            SchedGetAttrTarget::Zombie(zombie_for_ioprio(pid as Pid, &caller_pid_ns)?)
        }
    };
    let (_, actor_cred) = scheduler_actor_snapshot();
    let target_cred = target.credential()?;
    dispatch_task_getscheduler(&SecurityTaskGetSchedulerContext::new(
        &actor_cred,
        &target_cred,
    ))?;
    let (
        state,
        reset_on_fork,
        fair_runtime_ns,
        deadline_runtime_ns,
        deadline_ns,
        period_ns,
        util_min,
        util_max,
    ) = target.snapshot()?;
    let mut out = SchedAttr {
        size: out_size.min(size_of::<SchedAttr>()) as u32,
        sched_policy: linux_policy_from_state(state, reset_on_fork) as u32 & !SCHED_RESET_ON_FORK,
        sched_flags: if reset_on_fork {
            SUPPORTED_SCHED_ATTR_FLAGS
        } else {
            0
        },
        sched_nice: state_nice(state),
        sched_priority: state_static_priority(state) as u32,
        sched_runtime: if matches!(state.class, SchedClass::Deadline) {
            deadline_runtime_ns
        } else {
            fair_runtime_ns
        },
        sched_deadline: deadline_ns,
        sched_period: period_ns,
        sched_util_min: util_min,
        sched_util_max: util_max,
    };
    out.sched_flags &= SUPPORTED_SCHED_ATTR_FLAGS;

    memory
        .validate_write_range(attr as usize, out_size)
        .map_err(map_usercopy_error)?;
    let copy_size = out_size.min(size_of::<SchedAttr>());
    vm_write_slice(memory, attr.cast::<u8>(), unsafe {
        core::slice::from_raw_parts((&out as *const SchedAttr).cast::<u8>(), copy_size)
    })
    .map_err(map_usercopy_error)?;

    Ok(0)
}

/// Linux's `getpriority` parameters are C `int`s.  Decode the low ABI word
/// explicitly: raw syscall callers are not required to sign-extend it to
/// x86_64's register width.
fn syscall_c_int(raw: usize) -> i32 {
    raw as u32 as i32
}

fn visible_live_task_for_getpriority(
    who: Pid,
    caller_pid_ns: &Arc<PidNamespace>,
) -> Option<AxTaskRef> {
    for process in process_domain().ok()?.registry().processes() {
        if process.is_zombie() {
            continue;
        }
        for tid in process.thread_ids() {
            let Ok(task) = get_task(tid) else {
                continue;
            };
            if visible_tid_in_namespace(&task, caller_pid_ns) == Some(who) {
                return Some(task);
            }
        }
    }
    None
}

fn zombie_nice_for_getpriority(
    who: Pid,
    caller_pid_ns: &Arc<PidNamespace>,
) -> Option<AxResult<i8>> {
    for process in process_domain().ok()?.registry().processes() {
        if !process.is_zombie() {
            continue;
        }
        let Some(target_pid_ns) = zombie_pid_ns(&process) else {
            continue;
        };
        if visible_process_pid_in_namespace(process.pid(), &target_pid_ns, caller_pid_ns)
            == Some(who)
        {
            return Some(zombie_scheduler_state(&process).map(|state| state.nice));
        }
    }
    None
}

fn process_pid_ns_for_getpriority(process: &Arc<Process>) -> Option<Arc<PidNamespace>> {
    if process.is_zombie() {
        return zombie_pid_ns(process);
    }
    process.thread_ids().find_map(|tid| {
        get_task(tid)
            .ok()
            .map(|task| task.as_thread().proc_data.pid_ns())
    })
}

fn getpriority_group_nice(
    who: Pid,
    caller_task: &AxTaskRef,
    caller_pid_ns: &Arc<PidNamespace>,
) -> AxResult<i8> {
    let registry = process_domain()?.registry();
    let target_group = if who == 0 {
        caller_task.as_thread().proc_data.proc.group()
    } else {
        let mut found = None;
        for process in registry.processes() {
            let Some(target_pid_ns) = process_pid_ns_for_getpriority(&process) else {
                continue;
            };
            let group = process.group();
            if visible_process_pid_in_namespace(group.pgid(), &target_pid_ns, caller_pid_ns)
                == Some(who)
            {
                found = Some(group);
                break;
            }
        }
        found.ok_or(AxError::NoSuchProcess)?
    };

    let mut best = None;
    for process in registry.processes() {
        if !Arc::ptr_eq(&process.group(), &target_group) {
            continue;
        }
        if process.is_zombie() {
            let Some(target_pid_ns) = zombie_pid_ns(&process) else {
                continue;
            };
            if caller_pid_ns.contains(&target_pid_ns) {
                record_lower_nice(&mut best, zombie_scheduler_state(&process)?.nice);
            }
            continue;
        }
        for tid in process.thread_ids() {
            let Ok(task) = get_task(tid) else {
                continue;
            };
            let thread = task.as_thread();
            if caller_pid_ns.contains(&thread.proc_data.pid_ns()) {
                record_lower_nice(&mut best, sched_state(&task).nice);
            }
        }
    }
    best.ok_or(AxError::NoSuchProcess)
}

fn getpriority_user_nice(
    who: i32,
    caller_cred: &Cred,
    caller_pid_ns: &Arc<PidNamespace>,
) -> AxResult<i8> {
    let uid = if who == 0 {
        caller_cred.ids().ruid
    } else {
        caller_cred
            .user_ns()
            .make_kuid(who as u32)
            .ok_or(AxError::NoSuchProcess)?
    };

    let mut best = None;
    for process in process_domain()?.registry().processes() {
        if process.is_zombie() {
            let Some(target_pid_ns) = zombie_pid_ns(&process) else {
                continue;
            };
            let Some(snapshot) = process.zombie_payload() else {
                continue;
            };
            if caller_pid_ns.contains(&target_pid_ns) && snapshot.credential.ids().ruid == uid {
                record_lower_nice(&mut best, zombie_scheduler_state(&process)?.nice);
            }
            continue;
        }
        for tid in process.thread_ids() {
            let Ok(task) = get_task(tid) else {
                continue;
            };
            let thread = task.as_thread();
            if caller_pid_ns.contains(&thread.proc_data.pid_ns())
                && getpriority_matches_real_uid(&thread.real_uid(), &uid)
            {
                record_lower_nice(&mut best, sched_state(&task).nice);
            }
        }
    }
    best.ok_or(AxError::NoSuchProcess)
}

pub fn sys_getpriority(which: usize, who: usize) -> AxResult<isize> {
    let which = syscall_c_int(which);
    let who = syscall_c_int(who);
    debug!("sys_getpriority <= which: {which}, who: {who}");

    let caller_task = current().clone();
    let caller_pid_ns = caller_task.as_thread().proc_data.pid_ns();
    match which as u32 {
        PRIO_PROCESS => {
            let nice = if who == 0 {
                sched_state(&caller_task).nice
            } else if who < 0 {
                return Err(AxError::NoSuchProcess);
            } else if let Some(task) = visible_live_task_for_getpriority(who as Pid, &caller_pid_ns)
            {
                sched_state(&task).nice
            } else if let Some(nice) = zombie_nice_for_getpriority(who as Pid, &caller_pid_ns) {
                nice?
            } else {
                return Err(AxError::NoSuchProcess);
            };
            Ok(raw_priority_from_nice(nice))
        }
        PRIO_PGRP => {
            if who < 0 {
                return Err(AxError::NoSuchProcess);
            }
            Ok(raw_priority_from_nice(getpriority_group_nice(
                who as Pid,
                &caller_task,
                &caller_pid_ns,
            )?))
        }
        PRIO_USER => {
            let caller_cred = caller_task.as_thread().current_cred();
            Ok(raw_priority_from_nice(getpriority_user_nice(
                who,
                &caller_cred,
                &caller_pid_ns,
            )?))
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn record_setpriority_result(result: &mut AxResult<()>, attempt: AxResult<()>) {
    // Linux starts at ESRCH. A success proves that at least one target was
    // affected, but never erases an earlier target failure; later failures
    // replace the accumulator while the walk continues.
    match attempt {
        Ok(()) if matches!(result, Err(AxError::NoSuchProcess)) => *result = Ok(()),
        Ok(()) => {}
        Err(error) => *result = Err(error),
    }
}

fn setpriority_one(
    result: &mut AxResult<()>,
    actor_task: &AxTaskRef,
    actor_cred: &Arc<Cred>,
    task: AxTaskRef,
    new_nice: i8,
) {
    let attempt = scheduler_nice_target(actor_task, actor_cred, task)
        .and_then(|target| set_task_nice(actor_task, actor_cred, &target, new_nice));
    record_setpriority_result(result, attempt);
}

pub fn sys_setpriority(which: usize, who: usize, prio: usize) -> AxResult<isize> {
    let which = syscall_c_int(which);
    let who = syscall_c_int(who);
    let prio = syscall_c_int(prio);
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    if !matches!(which as u32, PRIO_PROCESS | PRIO_PGRP | PRIO_USER) {
        return Err(AxError::InvalidInput);
    }
    let new_nice = clamp_nice(prio);
    let (actor_task, actor_cred) = scheduler_actor_snapshot();
    let caller_pid_ns = actor_task.as_thread().proc_data.pid_ns();
    let mut result = Err(AxError::NoSuchProcess);
    match which as u32 {
        PRIO_PROCESS => {
            let task = if who == 0 {
                actor_task.clone()
            } else if who > 0 {
                visible_live_task_for_getpriority(who as Pid, &caller_pid_ns)
                    .ok_or(AxError::NoSuchProcess)?
            } else {
                return Err(AxError::NoSuchProcess);
            };
            setpriority_one(&mut result, &actor_task, &actor_cred, task, new_nice);
        }
        PRIO_PGRP => {
            if who < 0 {
                return Err(AxError::NoSuchProcess);
            }
            let registry = process_domain()?.registry();
            let target_group = if who == 0 {
                Some(actor_task.as_thread().proc_data.proc.group())
            } else {
                registry.processes().find_map(|process| {
                    (!process.is_zombie())
                        .then(|| process_pid_ns_for_getpriority(&process))
                        .flatten()
                        .filter(|pid_ns| {
                            visible_process_pid_in_namespace(
                                process.group().pgid(),
                                pid_ns,
                                &caller_pid_ns,
                            ) == Some(who as Pid)
                        })
                        .map(|_| process.group())
                })
            };
            let target_group = target_group.ok_or(AxError::NoSuchProcess)?;
            for process in registry.processes() {
                if process.is_zombie() || !Arc::ptr_eq(&process.group(), &target_group) {
                    continue;
                }
                for tid in process.thread_ids() {
                    if let Ok(task) = get_task(tid)
                        && !task.as_thread().pending_exit()
                        && caller_pid_ns.contains(&task.as_thread().proc_data.pid_ns())
                    {
                        setpriority_one(&mut result, &actor_task, &actor_cred, task, new_nice);
                    }
                }
            }
        }
        PRIO_USER => {
            let uid = if who == 0 {
                actor_cred.ids().ruid
            } else {
                actor_cred
                    .user_ns()
                    .make_kuid(who as u32)
                    .ok_or(AxError::NoSuchProcess)?
            };
            for process in process_domain()?.registry().processes() {
                if process.is_zombie() {
                    continue;
                }
                for tid in process.thread_ids() {
                    let Ok(task) = get_task(tid) else {
                        continue;
                    };
                    let thread = task.as_thread();
                    if thread.pending_exit() || !caller_pid_ns.contains(&thread.proc_data.pid_ns())
                    {
                        continue;
                    }
                    if thread.current_cred().ids().ruid == uid {
                        setpriority_one(&mut result, &actor_task, &actor_cred, task, new_nice);
                    }
                }
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    result.map(|_| 0)
}

const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_CLASS_MASK: u32 = 0x7;
const IOPRIO_PRIO_MASK: u32 = (1 << IOPRIO_CLASS_SHIFT) - 1;
const IOPRIO_LEVEL_MASK: u32 = 0x7;
const IOPRIO_CLASS_NONE: u32 = 0;
const IOPRIO_CLASS_RT: u32 = 1;
const IOPRIO_CLASS_BE: u32 = 2;
const IOPRIO_CLASS_IDLE: u32 = 3;
const IOPRIO_WHO_PROCESS: u32 = 1;
const IOPRIO_WHO_PGRP: u32 = 2;
const IOPRIO_WHO_USER: u32 = 3;

fn ioprio_class(ioprio: u32) -> u32 {
    (ioprio >> IOPRIO_CLASS_SHIFT) & IOPRIO_CLASS_MASK
}

fn ioprio_level(ioprio: u32) -> u32 {
    ioprio & IOPRIO_LEVEL_MASK
}

fn ioprio_value(class: u32, level: u32) -> u16 {
    ((class << IOPRIO_CLASS_SHIFT) | (level & IOPRIO_PRIO_MASK)) as u16
}

/// Validate the value shape accepted by Linux's direct `ioprio_set` path.
/// The kernel checks only the low three data bits for CLASS_NONE, so the
/// remaining data bits are retained as opaque hints just as they are in the
/// Linux `io_context` ABI.
fn validate_ioprio_shape(ioprio: u32) -> AxResult<()> {
    match ioprio_class(ioprio) {
        IOPRIO_CLASS_NONE if ioprio_level(ioprio) == 0 => Ok(()),
        IOPRIO_CLASS_NONE => Err(AxError::InvalidInput),
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE | IOPRIO_CLASS_IDLE => Ok(()),
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_ioprio(actor: &Cred, ioprio: u32) -> AxResult<()> {
    validate_ioprio_shape(ioprio)?;
    if ioprio_class(ioprio) == IOPRIO_CLASS_RT
        && !actor.has_effective_capability_in_own_user_ns(CAP_SYS_ADMIN)
        && !actor.has_effective_capability_in_own_user_ns(CAP_SYS_NICE)
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

fn ioprio_default_for_sched(class: SchedClass, nice: i8) -> u16 {
    let class = match class {
        SchedClass::Idle => IOPRIO_CLASS_IDLE,
        SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline => IOPRIO_CLASS_RT,
        SchedClass::Normal | SchedClass::Batch => IOPRIO_CLASS_BE,
    };
    let level = ((nice as i32 + 20) / 5).clamp(0, 7) as u32;
    ioprio_value(class, level)
}

fn ioprio_for_task(task: &AxTaskRef) -> AxResult<u16> {
    let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let raw = thread.io_priority_raw();
    if ioprio_class(raw as u32) == IOPRIO_CLASS_NONE {
        let state = sched_state(task);
        Ok(ioprio_default_for_sched(state.class, state.nice))
    } else {
        Ok(raw)
    }
}

fn ioprio_raw_for_task(task: &AxTaskRef) -> AxResult<u16> {
    task.try_as_thread()
        .ok_or(AxError::NoSuchProcess)
        .map(|thread| thread.io_priority_raw())
}

enum IoprioTarget {
    Live(AxTaskRef),
    Zombie(Arc<Process>),
}

impl IoprioTarget {
    fn raw_priority(&self) -> AxResult<u16> {
        match self {
            Self::Live(task) => ioprio_raw_for_task(task),
            Self::Zombie(process) => zombie_ioprio(process),
        }
    }

    fn effective_priority(&self) -> AxResult<u16> {
        match self {
            Self::Live(task) => ioprio_for_task(task),
            Self::Zombie(process) => {
                let raw = zombie_ioprio(process)?;
                if ioprio_class(raw as u32) == IOPRIO_CLASS_NONE {
                    let state = zombie_scheduler_state(process)?;
                    Ok(ioprio_default_for_sched(state.class, state.nice))
                } else {
                    Ok(raw)
                }
            }
        }
    }

    fn credential(&self) -> AxResult<Arc<Cred>> {
        match self {
            Self::Live(task) => task
                .try_as_thread()
                .ok_or(AxError::NoSuchProcess)
                .map(|thread| thread.current_cred()),
            Self::Zombie(process) => process
                .zombie_payload()
                .ok_or(AxError::NoSuchProcess)
                .map(|snapshot| snapshot.credential.clone()),
        }
    }

    fn set_priority(&self, priority: u16) -> AxResult<()> {
        match self {
            Self::Live(task) => task.as_thread().set_io_priority_raw(priority),
            Self::Zombie(process) => crate::task::set_zombie_ioprio(process, priority),
        }
    }
}

fn visible_tid_in_namespace(task: &AxTaskRef, caller_pid_ns: &Arc<PidNamespace>) -> Option<Pid> {
    let thread = task.try_as_thread()?;
    let target_pid_ns = thread.proc_data.pid_ns();
    // After non-leader exec the visible TID is intentionally rebound to the
    // process PID. Otherwise use the immutable kernel TID so PID-namespace
    // init (whose vpid is one) is rendered correctly.
    let tid = if thread.tid() != thread.kernel_tid() {
        thread.tid()
    } else {
        thread.kernel_tid()
    };
    caller_pid_ns.visible_pid_for(&target_pid_ns, tid)
}

fn visible_process_pid_in_namespace(
    process_pid: Pid,
    target_pid_ns: &Arc<PidNamespace>,
    caller_pid_ns: &Arc<PidNamespace>,
) -> Option<Pid> {
    caller_pid_ns.visible_pid_for(target_pid_ns, process_pid)
}

/// Reuse the live-task resolver used by signal/kill paths, then apply the
/// caller's PID namespace rendering. The fallback is needed when a nested
/// namespace has a vpid (for example its init task's `1`) that is also a
/// different global PID; it still scans the authoritative task table rather
/// than inventing a second task registry.
fn visible_task_for_ioprio(who: Pid, caller_pid_ns: &Arc<PidNamespace>) -> AxResult<AxTaskRef> {
    if let Ok(task) = get_visible_task(who)
        && visible_tid_in_namespace(&task, caller_pid_ns) == Some(who)
        && !task.as_thread().pending_exit()
    {
        return Ok(task);
    }

    try_tasks()?
        .into_iter()
        .find(|task| {
            !task.as_thread().pending_exit()
                && visible_tid_in_namespace(task, caller_pid_ns) == Some(who)
        })
        .ok_or(AxError::NoSuchProcess)
}

fn zombie_for_ioprio(who: Pid, caller_pid_ns: &Arc<PidNamespace>) -> AxResult<Arc<Process>> {
    if let Ok(process) = get_process_including_zombie(who)
        && process.is_zombie()
        && zombie_pid_ns(&process).is_some_and(|target_pid_ns| {
            visible_process_pid_in_namespace(process.pid(), &target_pid_ns, caller_pid_ns)
                == Some(who)
        })
    {
        return Ok(process);
    }

    for process in process_domain()?.registry().processes() {
        if process.is_zombie()
            && zombie_pid_ns(&process).is_some_and(|target_pid_ns| {
                visible_process_pid_in_namespace(process.pid(), &target_pid_ns, caller_pid_ns)
                    == Some(who)
            })
        {
            return Ok(process);
        }
    }
    Err(AxError::NoSuchProcess)
}

fn ioprio_process_target(
    who: Pid,
    actor_task: &AxTaskRef,
    caller_pid_ns: &Arc<PidNamespace>,
) -> AxResult<IoprioTarget> {
    if who == 0 {
        return Ok(IoprioTarget::Live(actor_task.clone()));
    }
    match visible_task_for_ioprio(who, caller_pid_ns) {
        Ok(task) => Ok(IoprioTarget::Live(task)),
        Err(AxError::NoSuchProcess) => {
            zombie_for_ioprio(who, caller_pid_ns).map(IoprioTarget::Zombie)
        }
        Err(error) => Err(error),
    }
}

/// Snapshot all matching tasks while retaining the exact task references.
/// This avoids re-looking up numeric TIDs after the target selection and
/// bounds the only allocation to the existing fallible task-table snapshot.
fn ioprio_multi_targets(
    which: u32,
    who: u32,
    actor_task: &AxTaskRef,
    actor_cred: &Cred,
) -> AxResult<Vec<IoprioTarget>> {
    let caller_pid_ns = actor_task.as_thread().proc_data.pid_ns();
    let tasks = try_tasks()?;
    let mut targets = Vec::new();
    match which {
        IOPRIO_WHO_PGRP => {
            let actor_group = actor_task.as_thread().proc_data.proc.group();
            // Resolve PGID in the caller's namespace by first finding a
            // visible process in the authoritative task table. This handles
            // a nested namespace's vpid without treating the global PGID as
            // the caller's number.
            let target_group = if who == 0 {
                Some(actor_group)
            } else {
                let mut target_group = None;
                for task in &tasks {
                    let thread = task.as_thread();
                    let group = thread.proc_data.proc.group();
                    if visible_process_pid_in_namespace(
                        group.pgid(),
                        &thread.proc_data.pid_ns(),
                        &caller_pid_ns,
                    ) == Some(who)
                    {
                        target_group = Some(group);
                        break;
                    }
                }
                if target_group.is_none() {
                    for process in process_domain()?.registry().processes() {
                        if !process.is_zombie() {
                            continue;
                        }
                        let Some(target_pid_ns) = zombie_pid_ns(&process) else {
                            continue;
                        };
                        let group = process.group();
                        if visible_process_pid_in_namespace(
                            group.pgid(),
                            &target_pid_ns,
                            &caller_pid_ns,
                        ) == Some(who)
                        {
                            target_group = Some(group);
                            break;
                        }
                    }
                }
                target_group
            };
            let target_group = target_group.ok_or(AxError::NoSuchProcess)?;
            for task in tasks {
                let thread = task.as_thread();
                if !caller_pid_ns.contains(&thread.proc_data.pid_ns()) {
                    continue;
                }
                let group = thread.proc_data.proc.group();
                if Arc::ptr_eq(&group, &target_group) {
                    targets.push(IoprioTarget::Live(task));
                }
            }
            for process in process_domain()?.registry().processes() {
                if !process.is_zombie() || !Arc::ptr_eq(&process.group(), &target_group) {
                    continue;
                }
                if zombie_pid_ns(&process)
                    .is_some_and(|target_pid_ns| caller_pid_ns.contains(&target_pid_ns))
                {
                    targets.push(IoprioTarget::Zombie(process));
                }
            }
        }
        IOPRIO_WHO_USER => {
            let uid = if who == 0 {
                actor_cred.ids().ruid
            } else {
                actor_cred
                    .user_ns()
                    .make_kuid(who)
                    .ok_or(AxError::NoSuchProcess)?
            };
            for task in tasks {
                let thread = task.as_thread();
                if caller_pid_ns.contains(&thread.proc_data.pid_ns()) && thread.real_uid() == uid {
                    targets.push(IoprioTarget::Live(task));
                }
            }
            for process in process_domain()?.registry().processes() {
                if !process.is_zombie() {
                    continue;
                }
                let Some(target_pid_ns) = zombie_pid_ns(&process) else {
                    continue;
                };
                let Some(snapshot) = process.zombie_payload() else {
                    continue;
                };
                if caller_pid_ns.contains(&target_pid_ns) && snapshot.credential.ids().ruid == uid {
                    targets.push(IoprioTarget::Zombie(process));
                }
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    if targets.is_empty() {
        Err(AxError::NoSuchProcess)
    } else {
        Ok(targets)
    }
}

fn ioprio_set_allowed(actor: &Cred, target: &IoprioTarget) -> AxResult<()> {
    let target_cred = target.credential()?;
    let actor_ids = actor.ids();
    let target_uid = target_cred.ids().ruid;
    if target_uid == actor_ids.ruid
        || target_uid == actor_ids.euid
        || actor.has_effective_capability_in_own_user_ns(CAP_SYS_NICE)
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

pub fn sys_ioprio_get(which: u32, who: u32) -> AxResult<isize> {
    debug!("sys_ioprio_get <= which: {which}, who: {who}");
    let actor_task = current().clone();
    let actor_cred = actor_task.as_thread().current_cred();
    let caller_pid_ns = actor_task.as_thread().proc_data.pid_ns();
    match which {
        IOPRIO_WHO_PROCESS => {
            Ok(ioprio_process_target(who, &actor_task, &caller_pid_ns)?.raw_priority()? as isize)
        }
        IOPRIO_WHO_PGRP | IOPRIO_WHO_USER => {
            let targets = ioprio_multi_targets(which, who, &actor_task, &actor_cred)?;
            let mut highest = None;
            for target in &targets {
                let priority = target.effective_priority()?;
                highest = Some(highest.map_or(priority, |current: u16| current.min(priority)));
            }
            highest
                .map(|priority| priority as isize)
                .ok_or(AxError::NoSuchProcess)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_ioprio_set(which: u32, who: u32, ioprio: u32) -> AxResult<isize> {
    debug!("sys_ioprio_set <= which: {which}, who: {who}, ioprio: {ioprio:#x}");
    let actor_task = current().clone();
    let actor_cred = actor_task.as_thread().current_cred();
    let caller_pid_ns = actor_task.as_thread().proc_data.pid_ns();
    // Linux validates the requested class/capability before selecting a
    // target, so an invalid realtime request wins over target lookup errors.
    validate_ioprio(&actor_cred, ioprio)?;
    let priority = ioprio as u16;
    match which {
        IOPRIO_WHO_PROCESS => {
            let target = ioprio_process_target(who, &actor_task, &caller_pid_ns)?;
            ioprio_set_allowed(&actor_cred, &target)?;
            target.set_priority(priority)?;
        }
        IOPRIO_WHO_PGRP | IOPRIO_WHO_USER => {
            let targets = ioprio_multi_targets(which, who, &actor_task, &actor_cred)?;
            // Preserve Linux's partial-update behavior if a later member is
            // not writable: each already-authorized task remains updated.
            for target in &targets {
                ioprio_set_allowed(&actor_cred, target)?;
                target.set_priority(priority)?;
            }
        }
        _ => return Err(AxError::InvalidInput),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn linux_affinity_masks_are_native_word_aligned() {
        assert_eq!(linux_cpumask_bytes(0), size_of::<usize>());
        assert_eq!(linux_cpumask_bytes(1), size_of::<usize>());
        assert_eq!(linux_cpumask_bytes(4), size_of::<usize>());
        assert_eq!(linux_cpumask_bytes(8), size_of::<usize>());
        assert_eq!(
            linux_cpumask_bytes(usize::BITS as usize),
            size_of::<usize>()
        );
        assert_eq!(
            linux_cpumask_bytes(usize::BITS as usize + 1),
            size_of::<usize>() * 2
        );
    }

    #[test]
    fn clock_sleep_result_distinguishes_interrupt_from_completion() {
        assert_eq!(
            flatten_clock_sleep_result(Ok(Err(Interrupted))),
            Ok(ClockSleepOutcome::Interrupted)
        );
        assert_eq!(
            flatten_clock_sleep_result(Ok(Ok(()))),
            Ok(ClockSleepOutcome::Completed)
        );
    }

    #[test]
    fn clock_sleep_interrupt_retries_only_pure_internal_wakes() {
        assert_eq!(
            classify_clock_sleep_interrupt(false, false, false, false),
            ClockSleepInterruptDisposition::Retry
        );
        assert_eq!(
            classify_clock_sleep_interrupt(true, false, false, false),
            ClockSleepInterruptDisposition::Return
        );
        assert_eq!(
            classify_clock_sleep_interrupt(false, true, false, false),
            ClockSleepInterruptDisposition::Return
        );
        assert_eq!(
            classify_clock_sleep_interrupt(false, false, true, false),
            ClockSleepInterruptDisposition::Return
        );
        assert_eq!(
            classify_clock_sleep_interrupt(false, false, false, true),
            ClockSleepInterruptDisposition::Return
        );
    }

    #[test]
    fn default_sigcont_does_not_make_clock_sleep_visible() {
        assert!(!clock_sleep_signal_is_visible(
            Signo::SIGCONT,
            false,
            SignalDisposition::Default,
        ));
        assert!(clock_sleep_signal_is_visible(
            Signo::SIGCONT,
            false,
            SignalDisposition::Handler(0x1000),
        ));
        assert!(clock_sleep_signal_is_visible(
            Signo::SIGTERM,
            false,
            SignalDisposition::Default,
        ));
        assert!(!clock_sleep_signal_is_visible(
            Signo::SIGTERM,
            true,
            SignalDisposition::Default,
        ));
    }

    #[test]
    fn clock_sleep_result_preserves_block_session_failure() {
        assert_eq!(
            flatten_clock_sleep_result(Err(BlockOnError::Busy)),
            Err(AxError::ResourceBusy)
        );
    }

    #[test]
    fn clock_nanosleep_rejects_unknown_flags() {
        assert_eq!(clock_nanosleep_is_absolute(0), Ok(false));
        assert_eq!(clock_nanosleep_is_absolute(TIMER_ABSTIME), Ok(true));
        assert_eq!(
            clock_nanosleep_is_absolute(TIMER_ABSTIME | 0x8000_0000),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn absolute_sleep_preserves_explicit_completion_ownership() {
        let deadline = Duration::from_secs(10);

        assert_eq!(
            finish_absolute_clock_sleep(
                ClockSleepOutcome::Completed,
                Duration::from_secs(1),
                deadline,
            ),
            Ok(0)
        );
        assert_eq!(
            finish_absolute_clock_sleep(
                ClockSleepOutcome::Interrupted,
                Duration::from_secs(11),
                deadline,
            ),
            Ok(0)
        );
        assert_eq!(
            finish_absolute_clock_sleep(
                ClockSleepOutcome::Interrupted,
                Duration::from_secs(9),
                deadline,
            ),
            Err(AxError::Interrupted)
        );
    }

    #[test]
    fn deadline_setattr_is_known_but_unsupported() {
        assert!(matches!(
            sched_class_for_set(SCHED_DEADLINE, SchedSetAbi::Attr),
            Err(AxError::OperationNotSupported)
        ));
    }

    #[test]
    fn deadline_legacy_setter_remains_invalid() {
        assert!(matches!(
            sched_class_for_set(SCHED_DEADLINE, SchedSetAbi::Legacy),
            Err(AxError::InvalidInput)
        ));
    }

    #[test]
    fn scheduler_update_rejects_missing_target_before_any_store() {
        let state_updates = Cell::new(0);
        let flag_updates = Cell::new(0);
        let result = apply_sched_state_with_reset::<()>(
            None,
            || {
                state_updates.set(state_updates.get() + 1);
                Ok(0)
            },
            |_| flag_updates.set(flag_updates.get() + 1),
        );
        assert_eq!(result, Err(AxError::NoSuchProcess));
        assert_eq!(state_updates.get(), 0);
        assert_eq!(flag_updates.get(), 0);
    }

    #[test]
    fn scheduler_failure_never_partially_stores_reset_on_fork() {
        let target = ();
        let flag_updates = Cell::new(0);
        let result = apply_sched_state_with_reset(
            Some(&target),
            || Err(AxError::NoSuchProcess),
            |_| flag_updates.set(flag_updates.get() + 1),
        );
        assert_eq!(result, Err(AxError::NoSuchProcess));
        assert_eq!(flag_updates.get(), 0);
    }

    #[test]
    fn supported_setter_policies_still_map_to_real_classes() {
        for (policy, class) in [
            (SCHED_NORMAL, SchedClass::Normal),
            (SCHED_BATCH, SchedClass::Batch),
            (SCHED_IDLE, SchedClass::Idle),
            (SCHED_FIFO, SchedClass::Fifo),
            (SCHED_RR, SchedClass::RoundRobin),
        ] {
            assert_eq!(
                sched_class_for_set(policy, SchedSetAbi::Attr).unwrap(),
                class
            );
            assert_eq!(
                sched_class_for_set(policy, SchedSetAbi::Legacy).unwrap(),
                class
            );
        }
    }

    #[test]
    fn deadline_priority_bounds_remain_queryable() {
        assert_eq!(
            linux_priority_bounds(SCHED_DEADLINE as i32).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn ioprio_values_match_linux_class_and_level_encoding() {
        assert_eq!(ioprio_value(IOPRIO_CLASS_NONE, 0), 0);
        assert_eq!(
            ioprio_value(IOPRIO_CLASS_BE, 4),
            (2 << IOPRIO_CLASS_SHIFT) | 4
        );
        assert_eq!(
            ioprio_class(ioprio_value(IOPRIO_CLASS_RT, 6) as u32),
            IOPRIO_CLASS_RT
        );
        assert_eq!(ioprio_level(ioprio_value(IOPRIO_CLASS_IDLE, 7) as u32), 7);
    }

    #[test]
    fn ioprio_validation_preserves_direct_syscall_hint_bits() {
        assert_eq!(
            validate_ioprio_shape(ioprio_value(IOPRIO_CLASS_NONE, 0) as u32),
            Ok(())
        );
        assert_eq!(
            validate_ioprio_shape(ioprio_value(IOPRIO_CLASS_NONE, 0x100) as u32),
            Ok(())
        );
        assert_eq!(
            validate_ioprio_shape(ioprio_value(IOPRIO_CLASS_NONE, 1) as u32),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            validate_ioprio_shape(ioprio_value(4, 0) as u32),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn ioprio_default_priority_derives_from_scheduler_class_and_nice() {
        assert_eq!(
            ioprio_default_for_sched(SchedClass::Normal, -20),
            ioprio_value(IOPRIO_CLASS_BE, 0)
        );
        assert_eq!(
            ioprio_default_for_sched(SchedClass::Batch, 19),
            ioprio_value(IOPRIO_CLASS_BE, 7)
        );
        assert_eq!(
            ioprio_default_for_sched(SchedClass::Idle, 19),
            ioprio_value(IOPRIO_CLASS_IDLE, 7)
        );
        assert_eq!(
            ioprio_default_for_sched(SchedClass::RoundRobin, 0),
            ioprio_value(IOPRIO_CLASS_RT, 4)
        );
    }

    #[test]
    fn ioprio_highest_target_priority_is_the_lowest_encoded_value() {
        let priorities = [
            ioprio_value(IOPRIO_CLASS_BE, 7),
            ioprio_value(IOPRIO_CLASS_RT, 4),
            ioprio_value(IOPRIO_CLASS_IDLE, 0),
        ];
        assert_eq!(
            priorities.into_iter().min(),
            Some(ioprio_value(IOPRIO_CLASS_RT, 4))
        );
    }

    #[test]
    fn getpriority_decodes_the_low_abi_int_word() {
        assert_eq!(syscall_c_int(0x1_0000_0002), 2);
        assert_eq!(syscall_c_int(0x0000_0000_ffff_ffff), -1);
        assert_eq!(syscall_c_int(0xffff_ffff_8000_0000), i32::MIN);
    }

    #[test]
    fn setpriority_accumulator_preserves_target_failures() {
        let mut result = Err(AxError::NoSuchProcess);
        // A first success clears only the initial ESRCH accumulator.
        record_setpriority_result(&mut result, Ok(()));
        assert_eq!(result, Ok(()));
        // A later failure is returned even though an earlier target changed.
        record_setpriority_result(&mut result, Err(AxError::PermissionDenied));
        assert_eq!(result, Err(AxError::PermissionDenied));
        // Success after that failure must not hide it.
        record_setpriority_result(&mut result, Ok(()));
        assert_eq!(result, Err(AxError::PermissionDenied));
        // A later failure replaces the recorded error while enumeration
        // continues, matching the kernel's per-target assignment.
        record_setpriority_result(&mut result, Err(AxError::InvalidInput));
        assert_eq!(result, Err(AxError::InvalidInput));
    }

    #[test]
    fn getpriority_user_filter_is_real_uid_only() {
        let real_uid = 1_000_u32;
        let effective_uid = 0_u32;
        assert!(getpriority_matches_real_uid(&real_uid, &real_uid));
        assert!(!getpriority_matches_real_uid(&real_uid, &effective_uid));
    }

    #[test]
    fn getpriority_aggregation_selects_the_lowest_nice() {
        let mut best = None;
        for nice in [8, -4, 12, 0] {
            record_lower_nice(&mut best, nice);
        }
        assert_eq!(best, Some(-4));
        assert_eq!(raw_priority_from_nice(best.unwrap()), 24);
    }
}
