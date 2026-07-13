use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::TimeValue;
use axtask::{
    AxCpuMask, AxTaskRef, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN, SchedClass,
    SchedState, TaskSchedError, current,
    future::{BlockOnError, Interrupted, block_on, interruptible},
    sched_state, set_sched_state, set_task_affinity,
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
    CLOCK_THREAD_CPUTIME_ID, PRIO_PGRP, PRIO_PROCESS, PRIO_USER, RLIMIT_NICE, SCHED_BATCH,
    SCHED_DEADLINE, SCHED_FIFO, SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE, SCHED_NORMAL,
    SCHED_RESET_ON_FORK, SCHED_RR, TIMER_ABSTIME, timespec,
};
use starry_process::{Pid, ProcessError};
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use crate::{
    task::{
        AlarmClock, AsThread, Cred, ProcStateHint, get_process_group, get_task, process_domain,
        security::{SchedulerSecurityOperation, SecuritySchedulerContext, dispatch_scheduler},
        sleep_until_clock, try_tasks, with_proc_state_hint,
    },
    time::TimeValueLike,
};

const SUPPORTED_SCHED_ATTR_FLAGS: u64 = SCHED_FLAG_RESET_ON_FORK as u64;
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
        // known capability as unsupported instead of running it as CFS.
        SCHED_DEADLINE => match abi {
            SchedSetAbi::Legacy => Err(AxError::InvalidInput),
            SchedSetAbi::Attr => Err(AxError::OperationNotSupported),
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

fn sched_reset_on_fork(task: &AxTaskRef) -> bool {
    task.try_as_thread()
        .is_some_and(|thread| thread.sched_reset_on_fork())
}

fn linux_policy_from_state(task: &AxTaskRef, state: SchedState) -> i32 {
    let base = match state.class {
        SchedClass::Normal => SCHED_NORMAL as i32,
        SchedClass::Batch => SCHED_BATCH as i32,
        SchedClass::Idle => SCHED_IDLE as i32,
        SchedClass::Fifo => SCHED_FIFO as i32,
        SchedClass::RoundRobin => SCHED_RR as i32,
    };

    if sched_reset_on_fork(task) {
        base | SCHED_RESET_ON_FORK as i32
    } else {
        base
    }
}

fn state_static_priority(state: SchedState) -> i32 {
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => state.rt_priority as i32,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => 0,
    }
}

fn state_nice(state: SchedState) -> i32 {
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => 0,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => state.nice as i32,
    }
}

fn write_sched_attr_kernel_size(attr: *const SchedAttr) -> AxResult<()> {
    let size_ptr = attr as *mut u32;
    size_ptr.vm_write(size_of::<SchedAttr>() as u32)?;
    Ok(())
}

fn read_sched_attr(attr: *const SchedAttr) -> AxResult<SchedAttr> {
    let mut attr_size = attr.cast::<u32>().vm_read()? as usize;
    if attr_size == 0 {
        attr_size = SCHED_ATTR_SIZE_VER0;
    }
    if !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX_SIZE).contains(&attr_size) {
        write_sched_attr_kernel_size(attr)?;
        return Err(LinuxError::E2BIG.into());
    }

    let mut out = SchedAttr::default();
    let copy_size = attr_size.min(size_of::<SchedAttr>());
    let src = vm_load(attr.cast::<u8>(), copy_size)?;
    let dst = unsafe {
        core::slice::from_raw_parts_mut((&mut out as *mut SchedAttr).cast::<u8>(), copy_size)
    };
    dst.copy_from_slice(&src);

    if attr_size > size_of::<SchedAttr>() {
        let extra = vm_load(
            attr.cast::<u8>().wrapping_add(size_of::<SchedAttr>()),
            attr_size - size_of::<SchedAttr>(),
        )?;
        if extra.iter().any(|byte| *byte != 0) {
            write_sched_attr_kernel_size(attr)?;
            return Err(LinuxError::E2BIG.into());
        }
    }

    out.sched_nice = out.sched_nice.clamp(-20, 19);
    Ok(out)
}

fn raw_priority_from_nice(nice: i8) -> isize {
    20 - nice as isize
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
    set_sched_state(task, state).map_err(|error| match error {
        TaskSchedError::Unsupported => AxError::OperationNotSupported,
        TaskSchedError::TaskExited => AxError::NoSuchProcess,
        TaskSchedError::RunQueueUnavailable(_) => AxError::BadState,
        TaskSchedError::Scheduler(_) => AxError::InvalidInput,
    })?;
    Ok(0)
}

