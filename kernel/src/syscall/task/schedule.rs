use core::time::Duration;

use axerrno::{AxError, AxResult};
use axhal::time::TimeValue;
use axtask::{
    AxCpuMask, current,
    future::{block_on, interruptible},
};
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_REALTIME, PRIO_PGRP, PRIO_PROCESS,
    PRIO_USER, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_FLAG_RESET_ON_FORK, SCHED_IDLE,
    SCHED_NORMAL, SCHED_RESET_ON_FORK, SCHED_RR, TIMER_ABSTIME, timespec,
};
use starry_vm::{VmMutPtr, VmPtr, vm_load, vm_write_slice};

use crate::{
    task::{AlarmClock, AsThread, get_process_data, get_process_group, get_task, sleep_until_clock},
    time::TimeValueLike,
};

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

fn validate_sched_policy(policy: i32) -> AxResult<i32> {
    match policy as u32 {
        SCHED_NORMAL | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => {
            Ok(policy)
        }
        _ => Err(AxError::InvalidInput),
    }
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
        get_task(pid as u32)?.cpumask()
    };
    let mask_bytes = mask.as_bytes();

    vm_write_slice(user_mask, mask_bytes)?;

    Ok(mask_bytes.len() as _)
}

pub fn sys_sched_setaffinity(
    pid: i32,
    cpusetsize: usize,
    user_mask: *const u8,
) -> AxResult<isize> {
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
        get_task(pid as u32)?.set_cpumask(cpu_mask);
    }

    Ok(0)
}

pub fn sys_sched_getscheduler(pid: i32) -> AxResult<isize> {
    let task = get_task(pid as u32)?;
    Ok(task.as_thread().sched_policy() as _)
}

pub fn sys_sched_setscheduler(
    pid: i32,
    policy: i32,
    param: *const SchedParam,
) -> AxResult<isize> {
    let policy = validate_sched_policy(policy & !(SCHED_RESET_ON_FORK as i32))?;

    let priority = unsafe { param.vm_read_uninit()?.assume_init() }.sched_priority;
    let task = get_task(pid as u32)?;
    let thread = task.as_thread();
    thread.set_sched_policy(policy);
    thread.set_sched_priority(priority);
    Ok(0)
}

pub fn sys_sched_getparam(pid: i32, param: *mut SchedParam) -> AxResult<isize> {
    let task = get_task(pid as u32)?;
    param.vm_write(SchedParam {
        sched_priority: task.as_thread().sched_priority(),
    })?;
    Ok(0)
}

pub fn sys_sched_setattr(pid: i32, attr: *const SchedAttr, flags: u32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let attr = unsafe { attr.vm_read_uninit()?.assume_init() };
    let policy = validate_sched_policy(attr.sched_policy as i32)?;
    let reset_on_fork = attr.sched_flags & SCHED_FLAG_RESET_ON_FORK as u64 != 0;

    let task = get_task(pid as u32)?;
    let thread = task.as_thread();
    thread.set_sched_policy(if reset_on_fork {
        policy | SCHED_RESET_ON_FORK as i32
    } else {
        policy
    });
    thread.set_sched_priority(attr.sched_priority as i32);
    Ok(0)
}

pub fn sys_sched_getattr(
    pid: i32,
    attr: *mut SchedAttr,
    size: u32,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let out_size = size as usize;
    if out_size < size_of::<u32>() {
        return Err(AxError::InvalidInput);
    }

    let task = get_task(pid as u32)?;
    let thread = task.as_thread();
    let policy = thread.sched_policy();
    let out = SchedAttr {
        size: size_of::<SchedAttr>() as u32,
        sched_policy: (policy & !(SCHED_RESET_ON_FORK as i32)) as u32,
        sched_flags: if policy & SCHED_RESET_ON_FORK as i32 != 0 {
            SCHED_FLAG_RESET_ON_FORK as u64
        } else {
            0
        },
        sched_nice: 0,
        sched_priority: thread.sched_priority() as u32,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
        sched_util_min: 0,
        sched_util_max: 0,
    };

    let copy_size = out_size.min(size_of::<SchedAttr>());
    vm_write_slice(
        attr.cast::<u8>(),
        &unsafe {
            core::slice::from_raw_parts(
                (&out as *const SchedAttr).cast::<u8>(),
                copy_size,
            )
        },
    )?;

    Ok(0)
}

pub fn sys_getpriority(which: u32, who: u32) -> AxResult<isize> {
    debug!("sys_getpriority <= which: {which}, who: {who}");

    match which {
        PRIO_PROCESS => {
            if who != 0 {
                let _proc = get_process_data(who)?;
            }
            Ok(20)
        }
        PRIO_PGRP => {
            if who != 0 {
                let _pg = get_process_group(who)?;
            }
            Ok(20)
        }
        PRIO_USER => {
            if who == 0 {
                Ok(20)
            } else {
                Err(AxError::NoSuchProcess)
            }
        }
        _ => Err(AxError::InvalidInput),
    }
}
