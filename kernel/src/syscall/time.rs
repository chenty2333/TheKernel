use core::{
    mem::{align_of, offset_of, size_of},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time, monotonic_time_nanos};
use axtask::current;
use kernel_guard::NoPreemptIrqSave;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    __kernel_clockid_t, __kernel_old_time_t, CAP_SYS_TIME, CAP_WAKE_ALARM, CLOCK_BOOTTIME,
    CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW,
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_ALARM, CLOCK_REALTIME_COARSE,
    CLOCK_TAI, CLOCK_THREAD_CPUTIME_ID, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD, SIGEV_THREAD_ID,
    TIMER_ABSTIME, itimerspec, itimerval, timespec, timeval, timezone,
};
use tk_linux_signal::Signo;
use tk_linux_time as linux_time;
use tk_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    mm::map_usercopy_error,
    syscall::RawSigevent,
    task::{
        AlarmClock, AlarmTokenReserveError, AsThread, ITimerType, PosixTimer, PosixTimerClock,
        PosixTimerNotify, TaskUsage, get_process_itimer, get_visible_task,
        poll_timer, refresh_posix_cpu_timer_armed, set_process_itimer, times_clock_ticks,
    },
    time::{
        SystemTimezone, TimeValueLike, set_system_timezone, set_wall_time, system_timezone,
        wall_time, wall_time_nanos,
    },
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

pub(crate) static TAI_CLOCK_CHANGES: axpoll::PollSet = axpoll::PollSet::new();

const CPUCLOCK_PROF: i32 = 0;
const CPUCLOCK_VIRT: i32 = 1;
const CPUCLOCK_SCHED: i32 = 2;
const CPUCLOCK_MAX: i32 = 3;
const CPUCLOCK_PERTHREAD_MASK: i32 = 4;
const CPUCLOCK_CLOCK_MASK: i32 = 3;
const CLOCKFD: i32 = CPUCLOCK_MAX;
const CLOCKFD_MASK: i32 = CPUCLOCK_PERTHREAD_MASK | CPUCLOCK_CLOCK_MASK;

fn clock_domain(clock_id: __kernel_clockid_t) -> AxResult<ClockDomain> {
    if let Some(clock) = decode_cpu_clock_id(clock_id) {
        return Ok(match clock.target {
            CpuClockTarget::Process(_) => ClockDomain::ProcessCpu,
            CpuClockTarget::Thread(_) => ClockDomain::ThreadCpu,
        });
    }

    if clock_id < 0 {
        return Err(AxError::InvalidInput);
    }

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
    // Fine POSIX clocks expose nanosecond timestamps and the timer subsystem can
    // program one-shot deadlines; keep tick granularity only for explicit
    // *_COARSE clocks.
    TimeValue::from_nanos(1)
}

fn clock_now(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    match clock_id as u32 {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
            let now = match clock_id as u32 {
                CLOCK_MONOTONIC_COARSE => {
                    quantize_clock_reading(monotonic_time(), coarse_clock_resolution())
                }
                _ => monotonic_time(),
            };
            // `CLOCK_MONOTONIC_RAW` is namespaced too: Linux's raw reader adds
            // the monotonic time-namespace offset exactly like the
            // non-raw one (`posix_get_monotonic_raw()`,
            // `kernel/time/posix-timers.c:222-227`, via
            // `timens_add_monotonic()`, `include/linux/time_namespace.h:73-78`).
            // "Raw" refers to the underlying clocksource being read without NTP
            // frequency/phase steering, not to the namespace.
            return Ok(current().as_thread().time_ns().apply_monotonic_offset(now));
        }
        CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => {
            return Ok(current()
                .as_thread()
                .time_ns()
                .apply_boottime_offset(monotonic_time()));
        }
        _ => {}
    }

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
        ClockDomain::ProcessCpu => cpu_clock_now(clock_id),
        ClockDomain::ThreadCpu => cpu_clock_now(clock_id),
        ClockDomain::Tai => Ok(tai_time()),
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