fn apply_sched_state_with_reset<T>(
    target: Option<&T>,
    apply: impl FnOnce() -> AxResult<isize>,
    store_reset: impl FnOnce(&T),
) -> AxResult<isize> {
    let target = target.ok_or(AxError::NoSuchProcess)?;
    let result = apply()?;
    store_reset(target);
    Ok(result)
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

fn update_sched_policy(task: &AxTaskRef, policy: i32, priority: i32) -> AxResult<isize> {
    let reset_on_fork = policy & SCHED_RESET_ON_FORK as i32 != 0;
    let class = sched_class_for_set(
        (policy & !(SCHED_RESET_ON_FORK as i32)) as u32,
        SchedSetAbi::Legacy,
    )?;
    let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let authority = scheduler_authority_snapshot(task)?;
    authority.authorize(SchedulerSecurityOperation::SetPolicy {
        realtime: matches!(class, SchedClass::Fifo | SchedClass::RoundRobin),
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
    }
    apply_sched_state_with_reset(
        Some(thread),
        || apply_sched_state(task, state),
        |thread| thread.set_sched_reset_on_fork(reset_on_fork),
    )
}

fn update_sched_param(task: &AxTaskRef, priority: i32) -> AxResult<isize> {
    let mut state = sched_state(task);
    let realtime = matches!(state.class, SchedClass::Fifo | SchedClass::RoundRobin);
    scheduler_authority_snapshot(task)?
        .authorize(SchedulerSecurityOperation::SetParam { realtime })?;
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            state.rt_priority = validate_rt_priority(priority)?;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            validate_static_priority(priority)?;
            state.rt_priority = 0;
        }
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

fn min_nice_for_threads<I>(threads: I) -> AxResult<i8>
where
    I: IntoIterator<Item = Pid>,
{
    let mut best: Option<i8> = None;

    for tid in threads {
        let Ok(task) = get_task(tid) else {
            continue;
        };
        let nice = sched_state(&task).nice;
        best = Some(best.map_or(nice, |curr| curr.min(nice)));
    }

    best.ok_or(AxError::NoSuchProcess)
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
    actor_cred: &Arc<Cred>,
    target: &SchedulerNiceTarget,
    new_nice: i8,
) -> AxResult<()> {
    let task = &target.task;
    let target_thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let mut state = sched_state(task);
    let rlimit_nice = target_thread.proc_data.rlim.read()[RLIMIT_NICE].current;
    SchedulerAuthoritySnapshot::new(actor_cred.clone(), target.credential.clone()).authorize(
        SchedulerSecurityOperation::SetNice {
            current_nice: state.nice,
            requested_nice: new_nice,
            rlimit_nice,
        },
    )?;
    state.nice = new_nice;
    apply_sched_state(task, state).map(|_| ())
}

pub fn sys_sched_yield() -> AxResult<isize> {
    axtask::yield_now();
    Ok(0)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ClockSleepOutcome {
    Completed,
    Interrupted,
}

fn flatten_clock_sleep_result(
    result: Result<Result<AxResult<()>, Interrupted>, BlockOnError>,
) -> AxResult<ClockSleepOutcome> {
    match result {
        Err(error) => Err(error.into()),
        Ok(Err(_)) => Ok(ClockSleepOutcome::Interrupted),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Ok(Ok(()))) => Ok(ClockSleepOutcome::Completed),
    }
}

fn sleep_relative(dur: TimeValue) -> AxResult<TimeValue> {
    debug!("sleep_impl <= {dur:?}");

    if dur.is_zero() {
        return Ok(dur);
    }
    let start = AlarmClock::Monotonic.now();
    let deadline = start.checked_add(dur).unwrap_or(Duration::MAX);

    // We detect EINTR manually if the slept time is not enough.
    let result = with_proc_state_hint(ProcStateHint::Interruptible, || {
        block_on(interruptible(sleep_until_clock(
            AlarmClock::Monotonic,
            deadline,
        )))
    });
    let _ = flatten_clock_sleep_result(result)?;

    Ok(AlarmClock::Monotonic.now() - start)
}

fn sleep_absolute(clock: AlarmClock, deadline: TimeValue) -> AxResult<bool> {
    debug!("sleep_absolute <= clock: {clock:?}, deadline: {deadline:?}");

    let result = with_proc_state_hint(ProcStateHint::Interruptible, || {
        block_on(interruptible(sleep_until_clock(clock, deadline)))
    });
    let _ = flatten_clock_sleep_result(result)?;
    Ok(clock.now() >= deadline)
}

fn remaining_relative_sleep(req: TimeValue, actual: TimeValue) -> Option<TimeValue> {
    if actual < req {
        Some(req - actual)
    } else {
        None
    }
}

/// Sleep some nanoseconds
pub fn sys_nanosleep(req: *const timespec, rem: *mut timespec) -> AxResult<isize> {
    // FIXME: AnyBitPattern
    let req = unsafe { req.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    let actual = sleep_relative(req)?;

    if let Some(diff) = remaining_relative_sleep(req, actual) {
        debug!("sys_nanosleep => rem: {diff:?}");
        if let Some(rem) = rem.nullable() {
            rem.vm_write(timespec::from_time_value(diff))?;
        }
        Err(AxError::Interrupted)
    } else {
        Ok(0)
    }
}

pub fn sys_clock_nanosleep(
    clock_id: __kernel_clockid_t,
    flags: u32,
    req: *const timespec,
    rem: *mut timespec,
) -> AxResult<isize> {
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

    let req = unsafe { req.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    if flags & TIMER_ABSTIME != 0 {
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
        if sleep_absolute(clock, deadline)? {
            Ok(0)
        } else {
            Err(AxError::Interrupted)
        }
    } else {
        let actual = sleep_relative(req)?;

        if let Some(diff) = remaining_relative_sleep(req, actual) {
            debug!("sys_clock_nanosleep => rem: {diff:?}");
            if let Some(rem) = rem.nullable() {
                rem.vm_write(timespec::from_time_value(diff))?;
            }
            Err(AxError::Interrupted)
        } else {
            Ok(0)
        }
    }
}

pub fn sys_sched_getaffinity(pid: i32, cpusetsize: usize, user_mask: *mut u8) -> AxResult<isize> {
    if cpusetsize * 8 < axhal::cpu_num() {
        return Err(AxError::InvalidInput);
    }

    let mask = sched_target(pid)?.cpumask();
    let mask_bytes = mask.as_bytes();

    vm_write_slice(user_mask, mask_bytes)?;

    Ok(mask_bytes.len() as _)
}

pub fn sys_sched_setaffinity(pid: i32, cpusetsize: usize, user_mask: *const u8) -> AxResult<isize> {
    if cpusetsize == 0 {
        return Err(AxError::InvalidInput);
    }

    let size = cpusetsize.min(axhal::cpu_num().div_ceil(8));
    let user_mask = vm_load(user_mask, size)?;
    let mut cpu_mask = AxCpuMask::new();

    for i in 0..(size * 8).min(axhal::cpu_num()) {
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

pub fn sys_getcpu(cpu: *mut u32, node: *mut u32) -> AxResult<isize> {
    // Linux reports the CPU on which this call is actually executing.  A
    // concurrent affinity change may have published a restrictive mask before
    // the target reaches its migration safe point; reporting an allowed-but-
    // fictional CPU during that window would expose shadow scheduler state.
    let cpu_id = axhal::percpu::this_cpu_id();
    if !cpu.is_null() {
        cpu.vm_write(cpu_id as u32)?;
    }
    if !node.is_null() {
        node.vm_write(0)?;
    }
    Ok(0)
}

pub fn sys_sched_getscheduler(pid: i32) -> AxResult<isize> {
    let task = sched_target(pid)?;
    Ok(linux_policy_from_state(&task, sched_state(&task)) as isize)
}

pub fn sys_sched_setparam(pid: i32, param: *const SchedParam) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let priority = unsafe { param.vm_read_uninit()?.assume_init() }.sched_priority;
    let task = sched_target(pid)?;
    update_sched_param(&task, priority)
}

pub fn sys_sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let priority = unsafe { param.vm_read_uninit()?.assume_init() }.sched_priority;
    let task = sched_target(pid)?;
    update_sched_policy(&task, policy, priority)
}

pub fn sys_sched_getparam(pid: i32, param: *mut SchedParam) -> AxResult<isize> {
    if param.is_null() {
        return Err(AxError::InvalidInput);
    }
    let task = sched_target(pid)?;
    param.vm_write(SchedParam {
        sched_priority: state_static_priority(sched_state(&task)),
    })?;
    Ok(0)
}

pub fn sys_sched_get_priority_max(policy: i32) -> AxResult<isize> {
    Ok(linux_priority_bounds(policy)?.1)
}

pub fn sys_sched_get_priority_min(policy: i32) -> AxResult<isize> {
    Ok(linux_priority_bounds(policy)?.0)
}

pub fn sys_sched_rr_get_interval(pid: i32, interval: *mut timespec) -> AxResult<isize> {
    let task = sched_target(pid)?;
    interval.vm_write(timespec::from_time_value(rr_interval_for_state(
        sched_state(&task),
    )))?;
    Ok(0)
}

pub fn sys_sched_setattr(pid: i32, attr: *const SchedAttr, flags: u32) -> AxResult<isize> {
    if attr.is_null() || pid < 0 || flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let attr = read_sched_attr(attr)?;
    if attr.sched_flags & !SUPPORTED_SCHED_ATTR_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.sched_util_min != 0 || attr.sched_util_max != 0 {
        return Err(AxError::InvalidInput);
    }

    // Reject the known-but-unimplemented class before task lookup, permission
    // checks, or scheduler-state publication. This keeps failure free of
    // target-dependent side effects and prevents a fake Deadline snapshot.
    let class = sched_class_for_set(attr.sched_policy, SchedSetAbi::Attr)?;
    let task = sched_target(pid)?;
    let thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let authority = scheduler_authority_snapshot(&task)?;
    authority.authorize(SchedulerSecurityOperation::SetPolicy {
        realtime: matches!(class, SchedClass::Fifo | SchedClass::RoundRobin),
    })?;
    let mut state = sched_state(&task);
    let old_nice = state.nice;
    state.class = class;
    match class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            if attr.sched_runtime != 0 || attr.sched_deadline != 0 || attr.sched_period != 0 {
                return Err(AxError::InvalidInput);
            }
            state.rt_priority = validate_rt_priority(attr.sched_priority as i32)?;
            state.nice = 0;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            if attr.sched_runtime != 0 || attr.sched_deadline != 0 || attr.sched_period != 0 {
                return Err(AxError::InvalidInput);
            }
            state.rt_priority = validate_static_priority(attr.sched_priority as i32)?;
            let requested_nice = validate_nice(attr.sched_nice)?;
            let rlimit_nice = thread.proc_data.rlim.read()[RLIMIT_NICE].current;
            authority.authorize(SchedulerSecurityOperation::SetNice {
                current_nice: old_nice,
                requested_nice,
                rlimit_nice,
            })?;
            state.nice = requested_nice;
        }
    }
    let reset_on_fork = attr.sched_flags & SUPPORTED_SCHED_ATTR_FLAGS != 0;
    apply_sched_state_with_reset(
        Some(thread),
        || apply_sched_state(&task, state),
        |thread| thread.set_sched_reset_on_fork(reset_on_fork),
    )
}

pub fn sys_sched_getattr(pid: i32, attr: *mut SchedAttr, size: u32, flags: u32) -> AxResult<isize> {
    let out_size = size as usize;
    if attr.is_null()
        || pid < 0
        || out_size > SCHED_ATTR_MAX_SIZE
        || out_size < SCHED_ATTR_SIZE_VER0
        || flags != 0
    {
        return Err(AxError::InvalidInput);
    }

    let task = sched_target(pid)?;
    let state = sched_state(&task);
    let mut out = SchedAttr {
        size: out_size.min(size_of::<SchedAttr>()) as u32,
        sched_policy: linux_policy_from_state(&task, state) as u32 & !(SCHED_RESET_ON_FORK as u32),
        sched_flags: if sched_reset_on_fork(&task) {
            SUPPORTED_SCHED_ATTR_FLAGS
        } else {
            0
        },
        sched_nice: state_nice(state),
        sched_priority: state_static_priority(state) as u32,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
        sched_util_min: 0,
        sched_util_max: 0,
    };
    out.sched_flags &= SUPPORTED_SCHED_ATTR_FLAGS;

    let copy_size = out_size.min(size_of::<SchedAttr>());
    vm_write_slice(attr.cast::<u8>(), &unsafe {
        core::slice::from_raw_parts((&out as *const SchedAttr).cast::<u8>(), copy_size)
    })?;

    Ok(0)
}

pub fn sys_getpriority(which: u32, who: u32) -> AxResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            if who == 0 {
                let curr = current();
                Ok(raw_priority_from_nice(sched_state(&curr).nice))
            } else {
                let task = get_task(who)?;
                Ok(raw_priority_from_nice(sched_state(&task).nice))
            }
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                current().as_thread().proc_data.proc.group().pgid()
            } else {
                who
            };
            let group = get_process_group(pgid)?;
            let group_processes = group
                .try_processes(process_domain()?.registry())
                .map_err(process_error)?;
            Ok(raw_priority_from_nice(min_nice_for_threads(
                group_processes
                    .into_iter()
                    .flat_map(|proc| proc.thread_ids()),
            )?))
        }
        PRIO_USER => {
            let current = current();
            let cred = current.as_thread().current_cred();
            let uid = if who == 0 {
                cred.ids().ruid
            } else {
                cred.user_ns().make_kuid(who).ok_or(AxError::InvalidInput)?
            };
            Ok(raw_priority_from_nice(min_nice_for_threads(
                try_tasks()?.into_iter().filter_map(|task| {
                    let thread = task.try_as_thread()?;
                    let ids = thread.current_cred().ids();
                    (ids.ruid == uid || ids.euid == uid).then_some(thread.tid())
                }),
            )?))
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_setpriority(which: u32, who: u32, prio: i32) -> AxResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    let new_nice = clamp_nice(prio);
    let (actor_task, actor_cred) = scheduler_actor_snapshot();
    let mut targets = Vec::new();
    match which {
        PRIO_PROCESS => {
            let task = if who == 0 {
                actor_task.clone()
            } else {
                get_task(who)?
            };
            let target = scheduler_nice_target(&actor_task, &actor_cred, task)?;
            targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            targets.push(target);
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                actor_task.as_thread().proc_data.proc.group().pgid()
            } else {
                who
            };
            let group = get_process_group(pgid)?;
            for process in group
                .try_processes(process_domain()?.registry())
                .map_err(process_error)?
            {
                for tid in process.thread_ids() {
                    if let Ok(task) = get_task(tid) {
                        let target = scheduler_nice_target(&actor_task, &actor_cred, task)?;
                        targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                        targets.push(target);
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
                    .make_kuid(who)
                    .ok_or(AxError::InvalidInput)?
            };
            for task in try_tasks()? {
                if task.try_as_thread().is_none() {
                    continue;
                }
                let target = scheduler_nice_target(&actor_task, &actor_cred, task)?;
                let ids = target.credential.ids();
                if ids.ruid == uid || ids.euid == uid {
                    targets.try_reserve(1).map_err(|_| AxError::NoMemory)?;
                    targets.push(target);
                }
            }
        }
        _ => return Err(AxError::InvalidInput),
    }

    if targets.is_empty() {
        return Err(AxError::NoSuchProcess);
    }

    for target in &targets {
        set_task_nice(&actor_cred, target, new_nice)?;
    }

    Ok(0)
}

pub fn sys_ioprio_get(_which: u32, _who: u32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_ioprio_set(_which: u32, _who: u32, _ioprio: u32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn clock_sleep_result_preserves_timer_registration_failure() {
        assert_eq!(
            flatten_clock_sleep_result(Ok(Ok(Err(AxError::NoMemory)))),
            Err(AxError::NoMemory)
        );
    }

    #[test]
    fn clock_sleep_result_distinguishes_interrupt_from_completion() {
        assert_eq!(
            flatten_clock_sleep_result(Ok(Err(Interrupted))),
            Ok(ClockSleepOutcome::Interrupted)
        );
        assert_eq!(
            flatten_clock_sleep_result(Ok(Ok(Ok(())))),
            Ok(ClockSleepOutcome::Completed)
        );
    }

    #[test]
    fn clock_sleep_result_preserves_block_session_failure() {
        assert_eq!(
            flatten_clock_sleep_result(Err(BlockOnError::Busy)),
            Err(AxError::ResourceBusy)
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
}
