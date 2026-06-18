use alloc::{vec, vec::Vec};
use core::{
    mem::size_of,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::TimeValue;
use axtask::{
    AxCpuMask, AxTaskRef, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN, SchedClass,
    SchedState, current,
    future::{block_on, interruptible},
    sched_state, set_sched_state, set_task_affinity,
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CAP_SYS_ADMIN, CAP_SYS_NICE, CLOCK_BOOTTIME, CLOCK_MONOTONIC,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID, PRIO_PGRP, PRIO_PROCESS,
    PRIO_USER, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE,
    SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, TIMER_ABSTIME, timespec,
};
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use crate::{
    task::{
        AlarmClock, AsThread, ProcStateHint, get_process_group, get_task,
        has_pending_syscall_signal, processes, sleep_until_clock, with_proc_state_hint,
    },
    time::TimeValueLike,
};

const SUPPORTED_SCHED_ATTR_FLAGS: u64 = SCHED_FLAG_RESET_ON_FORK as u64;
const SCHED_ATTR_SIZE_VER0: usize = 48;
const SCHED_ATTR_MAX_SIZE: usize = 4096;
const IOPRIO_CLASS_SHIFT: u32 = 13;
const IOPRIO_PRIO_MASK: u32 = (1 << IOPRIO_CLASS_SHIFT) - 1;
const IOPRIO_NR_LEVELS: u32 = 8;
const IOPRIO_CLASS_NONE: u32 = 0;
const IOPRIO_CLASS_RT: u32 = 1;
const IOPRIO_CLASS_BE: u32 = 2;
const IOPRIO_CLASS_IDLE: u32 = 3;
const IOPRIO_WHO_PROCESS: u32 = 1;
const IOPRIO_WHO_PGRP: u32 = 2;
const IOPRIO_WHO_USER: u32 = 3;
const SCHED_RR_TIMESLICE_MS_DEFAULT: u32 = {
    let ms = (RR_TIMESLICE_TICKS * 1000) / axconfig::TICKS_PER_SEC;
    if ms == 0 { 1 } else { ms as u32 }
};
static SCHED_RR_TIMESLICE_MS: AtomicU32 = AtomicU32::new(SCHED_RR_TIMESLICE_MS_DEFAULT);
const SHORT_RELATIVE_SLEEP_LIMIT: Duration = Duration::from_micros(2000);
const PRECISE_RELATIVE_SLEEP_MIN: Duration = Duration::from_millis(50);
const PRECISE_RELATIVE_SLEEP_LIMIT: Duration = Duration::from_millis(250);
const PRECISE_RELATIVE_SLEEP_SPIN_TAIL: Duration = Duration::from_millis(80);

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

fn sched_class_from_policy(policy: i32) -> AxResult<SchedClass> {
    match policy as u32 {
        SCHED_NORMAL => Ok(SchedClass::Normal),
        SCHED_BATCH => Ok(SchedClass::Batch),
        SCHED_IDLE => Ok(SchedClass::Idle),
        SCHED_FIFO => Ok(SchedClass::Fifo),
        SCHED_RR => Ok(SchedClass::RoundRobin),
        SCHED_DEADLINE => Ok(SchedClass::Deadline),
        _ => Err(AxError::InvalidInput),
    }
}

fn has_sched_admin_capability() -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_NICE)
}

fn has_ioprio_realtime_capability() -> bool {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    proc_data.has_effective_capability(CAP_SYS_ADMIN)
        || proc_data.has_effective_capability(CAP_SYS_NICE)
}