/// The resolution half of `clock_getres(2)`, without the CPU-clock target
/// lookup that Linux performs first: `posix_cpu_clock_getres()` only reports a
/// resolution once `validate_clock_permissions()` has resolved the encoded pid
/// (`kernel/time/posix-cpu-timers.c:57-124,159-176`), which
/// [`sys_clock_getres`] performs before calling this.
fn clock_resolution(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    // `posix_cpu_clock_getres()` reports one nanosecond for the scheduler
    // clock, which covers CLOCK_PROCESS_CPUTIME_ID/CLOCK_THREAD_CPUTIME_ID and
    // their CPUCLOCK_SCHED encodings, and the rounded-up accounting tick for
    // the encoded PROF/VIRT clocks, which are charged once per tick
    // (`kernel/time/posix-cpu-timers.c:159-176`).
    if let Some(clock) = decode_cpu_clock_id(clock_id)
        && clock.which != CPUCLOCK_SCHED
    {
        return Ok(TimeValue::from_nanos(
            linux_time::accounting_tick_resolution_nanos(axconfig::TICKS_PER_SEC as u32),
        ));
    }
    match clock_domain(clock_id)? {
        ClockDomain::RealtimeCoarse | ClockDomain::MonotonicCoarse => Ok(coarse_clock_resolution()),
        ClockDomain::Realtime
        | ClockDomain::Monotonic
        | ClockDomain::ProcessCpu
        | ClockDomain::ThreadCpu
        | ClockDomain::Tai => Ok(fine_clock_resolution()),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KernelOldTimex {
    modes: u32,
    offset: i64,
    freq: i64,
    maxerror: i64,
    esterror: i64,
    status: i32,
    constant: i64,
    precision: i64,
    tolerance: i64,
    time: timeval,
    tick: i64,
    ppsfreq: i64,
    jitter: i64,
    shift: i32,
    stabil: i64,
    jitcnt: i64,
    calcnt: i64,
    errcnt: i64,
    stbcnt: i64,
    tai: i32,
    _padding: [i32; 11],
}

/// The published Linux timex state plus the generation that absolute
/// CLOCK_TAI timers snapshot, so an `ADJ_TAI` rebase can reproject them.
///
/// The initial value is Linux's: `STA_UNSYNC`, time constant 2, both error
/// estimates at `NTP_PHASE_LIMIT`, one `tick_usec` per `USER_HZ` tick, and a
/// zero TAI offset until userspace publishes one (`ntp_init()`,
/// `kernel/time/ntp.c:90-100`; the static timekeeper is zero initialized).
#[derive(Clone, Copy, Debug)]
struct TimexState {
    version: u64,
    value: linux_time::TimexState,
}

impl TimexState {
    const fn new() -> Self {
        Self {
            version: 0,
            value: linux_time::TimexState::INITIAL,
        }
    }
}

static TIMEX_STATE: SpinNoIrq<TimexState> = SpinNoIrq::new(TimexState::new());
/// Serializes an absolute CLOCK_TAI arm with the preallocated ADJ_TAI rebase
/// transaction.  The timex lock is never held while timer owners are locked.
pub(crate) static TAI_TIMER_REBASE_GATE: axsync::Mutex<()> = axsync::Mutex::new(());

/// Returns the currently published TAI-minus-UTC offset.  `ADJ_TAI` updates
/// the same timex state observed by CLOCK_TAI and POSIX TAI timers; keeping
/// this in one accessor prevents successful clock_adjtime(2) calls from
/// leaving a stale fixed offset in the read/timer paths.
fn tai_offset_seconds() -> i64 {
    TIMEX_STATE.lock().value.tai as i64
}

/// Snapshots the TAI offset and the timex publication generation together.
/// Absolute CLOCK_TAI timers retain this pair so ADJ_TAI can reproject their
/// realtime-heap deadline without perturbing relative arms.
pub(crate) fn tai_offset_snapshot() -> (i64, u64) {
    let state = TIMEX_STATE.lock();
    (state.value.tai as i64, state.version)
}

/// `ntp_clear()` (`kernel/time/ntp.c:353-359`) as reached through the
/// `TK_CLEAR_NTP` bit of `timekeeping_update()` (`kernel/time/timekeeping.c:31-34,
/// 804-808`).
///
/// Linux publishes every new wall clock with `TK_UPDATE_ALL`, so a successful
/// `settimeofday(2)`, `clock_settime(2)` on `CLOCK_REALTIME` or
/// `timekeeping_warp_clock()` (`do_settimeofday64()`,
/// `kernel/time/timekeeping.c:1679`; `__timekeeping_inject_offset()`,
/// `kernel/time/timekeeping.c:1742`; `timekeeping_warp_clock()`,
/// `kernel/time/timekeeping.c:1786-1790`) leaves the NTP state unsynchronised:
/// `adjtimex(2)` then reports `TIME_ERROR` with `STA_UNSYNC` set.  The
/// `ADJ_SETOFFSET` case is applied by `linux_time::adjust()` itself, because
/// that call reads the cleared state back in the same syscall.
///
/// The TAI rebase gate is taken so this update cannot interleave with a
/// concurrent `ADJ_TAI` publication; `tai` is untouched here, so no timer is
/// reprojected.
pub(crate) fn clear_ntp_state() -> AxResult<()> {
    let _gate = TAI_TIMER_REBASE_GATE.lock();
    let mut state = TIMEX_STATE.lock();
    let cleared = state.value.cleared();
    if cleared == state.value {
        return Ok(());
    }
    // The version only moves when the state does, so a generation snapshot
    // taken by an absolute CLOCK_TAI timer still names this publication.
    state.version = state.version.checked_add(1).ok_or(AxError::OutOfRange)?;
    state.value = cleared;
    Ok(())
}

pub(crate) fn tai_time() -> TimeValue {
    let nanos = wall_time_nanos() as i128 + tai_offset_seconds() as i128 * NANOS_PER_SEC as i128;
    TimeValue::from_nanos(nanos.clamp(0, u64::MAX as i128) as u64)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CpuClockTarget {
    Process(u32),
    Thread(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CpuClockId {
    target: CpuClockTarget,
    which: i32,
}

fn decode_cpu_clock_id(clock_id: __kernel_clockid_t) -> Option<CpuClockId> {
    let raw = clock_id;
    if raw < 0 {
        if (raw & CLOCKFD_MASK) == CLOCKFD {
            return None;
        }
        let which = raw & CPUCLOCK_CLOCK_MASK;
        if which >= CPUCLOCK_MAX {
            return None;
        }
        let id = !(raw >> 3);
        if id < 0 {
            return None;
        }
        let id = id as u32;
        let target = if raw & CPUCLOCK_PERTHREAD_MASK != 0 {
            CpuClockTarget::Thread(id)
        } else {
            CpuClockTarget::Process(id)
        };
        return Some(CpuClockId { target, which });
    }

    match raw as u32 {
        CLOCK_PROCESS_CPUTIME_ID => Some(CpuClockId {
            target: CpuClockTarget::Process(0),
            which: CPUCLOCK_SCHED,
        }),
        CLOCK_THREAD_CPUTIME_ID => Some(CpuClockId {
            target: CpuClockTarget::Thread(0),
            which: CPUCLOCK_SCHED,
        }),
        _ => None,
    }
}

fn usage_value_for_cpu_clock(usage: TaskUsage, which: i32) -> TimeValue {
    match which {
        CPUCLOCK_PROF | CPUCLOCK_SCHED => usage.utime() + usage.stime(),
        CPUCLOCK_VIRT => usage.utime(),
        _ => TimeValue::ZERO,
    }
}

/// `posix_cpu_clock_get()`'s target half: `pid_for_clock()` plus the clock
/// resolution Linux performs on it (`kernel/time/posix-cpu-timers.c:57-105`,
/// `:132-145`).
///
/// `gettime` is Linux's second argument: `clock_gettime(2)` passes true, which
/// additionally accepts the caller's own non-leader TID as a process clock
/// (`kernel/time/posix-cpu-timers.c:87-93`); `clock_getres(2)` and
/// `clock_settime(2)` pass false and require a thread-group leader.
fn resolve_cpu_clock_usage(
    clock_id: __kernel_clockid_t,
    gettime: bool,
) -> AxResult<TaskUsage> {
    let caller = current();
    let caller = caller.as_thread();
    let clock = decode_cpu_clock_id(clock_id).ok_or(AxError::InvalidInput)?;
    match clock.target {
        // An encoded pid of 0 targets current, or the process current belongs
        // to (`pid_for_clock()`, kernel/time/posix-cpu-timers.c:68-72).
        CpuClockTarget::Process(0) => Ok(caller.proc_data.self_usage()),
        CpuClockTarget::Thread(0) => Ok(TaskUsage::from_thread(caller)),
        CpuClockTarget::Process(pid) => {
            let tid = caller
                .pid_ns()
                .resolve_visible_pid(pid)
                .ok_or(AxError::InvalidInput)?;
            let task = get_visible_task(tid).map_err(|_| AxError::InvalidInput)?;
            let target = task.as_thread();
            // Linux's process lookup requires `pid_has_task(pid, PIDTYPE_TGID)`
            // and only falls back to the caller's own task id for
            // `clock_gettime(2)` (`kernel/time/posix-cpu-timers.c:87-101`).
            if target.tid() != caller.tid() && !target.is_thread_group_leader() {
                return Err(AxError::InvalidInput);
            }
            if !gettime && target.tid() != caller.tid() {
                return Err(AxError::InvalidInput);
            }
            Ok(target.proc_data.self_usage())
        }
        CpuClockTarget::Thread(tid) => {
            let tid = caller
                .pid_ns()
                .resolve_visible_pid(tid)
                .ok_or(AxError::InvalidInput)?;
            let task = get_visible_task(tid).map_err(|_| AxError::InvalidInput)?;
            let target = task.as_thread();
            // "if (thread) ... return (tsk && same_thread_group(tsk, current))
            // ? pid : NULL" (`kernel/time/posix-cpu-timers.c:80-83`).
            if target.proc_data.proc.pid() != caller.proc_data.proc.pid() {
                return Err(AxError::InvalidInput);
            }
            Ok(TaskUsage::from_thread(target))
        }
    }
}

fn cpu_clock_now(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    let decoded = decode_cpu_clock_id(clock_id).ok_or(AxError::InvalidInput)?;
    let usage = resolve_cpu_clock_usage(clock_id, true)?;
    Ok(usage_value_for_cpu_clock(usage, decoded.which))
}

fn clock_adjtime_is_realtime(clock_id: __kernel_clockid_t) -> AxResult<bool> {
    if clock_id < 0 {
        // Linux routes every negative non-CLOCKFD id through the CPU-clock
        // implementation. Those clocks have no clock_adjtime operation, even
        // when the encoded CPU-clock id is otherwise nonsensical.
        return if (clock_id & CLOCKFD_MASK) == CLOCKFD {
            Err(AxError::InvalidInput)
        } else {
            Ok(false)
        };
    }

    match clock_domain(clock_id)? {
        // CLOCK_REALTIME_ALARM shares the realtime domain but has no
        // clock_adjtime operation.
        ClockDomain::Realtime => Ok(clock_id as u32 == CLOCK_REALTIME),
        _ => Ok(false),
    }
}

/// Fills the `struct __kernel_timex` fields `ntp_adjtimex()` overwrites
/// (`kernel/time/ntp.c:809-829`).
///
/// `modes` is deliberately left alone: Linux never writes it back, so the
/// caller keeps exactly the bits it passed.  Without `CONFIG_NTP_PPS`,
/// `pps_fill_timex()` zeroes the PPS fields (`kernel/time/ntp.c:232-244`).
fn fill_timex_output(
    timex: &mut KernelOldTimex,
    status: i32,
    output: &linux_time::TimexOutput,
    sampled: TimeValue,
) {
    timex.offset = output.offset;
    timex.freq = output.freq;
    timex.maxerror = output.maxerror;
    timex.esterror = output.esterror;
    timex.status = output.status;
    timex.constant = output.constant;
    timex.precision = output.precision;
    timex.tolerance = output.tolerance;
    // `timex.time` reports the wall clock sampled before any `ADJ_SETOFFSET`
    // discontinuity, and carries nanoseconds in `tv_usec` only in nanosecond
    // resolution (`kernel/time/ntp.c:829-831`).
    timex.time = timeval::from_time_value(sampled);
    if status & linux_time::STA_NANO != 0 {
        timex.time.tv_usec = sampled.subsec_nanos() as _;
    }
    timex.tick = output.tick;
    timex.ppsfreq = 0;
    timex.jitter = 0;
    timex.shift = 0;
    timex.stabil = 0;
    timex.jitcnt = 0;
    timex.calcnt = 0;
    timex.errcnt = 0;
    timex.stbcnt = 0;
    timex.tai = output.tai;
}

/// The `struct __kernel_timex` fields `do_adjtimex(2)` reads
/// (`kernel/time/ntp.c:728-756`).
fn timex_request(timex: &KernelOldTimex) -> linux_time::TimexRequest {
    linux_time::TimexRequest {
        modes: timex.modes,
        offset: timex.offset,
        freq: timex.freq,
        maxerror: timex.maxerror,
        esterror: timex.esterror,
        status: timex.status,
        constant: timex.constant,
        tick: timex.tick,
        time_sec: timex.time.tv_sec,
        time_usec: timex.time.tv_usec,
    }
}

/// Wake-alarm admission for every entry point that names
/// `CLOCK_REALTIME_ALARM`/`CLOCK_BOOTTIME_ALARM`.
///
/// The rule itself (`rtc_available` then `CAP_WAKE_ALARM`, or capability only
/// for `timerfd_create(2)`) lives in `tk_linux_time::admit_wake_alarm`; only
/// the errno differs between entry points, and it differs in Linux too:
/// `clock_gettime(2)`/`clock_getres(2)` report a missing RTC as EINVAL
/// (`kernel/time/alarmtimer.c:600-620`), `timer_create(2)` and
/// `clock_nanosleep(2)` as EOPNOTSUPP (`kernel/time/alarmtimer.c:651-665,766-790`),
/// and every arming path reports a missing capability as EPERM.
pub(crate) fn wake_alarm_admission(
    clock_id: __kernel_clockid_t,
    usage: linux_time::WakeAlarmUse,
) -> AxResult<()> {
    let rtc_available = crate::pseudofs::dev::rtc::is_available();
    let cap_wake_alarm = current()
        .as_thread()
        .has_effective_capability(CAP_WAKE_ALARM);
    match linux_time::admit_wake_alarm(clock_id, usage, rtc_available, cap_wake_alarm) {
        Ok(()) => Ok(()),
        Err(linux_time::WakeAlarmReject::NoRtc) => Err(match usage {
            linux_time::WakeAlarmUse::Read => AxError::InvalidInput,
            linux_time::WakeAlarmUse::Arm | linux_time::WakeAlarmUse::TimerFd => {
                AxError::OperationNotSupported
            }
        }),
        Err(linux_time::WakeAlarmReject::NotPermitted) => Err(AxError::OperationNotPermitted),
        // `admit_wake_alarm()` has no flag test, so this variant is
        // unreachable here; the arm exists so that adding one is a compile
        // error rather than a silent EINVAL on the wrong path.
        Err(linux_time::WakeAlarmReject::BadFlags) => Err(AxError::InvalidInput),
    }
}

/// Wake-alarm admission for `clock_nanosleep(2)`, which is the one entry point
/// whose clock-specific implementation validates the flags itself
/// (`alarm_timer_nsleep()`, `kernel/time/alarmtimer.c:766-790`): the missing
/// RTC is EOPNOTSUPP, an unknown flag bit is EINVAL, and a missing
/// `CAP_WAKE_ALARM` is EPERM, in that order.  Every other clock ignores flag
/// bits other than `TIMER_ABSTIME`, so this is the only place the mask is
/// checked.
pub(crate) fn wake_alarm_nanosleep_admission(
    clock_id: __kernel_clockid_t,
    flags: u32,
) -> AxResult<()> {
    let rtc_available = crate::pseudofs::dev::rtc::is_available();
    let cap_wake_alarm = current()
        .as_thread()
        .has_effective_capability(CAP_WAKE_ALARM);
    match linux_time::check_nanosleep_wake_alarm(clock_id, flags, rtc_available, cap_wake_alarm) {
        Ok(()) => Ok(()),
        Err(linux_time::WakeAlarmReject::NoRtc) => Err(AxError::OperationNotSupported),
        Err(linux_time::WakeAlarmReject::BadFlags) => Err(AxError::InvalidInput),
        Err(linux_time::WakeAlarmReject::NotPermitted) => Err(AxError::OperationNotPermitted),
    }
}

/// Maps a timex policy rejection onto the Linux errno.
///
/// `timekeeping_validate_timex()` returns `-EPERM` only for a missing
/// `CAP_SYS_TIME`; every other rejection, including the wall-clock bounds of
/// `do_settimeofday64()`, is `-EINVAL`
/// (`kernel/time/timekeeping.c:2830-2901,1665-1690`).
fn map_timex_reject(reject: linux_time::Reject) -> AxError {
    match reject {
        linux_time::Reject::NotPermitted => AxError::OperationNotPermitted,
        _ => AxError::InvalidInput,
    }
}

/// The shared `do_adjtimex()` body (`kernel/time/timekeeping.c:2930-2991`)
/// used by `adjtimex(2)` and `clock_adjtime(2)`.
///
/// The two syscalls differ only in their userspace copy-out policy, which the
/// wrappers implement.  All validation, including the `CAP_SYS_TIME` decision
/// and the `ADJ_ADJTIME`/`ADJ_OFFSET_SINGLESHOT` ordering, is
/// `linux_time::adjust()`'s, because those orderings are observable.
fn clock_adjtime_core(clock_id: __kernel_clockid_t, timex: &mut KernelOldTimex) -> AxResult<isize> {
    if !clock_adjtime_is_realtime(clock_id)? {
        return Err(AxError::OperationNotSupported);
    }
    let request = timex_request(timex);
    let authority = linux_time::TimexAuthority {
        cap_sys_time: current().as_thread().has_effective_capability(CAP_SYS_TIME),
    };
    // Linux samples the wall clock once, before the `ADJ_SETOFFSET` injection,
    // and reports that sample back in `timex.time`.
    let sampled = wall_time();

    let _tai_timer_gate = TAI_TIMER_REBASE_GATE.lock();
    let (output, result, previous_tai, next, changed) = {
        let state = TIMEX_STATE.lock();
        // `NTP_INTERVAL_FREQ` is Linux's `HZ` (`include/linux/timex.h:151`), the
        // rate that both scales and un-scales `ADJ_OFFSET`
        // (`kernel/time/ntp.c:333,806`).  TheKernel's rate is its configured
        // tick rate, not the ABI's USER_HZ.
        let update = linux_time::adjust(
            &state.value,
            &request,
            authority,
            axconfig::TICKS_PER_SEC as i64,
        )
        .map_err(map_timex_reject)?;
        let previous_tai = state.value.tai;
        let next = if update.changed {
            TimexState {
                version: state.version.checked_add(1).ok_or(AxError::OutOfRange)?,
                value: update.next,
            }
        } else {
            *state
        };
        (
            update.output,
            update.result,
            previous_tai,
            next,
            update.changed,
        )
    };

    // `ADJ_SETOFFSET` injects a wall-clock discontinuity before the modes take
    // effect (`kernel/time/timekeeping.c:2955-2966`).  The target is validated
    // against `timespec64_valid_settod()` and the CLOCK_MONOTONIC floor, so an
    // impossible injection fails without publishing the timex state.
    if request.modes & linux_time::ADJ_SETOFFSET != 0 {
        let target = wall_time().as_nanos() as i128 + linux_time::setoffset_delta_ns(&request);
        linux_time::validate_settime_target(target, monotonic_time_nanos() as i128)
            .map_err(map_timex_reject)?;
        // The bound above proves the value fits the wall-clock model.
        set_wall_time(TimeValue::from_nanos(target as u64))?;
    }

    let tai_rebase = (next.value.tai != previous_tai).then_some((next.version, next.value.tai));
    // Timex and timer owners deliberately have opposing readers: firing and
    // gettime sample the TAI state while holding a timer owner.  Prepare the
    // complete allocation budget before publication, but never hold TIMEX
    // while acquiring those owners.
    let rebase_plan = tai_rebase
        .map(|_| crate::task::prepare_tai_absolute_posix_timer_rebase())
        .transpose()?;
    if changed {
        let mut state = TIMEX_STATE.lock();
        // The TAI gate excludes another adjusting writer, so the candidate
        // built above remains current while the preflight allocation ran.
        state.version = next.version;
        state.value = next.value;
        drop(state);
    }
    if let (Some((generation, tai)), Some(plan)) = (tai_rebase, rebase_plan) {
        plan.apply(generation, tai as i64);
        TAI_CLOCK_CHANGES.wake();
    }
    drop(_tai_timer_gate);

    fill_timex_output(timex, output.status, &output, sampled);
    Ok(result as isize)
}

fn posix_timer_clock(clock_id: __kernel_clockid_t) -> AxResult<PosixTimerClock> {
    if matches!(
        clock_id as u32,
        CLOCK_MONOTONIC_RAW | CLOCK_REALTIME_COARSE | CLOCK_MONOTONIC_COARSE
    ) {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if matches!(clock_id as u32, CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM) {
        return Ok(PosixTimerClock::Boottime);
    }
    match clock_domain(clock_id)? {
        ClockDomain::Realtime | ClockDomain::RealtimeCoarse => Ok(PosixTimerClock::Realtime),
        ClockDomain::Monotonic | ClockDomain::MonotonicCoarse => Ok(PosixTimerClock::Monotonic),
        ClockDomain::ProcessCpu => Ok(PosixTimerClock::ProcessCpu),
        ClockDomain::ThreadCpu => Ok(PosixTimerClock::ThreadCpu),
        ClockDomain::Tai => Ok(PosixTimerClock::Tai),
    }
}

fn map_posix_timer_admission_error(error: AlarmTokenReserveError) -> AxError {
    match error {
        // timer_create(2) defines temporary kernel timer-resource exhaustion
        // as EAGAIN. Admission happens here, so every later rearm is
        // allocation-free with respect to the alarm registry.
        AlarmTokenReserveError::CapacityExhausted => LinuxError::EAGAIN.into(),
        AlarmTokenReserveError::TokenSpaceExhausted => AxError::OutOfRange,
    }
}

fn duration_to_timespec(d: Duration) -> timespec {
    timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
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

fn duration_from_secs(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

fn saturating_add_duration(lhs: Duration, rhs: Duration) -> Duration {
    lhs.checked_add(rhs).unwrap_or(Duration::MAX)
}

fn saturating_sub_duration(lhs: Duration, rhs: Duration) -> Duration {
    lhs.checked_sub(rhs).unwrap_or(Duration::ZERO)
}

pub(crate) fn tai_deadline_as_realtime(deadline: Duration, offset_seconds: i64) -> Duration {
    if offset_seconds >= 0 {
        saturating_sub_duration(deadline, duration_from_secs(offset_seconds as u64))
    } else {
        saturating_add_duration(deadline, duration_from_secs(offset_seconds.unsigned_abs()))
    }
}

fn decode_timer_notify(event: Option<RawSigevent>) -> AxResult<PosixTimerNotify> {
    let Some(event) = event else {
        return Ok(PosixTimerNotify::Signal {
            signo: Signo::SIGALRM,
            target_tid: None,
            value: None,
        });
    };

    match event.notify() as u32 {
        SIGEV_NONE => Ok(PosixTimerNotify::None),
        SIGEV_SIGNAL => {
            let signo = decode_sigevent_signo(event.signo())?;
            Ok(PosixTimerNotify::Signal {
                signo,
                target_tid: None,
                value: Some(event.value_ptr_address()),
            })
        }
        // Linux accepts SIGEV_THREAD and treats it exactly like SIGEV_SIGNAL
        // with thread-group delivery (`good_sigevent()`,
        // kernel/time/posix-timers.c:404-420): the C library is what turns the
        // notification into a callback thread, and the kernel only validates
        // the signal number.
        SIGEV_THREAD => {
            let signo = decode_sigevent_signo(event.signo())?;
            Ok(PosixTimerNotify::Signal {
                signo,
                target_tid: None,
                value: Some(event.value_ptr_address()),
            })
        }
        SIGEV_THREAD_ID => {
            let signo = decode_sigevent_signo(event.signo())?;
            let tid = event.thread_id();
            if tid <= 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(PosixTimerNotify::Signal {
                signo,
                target_tid: Some(tid as _),
                value: Some(event.value_ptr_address()),
            })
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn decode_sigevent_signo(raw: i32) -> AxResult<Signo> {
    let raw = u8::try_from(raw).map_err(|_| AxError::InvalidInput)?;
    Signo::from_repr(raw).ok_or(AxError::InvalidInput)
}

fn timer_absolute_deadline(clock: PosixTimerClock, value: Duration) -> AxResult<Duration> {
    Ok(match clock {
        PosixTimerClock::Realtime => value,
        PosixTimerClock::Tai => {
            let offset = tai_offset_seconds();
            if offset >= 0 {
                saturating_sub_duration(value, duration_from_secs(offset as u64))
            } else {
                saturating_add_duration(value, duration_from_secs(offset.unsigned_abs()))
            }
        }
        PosixTimerClock::Monotonic => current()
            .as_thread()
            .time_ns()
            .host_monotonic_deadline(value),
        PosixTimerClock::Boottime => current()
            .as_thread()
            .time_ns()
            .host_boottime_deadline(value),
        // CPU-clock absolute values are already expressed in their accounting
        // domain; `timer_settime` arms them through PosixTimer::arm_cpu.
        PosixTimerClock::ProcessCpu | PosixTimerClock::ThreadCpu => value,
    })
}

fn timer_effective_alarm_clock(clock: PosixTimerClock, absolute: bool) -> AlarmClock {
    if absolute {
        clock.absolute_alarm_clock()
    } else {
        AlarmClock::Monotonic
    }
}

fn timer_relative_deadline(value: Duration) -> Duration {
    saturating_add_duration(AlarmClock::Monotonic.now(), value)
}

/// Converts the process ITIMER_REAL remainder to the unsigned-seconds result
/// required by alarm(2). Linux adds one second only for a sub-second remainder
/// or one of at least 500ms, then returns the native unsigned-int low word.
fn alarm_remaining_seconds_from_nanos(nanos: u128) -> u32 {
    let seconds = nanos / NANOS_PER_SEC as u128;
    let subsecond_nanos = nanos % NANOS_PER_SEC as u128;
    let round_up =
        (seconds == 0 && subsecond_nanos != 0) || subsecond_nanos >= (NANOS_PER_SEC / 2) as u128;
    seconds.wrapping_add(u128::from(round_up)) as u32
}

fn alarm_remaining_seconds(value: TimeValue) -> u32 {
    alarm_remaining_seconds_from_nanos(value.as_nanos())
}

fn alarm_seconds_to_nanos(seconds: u32) -> AxResult<usize> {
    let nanos = u128::from(seconds).saturating_mul(NANOS_PER_SEC as u128);
    usize::try_from(nanos).map_err(|_| AxError::OutOfRange)
}

// `write_timer_spec` copies the complete initialized object representation.
// Keep the x86_64 Linux ABI premise executable instead of relying on the
// generated binding's shape by inspection.
const _: () = {
    assert!(size_of::<__kernel_old_time_t>() == 8);
    assert!(align_of::<__kernel_old_time_t>() == 8);
    assert!(size_of::<isize>() == 8);
    assert!(align_of::<timeval>() == 8);
    assert!(size_of::<timeval>() == 16);
    assert!(offset_of!(timeval, tv_sec) == 0);
    assert!(offset_of!(timeval, tv_usec) == 8);
    assert!(align_of::<timezone>() == 4);
    assert!(size_of::<timezone>() == 8);
    assert!(offset_of!(timezone, tz_minuteswest) == 0);
    assert!(offset_of!(timezone, tz_dsttime) == 4);
    assert!(align_of::<itimerval>() == 8);
    assert!(size_of::<itimerval>() == 32);
    assert!(offset_of!(itimerval, it_interval) == 0);
    assert!(offset_of!(itimerval, it_value) == 16);
    assert!(align_of::<timespec>() == 8);
    assert!(size_of::<timespec>() == 16);
    assert!(offset_of!(timespec, tv_sec) == 0);
    assert!(offset_of!(timespec, tv_nsec) == 8);
    assert!(align_of::<itimerspec>() == 8);
    assert!(size_of::<itimerspec>() == 32);
    assert!(offset_of!(itimerspec, it_interval) == 0);
    assert!(offset_of!(itimerspec, it_value) == 16);
};

fn write_time_result<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut __kernel_old_time_t,
    seconds: __kernel_old_time_t,
) -> AxResult<()> {
    // `time(2)` copies exactly one initialized x86_64 time_t word.  All
    // provider failures are Linux EFAULT, including access and population
    // failures, so do not expose provider-specific errors here.
    if let Some(ptr) = VmPtr::nullable(ptr) {
        unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, seconds) }
            .map_err(map_time_usercopy_error)?;
    }
    Ok(())
}

fn map_time_usercopy_error(_error: UserCopyError) -> AxError {
    AxError::BadAddress
}

fn map_timer_usercopy_error(_error: UserCopyError) -> AxError {
    // Linux's POSIX timer entry points map failed get/put/copy_user operations
    // to EFAULT. Provider-side page population failure is part of that copy,
    // not the timer object's own fallible allocation path.
    AxError::BadAddress
}

fn read_timer_spec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const itimerspec,
) -> AxResult<itimerspec> {
    let value =
        tk_linux_usercopy::VmPtr::vm_read_uninit(ptr, memory).map_err(map_timer_usercopy_error)?;
    // SAFETY: the explicit provider initialized every byte of the value, and
    // `itimerspec` contains only integer fields on the supported x86_64 ABI.
    Ok(unsafe { value.assume_init() })
}

fn write_timer_id<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut i32,
    timerid: i32,
) -> AxResult<()> {
    tk_linux_usercopy::VmMutPtr::vm_write(ptr, memory, timerid).map_err(map_timer_usercopy_error)
}

fn write_timer_spec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut itimerspec,
    value: itimerspec,
) -> AxResult<()> {
    // `linux_raw_sys` does not expose bytemuck's `NoUninit` marker for its
    // repr(C) ABI structs.  The x86_64 `itimerspec` is four integer words with
    // no padding, so its complete object representation is initialized here.
    unsafe { tk_linux_usercopy::VmMutPtr::vm_write_unchecked(ptr, memory, value) }
        .map_err(map_timer_usercopy_error)
}

fn read_itimer_value<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const itimerval,
) -> AxResult<itimerval> {
    let value =
        tk_linux_usercopy::VmPtr::vm_read_uninit(ptr, memory).map_err(map_timer_usercopy_error)?;
    // SAFETY: the explicit provider initialized every byte of the value, and
    // `itimerval` contains only integer fields on the supported x86_64 ABI.
    Ok(unsafe { value.assume_init() })
}

fn write_itimer_value<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut itimerval,
    value: itimerval,
) -> AxResult<()> {
    // `itimerval` has no padding on the x86_64 Linux ABI, so its complete
    // object representation is initialized and safe to copy out.
    unsafe { tk_linux_usercopy::VmMutPtr::vm_write_unchecked(ptr, memory, value) }
        .map_err(map_timer_usercopy_error)
}

pub fn sys_timer_create<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    sigevent_ptr: *const RawSigevent,
    timerid_ptr: *mut i32,
) -> AxResult<isize> {
    // Linux copies the optional event before validating the clock, then
    // validates notification fields. The output pointer is checked at copyout.
    let event = if let Some(ptr) = tk_linux_usercopy::VmPtr::nullable(sigevent_ptr) {
        Some(RawSigevent::read_from_user(memory, ptr).map_err(map_timer_usercopy_error)?)
    } else {
        None
    };
    let clock = posix_timer_clock(clock_id)?;
    let mut notify = decode_timer_notify(event)?;
    let curr = current();
    let thread = curr.as_thread();
    if let PosixTimerNotify::Signal {
        target_tid: Some(tid),
        ..
    } = &mut notify
    {
        // sigev_notify_thread_id is namespace-local, while timer delivery and
        // task lookup retain the kernel-wide identity. Resolve exactly once at
        // syscall entry, just as tgkill does; a numeric local TID must never be
        // interpreted as some unrelated global task ID.
        let global_tid = thread
            .pid_ns()
            .resolve_visible_pid(*tid)
            .ok_or(AxError::InvalidInput)?;
        let target =
            crate::task::get_visible_task(global_tid).map_err(|_| AxError::InvalidInput)?;
        let target = target.try_as_thread().ok_or(AxError::InvalidInput)?;
        if target.proc_data.proc.pid() != thread.proc_data.proc.pid() || target.pending_exit() {
            return Err(AxError::InvalidInput);
        }
        *tid = global_tid;
    }

    let proc_data = current().as_thread().proc_data.clone();
    // Main and optional signal-retry alarm leases are acquired atomically
    // before the timer ID becomes visible.  A published timer therefore never
    // needs a fallible alarm allocation during settime or periodic rearm.
    let cpu_target_task = matches!(clock, PosixTimerClock::ThreadCpu).then(|| current().clone());
    let candidate = PosixTimer::try_new(clock, notify, cpu_target_task, None, None)
        .map_err(map_posix_timer_admission_error)?;
    let timerid = {
        let mut timers = proc_data.posix_timers.lock();
        if let Some((idx, slot)) = timers
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(candidate);
            idx
        } else {
            timers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            timers.push(Some(candidate));
            timers.len() - 1
        }
    };

    // The slot remains deliberately unpublished while copyout or admission may
    // fail.  Other threads reject operations on it, so rollback cannot delete a
    // timer that another thread has observed or recreated.
    let retire = |timerid: usize| {
        let mut timers = proc_data.posix_timers.lock();
        let slot = timers
            .get_mut(timerid)
            .expect("reserved POSIX timer slot disappeared during admission");
        debug_assert!(slot.as_ref().is_some_and(|timer| !timer.is_published()));
        slot.take()
    };

    if let Err(error) = write_timer_id(memory, timerid_ptr, timerid as i32) {
        drop(retire(timerid));
        return Err(error);
    }

    // Linux completes the clock-specific admission only after the identifier
    // has been copied out (`do_timer_create()`, kernel/time/posix-timers.c:537-548):
    // the id is already visible to the caller when an alarm clock is refused,
    // and the refusal is EOPNOTSUPP without an RTC before EPERM without
    // CAP_WAKE_ALARM (`alarm_timer_create()`, kernel/time/alarmtimer.c:651-665).
    if let Err(error) = wake_alarm_admission(clock_id, linux_time::WakeAlarmUse::Arm) {
        drop(retire(timerid));
        return Err(error);
    }
    {
        let mut timers = proc_data.posix_timers.lock();
        let timer = timers
            .get_mut(timerid)
            .and_then(Option::as_mut)
            .ok_or(AxError::BadState)?;
        timer.publish();
    }
    Ok(0)
}

pub fn sys_timer_settime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timerid: i32,
    flags: i32,
    new_value: *const itimerspec,
    old_value: *mut itimerspec,
) -> AxResult<isize> {
    if new_value.is_null() {
        return Err(AxError::InvalidInput);
    }

    let new_value = read_timer_spec(memory, new_value)?;
    let (interval, value) = itimerspec_to_durations(&new_value)?;
    if timerid < 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !TIMER_ABSTIME as i32 != 0 {
        return Err(AxError::InvalidInput);
    }
    let absolute = (flags & TIMER_ABSTIME as i32) != 0;
    let curr = current();
    let thr = curr.as_thread();
    let proc_data = thr.proc_data.clone();
    // An absolute TAI arm must either be included in an in-flight ADJ_TAI
    // rebase plan or observe the fully published newer offset.  Relative
    // timers take this gate too to keep the timer-vector capacity proof
    // simple and never hold it across a blocking operation.
    let _tai_timer_gate = TAI_TIMER_REBASE_GATE.lock();
    // Establish one accounting cutoff before sampling a CPU-clock deadline.
    // This is a no-op for wall clocks and makes a relative CPU arm begin from
    // all work already performed by the calling thread.
    poll_timer(&curr);

    let (old_interval, old_remaining, retry_publication, main_publication) = {
        let mut timers = proc_data.posix_timers.lock();
        let timer = timers
            .get_mut(timerid as usize)
            .and_then(Option::as_mut)
            .filter(|timer| timer.is_published())
            .ok_or(AxError::InvalidInput)?;
        let old_interval = timer.interval;
        let old_remaining = timer.remaining(&proc_data);
        let sequence = timer.sequence.checked_add(1).ok_or(AxError::OutOfRange)?;
        let effective_clock = timer_effective_alarm_clock(timer.clock, absolute);
        let cpu_clock = timer.is_cpu_clock();
        let tai_absolute =
            matches!(timer.clock, PosixTimerClock::Tai) && absolute && !value.is_zero();
        let tai_generation = tai_absolute.then(tai_offset_snapshot);
        let deadline = if cpu_clock {
            timer.arm_cpu(&proc_data, absolute, value)?;
            None
        } else if value.is_zero() {
            None
        } else if absolute {
            Some(match tai_generation {
                Some((offset_seconds, _)) => tai_deadline_as_realtime(value, offset_seconds),
                None => timer_absolute_deadline(timer.clock, value)?,
            })
        } else {
            Some(timer_relative_deadline(value))
        };

        let retry_publication = timer.reset_signal_delivery();
        timer.interval = interval;
        timer.sequence = sequence;
        timer.effective_clock = effective_clock;
        timer.set_tai_absolute_deadline(
            tai_absolute.then_some(value),
            tai_generation.map_or(0, |(_, generation)| generation),
        );
        // Linux cancels the hrtimer without clearing its expiry, and a
        // SIGEV_NONE timer reads that stale remainder back rather than zero
        // (`kernel/time/posix-timers.c:696-707`).  A timer with a real
        // delivery mode returns early with a zeroed setting instead, and so
        // does one that was never armed.
        timer.stale_deadline = if value.is_zero()
            && matches!(timer.notify, PosixTimerNotify::None)
            && !cpu_clock
        {
            timer.deadline
        } else {
            None
        };
        timer.deadline = deadline;
        let main_publication = if let Some(deadline) = deadline {
            timer.prepare_main_alarm(
                &proc_data,
                timerid as usize,
                effective_clock,
                deadline,
                sequence,
            )
        } else {
            timer.prepare_main_disarm()
        };
        (
            old_interval,
            old_remaining,
            retry_publication,
            main_publication,
        )
    };

    retry_publication.publish();
    main_publication.publish();
    refresh_posix_cpu_timer_armed(&proc_data);
    if proc_data
        .process_itimer_cpu_armed
        .load(core::sync::atomic::Ordering::Acquire)
        != 0
        && let Some(cpu) = crate::task::request_process_cpu_evaluation(&proc_data)
    {
        crate::deferred_work::wake_process_timer_worker(cpu);
    }

    if !old_value.is_null() {
        write_timer_spec(
            memory,
            old_value,
            itimerspec {
                it_interval: duration_to_timespec(old_interval),
                it_value: duration_to_timespec(old_remaining),
            },
        )?;
    }

    Ok(0)
}

