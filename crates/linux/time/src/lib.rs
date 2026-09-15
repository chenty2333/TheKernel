#![no_std]
#![forbid(unsafe_code)]
//! Pure Linux clock and `adjtimex(2)` validation, update, and render plans.

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub const CLOCK_MONOTONIC_RAW: i32 = 4;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
pub const CLOCK_BOOTTIME: i32 = 7;
pub const CLOCK_REALTIME_ALARM: i32 = 8;
pub const CLOCK_BOOTTIME_ALARM: i32 = 9;
pub const CLOCK_TAI: i32 = 11;
pub const ADJ_OFFSET: u32 = 0x0001;
pub const ADJ_FREQUENCY: u32 = 0x0002;
pub const ADJ_MAXERROR: u32 = 0x0004;
pub const ADJ_ESTERROR: u32 = 0x0008;
pub const ADJ_STATUS: u32 = 0x0010;
pub const ADJ_TIMECONST: u32 = 0x0020;
pub const ADJ_TAI: u32 = 0x0080;
pub const ADJ_SETOFFSET: u32 = 0x0100;
pub const ADJ_MICRO: u32 = 0x1000;
pub const ADJ_NANO: u32 = 0x2000;
pub const ADJ_TICK: u32 = 0x4000;
/// Kernel-side `ADJ_ADJTIME` (`include/linux/timex.h:58`): select the
/// `adjtime(3)` mode.  The userspace spelling of the singleshot mode is
/// `ADJ_ADJTIME | ADJ_OFFSET` (`0x8001`, `include/uapi/linux/timex.h:163`).
pub const ADJ_ADJTIME: u32 = 0x8000;
/// Kernel-side `ADJ_OFFSET_SINGLESHOT` (`include/linux/timex.h:59`), the
/// `ADJ_OFFSET` bit reused as the "singleshot" marker inside
/// `ADJ_ADJTIME` mode.  It is *not* the userspace `0x8001` value.
pub const ADJ_OFFSET_SINGLESHOT: u32 = 0x0001;
/// Kernel-side `ADJ_OFFSET_READONLY` (`include/linux/timex.h:60`), the same bit
/// as `ADJ_NANO`; inside `ADJ_ADJTIME` mode it requests a read of the
/// `adjtime(3)` residual instead of a replacement.
pub const ADJ_OFFSET_READONLY: u32 = 0x2000;
/// `ADJ_OFFSET_SS_READ` (`include/uapi/linux/timex.h:164`): `adjtime(3)` read.
pub const ADJ_OFFSET_SS_READ: u32 = ADJ_ADJTIME | ADJ_OFFSET | ADJ_OFFSET_READONLY;
pub const STA_PLL: i32 = 0x0001;
pub const STA_PPSFREQ: i32 = 0x0002;
pub const STA_PPSTIME: i32 = 0x0004;
pub const STA_FLL: i32 = 0x0008;
pub const STA_INS: i32 = 0x0010;
pub const STA_DEL: i32 = 0x0020;
pub const STA_UNSYNC: i32 = 0x0040;
pub const STA_FREQHOLD: i32 = 0x0080;
pub const STA_PPSSIGNAL: i32 = 0x0100;
pub const STA_PPSJITTER: i32 = 0x0200;
pub const STA_PPSWANDER: i32 = 0x0400;
pub const STA_PPSERROR: i32 = 0x0800;
pub const STA_CLOCKERR: i32 = 0x1000;
pub const STA_NANO: i32 = 0x2000;
pub const STA_MODE: i32 = 0x4000;
pub const STA_CLK: i32 = 0x8000;
/// `STA_RONLY` (`include/uapi/linux/timex.h:196-198`): status bits the caller
/// may never set; `process_adj_status()` preserves the old value of exactly
/// these bits.
pub const STA_RONLY: i32 = STA_PPSSIGNAL
    | STA_PPSJITTER
    | STA_PPSWANDER
    | STA_PPSERROR
    | STA_CLOCKERR
    | STA_NANO
    | STA_MODE
    | STA_CLK;
pub const TIME_OK: i32 = 0;
pub const TIME_INS: i32 = 1;
pub const TIME_DEL: i32 = 2;
pub const TIME_OOP: i32 = 3;
pub const TIME_WAIT: i32 = 4;
pub const TIME_ERROR: i32 = 5;

/// Linux `USER_HZ` (`arch/x86/include/asm/param.h`): the `struct timex` tick
/// bounds are expressed in 1/USER_HZ second units, independent of the kernel's
/// internal tick rate (`timekeeping_validate_timex()`,
/// `kernel/time/timekeeping.c:2844-2846`).
pub const USER_HZ: i64 = 100;
/// `ADJ_TICK` bounds `900000/USER_HZ ..= 1100000/USER_HZ`
/// (`kernel/time/timekeeping.c:2844-2846`).
pub const TICK_MIN: i64 = 900_000 / USER_HZ;
/// See [`TICK_MIN`].
pub const TICK_MAX: i64 = 1_100_000 / USER_HZ;
/// `tick_usec` of one USER_HZ tick (`ntp_init()`, `kernel/time/ntp.c:96`).
pub const TICK_USEC: i64 = 1_000_000 / USER_HZ;
/// `MAX_TAI_OFFSET` (`kernel/time/ntp.c:74`).
pub const MAX_TAI_OFFSET: i64 = 100_000;
/// `MAXPHASE` (`include/linux/timex.h:134`): the largest phase correction, in
/// nanoseconds.
pub const MAXPHASE: i64 = 500_000_000;
/// `NTP_PHASE_LIMIT` (`include/linux/timex.h:138`): the initial and maximum
/// dispersion reported by `maxerror`/`esterror`.
pub const NTP_PHASE_LIMIT: i64 = (MAXPHASE / 1_000) << 5;
/// `MAXTC` (`include/linux/timex.h:122`): the largest PLL time constant.
pub const MAXTC: i64 = 10;
/// `NTP_SCALE_SHIFT` (`include/linux/timex.h:149`).
pub const NTP_SCALE_SHIFT: u32 = 32;
/// `SHIFT_USEC` (`include/linux/timex.h:129`).
pub const SHIFT_USEC: u32 = 16;
/// `PPM_SCALE` (`include/linux/timex.h:130`): scaled-ppm units per ppm.
pub const PPM_SCALE: i64 = 1_000 << (NTP_SCALE_SHIFT - SHIFT_USEC);
/// `PPM_SCALE_INV_SHIFT` (`include/linux/timex.h:131`).
pub const PPM_SCALE_INV_SHIFT: u32 = 19;
/// `PPM_SCALE_INV` (`include/linux/timex.h:132-133`).
pub const PPM_SCALE_INV: i64 = (1i64 << (PPM_SCALE_INV_SHIFT + NTP_SCALE_SHIFT)) / PPM_SCALE + 1;
/// `MAXFREQ_SCALED` (`include/linux/timex.h:137`): the frequency clamp, and
/// therefore the value `tolerance` reports.
pub const MAXFREQ_SCALED: i64 = 500_000 << NTP_SCALE_SHIFT;
/// `txc->tolerance` (`kernel/time/ntp.c:824`).
pub const TOLERANCE: i64 = MAXFREQ_SCALED / PPM_SCALE;
/// `txc->precision` (`kernel/time/ntp.c:823`).
pub const PRECISION: i64 = 1;
/// `TIME_SETTOD_SEC_MAX` (`include/linux/time64.h:44`): `KTIME_SEC_MAX` minus
/// 30 years.  Larger wall-clock seconds are rejected by
/// `timespec64_valid_settod()` (`include/linux/time64.h:118-127`) instead of
/// being saturated.
pub const TIME_SETTOD_SEC_MAX: i64 = 8_277_292_036;
/// Nanoseconds in one second.
pub const NANOS_PER_SEC: i128 = 1_000_000_000;