fn can_manage_sched_target(task: &AxTaskRef) -> AxResult<()> {
    if has_sched_admin_capability() {
        return Ok(());
    }

    let actor = current();
    let actor_thread = actor.as_thread();
    let actor_euid = actor_thread.proc_data.euid();
    let target_thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let target_proc = &target_thread.proc_data;

    if actor_euid == target_proc.uid() || actor_euid == target_proc.euid() {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
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

fn linux_policy_from_state(state: SchedState) -> i32 {
    let base = match state.class {
        SchedClass::Normal => SCHED_NORMAL as i32,
        SchedClass::Batch => SCHED_BATCH as i32,
        SchedClass::Idle => SCHED_IDLE as i32,
        SchedClass::Fifo => SCHED_FIFO as i32,
        SchedClass::RoundRobin => SCHED_RR as i32,
        SchedClass::Deadline => SCHED_DEADLINE as i32,
    };

    if state.reset_on_fork {
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
        SchedClass::Fifo | SchedClass::RoundRobin | SchedClass::Deadline => 0,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => state.nice as i32,
    }
}

fn clear_deadline_state(state: &mut SchedState) {
    state.dl_runtime = 0;
    state.dl_deadline = 0;
    state.dl_period = 0;
}

fn validate_deadline_attr(attr: &SchedAttr) -> AxResult<()> {
    if attr.sched_priority != 0
        || attr.sched_runtime == 0
        || attr.sched_deadline == 0
        || attr.sched_runtime > attr.sched_deadline
        || attr.sched_deadline > attr.sched_period
    {
        return Err(AxError::InvalidInput);
    }
    Ok(())
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

fn ioprio_class(ioprio: u32) -> u32 {
    ioprio >> IOPRIO_CLASS_SHIFT
}

fn ioprio_level(ioprio: u32) -> u32 {
    ioprio & IOPRIO_PRIO_MASK
}

fn ioprio_value(class: u32, level: u32) -> u32 {
    (class << IOPRIO_CLASS_SHIFT) | level
}

fn validate_ioprio(ioprio: u32) -> AxResult<()> {
    let class = ioprio_class(ioprio);
    let level = ioprio_level(ioprio);
    match class {
        IOPRIO_CLASS_RT => {
            if !has_ioprio_realtime_capability() {
                return Err(AxError::OperationNotPermitted);
            }
            if level >= IOPRIO_NR_LEVELS {
                return Err(AxError::InvalidInput);
            }
            Ok(())
        }
        IOPRIO_CLASS_BE => {
            if level < IOPRIO_NR_LEVELS {
                Ok(())
            } else {
                Err(AxError::InvalidInput)
            }
        }
        IOPRIO_CLASS_IDLE => Ok(()),
        IOPRIO_CLASS_NONE => {
            if level == 0 {
                Ok(())
            } else {
                Err(AxError::InvalidInput)
            }
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn ioprio_from_nice(nice: i8) -> u32 {
    ioprio_value(IOPRIO_CLASS_BE, ((nice as i32 + 20) / 5) as u32)
}

fn effective_ioprio_for_task(task: &AxTaskRef) -> u32 {
    let raw = task.as_thread().proc_data.ioprio();
    if ioprio_class(raw) == IOPRIO_CLASS_NONE {
        ioprio_from_nice(sched_state(task).nice)
    } else {
        raw
    }
}

fn raw_ioprio_for_task(task: &AxTaskRef) -> u32 {
    task.as_thread().proc_data.ioprio()
}

fn ioprio_best(current: u32, candidate: u32) -> u32 {
    if candidate < current {
        candidate
    } else {
        current
    }
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
    if set_sched_state(task, state) {
        Ok(0)
    } else {
        Err(AxError::NoSuchProcess)
    }
}

fn update_sched_policy(task: &AxTaskRef, policy: i32, priority: i32) -> AxResult<isize> {
    let reset_on_fork = policy & SCHED_RESET_ON_FORK as i32 != 0;
    let class = sched_class_from_policy(policy & !(SCHED_RESET_ON_FORK as i32))?;
    can_manage_sched_target(task)?;
    let mut state = sched_state(task);
    state.class = class;
    match class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            if !has_sched_admin_capability() {
                return Err(AxError::OperationNotPermitted);
            }
            state.rt_priority = validate_rt_priority(priority)?;
            state.nice = 0;
            clear_deadline_state(&mut state);
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            state.rt_priority = validate_static_priority(priority)?;
            if matches!(class, SchedClass::Idle) {
                state.nice = 19;
            }
            clear_deadline_state(&mut state);
        }
        SchedClass::Deadline => {
            return Err(AxError::InvalidInput);
        }
    }
    state.reset_on_fork = reset_on_fork;
    apply_sched_state(task, state)
}

fn update_sched_param(task: &AxTaskRef, priority: i32) -> AxResult<isize> {
    can_manage_sched_target(task)?;
    let mut state = sched_state(task);
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            if !has_sched_admin_capability() {
                return Err(AxError::OperationNotPermitted);
            }
            state.rt_priority = validate_rt_priority(priority)?;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            validate_static_priority(priority)?;
            state.rt_priority = 0;
            clear_deadline_state(&mut state);
        }
        SchedClass::Deadline => {
            validate_static_priority(priority)?;
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

fn can_adjust_task_nice(task: &AxTaskRef, new_nice: i8) -> AxResult<()> {
    let actor = current();
    let actor_thread = actor.as_thread();
    let target_thread = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;

    if actor_thread.proc_data.euid() != 0
        && actor_thread.proc_data.euid() != target_thread.proc_data.uid()
        && actor_thread.proc_data.euid() != target_thread.proc_data.euid()
    {
        return Err(AxError::OperationNotPermitted);
    }

    let current_nice = sched_state(task).nice;
    if actor_thread.proc_data.euid() != 0 && new_nice < current_nice {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

fn set_task_nice(task: &AxTaskRef, new_nice: i8) -> AxResult<()> {
    can_adjust_task_nice(task, new_nice)?;

    let mut state = sched_state(task);
    state.nice = new_nice;
    apply_sched_state(task, state).map(|_| ())
}

pub fn sys_sched_yield() -> AxResult<isize> {
    axtask::yield_now();
    Ok(0)
}

fn sleep_relative(dur: TimeValue) -> TimeValue {
    debug!("sleep_impl <= {dur:?}");

    if dur.is_zero() {
        return dur;
    }
    let start = AlarmClock::Monotonic.now();
    let deadline = start.checked_add(dur).unwrap_or(Duration::MAX);

    if dur <= SHORT_RELATIVE_SLEEP_LIMIT {
        let curr = current();
        let _ = with_proc_state_hint(ProcStateHint::Interruptible, || {
            while AlarmClock::Monotonic.now() < deadline {
                if has_pending_syscall_signal(curr.as_thread()) {
                    break;
                }
                core::hint::spin_loop();
            }
        });
        let actual = AlarmClock::Monotonic.now().saturating_sub(start);
        return if actual < dur { Duration::ZERO } else { actual };
    }

    if (PRECISE_RELATIVE_SLEEP_MIN..=PRECISE_RELATIVE_SLEEP_LIMIT).contains(&dur) {
        let curr = current();
        let _ = with_proc_state_hint(ProcStateHint::Interruptible, || {
            let block_until = deadline.saturating_sub(PRECISE_RELATIVE_SLEEP_SPIN_TAIL);
            if block_until > start {
                let _ = block_on(interruptible(sleep_until_clock(
                    AlarmClock::Monotonic,
                    block_until,
                )));
            }
            while AlarmClock::Monotonic.now() < deadline {
                if has_pending_syscall_signal(curr.as_thread()) {
                    break;
                }
                core::hint::spin_loop();
            }
        });
        return AlarmClock::Monotonic.now() - start;
    }

    // We detect EINTR manually if the slept time is not enough.
    let _ = with_proc_state_hint(ProcStateHint::Interruptible, || {
        block_on(interruptible(sleep_until_clock(
            AlarmClock::Monotonic,
            deadline,
        )))
    });

    AlarmClock::Monotonic.now() - start
}

fn sleep_absolute(clock: AlarmClock, deadline: TimeValue) -> bool {
    debug!("sleep_absolute <= clock: {clock:?}, deadline: {deadline:?}");

    let _ = with_proc_state_hint(ProcStateHint::Interruptible, || {
        block_on(interruptible(sleep_until_clock(clock, deadline)))
    });
    clock.now() >= deadline
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

    let actual = sleep_relative(req);

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
        if sleep_absolute(clock, deadline) {
            Ok(0)
        } else {
            Err(AxError::Interrupted)
        }
    } else {
        let actual = sleep_relative(req);

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
    let size = cpusetsize.min(axhal::cpu_num().div_ceil(8));
    let user_mask = vm_load(user_mask, size)?;
    let mut cpu_mask = AxCpuMask::new();

    for i in 0..(size * 8).min(axhal::cpu_num()) {
        if user_mask[i / 8] & (1 << (i % 8)) != 0 {
            cpu_mask.set(i, true);
        }
    }

    if pid == 0 {
        if !axtask::set_current_affinity(cpu_mask) {
            return Err(AxError::InvalidInput);
        }
    } else {
        let task = sched_target(pid)?;
        can_manage_sched_target(&task)?;
        if !set_task_affinity(&task, cpu_mask) {
            return Err(AxError::InvalidInput);
        }
    }

    Ok(0)
}

pub fn sys_getcpu(cpu: *mut u32, node: *mut u32) -> AxResult<isize> {
    let curr = current();
    let mask = curr.cpumask();
    let mut cpu_id = axhal::percpu::this_cpu_id();
    if !mask.get(cpu_id) {
        cpu_id = (0..axhal::cpu_num())
            .find(|&candidate| mask.get(candidate))
            .ok_or(AxError::InvalidInput)?;
    }
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
    Ok(linux_policy_from_state(sched_state(&task)) as isize)
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

    let class = sched_class_from_policy(attr.sched_policy as i32)?;
    let task = sched_target(pid)?;
    can_manage_sched_target(&task)?;
    let mut state = sched_state(&task);
    state.class = class;
    match class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            if attr.sched_runtime != 0 || attr.sched_deadline != 0 || attr.sched_period != 0 {
                return Err(AxError::InvalidInput);
            }
            if !has_sched_admin_capability() {
                return Err(AxError::OperationNotPermitted);
            }
            state.rt_priority = validate_rt_priority(attr.sched_priority as i32)?;
            state.nice = 0;
            clear_deadline_state(&mut state);
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            if attr.sched_runtime != 0 || attr.sched_deadline != 0 || attr.sched_period != 0 {
                return Err(AxError::InvalidInput);
            }
            state.rt_priority = validate_static_priority(attr.sched_priority as i32)?;
            state.nice = validate_nice(attr.sched_nice)?;
            clear_deadline_state(&mut state);
        }
        SchedClass::Deadline => {
            if !has_sched_admin_capability() {
                return Err(AxError::OperationNotPermitted);
            }
            validate_deadline_attr(&attr)?;
            state.rt_priority = 0;
            state.nice = 0;
            state.dl_runtime = attr.sched_runtime;
            state.dl_deadline = attr.sched_deadline;
            state.dl_period = attr.sched_period;
        }
    }
    state.reset_on_fork = attr.sched_flags & SUPPORTED_SCHED_ATTR_FLAGS != 0;
    apply_sched_state(&task, state)
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
        sched_policy: linux_policy_from_state(state) as u32 & !(SCHED_RESET_ON_FORK as u32),
        sched_flags: if state.reset_on_fork {
            SUPPORTED_SCHED_ATTR_FLAGS
        } else {
            0
        },
        sched_nice: state_nice(state),
        sched_priority: state_static_priority(state) as u32,
        sched_runtime: state.dl_runtime,
        sched_deadline: state.dl_deadline,
        sched_period: state.dl_period,
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
            Ok(raw_priority_from_nice(min_nice_for_threads(
                group
                    .processes()
                    .into_iter()
                    .flat_map(|proc| proc.threads()),
            )?))
        }
        PRIO_USER => {
            let uid = if who == 0 {
                current().as_thread().proc_data.uid()
            } else {
                who
            };
            Ok(raw_priority_from_nice(min_nice_for_threads(
                processes()
                    .into_iter()
                    .filter(|proc_data| proc_data.uid() == uid || proc_data.euid() == uid)
                    .flat_map(|proc_data| proc_data.proc.threads()),
            )?))
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_setpriority(which: u32, who: u32, prio: i32) -> AxResult<isize> {
    debug!("sys_setpriority <= which: {which}, who: {who}, prio: {prio}");

    let new_nice = clamp_nice(prio);
    let targets: Vec<AxTaskRef> = match which {
        PRIO_PROCESS => {
            if who == 0 {
                vec![current().clone()]
            } else {
                vec![get_task(who)?]
            }
        }
        PRIO_PGRP => {
            let pgid = if who == 0 {
                current().as_thread().proc_data.proc.group().pgid()
            } else {
                who
            };
            let group = get_process_group(pgid)?;
            group
                .processes()
                .into_iter()
                .flat_map(|proc| proc.threads())
                .filter_map(|tid| get_task(tid).ok())
                .collect()
        }
        PRIO_USER => {
            let uid = if who == 0 {
                current().as_thread().proc_data.uid()
            } else {
                who
            };
            processes()
                .into_iter()
                .filter(|proc_data| proc_data.uid() == uid || proc_data.euid() == uid)
                .flat_map(|proc_data| proc_data.proc.threads())
                .filter_map(|tid| get_task(tid).ok())
                .collect()
        }
        _ => return Err(AxError::InvalidInput),
    };

    if targets.is_empty() {
        return Err(AxError::NoSuchProcess);
    }

    for task in &targets {
        set_task_nice(task, new_nice)?;
    }

    Ok(0)
}

pub fn sys_ioprio_get(which: u32, who: u32) -> AxResult<isize> {
    debug!("sys_ioprio_get <= which: {which}, who: {who}");

    match which {
        IOPRIO_WHO_PROCESS => {
            let task = if who == 0 {
                current().clone()
            } else {
                get_task(who)?
            };
            Ok(raw_ioprio_for_task(&task) as isize)
        }
        IOPRIO_WHO_PGRP => {
            let pgid = if who == 0 {
                current().as_thread().proc_data.proc.group().pgid()
            } else {
                who
            };
            let group = get_process_group(pgid)?;
            let mut best = None;
            for task in group
                .processes()
                .into_iter()
                .flat_map(|proc| proc.threads())
                .filter_map(|tid| get_task(tid).ok())
            {
                let prio = effective_ioprio_for_task(&task);
                best = Some(best.map_or(prio, |current| ioprio_best(current, prio)));
            }
            best.map(|prio| prio as isize).ok_or(AxError::NoSuchProcess)
        }
        IOPRIO_WHO_USER => {
            let uid = if who == 0 {
                current().as_thread().proc_data.uid()
            } else {
                who
            };
            let mut best = None;
            for task in processes()
                .into_iter()
                .filter(|proc_data| proc_data.uid() == uid)
                .flat_map(|proc_data| proc_data.proc.threads())
                .filter_map(|tid| get_task(tid).ok())
            {
                let prio = effective_ioprio_for_task(&task);
                best = Some(best.map_or(prio, |current| ioprio_best(current, prio)));
            }
            best.map(|prio| prio as isize).ok_or(AxError::NoSuchProcess)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_ioprio_set(which: u32, who: u32, ioprio: u32) -> AxResult<isize> {
    debug!("sys_ioprio_set <= which: {which}, who: {who}, ioprio: {ioprio}");

    validate_ioprio(ioprio)?;
    let targets: Vec<AxTaskRef> = match which {
        IOPRIO_WHO_PROCESS => {
            if who == 0 {
                vec![current().clone()]
            } else {
                vec![get_task(who)?]
            }
        }
        IOPRIO_WHO_PGRP => {
            let pgid = if who == 0 {
                current().as_thread().proc_data.proc.group().pgid()
            } else {
                who
            };
            let group = get_process_group(pgid)?;
            group
                .processes()
                .into_iter()
                .flat_map(|proc| proc.threads())
                .filter_map(|tid| get_task(tid).ok())
                .collect()
        }
        IOPRIO_WHO_USER => {
            let uid = if who == 0 {
                current().as_thread().proc_data.uid()
            } else {
                who
            };
            processes()
                .into_iter()
                .filter(|proc_data| proc_data.uid() == uid)
                .flat_map(|proc_data| proc_data.proc.threads())
                .filter_map(|tid| get_task(tid).ok())
                .collect()
        }
        _ => return Err(AxError::InvalidInput),
    };

    if targets.is_empty() {
        return Err(AxError::NoSuchProcess);
    }

    for task in &targets {
        can_manage_sched_target(task)?;
    }
    for task in targets {
        task.as_thread().proc_data.set_ioprio(ioprio);
    }

    Ok(0)
}