pub fn sys_timer_gettime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timerid: i32,
    curr_value: *mut itimerspec,
) -> AxResult<isize> {
    if timerid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    let (interval, remaining) = {
        let timers = proc_data.posix_timers.lock();
        let timer = timers
            .get(timerid as usize)
            .and_then(Option::as_ref)
            .filter(|timer| timer.is_published())
            .ok_or(AxError::InvalidInput)?;
        (timer.interval, timer.remaining(&proc_data))
    };

    write_timer_spec(
        memory,
        curr_value,
        itimerspec {
            it_interval: duration_to_timespec(interval),
            it_value: duration_to_timespec(remaining),
        },
    )?;
    Ok(0)
}

pub fn sys_timer_getoverrun(timerid: i32) -> AxResult<isize> {
    if timerid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    // Linux reports `it_overrun_last`, the overrun of the notification that
    // was actually delivered, and zero until one is (`timer_getoverrun()`,
    // kernel/time/posix-timers.c:279-289; `common_timer_set()` initializes it,
    // kernel/time/posix-timers.c:882-883).  The accumulating overrun that a
    // still-pending notification carries is reported through `si_overrun`
    // instead.
    let overrun = {
        let timers = proc_data.posix_timers.lock();
        timers
            .get(timerid as usize)
            .and_then(Option::as_ref)
            .filter(|timer| timer.is_published())
            .ok_or(AxError::InvalidInput)?
            .overrun_last
    };
    Ok(overrun as isize)
}