/// The resolution Linux reports for the CPU clocks that are charged once per
/// timer tick: `(NSEC_PER_SEC + hz - 1) / hz`, rounded up
/// (`posix_cpu_clock_getres()`, `kernel/time/posix-cpu-timers.c:159-176`).
/// `hz` is the internal timer tick rate (`CONFIG_HZ`), not `USER_HZ`; the
/// `*_COARSE` clocks truncate the same quotient instead of rounding it up.
pub const fn accounting_tick_resolution_nanos(tick_hz: u32) -> u64 {
    (1_000_000_000 + tick_hz as u64 - 1) / tick_hz as u64
}

/// How a syscall wants to use a wake-alarm clock.  Linux admits the two alarm
/// clocks per use, not per clock: reading one only needs a registered RTC
/// (`alarm_clock_get_timespec()`/`alarm_clock_getres()`,
/// `kernel/time/alarmtimer.c:600-620`), arming one also needs
/// `CAP_WAKE_ALARM` (`alarm_timer_create()`, `alarm_timer_nsleep()`,
/// `kernel/time/alarmtimer.c:651-665,766-790`), and `timerfd_create(2)` checks
/// only the capability (`fs/timerfd.c:424-444`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeAlarmUse {
    /// `clock_gettime(2)`/`clock_getres(2)`: EINVAL without an RTC.
    Read,
    /// `timer_create(2)`/`clock_nanosleep(2)`: EOPNOTSUPP without an RTC,
    /// EPERM without `CAP_WAKE_ALARM`.
    Arm,
    /// `timerfd_create(2)`: EPERM without `CAP_WAKE_ALARM`, no RTC test.
    TimerFd,
}

/// Why a wake-alarm clock was refused; the caller maps this to the errno of
/// its own entry point, exactly as Linux's separate implementations do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeAlarmReject {
    /// No RTC is registered, so the alarm clocks do not exist.
    NoRtc,
    /// `CAP_WAKE_ALARM` is missing.
    NotPermitted,
}

/// Whether `clock_id` is one of the wake-alarm clocks
/// (`include/uapi/linux/time.h`).
pub const fn is_wake_alarm_clock(clock_id: i32) -> bool {
    clock_id == CLOCK_REALTIME_ALARM || clock_id == CLOCK_BOOTTIME_ALARM
}

