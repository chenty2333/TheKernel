use axerrno::{AxError, AxResult};
use axhal::time::{
    NANOS_PER_SEC, TimeValue, monotonic_time, monotonic_time_nanos, nanos_to_ticks, wall_time,
    wall_time_nanos,
};
use axtask::current;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC,
    CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
    CLOCK_REALTIME_ALARM, CLOCK_REALTIME_COARSE, CLOCK_TAI, CLOCK_THREAD_CPUTIME_ID, itimerval,
    timespec, timeval,
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{AsThread, ITimerType},
    time::TimeValueLike,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClockDomain {
    Realtime,
    RealtimeCoarse,
    Monotonic,
    MonotonicCoarse,
    ProcessCpu,
    ThreadCpu,
    Tai,
}

const DEFAULT_TAI_OFFSET_SECS: u64 = 37;

fn clock_domain(clock_id: __kernel_clockid_t) -> AxResult<ClockDomain> {
    match clock_id as u32 {
        CLOCK_REALTIME | CLOCK_REALTIME_ALARM => Ok(ClockDomain::Realtime),
        CLOCK_REALTIME_COARSE => Ok(ClockDomain::RealtimeCoarse),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => {
            Ok(ClockDomain::Monotonic)
        }
        CLOCK_MONOTONIC_COARSE => Ok(ClockDomain::MonotonicCoarse),
        CLOCK_PROCESS_CPUTIME_ID => Ok(ClockDomain::ProcessCpu),
        CLOCK_THREAD_CPUTIME_ID => Ok(ClockDomain::ThreadCpu),
        CLOCK_TAI => Ok(ClockDomain::Tai),
        _ => Err(AxError::InvalidInput),
    }
}

fn fine_clock_resolution() -> TimeValue {
    // Linux reports 1ns resolution for high-resolution realtime/monotonic
    // clocks even when the underlying timer tick is coarser.
    TimeValue::from_nanos(1)
}

fn clock_now(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    match clock_domain(clock_id)? {
        ClockDomain::Realtime => Ok(wall_time()),
        ClockDomain::RealtimeCoarse => Ok(quantize_clock_reading(
            wall_time(),
            coarse_clock_resolution(),
        )),
        ClockDomain::Monotonic => Ok(monotonic_time()),
        ClockDomain::MonotonicCoarse => Ok(quantize_clock_reading(
            monotonic_time(),
            coarse_clock_resolution(),
        )),
        ClockDomain::ProcessCpu => {
            let usage = current().as_thread().proc_data.self_usage();
            Ok(usage.utime() + usage.stime())
        }
        ClockDomain::ThreadCpu => {
            let (utime, stime) = current().as_thread().time.borrow().output();
            Ok(utime + stime)
        }
        ClockDomain::Tai => Ok(TimeValue::from_nanos(
            wall_time_nanos() + DEFAULT_TAI_OFFSET_SECS * NANOS_PER_SEC,
        )),
    }
}

fn coarse_clock_resolution() -> TimeValue {
    TimeValue::from_nanos((NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64).max(1))
}

fn quantize_clock_reading(now: TimeValue, resolution: TimeValue) -> TimeValue {
    let resolution_ns = resolution.as_nanos() as u64;
    let now_ns = now.as_nanos() as u64;
    TimeValue::from_nanos(now_ns - (now_ns % resolution_ns))
}

fn clock_resolution(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    match clock_domain(clock_id)? {
        ClockDomain::RealtimeCoarse | ClockDomain::MonotonicCoarse => Ok(coarse_clock_resolution()),
        ClockDomain::Realtime
        | ClockDomain::Monotonic
        | ClockDomain::ProcessCpu
        | ClockDomain::ThreadCpu
        | ClockDomain::Tai => Ok(fine_clock_resolution()),
    }
}

pub fn sys_clock_gettime(clock_id: __kernel_clockid_t, ts: *mut timespec) -> AxResult<isize> {
    let now = clock_now(clock_id)?;
    ts.vm_write(timespec::from_time_value(now))?;
    Ok(0)
}

pub fn sys_gettimeofday(ts: *mut timeval) -> AxResult<isize> {
    ts.vm_write(timeval::from_time_value(wall_time()))?;
    Ok(0)
}

