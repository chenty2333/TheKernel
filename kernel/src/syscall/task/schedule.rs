use core::{mem::size_of, time::Duration};

use axerrno::{AxError, AxResult};
use axhal::time::{NANOS_PER_SEC, TimeValue};
use axtask::{
    AxCpuMask, AxTaskRef, RR_TIMESLICE_TICKS, RT_PRIORITY_MAX, RT_PRIORITY_MIN, SchedClass,
    SchedState, current,
    future::{block_on, interruptible},
    sched_state, set_sched_state,
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_REALTIME, PRIO_PGRP, PRIO_PROCESS,
    PRIO_USER, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE,
    SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, TIMER_ABSTIME, timespec,
};
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use crate::{
    task::{AlarmClock, AsThread, get_process_group, get_task, processes, sleep_until_clock},
    time::TimeValueLike,
};

const SUPPORTED_SCHED_ATTR_FLAGS: u64 = SCHED_FLAG_RESET_ON_FORK as u64;

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
    get_task(pid as Pid)
}

fn sched_class_from_policy(policy: i32) -> AxResult<SchedClass> {
    match policy as u32 {
        SCHED_NORMAL => Ok(SchedClass::Normal),
        SCHED_BATCH => Ok(SchedClass::Batch),
        SCHED_IDLE => Ok(SchedClass::Idle),
        SCHED_FIFO => Ok(SchedClass::Fifo),
        SCHED_RR => Ok(SchedClass::RoundRobin),
        SCHED_DEADLINE => Err(AxError::InvalidInput),
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_static_priority(priority: i32) -> AxResult<u8> {
    if priority == 0 { Ok(0) } else { Err(AxError::InvalidInput) }
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
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => 0,
    }
}

fn state_nice(state: SchedState) -> i32 {
    match state.class {
        SchedClass::Fifo | SchedClass::RoundRobin => 0,
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => state.nice as i32,
    }
}

fn raw_priority_from_nice(nice: i8) -> isize {
    20 - nice as isize
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
    state.reset_on_fork = reset_on_fork;
    apply_sched_state(task, state)
}

fn update_sched_param(task: &AxTaskRef, priority: i32) -> AxResult<isize> {
    let mut state = sched_state(task);
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
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE => Ok((0, 0)),
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

pub fn sys_sched_yield() -> AxResult<isize> {
    axtask::yield_now();
    Ok(0)
}

fn sleep_relative(dur: TimeValue) -> TimeValue {
    debug!("sleep_impl <= {dur:?}");

    let start = AlarmClock::Monotonic.now();
    let deadline = start.checked_add(dur).unwrap_or(Duration::MAX);

    // We detect EINTR manually if the slept time is not enough.
    let _ = block_on(interruptible(sleep_until_clock(
        AlarmClock::Monotonic,
        deadline,
    )));

    AlarmClock::Monotonic.now() - start
}

fn sleep_absolute(clock: AlarmClock, deadline: TimeValue) -> bool {
    debug!("sleep_absolute <= clock: {clock:?}, deadline: {deadline:?}");

    let _ = block_on(interruptible(sleep_until_clock(clock, deadline)));
    clock.now() >= deadline
}

/// Sleep some nanoseconds
pub fn sys_nanosleep(req: *const timespec, rem: *mut timespec) -> AxResult<isize> {
    // FIXME: AnyBitPattern
    let req = unsafe { req.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_nanosleep <= req: {req:?}");

    let actual = sleep_relative(req);

    if let Some(diff) = req.checked_sub(actual) {
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
        _ => {
            warn!("Unsupported clock_id: {clock_id}");
            return Err(AxError::InvalidInput);
        }
    };

    let req = unsafe { req.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
    debug!("sys_clock_nanosleep <= clock_id: {clock_id}, flags: {flags}, req: {req:?}");

    if flags & TIMER_ABSTIME != 0 {
        if sleep_absolute(clock, req) {
            Ok(0)
        } else {
            Err(AxError::Interrupted)
        }
    } else {
        let actual = sleep_relative(req);

        if let Some(diff) = req.checked_sub(actual) {
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

    let mask = if pid == 0 {
        current().cpumask()
    } else {
        sched_target(pid)?.cpumask()
    };
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
        axtask::set_current_affinity(cpu_mask);
    } else {
        if cpu_mask.is_empty() {
            return Err(AxError::InvalidInput);
        }
        sched_target(pid)?.set_cpumask(cpu_mask);
    }

    Ok(0)
}

pub fn sys_sched_getscheduler(pid: i32) -> AxResult<isize> {
    let task = sched_target(pid)?;
    Ok(linux_policy_from_state(sched_state(&task)) as isize)
}

pub fn sys_sched_setparam(pid: i32, param: *const SchedParam) -> AxResult<isize> {
    let priority = unsafe { param.vm_read_uninit()?.assume_init() }.sched_priority;
    let task = sched_target(pid)?;
    update_sched_param(&task, priority)
}

pub fn sys_sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> AxResult<isize> {
    let priority = unsafe { param.vm_read_uninit()?.assume_init() }.sched_priority;
    let task = sched_target(pid)?;
    update_sched_policy(&task, policy, priority)
}

pub fn sys_sched_getparam(pid: i32, param: *mut SchedParam) -> AxResult<isize> {
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
    let _ = sched_target(pid)?;
    let rr_quantum = Duration::from_nanos(
        RR_TIMESLICE_TICKS as u64 * (NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64),
    );
    interval.vm_write(timespec::from_time_value(rr_quantum))?;
    Ok(0)
}

pub fn sys_sched_setattr(pid: i32, attr: *const SchedAttr, flags: u32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let attr = unsafe { attr.vm_read_uninit()?.assume_init() };
    if attr.sched_flags & !SUPPORTED_SCHED_ATTR_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if attr.sched_runtime != 0
        || attr.sched_deadline != 0
        || attr.sched_period != 0
        || attr.sched_util_min != 0
        || attr.sched_util_max != 0
    {
        return Err(AxError::InvalidInput);
    }

    let class = sched_class_from_policy(attr.sched_policy as i32)?;
    let task = sched_target(pid)?;
    let mut state = sched_state(&task);
    state.class = class;
    match class {
        SchedClass::Fifo | SchedClass::RoundRobin => {
            state.rt_priority = validate_rt_priority(attr.sched_priority as i32)?;
            state.nice = 0;
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            state.rt_priority = validate_static_priority(attr.sched_priority as i32)?;
            state.nice = validate_nice(attr.sched_nice)?;
        }
    }
    state.reset_on_fork = attr.sched_flags & SUPPORTED_SCHED_ATTR_FLAGS != 0;
    apply_sched_state(&task, state)
}

pub fn sys_sched_getattr(pid: i32, attr: *mut SchedAttr, size: u32, flags: u32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let out_size = size as usize;
    if out_size < size_of::<u32>() {
        return Err(AxError::InvalidInput);
    }

    let task = sched_target(pid)?;
    let state = sched_state(&task);
    let out = SchedAttr {
        size: size_of::<SchedAttr>() as u32,
        sched_policy: linux_policy_from_state(state) as u32 & !(SCHED_RESET_ON_FORK as u32),
        sched_flags: if state.reset_on_fork {
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
            if who != 0 {
                return Err(AxError::NoSuchProcess);
            }
            Ok(raw_priority_from_nice(min_nice_for_threads(
                processes()
                    .into_iter()
                    .flat_map(|proc_data| proc_data.proc.threads()),
            )?))
        }
        _ => Err(AxError::InvalidInput),
    }
}