/// The single wake-alarm admission rule shared by `timer_create(2)`,
/// `clock_nanosleep(2)`, `clock_gettime(2)`, `clock_getres(2)` and
/// `timerfd_create(2)`; `rtc_available` is `alarmtimer_get_rtcdev()`
/// (`kernel/time/alarmtimer.c:640-650`) and `cap_wake_alarm` is
/// `capable(CAP_WAKE_ALARM)`.
pub const fn admit_wake_alarm(
    clock_id: i32,
    usage: WakeAlarmUse,
    rtc_available: bool,
    cap_wake_alarm: bool,
) -> Result<(), WakeAlarmReject> {
    if !is_wake_alarm_clock(clock_id) {
        return Ok(());
    }
    if !matches!(usage, WakeAlarmUse::TimerFd) && !rtc_available {
        return Err(WakeAlarmReject::NoRtc);
    }
    if matches!(usage, WakeAlarmUse::Arm | WakeAlarmUse::TimerFd) && !cap_wake_alarm {
        return Err(WakeAlarmReject::NotPermitted);
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clock {
    Realtime,
    Monotonic,
    ProcessCpu,
    ThreadCpu,
    MonotonicRaw,
    RealtimeCoarse,
    MonotonicCoarse,
    Boottime,
    Tai,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockRequest {
    GetTime {
        clock_id: i32,
    },
    GetResolution {
        clock_id: i32,
    },
    SetTime {
        clock_id: i32,
        sec: i64,
        nsec: i64,
    },
    Sleep {
        clock_id: i32,
        absolute: bool,
        sec: i64,
        nsec: i64,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockSnapshot {
    pub realtime_ns: i128,
    pub monotonic_ns: u64,
    pub boottime_ns: u64,
    pub tai_offset_secs: i32,
    pub coarse_resolution_ns: u64,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeatureSet {
    pub set_realtime: bool,
    pub tai: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockPlan {
    Read {
        clock: Clock,
        value_ns: i128,
    },
    Resolution {
        clock: Clock,
        nanoseconds: u64,
    },
    SetRealtime {
        nanoseconds: i128,
    },
    Sleep {
        clock: Clock,
        deadline_ns: i128,
        absolute: bool,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    InvalidClock,
    InvalidTime,
    Overflow,
    UnsupportedSetClock,
    /// `-EINVAL` from `timekeeping_validate_timex()`
    /// (`kernel/time/timekeeping.c:2826-2901`): an illegal `ADJ_ADJTIME`
    /// combination, an out-of-range `ADJ_TICK`, `ADJ_SETOFFSET` sub-second
    /// value, or an `ADJ_FREQUENCY` value that would overflow `PPM_SCALE`.
    InvalidMode,
    /// `-EPERM`: `CAP_SYS_TIME` is required for any modifying mode.
    NotPermitted,
    /// `-EINVAL`: the requested wall time precedes `CLOCK_MONOTONIC`
    /// (`do_settimeofday64()`, `kernel/time/timekeeping.c:1670-1680`) or is
    /// outside `timespec64_valid_settod()`.
    TimeBeforeMonotonic,
}
fn clock(id: i32) -> Result<Clock, Reject> {
    match id {
        CLOCK_REALTIME | CLOCK_REALTIME_ALARM => Ok(Clock::Realtime),
        CLOCK_MONOTONIC => Ok(Clock::Monotonic),
        CLOCK_PROCESS_CPUTIME_ID => Ok(Clock::ProcessCpu),
        CLOCK_THREAD_CPUTIME_ID => Ok(Clock::ThreadCpu),
        CLOCK_MONOTONIC_RAW => Ok(Clock::MonotonicRaw),
        CLOCK_REALTIME_COARSE => Ok(Clock::RealtimeCoarse),
        CLOCK_MONOTONIC_COARSE => Ok(Clock::MonotonicCoarse),
        CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => Ok(Clock::Boottime),
        CLOCK_TAI => Ok(Clock::Tai),
        _ => Err(Reject::InvalidClock),
    }
}
fn ns(sec: i64, nsec: i64) -> Result<i128, Reject> {
    if !(0..1_000_000_000).contains(&nsec) {
        return Err(Reject::InvalidTime);
    }
    (sec as i128)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(nsec as i128))
        .ok_or(Reject::Overflow)
}
pub fn plan_clock(
    r: ClockRequest,
    s: ClockSnapshot,
    features: FeatureSet,
) -> Result<ClockPlan, Reject> {
    match r {
        ClockRequest::GetTime { clock_id } => {
            let c = clock(clock_id)?;
            let v = match c {
                Clock::Realtime | Clock::RealtimeCoarse => s.realtime_ns,
                Clock::Monotonic | Clock::MonotonicRaw | Clock::MonotonicCoarse => {
                    s.monotonic_ns as i128
                }
                Clock::Boottime => s.boottime_ns as i128,
                Clock::Tai if features.tai => s
                    .realtime_ns
                    .checked_add((s.tai_offset_secs as i128) * 1_000_000_000)
                    .ok_or(Reject::Overflow)?,
                Clock::Tai => return Err(Reject::InvalidClock),
                Clock::ProcessCpu | Clock::ThreadCpu => return Err(Reject::InvalidClock),
            };
            Ok(ClockPlan::Read {
                clock: c,
                value_ns: if matches!(c, Clock::RealtimeCoarse | Clock::MonotonicCoarse) {
                    v - (v.rem_euclid(s.coarse_resolution_ns.max(1) as i128))
                } else {
                    v
                },
            })
        }
        ClockRequest::GetResolution { clock_id } => {
            let c = clock(clock_id)?;
            Ok(ClockPlan::Resolution {
                clock: c,
                nanoseconds: if matches!(c, Clock::RealtimeCoarse | Clock::MonotonicCoarse) {
                    s.coarse_resolution_ns.max(1)
                } else {
                    1
                },
            })
        }
        ClockRequest::SetTime {
            clock_id,
            sec,
            nsec,
        } => {
            if clock(clock_id)? != Clock::Realtime || !features.set_realtime {
                return Err(Reject::UnsupportedSetClock);
            }
            Ok(ClockPlan::SetRealtime {
                nanoseconds: ns(sec, nsec)?,
            })
        }
        ClockRequest::Sleep {
            clock_id,
            absolute,
            sec,
            nsec,
        } => {
            let c = clock(clock_id)?;
            if matches!(
                c,
                Clock::ProcessCpu | Clock::ThreadCpu | Clock::MonotonicRaw
            ) {
                return Err(Reject::InvalidClock);
            }
            Ok(ClockPlan::Sleep {
                clock: c,
                deadline_ns: ns(sec, nsec)?,
                absolute,
            })
        }
    }
}

/// The fields of `struct __kernel_timex` that `do_adjtimex(2)` reads
/// (`kernel/time/ntp.c:761-846`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimexRequest {
    pub modes: u32,
    /// `timex.offset`: nanoseconds with `ADJ_NANO`, microseconds otherwise.
    /// Relative to `ADJ_ADJTIME` mode, where it is always microseconds.
    pub offset: i64,
    /// `timex.freq`, in scaled ppm (`PPM_SCALE` units per ppm).
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: i32,
    /// `timex.constant`: the PLL time constant, and the `ADJ_TAI` operand.
    pub constant: i64,
    pub tick: i64,
    /// `timex.time.tv_sec`/`tv_usec`: the `ADJ_SETOFFSET` discontinuity.
    pub time_sec: i64,
    pub time_usec: i64,
}

/// The fields `ntp_adjtimex()` writes back (`kernel/time/ntp.c:809-829`).
/// `timex.modes` and the `pps*`/`jitter`/`shift`/`stabil`/`jitcnt`/`calcnt`/
/// `errcnt`/`stbcnt` inputs are deliberately absent: Linux never writes them
/// without `CONFIG_NTP_PPS`, and with that option disabled `pps_fill_timex()`
/// zeroes the PPS fields (`kernel/time/ntp.c:232-244`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimexOutput {
    pub offset: i64,
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: i32,
    pub constant: i64,
    pub precision: i64,
    pub tolerance: i64,
    pub tick: i64,
    pub tai: i32,
}

/// The Linux timex state: `struct ntp_data` (`kernel/time/ntp.c:56-99`) plus
/// the core timekeeper TAI offset (`include/linux/timekeeper_internal.h:190`).
///
/// TheKernel does not model `second_overflow()` tick-length steering, so the
/// fields that only feed it (`tick_length`, `tick_length_base`,
/// `ntp_tick_adj`, `time_reftime`, `ntp_next_leap_sec`) do not exist here.
/// Every field `adjtimex(2)` can observe does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexState {
    /// `ntp_data::time_status`.
    pub status: i32,
    /// `ntp_data::time_state`, the leap-second state machine.  Without
    /// `second_overflow()` it stays `TIME_OK`; `is_error_status()` supplies the
    /// `TIME_ERROR` result.
    pub state: i32,
    /// `ntp_data::time_offset`, scaled by `NTP_SCALE_SHIFT` and
    /// `NTP_INTERVAL_FREQ` exactly as Linux stores it.
    pub offset_scaled: i64,
    /// `ntp_data::time_freq`, in `PPM_SCALE` units per ppm.
    pub freq: i64,
    /// `ntp_data::time_constant`.
    pub constant: i64,
    /// `ntp_data::time_maxerror`.
    pub maxerror: i64,
    /// `ntp_data::time_esterror`.
    pub esterror: i64,
    /// `ntp_data::time_adjust`: the `adjtime(3)` residual in microseconds.
    pub adjust: i64,
    /// `ntp_data::tick_usec`: microseconds per USER_HZ tick.
    pub tick: i64,
    /// `struct timekeeper::tai_offset`: TAI minus UTC in seconds.
    pub tai: i32,
}

impl TimexState {
    /// `ntp_init()` state (`kernel/time/ntp.c:90-100`): `STA_UNSYNC`, time
    /// constant 2, both error estimates at `NTP_PHASE_LIMIT`, and one
    /// `tick_usec` per USER_HZ tick.  `tai_offset` is zero-initialized like the
    /// static timekeeper (`kernel/time/timekeeping.c:2042-2080`); userspace
    /// (an NTP daemon) is what publishes the 37 second TAI-UTC offset.
    pub const INITIAL: Self = Self {
        status: STA_UNSYNC,
        state: TIME_OK,
        offset_scaled: 0,
        freq: 0,
        constant: 2,
        maxerror: NTP_PHASE_LIMIT,
        esterror: NTP_PHASE_LIMIT,
        adjust: 0,
        tick: TICK_USEC,
        tai: 0,
    };
}

/// `CAP_SYS_TIME` availability of the calling task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexAuthority {
    pub cap_sys_time: bool,
}

/// One `adjtimex(2)`/`clock_adjtime(2)` transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexUpdate {
    /// State to publish when `changed` is set.
    pub next: TimexState,
    /// The fields to copy back into the caller's `struct timex`.
    pub output: TimexOutput,
    /// The syscall's return value: `ntp_adjtimex()`'s `result`
    /// (`kernel/time/ntp.c:811-813`), i.e. `TIME_ERROR` while the clock is
    /// unsynchronized.
    pub result: i32,
    /// Whether `next` differs from the current state; when false the caller
    /// must not publish a new generation.
    pub changed: bool,
}

/// `shift_right()` (`include/linux/timex.h:141-146`): shift with C semantics,
/// i.e. truncation towards zero rather than an arithmetic floor.
const fn shift_right(value: i64, shift: u32) -> i64 {
    let value = value as i128;
    let shifted = if value < 0 {
        -((-value) >> shift)
    } else {
        value >> shift
    };
    shifted as i64
}

/// `is_error_status()` for `!CONFIG_NTP_PPS` (`kernel/time/ntp.c:227-230`).
/// The PPS variants of this predicate require `CONFIG_NTP_PPS`, which also
/// enables the PPS input handling TheKernel does not implement.
pub const fn is_error_status(status: i32) -> bool {
    status & (STA_UNSYNC | STA_CLOCKERR) != 0
}

/// `timespec64_valid_settod()` (`include/linux/time64.h:118-127`): the
/// wall-clock bound that `do_sys_settimeofday64()` applies *before* the
/// `CAP_SYS_TIME` test (`kernel/time/time.c:174-180`).
pub fn validate_settime_bound(nanoseconds: i128) -> Result<(), Reject> {
    if nanoseconds < 0 || nanoseconds >= TIME_SETTOD_SEC_MAX as i128 * NANOS_PER_SEC {
        return Err(Reject::InvalidTime);
    }
    Ok(())
}

/// `do_settimeofday64()`'s monotonic guard
/// (`kernel/time/timekeeping.c:1670-1680`), which requires
/// `wall_to_monotonic <= ts - wall`, i.e. the new wall time must not precede
/// the current `CLOCK_MONOTONIC`.  It runs *after* the capability test, so a
/// caller that has to interleave a privilege decision must call
/// [`validate_settime_bound`] and this function separately.
pub fn validate_settime_floor(nanoseconds: i128, monotonic_ns: i128) -> Result<(), Reject> {
    if nanoseconds < monotonic_ns {
        return Err(Reject::TimeBeforeMonotonic);
    }
    Ok(())
}

/// The complete set-to-time rule in Linux's own order for callers that apply
/// both halves back to back: bound then monotonic floor.
pub fn validate_settime_target(nanoseconds: i128, monotonic_ns: i128) -> Result<(), Reject> {
    validate_settime_bound(nanoseconds)?;
    validate_settime_floor(nanoseconds, monotonic_ns)
}

/// The discontinuity `ADJ_SETOFFSET` injects
/// (`kernel/time/timekeeping.c:2953-2960`): `time.tv_sec` plus
/// `time.tv_usec`, the latter scaled to nanoseconds unless `ADJ_NANO` selects
/// nanosecond units.
pub fn setoffset_delta_ns(request: &TimexRequest) -> i128 {
    let subsecond_ns = if request.modes & ADJ_NANO != 0 {
        request.time_usec
    } else {
        request.time_usec * 1_000
    };
    request.time_sec as i128 * NANOS_PER_SEC + subsecond_ns as i128
}

/// `timekeeping_validate_timex()` (`kernel/time/timekeeping.c:2830-2901`).
/// Only these mode bits are validated; unknown bits are ignored, as Linux
/// documents by rejecting nothing else.
fn validate(request: &TimexRequest, authority: TimexAuthority) -> Result<(), Reject> {
    let modes = request.modes;
    if modes & ADJ_ADJTIME != 0 {
        // "singleshot must not be used with any other mode bits": inside the
        // adjtime branch the ADJ_OFFSET bit must be present.  This is checked
        // before the capability, so an invalid combination is EINVAL even for
        // an unprivileged caller.
        if modes & ADJ_OFFSET_SINGLESHOT == 0 {
            return Err(Reject::InvalidMode);
        }
        if modes & ADJ_OFFSET_READONLY == 0 && !authority.cap_sys_time {
            return Err(Reject::NotPermitted);
        }
    } else {
        // "In order to modify anything, you gotta be super-user!"
        if modes != 0 && !authority.cap_sys_time {
            return Err(Reject::NotPermitted);
        }
        // "if the quartz is off by more than 10% then something is VERY wrong!"
        if modes & ADJ_TICK != 0 && !(TICK_MIN..=TICK_MAX).contains(&request.tick) {
            return Err(Reject::InvalidMode);
        }
    }
    if modes & ADJ_SETOFFSET != 0 {
        if !authority.cap_sys_time {
            return Err(Reject::NotPermitted);
        }
        if request.time_usec < 0 {
            return Err(Reject::InvalidMode);
        }
        let limit = if modes & ADJ_NANO != 0 {
            1_000_000_000
        } else {
            1_000_000
        };
        if request.time_usec >= limit {
            return Err(Reject::InvalidMode);
        }
    }
    if modes & ADJ_FREQUENCY != 0 {
        if request.freq < i64::MIN / PPM_SCALE {
            return Err(Reject::InvalidMode);
        }
        if request.freq > i64::MAX / PPM_SCALE {
            return Err(Reject::InvalidMode);
        }
    }
    Ok(())
}

/// `process_adj_status()` (`kernel/time/ntp.c:696-726`): only `STA_RONLY` bits
/// survive from the previous status, and only non-`STA_RONLY` bits are taken
/// from the request.
fn update_status(next: &mut TimexState, request: &TimexRequest) {
    if next.status & STA_PLL != 0 && request.status & STA_PLL == 0 {
        // Linux assigns `STA_UNSYNC` here, but the `&= STA_RONLY` merge below
        // immediately clears it again (STA_UNSYNC is not a read-only bit); the
        // lasting effects are the leap state, `time_reftime` and the PPS
        // calibration interval, none of which steer a TheKernel clock.
        next.state = TIME_OK;
    }
    next.status &= STA_RONLY;
    next.status |= request.status & !STA_RONLY;
}

/// `ntp_update_offset()` (`kernel/time/ntp.c:266-312`).  The phase update is
/// ignored unless `STA_PLL` is set, is clamped (never rejected) to
/// `MAXPHASE`, and is stored exactly as Linux stores it.
fn update_offset(next: &mut TimexState, request: &TimexRequest) {
    if next.status & STA_PLL == 0 {
        return;
    }
    let mut offset = request.offset;
    if next.status & STA_NANO == 0 {
        // "Make sure the multiplication below won't overflow"
        offset = offset.clamp(-1_000_000, 1_000_000) * 1_000;
    }
    // "Scale the phase adjustment and clamp to the operating range."
    let offset = offset.clamp(-MAXPHASE, MAXPHASE);
    // `ntpdata->time_offset = div_s64(offset64 << NTP_SCALE_SHIFT, NTP_INTERVAL_FREQ)`
    // with `NTP_INTERVAL_FREQ == HZ`; TheKernel has a single tick rate, so the
    // ABI's USER_HZ is used for it.
    next.offset_scaled = (((offset as i128) << NTP_SCALE_SHIFT) / USER_HZ as i128) as i64;
}

/// `process_adjtimex_modes()` (`kernel/time/ntp.c:728-756`).
fn apply_modes(next: &mut TimexState, request: &TimexRequest) {
    let modes = request.modes;
    if modes & ADJ_STATUS != 0 {
        update_status(next, request);
    }
    // Resolution selection runs after the status merge and before every other
    // mode, so ADJ_MICRO wins when both resolution bits are set.
    if modes & ADJ_NANO != 0 {
        next.status |= STA_NANO;
    }
    if modes & ADJ_MICRO != 0 {
        next.status &= !STA_NANO;
    }
    if modes & ADJ_FREQUENCY != 0 {
        let scaled = (request.freq as i128) * (PPM_SCALE as i128);
        next.freq = scaled.clamp(-(MAXFREQ_SCALED as i128), MAXFREQ_SCALED as i128) as i64;
    }
    if modes & ADJ_MAXERROR != 0 {
        next.maxerror = request.maxerror.clamp(0, NTP_PHASE_LIMIT);
    }
    if modes & ADJ_ESTERROR != 0 {
        next.esterror = request.esterror.clamp(0, NTP_PHASE_LIMIT);
    }
    if modes & ADJ_TIMECONST != 0 {
        // Microsecond callers see (and set) the time constant four higher.
        let mut constant = request.constant.clamp(0, MAXTC);
        if next.status & STA_NANO == 0 {
            constant += 4;
        }
        next.constant = constant.clamp(0, MAXTC);
    }
    // Out-of-range TAI offsets are ignored, not rejected.
    if modes & ADJ_TAI != 0 && (0..=MAX_TAI_OFFSET).contains(&request.constant) {
        next.tai = request.constant as i32;
    }
    if modes & ADJ_OFFSET != 0 {
        update_offset(next, request);
    }
    if modes & ADJ_TICK != 0 {
        next.tick = request.tick;
    }
}

/// `txc->offset` as rendered by `ntp_adjtimex()` (`kernel/time/ntp.c:806-808`).
fn render_offset(state: &TimexState) -> i64 {
    let nanos = shift_right(
        (state.offset_scaled as i128 * USER_HZ as i128) as i64,
        NTP_SCALE_SHIFT,
    );
    if state.status & STA_NANO != 0 {
        nanos
    } else {
        nanos / 1_000
    }
}

/// `txc->freq` as rendered by `ntp_adjtimex()` (`kernel/time/ntp.c:815-816`).
fn render_freq(time_freq: i64) -> i64 {
    shift_right(
        shift_right(time_freq, PPM_SCALE_INV_SHIFT) * PPM_SCALE_INV,
        NTP_SCALE_SHIFT,
    )
}

/// `do_adjtimex()` -> `ntp_adjtimex()` (`kernel/time/ntp.c:761-846`): validate
/// the request, apply the modes in Linux order, and render the result.
///
/// This is the whole pure policy of the syscall: the caller supplies the
/// current state, the `CAP_SYS_TIME` decision, and the discontinuity it
/// applies for `ADJ_SETOFFSET` (`setoffset_delta_ns()`), and publishes
/// `next` only when `changed` is set.
pub fn adjust(
    state: &TimexState,
    request: &TimexRequest,
    authority: TimexAuthority,
) -> Result<TimexUpdate, Reject> {
    validate(request, authority)?;
    let modes = request.modes;
    let mut next = *state;
    let mut output = TimexOutput::default();
    if modes & ADJ_ADJTIME != 0 {
        // adjtime(3) mode: report the residual being replaced, and never touch
        // the NTP offset/frequency state.
        output.offset = state.adjust;
        if modes & ADJ_OFFSET_READONLY == 0 {
            next.adjust = request.offset;
        }
    } else {
        if modes != 0 {
            apply_modes(&mut next, request);
        }
        output.offset = render_offset(&next);
    }
    output.freq = render_freq(next.freq);
    output.maxerror = next.maxerror;
    output.esterror = next.esterror;
    output.status = next.status;
    output.constant = next.constant;
    output.precision = PRECISION;
    output.tolerance = TOLERANCE;
    output.tick = next.tick;
    output.tai = next.tai;
    let result = if is_error_status(next.status) {
        TIME_ERROR
    } else {
        next.state
    };
    let changed = next != *state;
    Ok(TimexUpdate {
        next,
        output,
        result,
        changed,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> ClockSnapshot {
        ClockSnapshot {
            realtime_ns: 1_000_000_001,
            monotonic_ns: 9,
            boottime_ns: 10,
            tai_offset_secs: 37,
            coarse_resolution_ns: 10,
        }
    }

    /// A privileged caller, i.e. the case every guest differential asserts.
    fn root() -> TimexAuthority {
        TimexAuthority { cap_sys_time: true }
    }

    fn read(state: &TimexState) -> TimexUpdate {
        adjust(state, &TimexRequest::default(), root()).unwrap()
    }

    fn request(modes: u32) -> TimexRequest {
        TimexRequest {
            modes,
            ..TimexRequest::default()
        }
    }

    #[test]
    fn coarse_and_time_bounds() {
        assert_eq!(
            plan_clock(
                ClockRequest::GetTime {
                    clock_id: CLOCK_REALTIME_COARSE
                },
                s(),
                FeatureSet::default()
            )
            .unwrap(),
            ClockPlan::Read {
                clock: Clock::RealtimeCoarse,
                value_ns: 1_000_000_000
            }
        );
        assert_eq!(
            plan_clock(
                ClockRequest::SetTime {
                    clock_id: CLOCK_REALTIME,
                    sec: 0,
                    nsec: 1_000_000_000
                },
                s(),
                FeatureSet {
                    set_realtime: true,
                    tai: false
                }
            ),
            Err(Reject::InvalidTime)
        );
    }

    #[test]
    fn initial_state_matches_linux_ntp_init() {
        let state = TimexState::INITIAL;
        assert_eq!(state.status, STA_UNSYNC);
        assert_eq!(state.constant, 2);
        assert_eq!(state.maxerror, 16_000_000);
        assert_eq!(state.esterror, 16_000_000);
        assert_eq!(state.tick, 10_000);
        assert_eq!(state.tai, 0);
        assert_eq!(state.freq, 0);
        assert_eq!(state.adjust, 0);
        assert_eq!(state.offset_scaled, 0);
    }

    #[test]
    fn bare_read_reports_time_error_and_echoes_modes() {
        let update = read(&TimexState::INITIAL);
        // `adjtimex(2)` returns TIME_ERROR while STA_UNSYNC is set.
        assert_eq!(update.result, TIME_ERROR);
        assert_eq!(update.output.status, STA_UNSYNC);
        assert_eq!(update.output.maxerror, 16_000_000);
        assert_eq!(update.output.esterror, 16_000_000);
        assert_eq!(update.output.constant, 2);
        assert_eq!(update.output.precision, 1);
        assert_eq!(update.output.tolerance, 32_768_000);
        assert_eq!(update.output.tick, 10_000);
        assert_eq!(update.output.tai, 0);
        assert_eq!(update.output.offset, 0);
        assert_eq!(update.output.freq, 0);
        assert!(!update.changed);
        // A read never requires CAP_SYS_TIME.
        let unprivileged = adjust(
            &TimexState::INITIAL,
            &TimexRequest::default(),
            TimexAuthority {
                cap_sys_time: false,
            },
        )
        .unwrap();
        assert_eq!(unprivileged.result, TIME_ERROR);
    }

    #[test]
    fn sync_status_clears_unsync_and_result() {
        let mut state = TimexState::INITIAL;
        let mut req = request(ADJ_STATUS);
        req.status = STA_PLL;
        let update = adjust(&state, &req, root()).unwrap();
        assert_eq!(update.output.status, STA_PLL);
        assert_eq!(update.result, TIME_OK);
        assert!(update.changed);
        state = update.next;
        // Clearing STA_PLL again drops every writable bit, because the status
        // merge takes only STA_RONLY bits from the previous value.
        let mut clear = request(ADJ_STATUS);
        clear.status = 0;
        let update = adjust(&state, &clear, root()).unwrap();
        assert_eq!(update.output.status, 0);
        assert_eq!(update.result, TIME_OK);
    }

    #[test]
    fn read_only_status_bits_are_preserved_and_never_settable() {
        let state = TimexState {
            status: STA_UNSYNC | STA_NANO | STA_CLK | STA_PPSJITTER,
            ..TimexState::INITIAL
        };
        let mut req = request(ADJ_STATUS);
        // STA_MODE, STA_CLOCKERR and the PPS condition bits are read-only.
        req.status = STA_PLL | STA_MODE | STA_CLOCKERR | STA_PPSERROR;
        let update = adjust(&state, &req, root()).unwrap();
        assert_eq!(
            update.output.status,
            STA_PLL | STA_NANO | STA_CLK | STA_PPSJITTER
        );
        // STA_CLOCKERR is an error status and forces TIME_ERROR.
        assert_eq!(update.result, TIME_OK);
        assert_eq!(STA_RONLY & STA_UNSYNC, 0);
    }

    #[test]
    fn clock_error_status_forces_time_error() {
        let state = TimexState {
            status: STA_PLL | STA_CLOCKERR,
            ..TimexState::INITIAL
        };
        assert_eq!(read(&state).result, TIME_ERROR);
        assert!(is_error_status(STA_UNSYNC));
        assert!(is_error_status(STA_CLOCKERR));
        assert!(!is_error_status(STA_PLL));
    }

    #[test]
    fn micro_and_nano_precedence_and_time_constant_offset() {
        // ADJ_NANO selects nanoseconds ...
        let update = adjust(&TimexState::INITIAL, &request(ADJ_NANO), root()).unwrap();
        assert_eq!(update.output.status & STA_NANO, STA_NANO);
        // ... ADJ_MICRO clears it, and a request with both ends up microsecond.
        let update = adjust(&TimexState::INITIAL, &request(ADJ_MICRO | ADJ_NANO), root()).unwrap();
        assert_eq!(update.output.status & STA_NANO, 0);
        // The time constant is reported four higher in microsecond mode.
        let mut req = request(ADJ_TIMECONST);
        req.constant = 3;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root())
                .unwrap()
                .output
                .constant,
            7
        );
        let mut req = request(ADJ_TIMECONST | ADJ_NANO);
        req.constant = 3;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root())
                .unwrap()
                .output
                .constant,
            3
        );
        // ... and is clamped to MAXTC after the microsecond offset is applied.
        let mut req = request(ADJ_TIMECONST);
        req.constant = 10;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root())
                .unwrap()
                .output
                .constant,
            10
        );
    }

    #[test]
    fn accounting_tick_resolution_rounds_up() {
        // Linux rounds the accounting tick up to whole nanoseconds; the
        // *_COARSE clocks truncate the same quotient.
        assert_eq!(accounting_tick_resolution_nanos(100), 10_000_000);
        assert_eq!(accounting_tick_resolution_nanos(250), 4_000_000);
        assert_eq!(accounting_tick_resolution_nanos(300), 3_333_334);
        assert_eq!(accounting_tick_resolution_nanos(1000), 1_000_000);
    }

    #[test]
    fn tick_bounds_use_user_hz_not_the_kernel_tick_rate() {
        assert_eq!((TICK_MIN, TICK_MAX), (9_000, 11_000));
        let mut ok = request(ADJ_TICK);
        ok.tick = 9_000;
        assert!(adjust(&TimexState::INITIAL, &ok, root()).is_ok());
        let mut ok = request(ADJ_TICK);
        ok.tick = 11_000;
        assert!(adjust(&TimexState::INITIAL, &ok, root()).is_ok());
        let mut low = request(ADJ_TICK);
        low.tick = 8_999;
        assert_eq!(
            adjust(&TimexState::INITIAL, &low, root()),
            Err(Reject::InvalidMode)
        );
        let mut high = request(ADJ_TICK);
        high.tick = 11_001;
        assert_eq!(
            adjust(&TimexState::INITIAL, &high, root()),
            Err(Reject::InvalidMode)
        );
    }

    #[test]
    fn unknown_mode_bits_are_ignored() {
        let mut req = request(ADJ_TICK | 0x0002_0000 | 0x4000_0000);
        req.tick = 10_000;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.tick, 10_000);
        // Unknown bits alone are still "modes != 0" and need CAP_SYS_TIME.
        assert_eq!(
            adjust(
                &TimexState::INITIAL,
                &request(0x0002_0000),
                TimexAuthority {
                    cap_sys_time: false
                }
            ),
            Err(Reject::NotPermitted)
        );
    }

    #[test]
    fn offset_requires_pll_and_clamps_instead_of_failing() {
        // Without STA_PLL the phase update is discarded entirely.
        let mut req = request(ADJ_OFFSET | ADJ_NANO);
        req.offset = 1_234;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.offset, 0);
        // With STA_PLL set in the same call it applies.
        let mut req = request(ADJ_STATUS | ADJ_OFFSET | ADJ_NANO);
        req.status = STA_PLL;
        req.offset = 1_234;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        // The scaled residual renders one nanosecond low: `time_offset` is
        // `offset << 32 / NTP_INTERVAL_FREQ` and the render multiplies the
        // truncated quotient back, exactly as Linux does.
        assert_eq!(update.output.offset, 1_233);
        assert_eq!(update.next.offset_scaled, (1_234i128 << 32) as i64 / 100);
        // Oversized phases saturate at MAXPHASE rather than failing.
        let mut req = request(ADJ_STATUS | ADJ_OFFSET | ADJ_NANO);
        req.status = STA_PLL;
        req.offset = 1_000_000_000;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.offset, 500_000_000);
        // Microsecond mode clamps to one second before scaling.
        let mut req = request(ADJ_STATUS | ADJ_OFFSET);
        req.status = STA_PLL;
        req.offset = 5_000_000;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.offset, 500_000);
        // A negative phase keeps its sign (truncating, not flooring, shifts).
        let mut req = request(ADJ_STATUS | ADJ_OFFSET | ADJ_NANO);
        req.status = STA_PLL;
        req.offset = -1_234;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.offset, -1_233);
    }

    #[test]
    fn singleshot_reports_the_previous_residual() {
        let state = TimexState {
            adjust: 1_234,
            ..TimexState::INITIAL
        };
        // ADJ_OFFSET_SINGLESHOT replaces the residual and reports the old one.
        let mut req = request(ADJ_ADJTIME | ADJ_OFFSET);
        req.offset = -500;
        let update = adjust(&state, &req, root()).unwrap();
        assert_eq!(update.output.offset, 1_234);
        assert_eq!(update.next.adjust, -500);
        assert!(update.changed);
        // ADJ_OFFSET_SS_READ reports the residual without replacing it.
        let req = request(ADJ_OFFSET_SS_READ);
        let update = adjust(&state, &req, root()).unwrap();
        assert_eq!(update.output.offset, 1_234);
        assert!(!update.changed);
        // The residual is reported in microseconds regardless of STA_NANO.
        let state = TimexState {
            status: STA_UNSYNC | STA_NANO,
            adjust: 7,
            ..TimexState::INITIAL
        };
        assert_eq!(adjust(&state, &req, root()).unwrap().output.offset, 7);
    }

    #[test]
    fn adjtime_mode_validation_precedes_capability() {
        // ADJ_ADJTIME without the ADJ_OFFSET bit is EINVAL, not EPERM, and
        // ADJ_OFFSET_SS_READ needs no capability at all.
        let unprivileged = TimexAuthority {
            cap_sys_time: false,
        };
        assert_eq!(
            adjust(&TimexState::INITIAL, &request(ADJ_ADJTIME), unprivileged),
            Err(Reject::InvalidMode)
        );
        assert_eq!(
            adjust(
                &TimexState::INITIAL,
                &request(ADJ_ADJTIME | ADJ_OFFSET),
                unprivileged
            ),
            Err(Reject::NotPermitted)
        );
        assert!(
            adjust(
                &TimexState::INITIAL,
                &request(ADJ_OFFSET_SS_READ),
                unprivileged
            )
            .is_ok()
        );
        // Every other modifying mode is EPERM for an unprivileged caller.
        for modes in [
            ADJ_OFFSET,
            ADJ_FREQUENCY,
            ADJ_MAXERROR,
            ADJ_ESTERROR,
            ADJ_STATUS,
            ADJ_TIMECONST,
            ADJ_TAI,
            ADJ_TICK,
            ADJ_SETOFFSET,
        ] {
            assert_eq!(
                adjust(&TimexState::INITIAL, &request(modes), unprivileged),
                Err(Reject::NotPermitted),
                "modes {modes:#x}"
            );
        }
    }

    #[test]
    fn maxerror_esterror_and_frequency_clamps() {
        let mut req = request(ADJ_MAXERROR | ADJ_ESTERROR);
        req.maxerror = -5;
        req.esterror = i64::MAX;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.output.maxerror, 0);
        assert_eq!(update.output.esterror, 16_000_000);
        // 1 ppm round-trips through Linux's fixed-point inverse ...
        let mut req = request(ADJ_FREQUENCY);
        req.freq = 65_536;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.next.freq, 65_536 * PPM_SCALE);
        assert_eq!(update.output.freq, 65_536);
        // ... and 1000 ppm saturates at exactly +MAXFREQ (500 ppm).
        let mut req = request(ADJ_FREQUENCY);
        req.freq = 1_000 * 65_536;
        let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
        assert_eq!(update.next.freq, MAXFREQ_SCALED);
        assert_eq!(update.output.freq, 32_768_000);
        // Overflowing operands are rejected, exactly at the Linux boundary.
        let mut req = request(ADJ_FREQUENCY);
        req.freq = i64::MAX / PPM_SCALE;
        assert!(adjust(&TimexState::INITIAL, &req, root()).is_ok());
        let mut req = request(ADJ_FREQUENCY);
        req.freq = i64::MAX / PPM_SCALE + 1;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root()),
            Err(Reject::InvalidMode)
        );
        let mut req = request(ADJ_FREQUENCY);
        req.freq = i64::MIN / PPM_SCALE;
        assert!(adjust(&TimexState::INITIAL, &req, root()).is_ok());
        let mut req = request(ADJ_FREQUENCY);
        req.freq = i64::MIN / PPM_SCALE - 1;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root()),
            Err(Reject::InvalidMode)
        );
    }

    #[test]
    fn tai_offsets_outside_the_range_are_ignored() {
        let mut req = request(ADJ_TAI);
        req.constant = MAX_TAI_OFFSET;
        assert_eq!(
            adjust(&TimexState::INITIAL, &req, root())
                .unwrap()
                .output
                .tai,
            MAX_TAI_OFFSET as i32
        );
        for constant in [-1, MAX_TAI_OFFSET + 1, i64::MAX, i64::MIN] {
            let mut req = request(ADJ_TAI);
            req.constant = constant;
            let update = adjust(&TimexState::INITIAL, &req, root()).unwrap();
            assert_eq!(update.output.tai, 0, "constant {constant}");
            assert!(!update.changed);
        }
    }

    #[test]
    fn setoffset_validation_and_delta() {
        let mut req = request(ADJ_SETOFFSET);
        req.time_sec = 1;
        req.time_usec = 999_999;
        assert_eq!(
            adjust(
                &TimexState::INITIAL,
                &req,
                TimexAuthority {
                    cap_sys_time: false
                }
            ),
            Err(Reject::NotPermitted)
        );
        assert_eq!(setoffset_delta_ns(&req), 1_999_999_000);
        let mut negative = request(ADJ_SETOFFSET);
        negative.time_sec = -2;
        negative.time_usec = 500_000;
        assert_eq!(setoffset_delta_ns(&negative), -1_500_000_000);

        let mut bad = request(ADJ_SETOFFSET);
        bad.time_usec = -1;
        assert_eq!(
            adjust(&TimexState::INITIAL, &bad, root()),
            Err(Reject::InvalidMode)
        );
        let mut bad = request(ADJ_SETOFFSET);
        bad.time_usec = 1_000_000;
        assert_eq!(
            adjust(&TimexState::INITIAL, &bad, root()),
            Err(Reject::InvalidMode)
        );
        let mut ok = request(ADJ_SETOFFSET | ADJ_NANO);
        ok.time_usec = 999_999_999;
        assert_eq!(setoffset_delta_ns(&ok), 999_999_999);
        let mut bad = request(ADJ_SETOFFSET | ADJ_NANO);
        bad.time_usec = 1_000_000_000;
        assert_eq!(
            adjust(&TimexState::INITIAL, &bad, root()),
            Err(Reject::InvalidMode)
        );
    }

    #[test]
    fn settime_bound_and_floor_split_in_linux_order() {
        let max = TIME_SETTOD_SEC_MAX as i128 * NANOS_PER_SEC;
        // The bound is independent of the monotonic floor and vice versa, so a
        // caller can interleave the capability test the way Linux does.
        assert_eq!(validate_settime_bound(max), Err(Reject::InvalidTime));
        assert_eq!(validate_settime_bound(-1), Err(Reject::InvalidTime));
        assert_eq!(validate_settime_bound(0), Ok(()));
        assert_eq!(
            validate_settime_floor(0, 5 * NANOS_PER_SEC),
            Err(Reject::TimeBeforeMonotonic)
        );
        // The composition keeps the bound first, as do_settimeofday64() does.
        assert_eq!(validate_settime_target(max, 0), Err(Reject::InvalidTime));
    }

    #[test]
    fn settime_target_bounds_and_monotonic_floor() {
        let ten = 10 * NANOS_PER_SEC;
        assert_eq!(validate_settime_target(ten, 5 * NANOS_PER_SEC), Ok(()));
        // Equal to CLOCK_MONOTONIC is accepted; earlier is EINVAL.
        assert_eq!(
            validate_settime_target(5 * NANOS_PER_SEC, 5 * NANOS_PER_SEC),
            Ok(())
        );
        assert_eq!(
            validate_settime_target(4 * NANOS_PER_SEC, 5 * NANOS_PER_SEC),
            Err(Reject::TimeBeforeMonotonic)
        );
        // The set-to-time bound is exclusive and precedes the floor check.
        let max = TIME_SETTOD_SEC_MAX as i128 * NANOS_PER_SEC;
        assert_eq!(validate_settime_target(max - 1, 0), Ok(()));
        assert_eq!(validate_settime_target(max, 0), Err(Reject::InvalidTime));
        assert_eq!(
            validate_settime_target(i64::MAX as i128 * NANOS_PER_SEC, 0),
            Err(Reject::InvalidTime)
        );
        assert_eq!(validate_settime_target(-1, 0), Err(Reject::InvalidTime));
        assert_eq!(TIME_SETTOD_SEC_MAX, 8_277_292_036);
    }

    #[test]
    fn wake_alarm_admission_is_per_use() {
        use WakeAlarmUse::{Arm, Read, TimerFd};
        // Ordinary clocks are never gated.
        for usage in [Read, Arm, TimerFd] {
            assert_eq!(
                admit_wake_alarm(CLOCK_REALTIME, usage, false, false),
                Ok(())
            );
            assert_eq!(
                admit_wake_alarm(CLOCK_BOOTTIME, usage, false, false),
                Ok(())
            );
        }
        // Reading an alarm clock needs the RTC but no capability ...
        assert_eq!(
            admit_wake_alarm(CLOCK_REALTIME_ALARM, Read, false, false),
            Err(WakeAlarmReject::NoRtc)
        );
        assert_eq!(
            admit_wake_alarm(CLOCK_REALTIME_ALARM, Read, true, false),
            Ok(())
        );
        // ... arming needs both, RTC first ...
        assert_eq!(
            admit_wake_alarm(CLOCK_BOOTTIME_ALARM, Arm, false, false),
            Err(WakeAlarmReject::NoRtc)
        );
        assert_eq!(
            admit_wake_alarm(CLOCK_BOOTTIME_ALARM, Arm, true, false),
            Err(WakeAlarmReject::NotPermitted)
        );
        assert_eq!(
            admit_wake_alarm(CLOCK_BOOTTIME_ALARM, Arm, true, true),
            Ok(())
        );
        // ... and timerfd_create(2) never tests the RTC.
        assert_eq!(
            admit_wake_alarm(CLOCK_REALTIME_ALARM, TimerFd, false, true),
            Ok(())
        );
        assert_eq!(
            admit_wake_alarm(CLOCK_REALTIME_ALARM, TimerFd, true, false),
            Err(WakeAlarmReject::NotPermitted)
        );
        assert!(is_wake_alarm_clock(CLOCK_REALTIME_ALARM));
        assert!(is_wake_alarm_clock(CLOCK_BOOTTIME_ALARM));
        assert!(!is_wake_alarm_clock(CLOCK_REALTIME));
    }

    #[test]
    fn adjust_is_a_pure_function_of_state_and_request() {
        let mut req = request(ADJ_STATUS | ADJ_OFFSET | ADJ_TIMECONST);
        req.status = STA_PLL;
        req.offset = 500_000;
        req.constant = 4;
        let state = TimexState::INITIAL;
        let first = adjust(&state, &req, root()).unwrap();
        let second = adjust(&state, &req, root()).unwrap();
        assert_eq!(first, second);
        // Applying the same request to the resulting state is idempotent.
        let again = adjust(&first.next, &req, root()).unwrap();
        assert!(!again.changed);
        assert_eq!(again.output, first.output);
    }
}