pub fn sys_clock_getres(clock_id: __kernel_clockid_t, res: *mut timespec) -> AxResult<isize> {
    let resolution = clock_resolution(clock_id)?;
    if let Some(res) = res.nullable() {
        res.vm_write(timespec::from_time_value(resolution))?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{
        CLOCK_BOOTTIME_ALARM, CLOCK_REALTIME_ALARM, CLOCK_TAI, MAX_CLOCKS,
    };

    use super::*;

    #[test]
    fn clock_domain_accepts_supported_ids() {
        assert_eq!(clock_domain(CLOCK_REALTIME as _), Ok(ClockDomain::Realtime));
        assert_eq!(
            clock_domain(CLOCK_REALTIME_COARSE as _),
            Ok(ClockDomain::RealtimeCoarse)
        );
        assert_eq!(
            clock_domain(CLOCK_REALTIME_ALARM as _),
            Ok(ClockDomain::Realtime)
        );
        assert_eq!(
            clock_domain(CLOCK_MONOTONIC as _),
            Ok(ClockDomain::Monotonic)
        );
        assert_eq!(
            clock_domain(CLOCK_MONOTONIC_RAW as _),
            Ok(ClockDomain::Monotonic)
        );
        assert_eq!(
            clock_domain(CLOCK_MONOTONIC_COARSE as _),
            Ok(ClockDomain::MonotonicCoarse)
        );
        assert_eq!(
            clock_domain(CLOCK_BOOTTIME as _),
            Ok(ClockDomain::Monotonic)
        );
        assert_eq!(
            clock_domain(CLOCK_BOOTTIME_ALARM as _),
            Ok(ClockDomain::Monotonic)
        );
        assert_eq!(
            clock_domain(CLOCK_PROCESS_CPUTIME_ID as _),
            Ok(ClockDomain::ProcessCpu)
        );
        assert_eq!(
            clock_domain(CLOCK_THREAD_CPUTIME_ID as _),
            Ok(ClockDomain::ThreadCpu)
        );
        assert_eq!(clock_domain(CLOCK_TAI as _), Ok(ClockDomain::Tai));
    }

    #[test]
    fn clock_domain_rejects_invalid_ids() {
        assert_eq!(clock_domain(-1), Err(AxError::InvalidInput));
        assert_eq!(clock_domain(MAX_CLOCKS as _), Err(AxError::InvalidInput));
        assert_eq!(
            clock_domain((MAX_CLOCKS + 1) as _),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn clock_resolution_requires_supported_clock() {
        assert_eq!(
            clock_resolution(CLOCK_REALTIME as _),
            Ok(fine_clock_resolution())
        );
        assert_eq!(
            clock_resolution(CLOCK_REALTIME_COARSE as _),
            Ok(coarse_clock_resolution())
        );
        assert_eq!(clock_resolution(-1), Err(AxError::InvalidInput));
    }

    #[test]
    fn quantized_clock_readings_snap_to_resolution() {
        assert_eq!(
            quantize_clock_reading(TimeValue::from_nanos(123_456_789), TimeValue::from_nanos(10)),
            TimeValue::from_nanos(123_456_780)
        );
        assert_eq!(
            quantize_clock_reading(TimeValue::from_nanos(123_456_789), TimeValue::from_nanos(1)),
            TimeValue::from_nanos(123_456_789)
        );
    }
}

#[repr(C)]
pub struct Tms {
    /// user time
    tms_utime: usize,
    /// system time
    tms_stime: usize,
    /// user time of children
    tms_cutime: usize,
    /// system time of children
    tms_cstime: usize,
}

pub fn sys_times(tms: *mut Tms) -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let self_usage = proc_data.self_usage();
    let child_usage = proc_data.children_usage();
    tms.vm_write(Tms {
        tms_utime: self_usage.utime_ticks() as usize,
        tms_stime: self_usage.stime_ticks() as usize,
        tms_cutime: child_usage.utime_ticks() as usize,
        tms_cstime: child_usage.stime_ticks() as usize,
    })?;
    Ok(nanos_to_ticks(monotonic_time_nanos()) as _)
}

pub fn sys_getitimer(which: i32, value: *mut itimerval) -> AxResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(AxError::InvalidInput)?;
    let (it_interval, it_value) = current().as_thread().time.borrow().get_itimer(ty);

    value.vm_write(itimerval {
        it_interval: timeval::from_time_value(it_interval),
        it_value: timeval::from_time_value(it_value),
    })?;
    Ok(0)
}

pub fn sys_setitimer(
    which: i32,
    new_value: *const itimerval,
    old_value: *mut itimerval,
) -> AxResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(AxError::InvalidInput)?;
    let curr = current();

    let (interval, remained) = match new_value.nullable() {
        Some(new_value) => {
            // FIXME: AnyBitPattern
            let new_value = unsafe { new_value.vm_read_uninit()?.assume_init() };
            (
                new_value.it_interval.try_into_time_value()?.as_nanos() as usize,
                new_value.it_value.try_into_time_value()?.as_nanos() as usize,
            )
        }
        None => (0, 0),
    };

    debug!("sys_setitimer <= type: {ty:?}, interval: {interval:?}, remained: {remained:?}");

    let old = curr
        .as_thread()
        .time
        .borrow_mut()
        .set_itimer(ty, interval, remained);

    if let Some(old_value) = old_value.nullable() {
        old_value.vm_write(itimerval {
            it_interval: timeval::from_time_value(old.0),
            it_value: timeval::from_time_value(old.1),
        })?;
    }
    Ok(0)
}
