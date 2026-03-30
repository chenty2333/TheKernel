use core::time::Duration;

use axerrno::{AxError, AxResult};
use bitflags::bitflags;
use linux_raw_sys::general::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_REALTIME, TFD_CLOEXEC, TFD_NONBLOCK,
    TFD_TIMER_ABSTIME, itimerspec, timespec,
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{FileLike, add_file_like, timerfd::TimerFd},
    time::TimeValueLike,
};

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    struct TimerFdCreateFlags: u32 {
        const CLOEXEC = TFD_CLOEXEC;
        const NONBLOCK = TFD_NONBLOCK;
    }
}

fn validate_clockid(clockid: i32) -> AxResult<crate::file::timerfd::TimerClock> {
    match clockid as u32 {
        CLOCK_REALTIME => Ok(crate::file::timerfd::TimerClock::Realtime),
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => Ok(crate::file::timerfd::TimerClock::Monotonic),
        _ => Err(AxError::InvalidInput),
    }
}

fn itimerspec_to_durations(its: &itimerspec) -> AxResult<(Duration, Duration)> {
    let interval = its.it_interval.try_into_time_value()?;
    let value = its.it_value.try_into_time_value()?;
    Ok((
        Duration::new(interval.as_secs(), interval.subsec_nanos()),
        Duration::new(value.as_secs(), value.subsec_nanos()),
    ))
}

fn duration_to_timespec(d: Duration) -> timespec {
    timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    }
}

pub fn sys_timerfd_create(clockid: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_timerfd_create <= clockid: {clockid}, flags: {flags:#x}");

    let clock = validate_clockid(clockid)?;
    let flags = TimerFdCreateFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;

    let tfd = TimerFd::new(clock);
    tfd.set_nonblocking(flags.contains(TimerFdCreateFlags::NONBLOCK))?;
    add_file_like(tfd as _, flags.contains(TimerFdCreateFlags::CLOEXEC)).map(|fd| fd as _)
}

pub fn sys_timerfd_settime(
    fd: i32,
    flags: i32,
    new_value: *const itimerspec,
    old_value: *mut itimerspec,
) -> AxResult<isize> {
    debug!("sys_timerfd_settime <= fd: {fd}, flags: {flags}");

    let flags = flags as u32;
    if flags & !TFD_TIMER_ABSTIME != 0 {
        return Err(AxError::InvalidInput);
    }
    let absolute = (flags & TFD_TIMER_ABSTIME) != 0;

    let new_value = unsafe { new_value.vm_read_uninit()?.assume_init() };
    let (interval, value) = itimerspec_to_durations(&new_value)?;

    let tfd = TimerFd::from_fd(fd)?;
    let (old_interval, old_value_dur) = tfd.settime(absolute, interval, value);

    if let Some(old_value) = old_value.nullable() {
        old_value.vm_write(itimerspec {
            it_interval: duration_to_timespec(old_interval),
            it_value: duration_to_timespec(old_value_dur),
        })?;
    }

    Ok(0)
}

pub fn sys_timerfd_gettime(fd: i32, curr_value: *mut itimerspec) -> AxResult<isize> {
    debug!("sys_timerfd_gettime <= fd: {fd}");

    let tfd = TimerFd::from_fd(fd)?;
    let (interval, value) = tfd.gettime();

    curr_value.vm_write(itimerspec {
        it_interval: duration_to_timespec(interval),
        it_value: duration_to_timespec(value),
    })?;

    Ok(0)
}
