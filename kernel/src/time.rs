use core::{
    hint::spin_loop,
    sync::atomic::{AtomicI64, AtomicU64, Ordering, fence},
};

use axerrno::{AxError, AxResult};
use axhal::time::TimeValue;
use axpoll::PollSet;
use linux_raw_sys::general::{
    __kernel_old_timespec, __kernel_old_timeval, __kernel_sock_timeval, __kernel_timespec,
    timespec, timeval,
};

static WALL_TIME_OFFSET_NANOS: AtomicI64 = AtomicI64::new(0);
/// Even values are stable publications; odd values mean a writer is active.
static WALL_TIME_PUBLICATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// System-wide admission budget for active cancel-on-set readiness waits.
const WALL_TIME_DISCONTINUITY_WAIT_CAPACITY: usize = 64;
static WALL_TIME_DISCONTINUITY_WAITERS: PollSet<WALL_TIME_DISCONTINUITY_WAIT_CAPACITY> =
    PollSet::new();

fn apply_wall_time_offset(base_nanos: u64, offset_nanos: i64) -> u64 {
    let adjusted = base_nanos as i128 + offset_nanos as i128;
    adjusted.clamp(0, u64::MAX as i128) as u64
}

fn read_wall_time_publication<T>(
    sequence: &AtomicU64,
    mut read_value: impl FnMut() -> T,
) -> (T, u64) {
    loop {
        let before = sequence.load(Ordering::Acquire);
        if before & 1 != 0 {
            spin_loop();
            continue;
        }
        let value = read_value();
        // This is the read barrier in the sequence-counter protocol.  The
        // second sequence observation must not be performed before any of the
        // payload reads above; otherwise a weakly ordered CPU could accept a
        // new offset together with the old generation.
        fence(Ordering::Acquire);
        let after = sequence.load(Ordering::Acquire);
        if before == after {
            return (value, after >> 1);
        }
    }
}

fn wall_time_nanos_with_generation() -> (u64, u64) {
    read_wall_time_publication(&WALL_TIME_PUBLICATION_SEQUENCE, || {
        apply_wall_time_offset(
            axhal::time::wall_time_nanos(),
            WALL_TIME_OFFSET_NANOS.load(Ordering::Relaxed),
        )
    })
}

pub fn wall_time_nanos() -> u64 {
    wall_time_nanos_with_generation().0
}

pub fn wall_time() -> TimeValue {
    TimeValue::from_nanos(wall_time_nanos_with_generation().0)
}

/// Atomically snapshots wall time and its discontinuity generation.
pub(crate) fn wall_time_with_discontinuity_generation() -> (TimeValue, u64) {
    let (nanos, generation) = wall_time_nanos_with_generation();
    (TimeValue::from_nanos(nanos), generation)
}

fn next_wall_time_publication(stable: u64) -> Option<(u64, u64)> {
    if stable & 1 != 0 {
        return None;
    }
    Some((stable.checked_add(1)?, stable.checked_add(2)?))
}

pub fn set_wall_time(new_time: TimeValue) -> AxResult<()> {
    // A local timer interrupt may read wall time, so the writer must not be
    // interrupted or preempted while the publication sequence is odd.
    let publication_guard = kernel_guard::NoPreemptIrqSave::new();
    let mut stable = WALL_TIME_PUBLICATION_SEQUENCE.load(Ordering::Acquire);
    let published = loop {
        if stable & 1 != 0 {
            spin_loop();
            stable = WALL_TIME_PUBLICATION_SEQUENCE.load(Ordering::Acquire);
            continue;
        }
        let Some((updating, published)) = next_wall_time_publication(stable) else {
            return Err(AxError::OutOfRange);
        };
        match WALL_TIME_PUBLICATION_SEQUENCE.compare_exchange_weak(
            stable,
            updating,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break published,
            Err(observed) => stable = observed,
        }
    };

    let base_nanos = axhal::time::wall_time_nanos() as i128;
    let target_nanos = new_time.as_nanos().min(u64::MAX as u128) as i128;
    let offset = (target_nanos - base_nanos).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    // The odd/even sequence prevents readers from combining a new offset with
    // an old cancellation generation. Wake readiness consumers only after the
    // complete publication is visible, and never while holding object locks.
    WALL_TIME_OFFSET_NANOS.store(offset, Ordering::Relaxed);
    WALL_TIME_PUBLICATION_SEQUENCE.store(published, Ordering::Release);
    drop(publication_guard);
    WALL_TIME_DISCONTINUITY_WAITERS.wake();
    crate::task::notify_realtime_clock_change();
    Ok(())
}

/// Returns the publication generation of discontinuous wall-clock changes.
pub(crate) fn wall_time_discontinuity_generation() -> u64 {
    read_wall_time_publication(&WALL_TIME_PUBLICATION_SEQUENCE, || ()).1
}

/// Returns the bounded readiness source for discontinuous wall-clock changes.
pub(crate) fn wall_time_discontinuity_waiters()
-> &'static PollSet<WALL_TIME_DISCONTINUITY_WAIT_CAPACITY> {
    &WALL_TIME_DISCONTINUITY_WAITERS
}

#[cfg(test)]
mod publication_tests {
    use core::{cell::Cell, sync::atomic::AtomicI64};

    use super::*;

    #[test]
    fn snapshot_retries_instead_of_pairing_old_generation_with_new_value() {
        let sequence = AtomicU64::new(0);
        let value = AtomicI64::new(7);
        let first = Cell::new(true);

        let (observed, generation) = read_wall_time_publication(&sequence, || {
            let observed = value.load(Ordering::Relaxed);
            if first.replace(false) {
                sequence.store(1, Ordering::Release);
                value.store(9, Ordering::Relaxed);
                sequence.store(2, Ordering::Release);
            }
            observed
        });

        assert_eq!((observed, generation), (9, 1));
    }

    #[test]
    fn publication_generation_never_wraps() {
        assert_eq!(next_wall_time_publication(0), Some((1, 2)));
        assert_eq!(next_wall_time_publication(1), None);
        assert_eq!(next_wall_time_publication(u64::MAX - 1), None);
    }
}

/// A helper trait for converting from and to `TimeValue`.
pub trait TimeValueLike {
    /// Converts from `TimeValue`.
    fn from_time_value(tv: TimeValue) -> Self;

    /// Tries to convert into `TimeValue`.
    fn try_into_time_value(self) -> AxResult<TimeValue>;
}

impl TimeValueLike for TimeValue {
    fn from_time_value(tv: TimeValue) -> Self {
        tv
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        Ok(self)
    }
}

impl TimeValueLike for timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for __kernel_timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for __kernel_old_timespec {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_nsec: tv.subsec_nanos() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_nsec < 0 || self.tv_nsec > 999_999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(self.tv_sec as u64, self.tv_nsec as u32))
    }
}

impl TimeValueLike for timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}

impl TimeValueLike for __kernel_old_timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}

impl TimeValueLike for __kernel_sock_timeval {
    fn from_time_value(tv: TimeValue) -> Self {
        Self {
            tv_sec: tv.as_secs() as _,
            tv_usec: tv.subsec_micros() as _,
        }
    }

    fn try_into_time_value(self) -> AxResult<TimeValue> {
        if self.tv_usec < 0 || self.tv_usec > 999_999 || self.tv_sec < 0 {
            return Err(AxError::InvalidInput);
        }
        Ok(TimeValue::new(
            self.tv_sec as u64,
            self.tv_usec as u32 * 1000,
        ))
    }
}
