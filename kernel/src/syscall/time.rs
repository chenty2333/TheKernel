use core::time::Duration;

use axerrno::{AxError, AxResult};
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time, monotonic_time_nanos};
use axtask::current;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    __kernel_clockid_t, CAP_SYS_TIME, CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC,
    CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
    CLOCK_REALTIME_ALARM, CLOCK_REALTIME_COARSE, CLOCK_TAI, CLOCK_THREAD_CPUTIME_ID, SIGEV_NONE,
    SIGEV_SIGNAL, SIGEV_THREAD, SIGEV_THREAD_ID, TIMER_ABSTIME, itimerspec, itimerval, sigevent,
    timespec, timeval, timezone,
};
use starry_signal::Signo;
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{
        AlarmClock, AsThread, ITimerType, PosixTimer, PosixTimerClock, PosixTimerNotify, TaskUsage,
        get_task, nanos_to_clock_ticks, poll_timer, register_posix_timer_alarm,
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

fn sys_do_clock_adjtime(
    clock_id: __kernel_clockid_t,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    if clock_id as u32 != CLOCK_REALTIME {
        return Err(AxError::InvalidInput);
    }

    let mut timex = unsafe { timex_ptr.vm_read_uninit()?.assume_init() };
    let modes = timex.modes;
    if timex_invalid_adjadjtime_mode(modes) {
        return Err(AxError::InvalidInput);
    }
    let privileged = current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_TIME);

    if !privileged && modes != 0 && modes != ADJ_OFFSET_SS_READ {
        return Err(AxError::OperationNotPermitted);
    }

    let mut state = TIMEX_STATE.lock();
    if modes != 0 && modes != ADJ_OFFSET_SS_READ {
        update_timex_state(&mut state, &timex)?;
    }

    fill_timex_output(&mut timex, *state);
    timex_ptr.vm_write(timex)?;
    Ok(state.time_state())
}