pub fn sys_timer_delete(timerid: i32) -> AxResult<isize> {
    if timerid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    let retired = {
        let mut timers = proc_data.posix_timers.lock();
        let slot = timers
            .get_mut(timerid as usize)
            .ok_or(AxError::InvalidInput)?;
        if !slot.as_ref().is_some_and(PosixTimer::is_published) {
            return Err(AxError::InvalidInput);
        }
        slot.take()
    };
    // Token drop removes both main and retry deadlines.  Keep all action
    // destruction outside the per-process timer owner lock.
    drop(retired);
    refresh_posix_cpu_timer_armed(&proc_data);
    Ok(0)
}

pub fn sys_clock_gettime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    ts: *mut timespec,
) -> AxResult<isize> {
    // `clock_gettime(2)` on a wake-alarm clock reports EINVAL when the kernel
    // has no RTC, before the clock is read (`alarm_clock_get_timespec()`,
    // kernel/time/alarmtimer.c:600-620); reading needs no capability.
    wake_alarm_admission(clock_id, linux_time::WakeAlarmUse::Read)?;
    let now = clock_now(clock_id)?;
    // SAFETY: `timespec` is two initialized integer words on the x86_64 Linux
    // ABI; the layout assertions above cover the complete object extent.
    unsafe { VmMutPtr::vm_write_unchecked(ts, memory, timespec::from_time_value(now)) }
        .map_err(map_usercopy_error)?;
    Ok(0)
}

