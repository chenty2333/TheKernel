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
pub const ADJ_OFFSET: u32 = 0x1;
pub const ADJ_FREQUENCY: u32 = 0x2;
pub const ADJ_MAXERROR: u32 = 0x4;
pub const ADJ_ESTERROR: u32 = 0x8;
pub const ADJ_STATUS: u32 = 0x10;
pub const ADJ_TIMECONST: u32 = 0x20;
pub const ADJ_MICRO: u32 = 0x1000;
pub const ADJ_NANO: u32 = 0x2000;
pub const ADJ_TICK: u32 = 0x4000;
pub const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
pub const ADJ_OFFSET_SS_READ: u32 = 0xa001;
pub const ADJ_ALL: u32 = ADJ_OFFSET
    | ADJ_FREQUENCY
    | ADJ_MAXERROR
    | ADJ_ESTERROR
    | ADJ_STATUS
    | ADJ_TIMECONST
    | ADJ_MICRO
    | ADJ_NANO
    | ADJ_TICK;
pub const STA_UNSYNC: i32 = 0x40;
pub const STA_PLL: i32 = 0x0001;
pub const STA_PPSFREQ: i32 = 0x0002;
pub const STA_PPSTIME: i32 = 0x0004;
pub const STA_FLL: i32 = 0x0008;
pub const STA_INS: i32 = 0x0010;
pub const STA_DEL: i32 = 0x0020;
pub const STA_FREQHOLD: i32 = 0x0080;
pub const STA_NANO: i32 = 0x2000;
pub const STA_MODE: i32 = 0x4000;
pub const TIME_OK: i32 = 0;
pub const TIME_ERROR: i32 = 5;
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
    UnknownMode,
    ConflictingResolution,
    InvalidStatus,
    InvalidTick,
    InvalidTimeConstant,
    StaleVersion,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timex {
    pub modes: u32,
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

impl Timex {
    /// A zero-filled Linux timex value suitable for static initialization.
    pub const ZERO: Self = Self {
        modes: 0,
        offset: 0,
        freq: 0,
        maxerror: 0,
        esterror: 0,
        status: 0,
        constant: 0,
        precision: 0,
        tolerance: 0,
        tick: 0,
        tai: 0,
    };
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexSnapshot {
    pub version: u64,
    pub value: Timex,
    pub tick_min: i64,
    pub tick_max: i64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexAdjustmentPlan {
    pub expected_version: u64,
    pub next: Timex,
    pub time_state: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimexRenderPlan {
    pub version: u64,
    pub value: Timex,
    pub time_state: i32,
}
pub fn plan_adjust(request: Timex, snapshot: TimexSnapshot) -> Result<TimexAdjustmentPlan, Reject> {
    let m = request.modes;
    if m == ADJ_OFFSET_SS_READ {
        return Ok(TimexAdjustmentPlan {
            expected_version: snapshot.version,
            next: snapshot.value,
            time_state: if snapshot.value.status & STA_UNSYNC != 0 {
                TIME_ERROR
            } else {
                TIME_OK
            },
        });
    }
    if m == ADJ_OFFSET_SINGLESHOT {
        let mut next = snapshot.value;
        next.offset = request.offset;
        return Ok(TimexAdjustmentPlan {
            expected_version: snapshot.version,
            next,
            time_state: if next.status & STA_UNSYNC != 0 {
                TIME_ERROR
            } else {
                TIME_OK
            },
        });
    }
    if m & !ADJ_ALL != 0 {
        return Err(Reject::UnknownMode);
    }
    let mut n = snapshot.value;
    // Keep Linux's update/validation order: resolution first, then status,
    // offset, frequency, the error estimates, time constant, and tick.
    if m & ADJ_MICRO != 0 {
        n.status &= !STA_NANO
    }
    if m & ADJ_NANO != 0 {
        n.status |= STA_NANO
    }
    if m & ADJ_STATUS != 0 {
        const SETTABLE_STATUS: i32 = STA_PLL
            | STA_PPSFREQ
            | STA_PPSTIME
            | STA_FLL
            | STA_INS
            | STA_DEL
            | STA_UNSYNC
            | STA_FREQHOLD
            | STA_MODE;
        if request.status & !SETTABLE_STATUS != 0 {
            return Err(Reject::InvalidStatus);
        }
        n.status = (n.status & !SETTABLE_STATUS) | (request.status & SETTABLE_STATUS);
    }
    if m & ADJ_OFFSET != 0 {
        let limit = if n.status & STA_NANO != 0 {
            500_000_000
        } else {
            500_000
        };
        if request.offset <= -limit || request.offset >= limit {
            return Err(Reject::InvalidTime);
        }
        n.offset = request.offset;
    }
    if m & ADJ_FREQUENCY != 0 {
        n.freq = request.freq.clamp(-32_768_000, 32_768_000);
    }
    if m & ADJ_MAXERROR != 0 {
        n.maxerror = request.maxerror
    }
    if m & ADJ_ESTERROR != 0 {
        n.esterror = request.esterror
    }
    if m & ADJ_TIMECONST != 0 {
        n.constant = request.constant
    }
    if m & ADJ_TICK != 0 {
        if request.tick < snapshot.tick_min || request.tick > snapshot.tick_max {
            return Err(Reject::InvalidTick);
        }
        n.tick = request.tick
    }
    Ok(TimexAdjustmentPlan {
        expected_version: snapshot.version,
        next: n,
        time_state: if n.status & STA_UNSYNC != 0 {
            TIME_ERROR
        } else {
            TIME_OK
        },
    })
}
pub fn commit_adjust(
    plan: TimexAdjustmentPlan,
    current: TimexSnapshot,
) -> Result<TimexSnapshot, Reject> {
    if current.version != plan.expected_version {
        return Err(Reject::StaleVersion);
    }
    let version = current.version.checked_add(1).ok_or(Reject::Overflow)?;
    Ok(TimexSnapshot {
        version,
        value: plan.next,
        tick_min: current.tick_min,
        tick_max: current.tick_max,
    })
}
pub const fn render(snapshot: TimexSnapshot) -> TimexRenderPlan {
    TimexRenderPlan {
        version: snapshot.version,
        value: snapshot.value,
        time_state: if snapshot.value.status & STA_UNSYNC != 0 {
            TIME_ERROR
        } else {
            TIME_OK
        },
    }
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
    fn timex_versioned() {
        let x = TimexSnapshot {
            version: 2,
            value: Timex {
                tick: 1,
                ..Timex::default()
            },
            tick_min: 1,
            tick_max: 1,
        };
        let p = plan_adjust(
            Timex {
                modes: ADJ_NANO,
                ..Timex::default()
            },
            x,
        )
        .unwrap();
        assert_eq!(
            commit_adjust(p, TimexSnapshot { version: 3, ..x }),
            Err(Reject::StaleVersion)
        );
    }

    #[test]
    fn timex_keeps_legacy_singleshot_and_validation_order() {
        let snapshot = TimexSnapshot {
            version: 1,
            value: Timex::default(),
            tick_min: 9,
            tick_max: 11,
        };
        assert_eq!(
            plan_adjust(
                Timex {
                    modes: ADJ_OFFSET_SINGLESHOT,
                    offset: 99,
                    ..Timex::default()
                },
                snapshot,
            )
            .unwrap()
            .next
            .offset,
            99
        );
        assert!(
            plan_adjust(
                Timex {
                    modes: ADJ_OFFSET_SS_READ,
                    ..Timex::default()
                },
                snapshot,
            )
            .is_ok()
        );
        assert_eq!(
            plan_adjust(
                Timex {
                    modes: ADJ_OFFSET | ADJ_TICK,
                    offset: 500_000,
                    tick: 0,
                    ..Timex::default()
                },
                snapshot,
            ),
            Err(Reject::InvalidTime)
        );
    }
}
