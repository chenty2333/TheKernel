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
    __kernel_clockid_t, __kernel_old_time_t, CAP_SYS_TIME, CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM,
    CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID,
    CLOCK_REALTIME, CLOCK_REALTIME_ALARM, CLOCK_REALTIME_COARSE, CLOCK_TAI,
    CLOCK_THREAD_CPUTIME_ID, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD, SIGEV_THREAD_ID,
    TIMER_ABSTIME, itimerspec, itimerval, timespec, timeval, timezone,
};
use thekernel_linux_signal::Signo;
use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    mm::map_usercopy_error,
    syscall::RawSigevent,
    task::{
        AlarmClock, AlarmTokenReserveError, AsThread, ITimerType, PosixTimer, PosixTimerClock,
        PosixTimerNotify, TaskUsage, get_process_itimer, get_task, poll_timer, set_process_itimer,
        times_clock_ticks,
    },
    time::{TimeValueLike, set_wall_time, wall_time, wall_time_nanos},
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
const ADJ_OFFSET: u32 = 0x0001;
const ADJ_FREQUENCY: u32 = 0x0002;
const ADJ_MAXERROR: u32 = 0x0004;
const ADJ_ESTERROR: u32 = 0x0008;
const ADJ_STATUS: u32 = 0x0010;
const ADJ_TIMECONST: u32 = 0x0020;
const ADJ_MICRO: u32 = 0x1000;
const ADJ_NANO: u32 = 0x2000;
const ADJ_TICK: u32 = 0x4000;
const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
const ADJ_OFFSET_SS_READ: u32 = 0xa001;
const ADJ_ALL: u32 = ADJ_OFFSET
    | ADJ_FREQUENCY
    | ADJ_MAXERROR
    | ADJ_ESTERROR
    | ADJ_STATUS
    | ADJ_TIMECONST
    | ADJ_TICK;

const STA_PLL: i32 = 0x0001;
const STA_PPSFREQ: i32 = 0x0002;
const STA_PPSTIME: i32 = 0x0004;
const STA_FLL: i32 = 0x0008;
const STA_INS: i32 = 0x0010;
const STA_DEL: i32 = 0x0020;
const STA_UNSYNC: i32 = 0x0040;
const STA_FREQHOLD: i32 = 0x0080;
const STA_NANO: i32 = 0x2000;
const STA_MODE: i32 = 0x4000;

const TIME_OK: isize = 0;
const TIME_ERROR: isize = 5;
const CPUCLOCK_PROF: i32 = 0;
const CPUCLOCK_VIRT: i32 = 1;
const CPUCLOCK_SCHED: i32 = 2;
const CPUCLOCK_MAX: i32 = 3;
const CPUCLOCK_PERTHREAD_MASK: i32 = 4;
const CPUCLOCK_CLOCK_MASK: i32 = 3;
const CLOCKFD: i32 = CPUCLOCK_MAX;
const CLOCKFD_MASK: i32 = CPUCLOCK_PERTHREAD_MASK | CPUCLOCK_CLOCK_MASK;

const TIMEX_SETTABLE_STATUS_BITS: i32 = STA_PLL
    | STA_PPSFREQ
    | STA_PPSTIME
    | STA_FLL
    | STA_INS
    | STA_DEL
    | STA_UNSYNC
    | STA_FREQHOLD
    | STA_MODE;