pub fn sys_time<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    tloc: *mut __kernel_old_time_t,
) -> AxResult<isize> {
    // Sample once so the return value and optional stored value are identical.
    let seconds = wall_time().as_secs() as __kernel_old_time_t;
    write_time_result(memory, tloc, seconds)?;
    Ok(seconds as isize)
}

pub fn sys_gettimeofday<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ts: *mut timeval,
    tz: *mut timezone,
) -> AxResult<isize> {
    let now = wall_time();
    if let Some(ts) = VmPtr::nullable(ts) {
        // SAFETY: `timeval` is two initialized integer words on the x86_64
        // Linux ABI; the layout assertions above cover the object extent.
        unsafe { VmMutPtr::vm_write_unchecked(ts, memory, timeval::from_time_value(now)) }
            .map_err(map_usercopy_error)?;
    }
    if let Some(tz) = VmPtr::nullable(tz) {
        // Linux reports the single process-wide timezone retained by
        // settimeofday(2) in `sys_tz` (`kernel/time/time.c:140-155`); it stays
        // zero until userspace sets one, and no clock reads it.
        let retained = system_timezone();
        // SAFETY: `timezone` contains only its two initialized i32 fields;
        // generated linux_raw_sys layout is asserted by the compiler below.
        unsafe {
            VmMutPtr::vm_write_unchecked(
                tz,
                memory,
                timezone {
                    tz_minuteswest: retained.minutes_west,
                    tz_dsttime: retained.dst_time,
                },
            )
        }
        .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_settimeofday<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ts: *const timeval,
    tz: *const timezone,
) -> AxResult<isize> {
    // `settimeofday(2)` reads the timeval and validates its sub-second field
    // before it touches the timezone pointer, so a malformed `tv_usec` is
    // EINVAL even when `tz` would fault; the seconds are validated later by
    // `timespec64_valid_settod()` inside `do_sys_settimeofday64()`
    // (`kernel/time/time.c:199-222`).
    let ts = if let Some(ts) = VmPtr::nullable(ts) {
        let ts = unsafe {
            VmPtr::vm_read_uninit(ts, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        if !(0..1_000_000).contains(&ts.tv_usec) {
            return Err(AxError::InvalidInput);
        }
        Some(ts)
    } else {
        None
    };

    let tz = if let Some(tz) = VmPtr::nullable(tz) {
        Some(unsafe {
            VmPtr::vm_read_uninit(tz, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        })
    } else {
        None
    };

    let requested_nanos = ts.map(|ts| -> i128 {
        ts.tv_sec as i128 * NANOS_PER_SEC as i128 + ts.tv_usec as i128 * 1_000
    });

    // `timespec64_valid_settod()` runs before the capability test while the
    // CLOCK_MONOTONIC floor runs after it (`kernel/time/time.c:174-197`,
    // `kernel/time/timekeeping.c:1665-1690`), so only the bound is checked
    // here and the floor is applied once the timezone has been retained.
    if let Some(requested_nanos) = requested_nanos {
        linux_time::validate_settime_bound(requested_nanos).map_err(map_timex_reject)?;
    }

    if !current().as_thread().has_effective_capability(CAP_SYS_TIME) {
        return Err(AxError::OperationNotPermitted);
    }

    if let Some(tz) = tz {
        if tz.tz_minuteswest < -15 * 60 || tz.tz_minuteswest > 15 * 60 {
            return Err(AxError::InvalidInput);
        }
        // `sys_tz` is stored before the wall clock is touched, so a rejected
        // time still leaves the retained timezone changed
        // (`kernel/time/time.c:205-222`).
        set_system_timezone(SystemTimezone {
            minutes_west: tz.tz_minuteswest,
            dst_time: tz.tz_dsttime,
        });
        // The `firsttime` one-shot is spent by the *first stored timezone*,
        // whatever that call does with the clock, and only a store that omits
        // the clock value warps it:
        //
        //     sys_tz = *tz;
        //     update_vsyscall_tz();
        //     if (firsttime) {
        //             firsttime = 0;
        //             if (!tv)
        //                     timekeeping_warp_clock();
        //     }
        //
        // (`do_sys_settimeofday64()`, kernel/time/time.c:185-197). Spending the
        // one-shot only on the `tv == NULL` branch would let a later
        // `settimeofday(NULL, &tz)` warp `CLOCK_REALTIME` by
        // `tz_minuteswest * 60` -- up to +-15 hours -- after a call that already
        // set the clock.
        if crate::time::warp_first_timezone(tz.tz_minuteswest, requested_nanos.is_none())? {
            // `timekeeping_warp_clock()` reaches `ntp_clear()` through
            // `timekeeping_inject_offset()` (`kernel/time/timekeeping.c:1786-1790`),
            // but only when the retained timezone actually moves the clock.
            clear_ntp_state()?;
        }
    }

    if let Some(requested_nanos) = requested_nanos {
        linux_time::validate_settime_floor(requested_nanos, monotonic_time_nanos() as i128)
            .map_err(map_timex_reject)?;
        // The bound above proves the value fits the wall-clock model.
        set_wall_time(TimeValue::from_nanos(requested_nanos as u64))?;
        clear_ntp_state()?;
    }
    Ok(0)
}

pub fn sys_clock_getres<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    res: *mut timespec,
) -> AxResult<isize> {
    // As for clock_gettime(2): a wake-alarm clock does not exist without an
    // RTC (`alarm_clock_getres()`, kernel/time/alarmtimer.c:600-620).
    wake_alarm_admission(clock_id, linux_time::WakeAlarmUse::Read)?;
    // `posix_cpu_clock_getres()` validates the encoded pid before it reports
    // anything (`validate_clock_permissions()` -> `pid_for_clock(clock,
    // false)`, `kernel/time/posix-cpu-timers.c:57-124`), so a clock naming a
    // pid that does not exist, or a foreign thread, is EINVAL no matter what
    // the resolution would have been.  `clock_getres(2)` passes
    // `gettime = false`, which also refuses the caller's own non-leader TID.
    if decode_cpu_clock_id(clock_id).is_some() {
        resolve_cpu_clock_usage(clock_id, false)?;
    }
    let resolution = clock_resolution(clock_id)?;
    if let Some(res) = VmPtr::nullable(res) {
        // SAFETY: `timespec` is a fully initialized two-word ABI value.
        unsafe { VmMutPtr::vm_write_unchecked(res, memory, timespec::from_time_value(resolution)) }
            .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub fn sys_clock_settime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    ts: *const timespec,
) -> AxResult<isize> {
    // `clock_settime(2)` classifies the clock first *only* to decide whether it
    // has a setter at all: `if (!kc || !kc->clock_set) return -EINVAL` runs
    // before the timespec is copied (`kernel/time/posix-timers.c:1123-1130`).
    // That covers every positive clock except CLOCK_REALTIME, whose
    // `clock_realtime` table is the only one with `.clock_set`.
    //
    // The negative (encoded) ids are different: `clockid_to_kclock()` routes
    // them to `clock_posix_cpu` or `clock_posix_dynamic`
    // (`kernel/time/posix-timers.c:1541-1554`), and both *do* provide
    // `.clock_set` (`kernel/time/posix-cpu-timers.c:1706-1718`,
    // `kernel/time/posix-timers.c:1290-1310`), so the copy-in happens before
    // they are refused.  A CPU clock can never be reset, but the refusal is
    // EPERM for a valid target and EINVAL for one that names no task
    // (`posix_cpu_clock_set()`, `kernel/time/posix-cpu-timers.c:180-189`).
    if clock_id >= 0 && clock_id as u32 != CLOCK_REALTIME {
        return Err(AxError::InvalidInput);
    }
    // `clockid_to_kclock()` sends every negative id to `clock_posix_cpu` unless
    // bits 2..0 are all set, which selects `clock_posix_dynamic`
    // (`kernel/time/posix-timers.c:1541-1554`).  Both carry a `.clock_set`, so
    // neither is refused before the copy-in; a dynamic clock that names no open
    // descriptor is EINVAL afterwards, which `decode_cpu_clock_id` already
    // reports for a CLOCKFD encoding
    // (`posix_clock_ioctl()`, `kernel/time/posix-clock.c:230-250`).
    let cpu_clock = decode_cpu_clock_id(clock_id);
    if clock_id < 0 && (clock_id & CLOCKFD_MASK) != CLOCKFD && cpu_clock.is_none() {
        return Err(AxError::InvalidInput);
    }
    let requested = unsafe {
        VmPtr::vm_read_uninit(ts, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    }
    .try_into_time_value()?;
    if cpu_clock.is_some() {
        // `posix_cpu_clock_set()` runs the permission lookup and then refuses
        // with EPERM; it never reads the timespec, because `clock_settime(2)`
        // has already copied it in (`get_timespec64()`,
        // `kernel/time/posix-timers.c:1131-1133`).
        resolve_cpu_clock_usage(clock_id, false)?;
        return Err(AxError::OperationNotPermitted);
    }
    let requested_nanos = requested.as_nanos() as i128;
    // `do_sys_settimeofday64()` validates the wall-clock bound before the
    // `CAP_SYS_TIME` test and applies the CLOCK_MONOTONIC floor afterwards
    // (`kernel/time/time.c:174-197`, `kernel/time/timekeeping.c:1665-1690`).
    linux_time::validate_settime_bound(requested_nanos).map_err(map_timex_reject)?;
    if !current().as_thread().has_effective_capability(CAP_SYS_TIME) {
        return Err(AxError::OperationNotPermitted);
    }
    linux_time::validate_settime_floor(requested_nanos, monotonic_time_nanos() as i128)
        .map_err(map_timex_reject)?;
    set_wall_time(requested)?;
    // `clock_realtime`'s `.clock_set` is `do_sys_settimeofday64()` with a NULL
    // timezone (`kernel/time/posix-timers.c:1123-1133`), so it publishes with
    // `TK_UPDATE_ALL` and clears the NTP state the same way.
    clear_ntp_state()?;
    Ok(0)
}

pub fn sys_clock_adjtime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    // `clock_adjtime(2)` copies the timex in first and, unlike `adjtimex(2)`,
    // copies it back only when the operation succeeded
    // (`kernel/time/posix-timers.c:1172-1187`).
    let mut timex = unsafe {
        VmPtr::vm_read_uninit(timex_ptr, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let result = clock_adjtime_core(clock_id, &mut timex)?;
    // SAFETY: `timex` was initialized by the copy-in above and every field
    // update preserves its fully initialized object representation.
    unsafe { VmMutPtr::vm_write_unchecked(timex_ptr, memory, timex) }
        .map_err(map_usercopy_error)?;
    Ok(result)
}

pub fn sys_adjtimex<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    // `adjtimex(2)` always copies the timex back, even when the operation
    // failed, and replaces that failure with EFAULT if the copy fails
    // (`kernel/time/time.c:269-282`).
    let mut timex = unsafe {
        VmPtr::vm_read_uninit(timex_ptr, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let result = clock_adjtime_core(CLOCK_REALTIME as _, &mut timex);
    // SAFETY: as above; a failed operation leaves every field initialized.
    unsafe { VmMutPtr::vm_write_unchecked(timex_ptr, memory, timex) }
        .map_err(map_usercopy_error)?;
    result
}

#[repr(C)]
pub struct Tms {
    /// user time
    tms_utime: i64,
    /// system time
    tms_stime: i64,
    /// user time of children
    tms_cutime: i64,
    /// system time of children
    tms_cstime: i64,
}

pub fn sys_times<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    tms: *mut Tms,
) -> AxResult<isize> {
    if let Some(tms) = VmPtr::nullable(tms) {
        let curr = current();
        // Flush the currently executing task before taking the lock-free
        // group snapshot; siblings are sampled from their atomic snapshots.
        poll_timer(&curr);
        let proc_data = &curr.as_thread().proc_data;
        let self_usage = proc_data.self_usage();
        let child_usage = proc_data.children_usage();
        // SAFETY: `Tms` is repr(C) over four initialized native clock_t words and has
        // no implicit padding on the supported x86_64 ABI.
        unsafe {
            VmMutPtr::vm_write_unchecked(
                tms,
                memory,
                Tms {
                    tms_utime: self_usage.utime_ticks() as i64,
                    tms_stime: self_usage.stime_ticks() as i64,
                    tms_cutime: child_usage.utime_ticks() as i64,
                    tms_cstime: child_usage.stime_ticks() as i64,
                },
            )
        }
        .map_err(map_usercopy_error)?;
    }
    Ok(times_clock_ticks(monotonic_time_nanos()) as isize)
}

/// Implements Linux alarm(2) by replacing the process-wide ITIMER_REAL with
/// a one-shot timer. The existing process timer lease and publication path are
/// deliberately shared with setitimer(2), so the two interfaces replace one
/// another without introducing a second timer registry.
pub fn sys_alarm(seconds: u32) -> AxResult<isize> {
    let remaining_ns = alarm_seconds_to_nanos(seconds)?;
    let curr = current();
    poll_timer(&curr);
    let ((_, old_remaining), _) = set_process_itimer(
        &curr.as_thread().proc_data,
        ITimerType::Real,
        0,
        remaining_ns,
    )?;
    Ok(alarm_remaining_seconds(old_remaining) as isize)
}

pub fn sys_getitimer<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    which: i32,
    value: *mut itimerval,
) -> AxResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(AxError::InvalidInput)?;
    let curr = current();
    poll_timer(&curr);
    let (it_interval, it_value) = get_process_itimer(&curr.as_thread().proc_data, ty);

    write_itimer_value(
        memory,
        value,
        itimerval {
            it_interval: timeval::from_time_value(it_interval),
            it_value: timeval::from_time_value(it_value),
        },
    )?;
    Ok(0)
}

pub fn sys_setitimer<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    which: i32,
    new_value: *const itimerval,
    old_value: *mut itimerval,
) -> AxResult<isize> {
    // Linux copies and validates the replacement before dispatching `which`.
    // Preserve EFAULT/EINVAL precedence for combined bad-pointer/bad-selector
    // calls instead of rejecting the selector before touching userspace.
    let (interval, remained) = if !new_value.is_null() {
        let new_value = read_itimer_value(memory, new_value)?;
        let interval = usize::try_from(new_value.it_interval.try_into_time_value()?.as_nanos())
            .map_err(|_| AxError::OutOfRange)?;
        let remaining = usize::try_from(new_value.it_value.try_into_time_value()?.as_nanos())
            .map_err(|_| AxError::OutOfRange)?;
        (interval, remaining)
    } else {
        (0, 0)
    };
    let ty = ITimerType::from_repr(which).ok_or(AxError::InvalidInput)?;
    let curr = current();

    debug!("sys_setitimer <= type: {ty:?}, interval: {interval:?}, remained: {remained:?}");

    poll_timer(&curr);
    let ((old_interval, old_remaining), cpu_epoch) =
        set_process_itimer(&curr.as_thread().proc_data, ty, interval, remained)?;
    if let Some(epoch) = cpu_epoch {
        // The interval that began at the pre-arm poll crosses the exact arm
        // cutoff. Account it to the lifetime total while the old local epoch
        // conservatively omits it from the newly armed eligible clock, then
        // start the next interval in the returned (or a newer) generation.
        poll_timer(&curr);
        let _guard = NoPreemptIrqSave::new();
        curr.as_thread()
            .time
            .borrow_mut()
            .sync_process_cpu_timer_epoch(ty, epoch);
    }

    if !old_value.is_null() {
        write_itimer_value(
            memory,
            old_value,
            itimerval {
                it_interval: timeval::from_time_value(old_interval),
                it_value: timeval::from_time_value(old_remaining),
            },
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{mem::MaybeUninit, ops::Range};

    use linux_raw_sys::general::{
        CLOCK_BOOTTIME_ALARM, CLOCK_REALTIME_ALARM, CLOCK_TAI, MAX_CLOCKS,
    };
    use tk_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

    use super::*;

    struct TestMemory {
        bytes: alloc::vec::Vec<u8>,
        reject_writes: bool,
    }

    impl TestMemory {
        fn range(&self, start: usize, len: usize) -> Result<Range<usize>, UserCopyError> {
            let end = start.checked_add(len).ok_or(UserCopyError::BadAddress)?;
            (end <= self.bytes.len())
                .then_some(start..end)
                .ok_or(UserCopyError::BadAddress)
        }
    }

    // SAFETY: TestMemory treats user addresses as checked byte offsets and
    // initializes every destination byte before returning a successful read.
    unsafe impl UserMemory for TestMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let range = self.range(start, dst.len())?;
            for (output, input) in dst.iter_mut().zip(&self.bytes[range]) {
                output.write(*input);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
            if self.reject_writes {
                return Err(UserCopyError::BadAddress);
            }
            let range = self.range(start, src.len())?;
            self.bytes[range].copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn boottime_timer_retains_its_namespace_clock_domain() {
        for clock in [CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM] {
            assert_eq!(posix_timer_clock(clock as _), Ok(PosixTimerClock::Boottime));
        }
        assert_eq!(
            PosixTimerClock::Boottime.absolute_alarm_clock(),
            AlarmClock::Monotonic
        );
    }

    #[test]
    fn timer_usercopy_helpers_bind_one_provider_at_unaligned_addresses() {
        let mut provider = TestMemory {
            bytes: vec![0; 128],
            reject_writes: false,
        };
        let input = itimerspec {
            it_interval: timespec {
                tv_sec: 11,
                tv_nsec: 22,
            },
            it_value: timespec {
                tv_sec: 33,
                tv_nsec: 44,
            },
        };
        let input_bytes = unsafe {
            core::slice::from_raw_parts(
                (&input as *const itimerspec).cast::<u8>(),
                core::mem::size_of::<itimerspec>(),
            )
        };
        let input_addr = 5;
        let timerid_addr = 3;
        let output_addr = 37;
        provider.bytes[input_addr..input_addr + input_bytes.len()].copy_from_slice(input_bytes);

        let copied = {
            let mut memory = UserMemoryContext::new(&mut provider);
            let copied = read_timer_spec(
                &mut memory,
                core::ptr::without_provenance::<itimerspec>(input_addr),
            )
            .unwrap();
            write_timer_id(
                &mut memory,
                core::ptr::without_provenance_mut::<i32>(timerid_addr),
                17,
            )
            .unwrap();
            write_timer_spec(&mut memory, output_addr as *mut itimerspec, copied).unwrap();
            copied
        };

        assert_eq!(copied.it_interval.tv_sec, input.it_interval.tv_sec);
        assert_eq!(copied.it_interval.tv_nsec, input.it_interval.tv_nsec);
        assert_eq!(copied.it_value.tv_sec, input.it_value.tv_sec);
        assert_eq!(copied.it_value.tv_nsec, input.it_value.tv_nsec);
        assert_eq!(
            i32::from_ne_bytes(
                provider.bytes[timerid_addr..timerid_addr + core::mem::size_of::<i32>()]
                    .try_into()
                    .unwrap()
            ),
            17
        );
        assert_eq!(
            &provider.bytes[output_addr..output_addr + input_bytes.len()],
            input_bytes
        );
    }

    #[test]
    fn itimer_usercopy_helpers_bind_one_provider_at_unaligned_addresses() {
        let mut provider = TestMemory {
            bytes: vec![0; 128],
            reject_writes: false,
        };
        let input = itimerval {
            it_interval: timeval {
                tv_sec: 11,
                tv_usec: 22,
            },
            it_value: timeval {
                tv_sec: 33,
                tv_usec: 44,
            },
        };
        let input_bytes = unsafe {
            core::slice::from_raw_parts(
                (&input as *const itimerval).cast::<u8>(),
                core::mem::size_of::<itimerval>(),
            )
        };
        let input_addr = 5;
        let output_addr = 37;
        provider.bytes[input_addr..input_addr + input_bytes.len()].copy_from_slice(input_bytes);

        let copied = {
            let mut memory = UserMemoryContext::new(&mut provider);
            let copied = read_itimer_value(
                &mut memory,
                core::ptr::without_provenance::<itimerval>(input_addr),
            )
            .unwrap();
            write_itimer_value(&mut memory, output_addr as *mut itimerval, copied).unwrap();
            copied
        };

        assert_eq!(copied.it_interval.tv_sec, input.it_interval.tv_sec);
        assert_eq!(copied.it_interval.tv_usec, input.it_interval.tv_usec);
        assert_eq!(copied.it_value.tv_sec, input.it_value.tv_sec);
        assert_eq!(copied.it_value.tv_usec, input.it_value.tv_usec);
        assert_eq!(
            &provider.bytes[output_addr..output_addr + input_bytes.len()],
            input_bytes
        );
    }

    #[test]
    fn timer_usercopy_helper_maps_copyout_failure() {
        let mut provider = TestMemory {
            bytes: vec![0; 32],
            reject_writes: true,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            write_timer_id(&mut memory, core::ptr::without_provenance_mut::<i32>(3), 1),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            map_timer_usercopy_error(UserCopyError::NoMemory),
            AxError::BadAddress
        );
    }

    #[test]
    fn time_result_is_native_64bit_unaligned_and_maps_all_copyout_failures_to_efault() {
        let seconds = 0x1_0000_0001_i64 as __kernel_old_time_t;
        let mut provider = TestMemory {
            bytes: vec![0; 32],
            reject_writes: false,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        write_time_result(&mut memory, core::ptr::null_mut(), seconds).unwrap();
        write_time_result(
            &mut memory,
            core::ptr::without_provenance_mut::<__kernel_old_time_t>(3),
            seconds,
        )
        .unwrap();
        assert_eq!(&provider.bytes[3..11], &seconds.to_ne_bytes());

        for error in [
            UserCopyError::BadAddress,
            UserCopyError::AccessDenied,
            UserCopyError::NoMemory,
        ] {
            assert_eq!(map_time_usercopy_error(error), AxError::BadAddress);
        }
    }

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

    fn make_process_cpuclock(pid: u32, which: i32) -> __kernel_clockid_t {
        (!(pid as i32) << 3) | which
    }

    fn make_thread_cpuclock(tid: u32, which: i32) -> __kernel_clockid_t {
        make_process_cpuclock(tid, which | CPUCLOCK_PERTHREAD_MASK)
    }

    #[test]
    fn clock_domain_accepts_linux_encoded_cpu_clock_ids() {
        assert_eq!(
            clock_domain(make_process_cpuclock(123, CPUCLOCK_SCHED)),
            Ok(ClockDomain::ProcessCpu)
        );
        assert_eq!(
            clock_domain(make_thread_cpuclock(456, CPUCLOCK_SCHED)),
            Ok(ClockDomain::ThreadCpu)
        );
        assert_eq!(
            clock_domain(make_thread_cpuclock(456, CPUCLOCK_VIRT)),
            Ok(ClockDomain::ThreadCpu)
        );
    }

    #[test]
    fn clock_domain_rejects_invalid_ids() {
        assert_eq!(clock_domain(-1), Err(AxError::InvalidInput));
        assert_eq!(
            clock_domain(make_process_cpuclock(1, CLOCKFD)),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            clock_domain(make_thread_cpuclock(1, CLOCKFD)),
            Err(AxError::InvalidInput)
        );
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
        // The resolution itself does not depend on the target, which is what
        // separates this pure half from the lookup `sys_clock_getres` performs
        // first (`posix_cpu_clock_getres()`,
        // kernel/time/posix-cpu-timers.c:159-176).
        assert_eq!(
            clock_resolution(make_thread_cpuclock(123, CPUCLOCK_SCHED)),
            Ok(fine_clock_resolution())
        );
        assert_eq!(clock_resolution(-1), Err(AxError::InvalidInput));
    }

    #[test]
    fn quantized_clock_readings_snap_to_resolution() {
        assert_eq!(
            quantize_clock_reading(
                TimeValue::from_nanos(123_456_789),
                TimeValue::from_nanos(10)
            ),
            TimeValue::from_nanos(123_456_780)
        );
        assert_eq!(
            quantize_clock_reading(TimeValue::from_nanos(123_456_789), TimeValue::from_nanos(1)),
            TimeValue::from_nanos(123_456_789)
        );
    }

    /// Encodes a CPU clock the way `MAKE_PROCESS_CPUCLOCK`/`MAKE_THREAD_CPUCLOCK`
    /// do in `include/uapi/linux/posix-timers.h`.
    fn encoded_cpu_clock(tid: i32, which: i32, per_thread: bool) -> __kernel_clockid_t {
        (!tid << 3)
            | which
            | if per_thread {
                CPUCLOCK_PERTHREAD_MASK
            } else {
                0
            }
    }

    #[test]
    fn clock_resolution_matches_linux_cpu_and_coarse_clocks() {
        let tick = axconfig::TICKS_PER_SEC as u64;
        let accounting = linux_time::accounting_tick_resolution_nanos(tick as u32);
        // The plain CPU clock ids are the scheduler clock, which Linux reports
        // as one nanosecond, as are their CPUCLOCK_SCHED encodings.
        assert_eq!(
            clock_resolution(CLOCK_PROCESS_CPUTIME_ID as __kernel_clockid_t)
                .unwrap()
                .as_nanos(),
            1
        );
        assert_eq!(
            clock_resolution(CLOCK_THREAD_CPUTIME_ID as __kernel_clockid_t)
                .unwrap()
                .as_nanos(),
            1
        );
        assert_eq!(
            clock_resolution(encoded_cpu_clock(1, CPUCLOCK_SCHED, true))
                .unwrap()
                .as_nanos(),
            1
        );
        assert_eq!(
            clock_resolution(encoded_cpu_clock(1, CPUCLOCK_SCHED, false))
                .unwrap()
                .as_nanos(),
            1
        );
        // The encoded PROF and VIRT clocks are charged once per tick, so their
        // resolution is the rounded-up accounting tick.
        for per_thread in [true, false] {
            for which in [CPUCLOCK_PROF, CPUCLOCK_VIRT] {
                assert_eq!(
                    clock_resolution(encoded_cpu_clock(1, which, per_thread))
                        .unwrap()
                        .as_nanos() as u64,
                    accounting
                );
            }
        }
        assert_eq!(
            clock_resolution(CLOCK_MONOTONIC as __kernel_clockid_t)
                .unwrap()
                .as_nanos(),
            1
        );
        assert_eq!(
            clock_resolution(CLOCK_MONOTONIC_COARSE as __kernel_clockid_t)
                .unwrap()
                .as_nanos() as u64,
            (NANOS_PER_SEC / tick).max(1)
        );
        // A CLOCKFD encoding is not a clock id at all.
        assert_eq!(
            clock_resolution(!987 << 3 | CLOCKFD),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn timex_planner_accepts_legacy_singleshot_modes() {
        // The two legacy adjtime(3) spellings stay accepted by the policy
        // layer: ADJ_ADJTIME must be accompanied by the singleshot bit, and
        // ADJ_OFFSET_SS_READ is that same request in read-only form.
        for modes in [
            linux_time::ADJ_ADJTIME | linux_time::ADJ_OFFSET_SINGLESHOT,
            linux_time::ADJ_OFFSET_SS_READ,
        ] {
            let request = linux_time::TimexRequest {
                modes,
                ..linux_time::TimexRequest::default()
            };
            assert!(
                linux_time::adjust(
                    &linux_time::TimexState::INITIAL,
                    &request,
                    linux_time::TimexAuthority { cap_sys_time: true },
                    axconfig::TICKS_PER_SEC as i64,
                )
                .is_ok()
            );
        }
        // A bare ADJ_ADJTIME is the one illegal combination.
        let request = linux_time::TimexRequest {
            modes: linux_time::ADJ_ADJTIME,
            ..linux_time::TimexRequest::default()
        };
        assert_eq!(
            linux_time::adjust(
                &linux_time::TimexState::INITIAL,
                &request,
                linux_time::TimexAuthority { cap_sys_time: true },
                axconfig::TICKS_PER_SEC as i64,
            ),
            Err(linux_time::Reject::InvalidMode)
        );
    }

    #[test]
    fn timex_request_maps_every_input_field() {
        // SAFETY: `KernelOldTimex` is a plain C record and zero is a valid
        // representation for each of its fields.
        let mut timex: KernelOldTimex = unsafe { core::mem::zeroed() };
        timex.modes = linux_time::ADJ_OFFSET | linux_time::ADJ_NANO;
        timex.offset = -4321;
        timex.freq = 65_536;
        timex.maxerror = 1;
        timex.esterror = 2;
        timex.status = linux_time::STA_PLL;
        timex.constant = 3;
        timex.tick = 10_000;
        timex.time = timeval {
            tv_sec: 11,
            tv_usec: 22,
        };

        let request = timex_request(&timex);
        assert_eq!(request.modes, linux_time::ADJ_OFFSET | linux_time::ADJ_NANO);
        assert_eq!(request.offset, -4321);
        assert_eq!(request.freq, 65_536);
        assert_eq!(request.maxerror, 1);
        assert_eq!(request.esterror, 2);
        assert_eq!(request.status, linux_time::STA_PLL);
        assert_eq!(request.constant, 3);
        assert_eq!(request.tick, 10_000);
        assert_eq!(request.time_sec, 11);
        assert_eq!(request.time_usec, 22);
    }

    #[test]
    fn timex_output_keeps_modes_and_renders_resolution_correct_time() {
        let output = linux_time::TimexOutput {
            offset: 1,
            freq: 2,
            maxerror: 3,
            esterror: 4,
            status: linux_time::STA_NANO,
            constant: 5,
            precision: 1,
            tolerance: linux_time::TOLERANCE,
            tick: 10_000,
            tai: 7,
        };
        let sampled = TimeValue::new(1, 123_456_789);
        // SAFETY: as above.
        let mut timex: KernelOldTimex = unsafe { core::mem::zeroed() };
        timex.modes = 0x5a5a;

        fill_timex_output(&mut timex, output.status, &output, sampled);

        // Linux never writes `modes` back, so the caller keeps its own bits.
        assert_eq!(timex.modes, 0x5a5a);
        assert_eq!(timex.offset, 1);
        assert_eq!(timex.freq, 2);
        assert_eq!(timex.maxerror, 3);
        assert_eq!(timex.esterror, 4);
        assert_eq!(timex.status, linux_time::STA_NANO);
        assert_eq!(timex.constant, 5);
        assert_eq!(timex.precision, 1);
        assert_eq!(timex.tolerance, linux_time::TOLERANCE);
        assert_eq!(timex.tick, 10_000);
        assert_eq!(timex.tai, 7);
        // Nanosecond resolution carries the sub-second value in `tv_usec`.
        assert_eq!(timex.time.tv_sec, 1);
        assert_eq!(timex.time.tv_usec, 123_456_789);

        // Without STA_NANO the same instant renders in microseconds.
        fill_timex_output(&mut timex, 0, &output, sampled);
        assert_eq!(timex.time.tv_sec, 1);
        assert_eq!(timex.time.tv_usec, 123_456);
    }

    #[test]
    fn timex_rejections_map_to_linux_errnos() {
        assert_eq!(
            map_timex_reject(linux_time::Reject::NotPermitted),
            AxError::OperationNotPermitted
        );
        for reject in [
            linux_time::Reject::InvalidClock,
            linux_time::Reject::InvalidTime,
            linux_time::Reject::Overflow,
            linux_time::Reject::UnsupportedSetClock,
            linux_time::Reject::InvalidMode,
            linux_time::Reject::TimeBeforeMonotonic,
        ] {
            assert_eq!(map_timex_reject(reject), AxError::InvalidInput);
        }
    }

    #[test]
    fn clock_adjtime_copies_timex_before_classifying_clock() {
        let timex_ptr = core::ptr::without_provenance_mut::<KernelOldTimex>(0);

        let mut valid_provider = TestMemory {
            bytes: vec![0; size_of::<KernelOldTimex>()],
            reject_writes: false,
        };
        let mut valid_memory = UserMemoryContext::new(&mut valid_provider);
        assert_eq!(
            sys_clock_adjtime(&mut valid_memory, CLOCK_MONOTONIC as _, timex_ptr),
            Err(AxError::OperationNotSupported)
        );
        assert_eq!(
            sys_clock_adjtime(&mut valid_memory, -1, timex_ptr),
            Err(AxError::OperationNotSupported)
        );

        let mut invalid_provider = TestMemory {
            bytes: vec![],
            reject_writes: false,
        };
        let mut invalid_memory = UserMemoryContext::new(&mut invalid_provider);
        assert_eq!(
            sys_clock_adjtime(&mut invalid_memory, -1, timex_ptr),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn clock_adjtime_distinguishes_realtime_supported_invalid_and_unsupported_clocks() {
        assert_eq!(clock_adjtime_is_realtime(CLOCK_REALTIME as _), Ok(true));
        assert_eq!(clock_adjtime_is_realtime(CLOCK_MONOTONIC as _), Ok(false));
        assert_eq!(clock_adjtime_is_realtime(-1), Ok(false));
        assert_eq!(clock_adjtime_is_realtime(-5), Err(AxError::InvalidInput));
        assert_eq!(
            clock_adjtime_is_realtime(0x7fff),
            Err(AxError::InvalidInput)
        );

        let mut provider = TestMemory {
            bytes: vec![0; size_of::<KernelOldTimex>()],
            reject_writes: false,
        };
        let mut memory = UserMemoryContext::new(&mut provider);
        assert_eq!(
            sys_clock_adjtime(
                &mut memory,
                0x7fff,
                core::ptr::without_provenance_mut::<KernelOldTimex>(0),
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn sigevent_signo_does_not_wrap_before_validation() {
        assert_eq!(decode_sigevent_signo(1), Ok(Signo::SIGHUP));
        assert_eq!(decode_sigevent_signo(64), Ok(Signo::SIGRT32));
        assert_eq!(decode_sigevent_signo(0), Err(AxError::InvalidInput));
        assert_eq!(decode_sigevent_signo(257), Err(AxError::InvalidInput));
        assert_eq!(decode_sigevent_signo(-1), Err(AxError::InvalidInput));
    }

    #[test]
    fn relative_posix_timers_use_a_monotonic_effective_basis() {
        for clock in [
            PosixTimerClock::Realtime,
            PosixTimerClock::Monotonic,
            PosixTimerClock::Tai,
        ] {
            assert_eq!(
                timer_effective_alarm_clock(clock, false),
                AlarmClock::Monotonic
            );
        }
        assert_eq!(
            timer_effective_alarm_clock(PosixTimerClock::Realtime, true),
            AlarmClock::Realtime
        );
        assert_eq!(
            timer_effective_alarm_clock(PosixTimerClock::Tai, true),
            AlarmClock::Realtime
        );
    }

    #[test]
    fn posix_cpu_timers_preserve_the_requested_accounting_domain() {
        assert_eq!(
            posix_timer_clock(CLOCK_PROCESS_CPUTIME_ID as _),
            Ok(PosixTimerClock::ProcessCpu)
        );
        assert_eq!(
            posix_timer_clock(CLOCK_THREAD_CPUTIME_ID as _),
            Ok(PosixTimerClock::ThreadCpu)
        );
    }

    #[test]
    fn alarm_remaining_seconds_matches_linux_half_second_rule_and_wraps() {
        let second = NANOS_PER_SEC as u128;
        assert_eq!(alarm_remaining_seconds_from_nanos(0), 0);
        assert_eq!(alarm_remaining_seconds_from_nanos(second / 10), 1);
        assert_eq!(alarm_remaining_seconds_from_nanos(second), 1);
        assert_eq!(alarm_remaining_seconds_from_nanos(second + second / 10), 1);
        assert_eq!(
            alarm_remaining_seconds_from_nanos(second + second / 2 - 1),
            1
        );
        assert_eq!(alarm_remaining_seconds_from_nanos(second + second / 2), 2);
        assert_eq!(
            alarm_remaining_seconds_from_nanos((u128::from(u32::MAX) + 1) * second),
            0
        );
        assert_eq!(
            alarm_remaining_seconds(TimeValue::new(u64::MAX, (NANOS_PER_SEC / 2) as u32)),
            0
        );
    }

    #[test]
    fn alarm_seconds_to_nanos_accepts_the_unsigned_api_boundary() {
        assert_eq!(alarm_seconds_to_nanos(0), Ok(0));
        assert_eq!(
            alarm_seconds_to_nanos(u32::MAX),
            Ok(u64::from(u32::MAX) as usize * NANOS_PER_SEC as usize)
        );
    }
}