fn posix_timer_clock(clock_id: __kernel_clockid_t) -> AxResult<PosixTimerClock> {
    match clock_domain(clock_id)? {
        ClockDomain::Realtime | ClockDomain::RealtimeCoarse => Ok(PosixTimerClock::Realtime),
        ClockDomain::Monotonic | ClockDomain::MonotonicCoarse => Ok(PosixTimerClock::Monotonic),
        ClockDomain::ProcessCpu => Ok(PosixTimerClock::ProcessCpu),
        ClockDomain::ThreadCpu => Ok(PosixTimerClock::ThreadCpu),
        ClockDomain::Tai => Ok(PosixTimerClock::Tai),
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

fn decode_timer_notify(event: Option<sigevent>) -> AxResult<PosixTimerNotify> {
    let Some(event) = event else {
        return Ok(PosixTimerNotify::Signal {
            signo: Signo::SIGALRM,
            target_tid: None,
        });
    };

    match event.sigev_notify as u32 {
        SIGEV_NONE => Ok(PosixTimerNotify::None),
        SIGEV_SIGNAL | SIGEV_THREAD => {
            let signo = Signo::from_repr(event.sigev_signo as u8).ok_or(AxError::InvalidInput)?;
            Ok(PosixTimerNotify::Signal {
                signo,
                target_tid: None,
            })
        }
        SIGEV_THREAD_ID => {
            let signo = Signo::from_repr(event.sigev_signo as u8).ok_or(AxError::InvalidInput)?;
            let tid = unsafe { event._sigev_un._tid };
            if tid <= 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(PosixTimerNotify::Signal {
                signo,
                target_tid: Some(tid as _),
            })
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn timer_absolute_deadline(
    clock: PosixTimerClock,
    clock_id: __kernel_clockid_t,
    value: Duration,
) -> AxResult<Duration> {
    Ok(match clock {
        PosixTimerClock::Realtime => value,
        PosixTimerClock::Tai => {
            saturating_sub_duration(value, duration_from_secs(DEFAULT_TAI_OFFSET_SECS))
        }
        PosixTimerClock::Monotonic => value,
        PosixTimerClock::ProcessCpu | PosixTimerClock::ThreadCpu => {
            let now_clock = clock_now(clock_id)?;
            let delta = saturating_sub_duration(value, now_clock);
            saturating_add_duration(AlarmClock::Monotonic.now(), delta)
        }
    })
}

fn timer_relative_deadline(clock: PosixTimerClock, value: Duration) -> Duration {
    saturating_add_duration(clock.alarm_clock().now(), value)
}

fn timer_remaining(timer: &PosixTimer) -> Duration {
    timer
        .deadline
        .map(|deadline| saturating_sub_duration(deadline, timer.clock.alarm_clock().now()))
        .unwrap_or(Duration::ZERO)
}

pub fn sys_timer_create(
    clock_id: __kernel_clockid_t,
    sigevent_ptr: *const sigevent,
    timerid_ptr: *mut i32,
) -> AxResult<isize> {
    if timerid_ptr.is_null() {
        return Err(AxError::BadAddress);
    }

    let clock = posix_timer_clock(clock_id)?;
    let notify = if let Some(ptr) = sigevent_ptr.nullable() {
        decode_timer_notify(Some(unsafe { ptr.vm_read_uninit()?.assume_init() }))?
    } else {
        decode_timer_notify(None)?
    };

    let proc_data = current().as_thread().proc_data.clone();
    let timerid = {
        let mut timers = proc_data.posix_timers.lock();
        if let Some((idx, slot)) = timers
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(PosixTimer::new(clock, notify));
            idx
        } else {
            timers.push(Some(PosixTimer::new(clock, notify)));
            timers.len() - 1
        }
    };

    timerid_ptr.vm_write(timerid as i32)?;
    Ok(0)
}

pub fn sys_timer_settime(
    timerid: i32,
    flags: i32,
    new_value: *const itimerspec,
    old_value: *mut itimerspec,
) -> AxResult<isize> {
    if new_value.is_null() {
        return Err(AxError::InvalidInput);
    }

    let new_value = unsafe { new_value.vm_read_uninit()?.assume_init() };
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

    let (old_interval, old_remaining) = {
        let mut timers = proc_data.posix_timers.lock();
        let timer = timers
            .get_mut(timerid as usize)
            .and_then(Option::as_mut)
            .ok_or(AxError::InvalidInput)?;
        let old_interval = timer.interval;
        let old_remaining = timer_remaining(timer);

        timer.interval = interval;
        timer.overrun = 0;
        timer.signal_pending = false;
        timer.sequence = timer.sequence.wrapping_add(1);
        timer.deadline = if value.is_zero() {
            None
        } else if absolute {
            Some(timer_absolute_deadline(
                timer.clock,
                clock_id_for_timer(timer.clock),
                value,
            )?)
        } else {
            Some(timer_relative_deadline(timer.clock, value))
        };

        if let Some(deadline) = timer.deadline {
            register_posix_timer_alarm(
                &proc_data,
                timerid as usize,
                timer.clock.alarm_clock(),
                deadline,
                timer.sequence,
            );
        }

        (old_interval, old_remaining)
    };

    if let Some(old_value) = old_value.nullable() {
        old_value.vm_write(itimerspec {
            it_interval: duration_to_timespec(old_interval),
            it_value: duration_to_timespec(old_remaining),
        })?;
    }

    Ok(0)
}

pub fn sys_timer_gettime(timerid: i32, curr_value: *mut itimerspec) -> AxResult<isize> {
    if timerid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    let (interval, remaining) = {
        let timers = proc_data.posix_timers.lock();
        let timer = timers
            .get(timerid as usize)
            .and_then(Option::as_ref)
            .ok_or(AxError::InvalidInput)?;
        (timer.interval, timer_remaining(timer))
    };

    curr_value.vm_write(itimerspec {
        it_interval: duration_to_timespec(interval),
        it_value: duration_to_timespec(remaining),
    })?;
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
    let mut timers = proc_data.posix_timers.lock();
    let slot = timers
        .get_mut(timerid as usize)
        .ok_or(AxError::InvalidInput)?;
    if slot.is_none() {
        return Err(AxError::InvalidInput);
    }
    *slot = None;
    Ok(0)
}

fn clock_id_for_timer(clock: PosixTimerClock) -> __kernel_clockid_t {
    match clock {
        PosixTimerClock::Realtime => CLOCK_REALTIME as _,
        PosixTimerClock::Monotonic => CLOCK_MONOTONIC as _,
        PosixTimerClock::Tai => CLOCK_TAI as _,
        PosixTimerClock::ProcessCpu => CLOCK_PROCESS_CPUTIME_ID as _,
        PosixTimerClock::ThreadCpu => CLOCK_THREAD_CPUTIME_ID as _,
    }
}

pub fn sys_clock_gettime(clock_id: __kernel_clockid_t, ts: *mut timespec) -> AxResult<isize> {
    let now = clock_now(clock_id)?;
    ts.vm_write(timespec::from_time_value(now))?;
    Ok(0)
}

pub fn sys_gettimeofday(ts: *mut timeval, tz: *mut timezone) -> AxResult<isize> {
    let now = wall_time();
    if let Some(ts) = ts.nullable() {
        ts.vm_write(timeval::from_time_value(now))?;
    }
    if let Some(tz) = tz.nullable() {
        tz.vm_write(timezone {
            tz_minuteswest: 0,
            tz_dsttime: 0,
        })?;
    }
    Ok(0)
}

pub fn sys_settimeofday(ts: *const timeval, tz: *const timezone) -> AxResult<isize> {
    let ts = if let Some(ts) = ts.nullable() {
        Some(unsafe { ts.vm_read_uninit()?.assume_init() })
    } else {
        None
    };

    let tz = if let Some(tz) = tz.nullable() {
        Some(unsafe { tz.vm_read_uninit()?.assume_init() })
    } else {
        None
    };

    let ts = ts.map(TimeValueLike::try_into_time_value).transpose()?;
    if !current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_TIME)
    {
        return Err(AxError::OperationNotPermitted);
    }

    if let Some(tz) = tz {
        if tz.tz_minuteswest < -15 * 60 || tz.tz_minuteswest > 15 * 60 {
            return Err(AxError::InvalidInput);
        }
    }

    if let Some(ts) = ts {
        set_wall_time(ts);
    }
    Ok(0)
}

pub fn sys_clock_getres(clock_id: __kernel_clockid_t, res: *mut timespec) -> AxResult<isize> {
    let resolution = clock_resolution(clock_id)?;
    if let Some(res) = res.nullable() {
        res.vm_write(timespec::from_time_value(resolution))?;
    }
    Ok(0)
}

pub fn sys_clock_settime(clock_id: __kernel_clockid_t, ts: *const timespec) -> AxResult<isize> {
    match clock_id as u32 {
        CLOCK_REALTIME => {
            let ts = unsafe { ts.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
            if !current()
                .as_thread()
                .proc_data
                .has_effective_capability(CAP_SYS_TIME)
            {
                return Err(AxError::OperationNotPermitted);
            }
            set_wall_time(ts);
            Ok(0)
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_clock_adjtime(
    clock_id: __kernel_clockid_t,
    timex_ptr: *mut KernelOldTimex,
) -> AxResult<isize> {
    sys_do_clock_adjtime(clock_id, timex_ptr)
}

pub fn sys_adjtimex(timex_ptr: *mut KernelOldTimex) -> AxResult<isize> {
    sys_do_clock_adjtime(CLOCK_REALTIME as _, timex_ptr)
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
    if let Some(tms) = tms.nullable() {
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
    }
    Ok(nanos_to_clock_ticks(monotonic_time_nanos()) as _)
}

pub fn sys_getitimer(which: i32, value: *mut itimerval) -> AxResult<isize> {
    let ty = ITimerType::from_repr(which).ok_or(AxError::InvalidInput)?;
    let curr = current();
    poll_timer(&curr);
    let (it_interval, it_value) = curr.as_thread().time.borrow().get_itimer(ty);

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

    poll_timer(&curr);
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