const TIMEX_SETTABLE_BIT_MODES: u32 = ADJ_ALL | ADJ_MICRO | ADJ_NANO;
const ADJ_SINGLESHOT_FLAG: u32 = ADJ_OFFSET_SINGLESHOT & !ADJ_OFFSET;

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
            return Ok(current()
                .as_thread()
                .proc_data
                .time_ns()
                .apply_monotonic_offset(now));
        }
        CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => {
            return Ok(current()
                .as_thread()
                .proc_data
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

#[derive(Clone, Copy, Debug)]
struct TimexState {
    offset: i64,
    freq: i64,
    maxerror: i64,
    esterror: i64,
    status: i32,
    constant: i64,
    precision: i64,
    tolerance: i64,
    tick: i64,
    tai: i32,
}

impl TimexState {
    const fn new() -> Self {
        Self {
            offset: 0,
            freq: 0,
            maxerror: 0,
            esterror: 0,
            status: 0,
            constant: 0,
            precision: 1,
            tolerance: 0,
            tick: (1_000_000 / axconfig::TICKS_PER_SEC as u64) as i64,
            tai: DEFAULT_TAI_OFFSET_SECS as i32,
        }
    }

    fn resolution_mode(self) -> u32 {
        if self.status & STA_NANO != 0 {
            ADJ_NANO
        } else {
            ADJ_MICRO
        }
    }

    fn time_state(self) -> isize {
        if self.status & STA_UNSYNC != 0 {
            TIME_ERROR
        } else {
            TIME_OK
        }
    }
}

static TIMEX_STATE: SpinNoIrq<TimexState> = SpinNoIrq::new(TimexState::new());

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

fn cpu_clock_now(clock_id: __kernel_clockid_t) -> AxResult<TimeValue> {
    let decoded = decode_cpu_clock_id(clock_id).ok_or(AxError::InvalidInput)?;
    let usage = match decoded.target {
        CpuClockTarget::Process(0) => current().as_thread().proc_data.self_usage(),
        CpuClockTarget::Thread(0) => TaskUsage::from_thread(current().as_thread()),
        CpuClockTarget::Process(pid) => {
            let task = get_task(pid)?;
            task.as_thread().proc_data.self_usage()
        }
        CpuClockTarget::Thread(tid) => {
            let task = get_task(tid)?;
            TaskUsage::from_thread(task.as_thread())
        }
    };
    Ok(usage_value_for_cpu_clock(usage, decoded.which))
}

fn timex_tick_bounds() -> (i64, i64) {
    let hz = axconfig::TICKS_PER_SEC as i64;
    (900_000 / hz, 1_100_000 / hz)
}

fn timex_resolution_is_nanos(modes: u32, state: TimexState) -> bool {
    if modes & ADJ_NANO != 0 {
        true
    } else if modes & ADJ_MICRO != 0 {
        false
    } else {
        state.status & STA_NANO != 0
    }
}

fn timex_modes_supported(modes: u32) -> bool {
    match modes {
        0 | ADJ_OFFSET_SINGLESHOT | ADJ_OFFSET_SS_READ => true,
        _ if modes & ADJ_SINGLESHOT_FLAG != 0 => false,
        _ => modes & !TIMEX_SETTABLE_BIT_MODES == 0,
    }
}

fn timex_invalid_adjadjtime_mode(modes: u32) -> bool {
    modes & ADJ_SINGLESHOT_FLAG != 0 && modes & ADJ_OFFSET == 0
}

fn fill_timex_output(timex: &mut KernelOldTimex, state: TimexState) {
    timex.modes = state.resolution_mode();
    timex.offset = state.offset;
    timex.freq = state.freq;
    timex.maxerror = state.maxerror;
    timex.esterror = state.esterror;
    timex.status = state.status;
    timex.constant = state.constant;
    timex.precision = state.precision;
    timex.tolerance = state.tolerance;
    timex.time = timeval::from_time_value(wall_time());
    timex.tick = state.tick;
    timex.ppsfreq = 0;
    timex.jitter = 0;
    timex.shift = 0;
    timex.stabil = 0;
    timex.jitcnt = 0;
    timex.calcnt = 0;
    timex.errcnt = 0;
    timex.stbcnt = 0;
    timex.tai = state.tai;
}

fn update_timex_state(state: &mut TimexState, timex: &KernelOldTimex) -> AxResult<()> {
    let modes = timex.modes;

    if !timex_modes_supported(modes) {
        return Err(AxError::InvalidInput);
    }

    if modes & ADJ_MICRO != 0 {
        state.status &= !STA_NANO;
    }
    if modes & ADJ_NANO != 0 {
        state.status |= STA_NANO;
    }

    if modes & ADJ_STATUS != 0 {
        if timex.status & !TIMEX_SETTABLE_STATUS_BITS != 0 {
            return Err(AxError::InvalidInput);
        }
        state.status = (state.status & !TIMEX_SETTABLE_STATUS_BITS)
            | (timex.status & TIMEX_SETTABLE_STATUS_BITS);
    }

    if modes & ADJ_OFFSET != 0 {
        let limit = if timex_resolution_is_nanos(modes, *state) {
            500_000_i64 * 1000
        } else {
            500_000_i64
        };
        if timex.offset <= -limit || timex.offset >= limit {
            return Err(AxError::InvalidInput);
        }
        state.offset = timex.offset;
    }

    if modes & ADJ_FREQUENCY != 0 {
        state.freq = timex.freq.clamp(-32_768_000, 32_768_000);
    }

    if modes & ADJ_MAXERROR != 0 {
        state.maxerror = timex.maxerror;
    }

    if modes & ADJ_ESTERROR != 0 {
        state.esterror = timex.esterror;
    }

    if modes & ADJ_TIMECONST != 0 {
        state.constant = timex.constant;
    }

    if modes & ADJ_TICK != 0 {
        let (min_tick, max_tick) = timex_tick_bounds();
        if timex.tick < min_tick || timex.tick > max_tick {
            return Err(AxError::InvalidInput);
        }
        state.tick = timex.tick;
    }

    if modes == ADJ_OFFSET_SINGLESHOT {
        state.offset = timex.offset;
    }

    Ok(())
}

fn sys_do_clock_adjtime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    if clock_id as u32 != CLOCK_REALTIME {
        return Err(AxError::InvalidInput);
    }

    let mut timex = unsafe {
        VmPtr::vm_read_uninit(timex_ptr, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    let modes = timex.modes;
    if timex_invalid_adjadjtime_mode(modes) {
        return Err(AxError::InvalidInput);
    }
    let privileged = current().as_thread().has_effective_capability(CAP_SYS_TIME);

    if !privileged && modes != 0 && modes != ADJ_OFFSET_SS_READ {
        return Err(AxError::OperationNotPermitted);
    }

    let mut state = TIMEX_STATE.lock();
    if modes != 0 && modes != ADJ_OFFSET_SS_READ {
        update_timex_state(&mut state, &timex)?;
    }

    fill_timex_output(&mut timex, *state);
    // SAFETY: `timex` was initialized by the preceding copy-in and every
    // field update preserves its fully initialized object representation.
    unsafe { VmMutPtr::vm_write_unchecked(timex_ptr, memory, timex) }
        .map_err(map_usercopy_error)?;
    Ok(state.time_state())
}

fn posix_timer_clock(clock_id: __kernel_clockid_t) -> AxResult<PosixTimerClock> {
    match clock_domain(clock_id)? {
        ClockDomain::Realtime | ClockDomain::RealtimeCoarse => Ok(PosixTimerClock::Realtime),
        ClockDomain::Monotonic | ClockDomain::MonotonicCoarse => Ok(PosixTimerClock::Monotonic),
        // CPU-clock POSIX timers need an accounting-threshold owner, not a
        // wall-monotonic alarm. Reject them honestly until that reusable
        // mechanism exists instead of firing while the target is asleep.
        ClockDomain::ProcessCpu | ClockDomain::ThreadCpu => Err(AxError::InvalidInput),
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
        SIGEV_SIGNAL | SIGEV_THREAD => {
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
            saturating_sub_duration(value, duration_from_secs(DEFAULT_TAI_OFFSET_SECS))
        }
        PosixTimerClock::Monotonic => current()
            .as_thread()
            .proc_data
            .time_ns()
            .host_monotonic_deadline(value),
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

fn timer_remaining(timer: &PosixTimer) -> Duration {
    timer
        .deadline
        .map(|deadline| saturating_sub_duration(deadline, timer.effective_clock.now()))
        .unwrap_or(Duration::ZERO)
}

/// Converts the process ITIMER_REAL remainder to the unsigned-seconds result
/// required by alarm(2). Linux adds one second only for a sub-second remainder
/// or one of at least 500ms, then returns the native unsigned-int low word.
fn alarm_remaining_seconds_from_nanos(nanos: u128) -> u32 {
    let seconds = nanos / NANOS_PER_SEC as u128;
    let subsecond_nanos = nanos % NANOS_PER_SEC as u128;
    let round_up = (seconds == 0 && subsecond_nanos != 0)
        || subsecond_nanos >= (NANOS_PER_SEC / 2) as u128;
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
    let value = thekernel_linux_usercopy::VmPtr::vm_read_uninit(ptr, memory)
        .map_err(map_timer_usercopy_error)?;
    // SAFETY: the explicit provider initialized every byte of the value, and
    // `itimerspec` contains only integer fields on the supported x86_64 ABI.
    Ok(unsafe { value.assume_init() })
}

fn write_timer_id<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut i32,
    timerid: i32,
) -> AxResult<()> {
    thekernel_linux_usercopy::VmMutPtr::vm_write(ptr, memory, timerid)
        .map_err(map_timer_usercopy_error)
}

fn write_timer_spec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut itimerspec,
    value: itimerspec,
) -> AxResult<()> {
    // `linux_raw_sys` does not expose bytemuck's `NoUninit` marker for its
    // repr(C) ABI structs.  The x86_64 `itimerspec` is four integer words with
    // no padding, so its complete object representation is initialized here.
    unsafe { thekernel_linux_usercopy::VmMutPtr::vm_write_unchecked(ptr, memory, value) }
        .map_err(map_timer_usercopy_error)
}

fn read_itimer_value<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const itimerval,
) -> AxResult<itimerval> {
    let value = thekernel_linux_usercopy::VmPtr::vm_read_uninit(ptr, memory)
        .map_err(map_timer_usercopy_error)?;
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
    unsafe { thekernel_linux_usercopy::VmMutPtr::vm_write_unchecked(ptr, memory, value) }
        .map_err(map_timer_usercopy_error)
}

pub fn sys_timer_create<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    sigevent_ptr: *const RawSigevent,
    timerid_ptr: *mut i32,
) -> AxResult<isize> {
    if timerid_ptr.is_null() {
        return Err(AxError::BadAddress);
    }

    let clock = posix_timer_clock(clock_id)?;
    let notify = if let Some(ptr) = thekernel_linux_usercopy::VmPtr::nullable(sigevent_ptr) {
        decode_timer_notify(Some(
            RawSigevent::read_from_user(memory, ptr).map_err(map_timer_usercopy_error)?,
        ))?
    } else {
        decode_timer_notify(None)?
    };

    let proc_data = current().as_thread().proc_data.clone();
    // Main and optional signal-retry alarm leases are acquired atomically
    // before the timer ID becomes visible.  A published timer therefore never
    // needs a fallible alarm allocation during settime or periodic rearm.
    let candidate = PosixTimer::try_new(clock, notify).map_err(map_posix_timer_admission_error)?;
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

    if let Err(error) = write_timer_id(memory, timerid_ptr, timerid as i32) {
        // The slot remains deliberately unpublished while copyout may fault.
        // Other threads reject operations on it, so rollback cannot delete a
        // timer that another thread has observed or recreated.
        let retired = {
            let mut timers = proc_data.posix_timers.lock();
            let slot = timers
                .get_mut(timerid)
                .expect("reserved POSIX timer slot disappeared during copyout");
            debug_assert!(slot.as_ref().is_some_and(|timer| !timer.is_published()));
            slot.take()
        };
        drop(retired);
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

    let (old_interval, old_remaining, retry_publication, main_publication) = {
        let mut timers = proc_data.posix_timers.lock();
        let timer = timers
            .get_mut(timerid as usize)
            .and_then(Option::as_mut)
            .filter(|timer| timer.is_published())
            .ok_or(AxError::InvalidInput)?;
        let old_interval = timer.interval;
        let old_remaining = timer_remaining(timer);
        let sequence = timer.sequence.checked_add(1).ok_or(AxError::OutOfRange)?;
        let effective_clock = timer_effective_alarm_clock(timer.clock, absolute);
        let deadline = if value.is_zero() {
            None
        } else if absolute {
            Some(timer_absolute_deadline(timer.clock, value)?)
        } else {
            Some(timer_relative_deadline(value))
        };

        let retry_publication = timer.reset_signal_delivery();
        timer.interval = interval;
        timer.sequence = sequence;
        timer.effective_clock = effective_clock;
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
        (timer.interval, timer_remaining(timer))
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
    let overrun = {
        let timers = proc_data.posix_timers.lock();
        timers
            .get(timerid as usize)
            .and_then(Option::as_ref)
            .filter(|timer| timer.is_published())
            .ok_or(AxError::InvalidInput)?
            .overrun
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
    Ok(0)
}

pub fn sys_clock_gettime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    ts: *mut timespec,
) -> AxResult<isize> {
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
        // SAFETY: `timezone` contains only its two initialized i32 fields;
        // generated linux_raw_sys layout is asserted by the compiler below.
        unsafe {
            VmMutPtr::vm_write_unchecked(
                tz,
                memory,
                timezone {
                    tz_minuteswest: 0,
                    tz_dsttime: 0,
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
    let ts = if let Some(ts) = VmPtr::nullable(ts) {
        Some(unsafe {
            VmPtr::vm_read_uninit(ts, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        })
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

    let ts = ts.map(TimeValueLike::try_into_time_value).transpose()?;
    if !current().as_thread().has_effective_capability(CAP_SYS_TIME) {
        return Err(AxError::OperationNotPermitted);
    }

    if let Some(tz) = tz
        && (tz.tz_minuteswest < -15 * 60 || tz.tz_minuteswest > 15 * 60)
    {
        return Err(AxError::InvalidInput);
    }

    if let Some(ts) = ts {
        set_wall_time(ts)?;
    }
    Ok(0)
}

pub fn sys_clock_getres<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    res: *mut timespec,
) -> AxResult<isize> {
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
    match clock_id as u32 {
        CLOCK_REALTIME => {
            let ts = unsafe {
                VmPtr::vm_read_uninit(ts, memory)
                    .map_err(map_usercopy_error)?
                    .assume_init()
            }
            .try_into_time_value()?;
            if !current().as_thread().has_effective_capability(CAP_SYS_TIME) {
                return Err(AxError::OperationNotPermitted);
            }
            set_wall_time(ts)?;
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_clock_adjtime<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    clock_id: __kernel_clockid_t,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    sys_do_clock_adjtime(memory, clock_id, timex_ptr)
}

pub fn sys_adjtimex<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    sys_do_clock_adjtime(memory, CLOCK_REALTIME as _, timex_ptr)
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
    use thekernel_linux_usercopy::{UserCopyError, UserMemory, UserMemoryContext, VmResult};

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

    #[test]
    fn timex_modes_reject_invalid_singleshot_marker() {
        assert!(timex_modes_supported(0));
        assert!(timex_modes_supported(ADJ_OFFSET_SINGLESHOT));
        assert!(timex_modes_supported(ADJ_OFFSET_SS_READ));
        assert!(!timex_modes_supported(ADJ_SINGLESHOT_FLAG));
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
        assert_eq!(
            posix_timer_clock(CLOCK_PROCESS_CPUTIME_ID as _),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            posix_timer_clock(CLOCK_THREAD_CPUTIME_ID as _),
            Err(AxError::InvalidInput)
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
        assert_eq!(
            alarm_remaining_seconds_from_nanos(second + second / 2),
            2
        );
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
