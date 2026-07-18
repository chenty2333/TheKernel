use alloc::{borrow::Cow, sync::Arc};
use core::{
    convert::TryFrom,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollSet, Pollable, PreparedPollRegistration};
use axtask::current;
use spin::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, anon_inode_stat},
    readiness::block_on_poll_io,
    task::{
        AlarmClock, AlarmPublication, AlarmToken, AlarmTokenReserveError, AsThread,
        prepare_pollset_alarm,
    },
    time::{
        wall_time_discontinuity_generation, wall_time_discontinuity_waiters,
        wall_time_with_discontinuity_generation,
    },
};

fn clock_snapshot(clock: AlarmClock) -> (Duration, u64) {
    match clock {
        AlarmClock::Realtime => wall_time_with_discontinuity_generation(),
        AlarmClock::Monotonic => (clock.now(), wall_time_discontinuity_generation()),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum TimerClock {
    Realtime,
    Monotonic,
    Boottime,
}

impl TimerClock {
    fn effective_alarm_clock(self, absolute: bool) -> AlarmClock {
        if !absolute {
            return AlarmClock::Monotonic;
        }
        match self {
            Self::Realtime => AlarmClock::Realtime,
            Self::Monotonic | Self::Boottime => AlarmClock::Monotonic,
        }
    }

    fn cancellation_enabled(self, absolute: bool, requested: bool) -> bool {
        requested && absolute && self == Self::Realtime
    }

    fn absolute_deadline_to_host(self, value: Duration) -> Duration {
        match self {
            Self::Realtime => value,
            Self::Monotonic => current()
                .as_thread()
                .proc_data
                .time_ns()
                .host_monotonic_deadline(value),
            Self::Boottime => current()
                .as_thread()
                .proc_data
                .time_ns()
                .host_boottime_deadline(value),
        }
    }
}

struct TimerFdInner {
    /// Number of expirations since last read.
    expirations: u64,
    /// Interval for repeating timers (Duration::ZERO = one-shot).
    interval: Duration,
    /// Absolute time of next expiration (None = disarmed).
    next_expiration: Option<Duration>,
    /// Clock basis used by the currently armed deadline.
    effective_clock: AlarmClock,
    /// Whether a wall-clock discontinuity cancels this arm.
    cancel_on_set: bool,
    /// Last wall-clock discontinuity generation observed by this timer.
    wall_generation: u64,
    /// One pending cancellation terminal for `read(2)`.
    cancelled: bool,
    /// Whether the pending cancellation currently contributes poll readiness.
    /// A cancelable disarm resets readiness like Linux's `ticks = 0`, while
    /// retaining the cancellation terminal for a direct read or later rearm.
    cancellation_readable: bool,
}

impl TimerFdInner {
    fn new(wall_generation: u64) -> Self {
        Self {
            expirations: 0,
            interval: Duration::ZERO,
            next_expiration: None,
            effective_clock: AlarmClock::Monotonic,
            cancel_on_set: false,
            wall_generation,
            cancelled: false,
            cancellation_readable: false,
        }
    }

    fn rearm(
        &mut self,
        effective_clock: AlarmClock,
        interval: Duration,
        next_expiration: Option<Duration>,
        cancel_on_set: bool,
        wall_generation: u64,
    ) -> bool {
        // Linux checks the pending cancellation from settime only for a
        // nonzero replacement arm. A cancelable disarm succeeds, resets poll
        // readiness, and retains the hidden cancellation sentinel for a
        // direct read or a later nonzero rearm.
        let armed = next_expiration.is_some();
        let report_cancellation = cancel_on_set && armed && self.cancelled;
        let preserve_cancellation = cancel_on_set && !armed && self.cancelled;
        self.expirations = 0;
        self.interval = interval;
        self.next_expiration = next_expiration;
        self.effective_clock = effective_clock;
        self.cancel_on_set = cancel_on_set;
        self.wall_generation = wall_generation;
        self.cancelled = preserve_cancellation;
        self.cancellation_readable = false;
        report_cancellation
    }

    fn refresh_at(&mut self, now: Duration, wall_generation: u64) {
        if self.cancel_on_set && self.wall_generation != wall_generation {
            self.wall_generation = wall_generation;
            self.cancelled = true;
            self.cancellation_readable = true;
            self.expirations = 0;

            // Linux does not rearm an already-expired timer after reporting
            // ECANCELED. Preserve a future deadline, but retire a deadline
            // made immediately due by this discontinuity.
            if self.next_expiration.is_some_and(|deadline| deadline <= now) {
                self.next_expiration = None;
            }
        }

        if !self.cancelled {
            self.update_expirations(now);
        }
    }

    fn take_cancellation(&mut self) -> bool {
        let cancelled = core::mem::take(&mut self.cancelled);
        self.cancellation_readable = false;
        cancelled
    }

    fn take_cancellation_error(&mut self) -> Option<AxError> {
        self.take_cancellation()
            .then(|| LinuxError::ECANCELED.into())
    }

    /// Lazily compute expirations based on current time.
    fn update_expirations(&mut self, now: Duration) {
        let Some(next) = self.next_expiration else {
            return;
        };
        if now < next {
            return;
        }

        if self.interval.is_zero() {
            // One-shot timer: fires once.
            self.expirations += 1;
            self.next_expiration = None;
        } else {
            // Repeating timer: count how many intervals have elapsed.
            let elapsed = now - next;
            let interval_nanos = self.interval.as_nanos();
            let count = 1 + elapsed.as_nanos() / interval_nanos;
            self.expirations = self
                .expirations
                .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));

            let next_nanos = interval_nanos.saturating_mul(count);
            let next_nanos = u64::try_from(next_nanos).unwrap_or(u64::MAX);
            self.next_expiration = Some(
                next.checked_add(Duration::from_nanos(next_nanos))
                    .unwrap_or(Duration::MAX),
            );
        }
    }
}

pub struct TimerFd {
    clock: TimerClock,
    /// Persistent registry lease owned by this open file description.
    alarm: AlarmToken,
    inner: Mutex<TimerFdInner>,
    non_blocking: AtomicBool,
    poll_rx: Arc<PollSet>,
}

impl TimerFd {
    pub fn try_new(clock: TimerClock) -> AxResult<Arc<Self>> {
        let alarm = AlarmToken::try_new().map_err(|error| match error {
            // Linux reports timerfd object-allocation failure as ENOMEM.
            AlarmTokenReserveError::CapacityExhausted => AxError::NoMemory,
            AlarmTokenReserveError::TokenSpaceExhausted => AxError::OutOfRange,
        })?;
        let poll_rx = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let wall_generation = wall_time_discontinuity_generation();
        Arc::try_new(Self {
            clock,
            alarm,
            inner: Mutex::new(TimerFdInner::new(wall_generation)),
            non_blocking: AtomicBool::new(false),
            poll_rx,
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn prepare_alarm(&self, clock: AlarmClock, deadline: Option<Duration>) -> AlarmPublication {
        match deadline {
            Some(deadline) => {
                prepare_pollset_alarm(&self.alarm, clock, deadline, self.poll_rx.clone())
            }
            None => self.alarm.prepare_disarm(),
        }
    }

    /// Arms or disarms the timer. Returns the old (interval, value) setting.
    pub fn settime(
        &self,
        absolute: bool,
        cancel_on_set: bool,
        interval: Duration,
        value: Duration,
    ) -> AxResult<(Duration, Duration)> {
        let mut inner = self.inner.lock();

        // Capture old state before modifying.
        let (old_now, old_wall_generation) = clock_snapshot(inner.effective_clock);
        inner.refresh_at(old_now, old_wall_generation);
        let old_interval = inner.interval;
        let old_value = inner
            .next_expiration
            .map(|exp| exp.saturating_sub(old_now))
            .unwrap_or(Duration::ZERO);

        let effective_clock = self.clock.effective_alarm_clock(absolute);
        let (new_now, new_wall_generation) = clock_snapshot(effective_clock);
        let deadline = if value.is_zero() {
            None
        } else if absolute {
            Some(self.clock.absolute_deadline_to_host(value))
        } else {
            Some(new_now.checked_add(value).unwrap_or(Duration::MAX))
        };
        let publication = self.prepare_alarm(effective_clock, deadline);
        let report_cancellation = inner.rearm(
            effective_clock,
            interval,
            deadline,
            self.clock.cancellation_enabled(absolute, cancel_on_set),
            new_wall_generation,
        );
        drop(inner);

        publication.publish();
        // Rearm/disarm changes the object operation and cancellation topology.
        // Wake retained registrations only after publishing the complete state.
        self.poll_rx.wake();

        if report_cancellation {
            Err(LinuxError::ECANCELED.into())
        } else {
            Ok((old_interval, old_value))
        }
    }

    /// Returns the current (interval, time-until-next-expiration).
    pub fn gettime(&self) -> (Duration, Duration) {
        let (result, publication) = {
            let mut inner = self.inner.lock();
            let was_armed = inner.next_expiration.is_some();
            let (now, wall_generation) = clock_snapshot(inner.effective_clock);
            inner.refresh_at(now, wall_generation);

            let value = inner
                .next_expiration
                .map(|exp| exp.saturating_sub(now))
                .unwrap_or(Duration::ZERO);
            let publication =
                (was_armed && inner.next_expiration.is_none()).then(|| self.alarm.prepare_disarm());
            ((inner.interval, value), publication)
        };
        if let Some(publication) = publication {
            publication.publish();
        }
        result
    }
}

impl FileLike for TimerFd {
    fn stat(&self) -> axio::Result<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> axio::Result<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(AxError::InvalidInput);
        }

        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
            let (result, publication) = {
                let mut inner = self.inner.lock();
                let was_armed = inner.next_expiration.is_some();
                let (now, wall_generation) = clock_snapshot(inner.effective_clock);
                inner.refresh_at(now, wall_generation);

                if let Some(error) = inner.take_cancellation_error() {
                    let publication = (was_armed && inner.next_expiration.is_none())
                        .then(|| self.alarm.prepare_disarm());
                    (Err(error), publication)
                } else if inner.expirations == 0 {
                    (Err(AxError::WouldBlock), None)
                } else {
                    let count = inner.expirations;
                    inner.expirations = 0;
                    let publication =
                        self.prepare_alarm(inner.effective_clock, inner.next_expiration);
                    (Ok(count), Some(publication))
                }
            };

            if let Some(publication) = publication {
                publication.publish();
            }
            let count = result?;
            dst.write(&count.to_ne_bytes())?;
            Ok(size_of::<u64>())
        })
    }

    fn write(&self, _src: &mut IoSrc) -> axio::Result<usize> {
        // timerfd is read-only
        Err(AxError::BadFileDescriptor)
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> axio::Result {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[timerfd]".into())
    }
}

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let (events, publication) = {
            let mut inner = self.inner.lock();
            let was_armed = inner.next_expiration.is_some();
            let (now, wall_generation) = clock_snapshot(inner.effective_clock);
            inner.refresh_at(now, wall_generation);

            let mut events = IoEvents::empty();
            events.set(
                IoEvents::READABLE,
                inner.cancellation_readable || inner.expirations > 0,
            );
            let publication =
                (was_armed && inner.next_expiration.is_none()).then(|| self.alarm.prepare_disarm());
            (events, publication)
        };
        if let Some(publication) = publication {
            publication.publish();
        }
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            let cancel_on_set = self.inner.lock().cancel_on_set;
            let mut prepared = PreparedPollRegistration::try_new(1 + usize::from(cancel_on_set))?;
            prepared.arm(&self.poll_rx, context.waker())?;
            if cancel_on_set {
                // The shared source is finite and is charged only by a live
                // cancelable arm, rather than every advertised realtime fd.
                prepared.arm(wall_time_discontinuity_waiters(), context.waker())?;
            }
            let registration = prepared.commit()?;
            if self.inner.lock().cancel_on_set != cancel_on_set {
                // settime publishes state before waking poll_rx. If its wake
                // raced before the first source was armed, close that gap now
                // so the outer check-arm-check loop rebuilds this topology.
                self.poll_rx.wake();
            }
            Ok(registration)
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use core::task::Context;

    use super::*;

    #[test]
    fn relative_arms_always_use_monotonic_time() {
        for clock in [
            TimerClock::Realtime,
            TimerClock::Monotonic,
            TimerClock::Boottime,
        ] {
            assert_eq!(clock.effective_alarm_clock(false), AlarmClock::Monotonic);
        }
        assert_eq!(
            TimerClock::Realtime.effective_alarm_clock(true),
            AlarmClock::Realtime
        );
    }

    #[test]
    fn cancel_on_set_is_effective_only_for_absolute_realtime() {
        assert!(TimerClock::Realtime.cancellation_enabled(true, true));
        assert!(!TimerClock::Realtime.cancellation_enabled(false, true));
        assert!(!TimerClock::Monotonic.cancellation_enabled(true, true));
        assert!(!TimerClock::Boottime.cancellation_enabled(true, true));
        assert!(!TimerClock::Realtime.cancellation_enabled(true, false));
    }

    #[test]
    fn one_clock_change_yields_one_cancellation_terminal() {
        let mut inner = TimerFdInner::new(7);
        inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(100)),
            true,
            7,
        );

        inner.refresh_at(Duration::from_secs(90), 8);
        assert!(inner.cancellation_readable);
        assert_eq!(
            inner.take_cancellation_error().map(LinuxError::from),
            Some(LinuxError::ECANCELED)
        );
        assert_eq!(inner.take_cancellation_error(), None);
        assert!(!inner.cancellation_readable);

        inner.refresh_at(Duration::from_secs(90), 8);
        assert!(!inner.take_cancellation());
        inner.refresh_at(Duration::from_secs(90), 9);
        assert!(inner.take_cancellation());
    }

    #[test]
    fn rearm_clears_pending_cancellation_and_rebases_generation() {
        let mut inner = TimerFdInner::new(11);
        inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(5)),
            true,
            11,
        );
        inner.refresh_at(Duration::from_secs(1), 12);
        assert!(inner.cancelled);
        assert!(inner.cancellation_readable);

        inner.rearm(
            AlarmClock::Monotonic,
            Duration::from_secs(2),
            Some(Duration::from_secs(7)),
            false,
            12,
        );
        assert!(!inner.cancelled);
        assert!(!inner.cancellation_readable);
        assert_eq!(inner.wall_generation, 12);
        assert_eq!(inner.effective_clock, AlarmClock::Monotonic);
    }

    #[test]
    fn cancellation_retires_a_deadline_made_due_by_the_clock_step() {
        let mut inner = TimerFdInner::new(3);
        inner.rearm(
            AlarmClock::Realtime,
            Duration::from_secs(1),
            Some(Duration::from_secs(10)),
            true,
            3,
        );
        inner.refresh_at(Duration::from_secs(20), 4);

        assert!(inner.take_cancellation());
        assert_eq!(inner.next_expiration, None);
        assert_eq!(inner.expirations, 0);
    }

    #[test]
    fn cancelable_realtime_wait_owns_the_bounded_clock_change_source() {
        let timer = TimerFd::try_new(TimerClock::Realtime).unwrap();
        timer.inner.lock().cancel_on_set = true;
        let mut context = Context::from_waker(core::task::Waker::noop());
        let registration = timer.register(&mut context, IoEvents::READABLE).unwrap();

        assert_eq!(registration.source_count(), 2);
    }

    #[test]
    fn monotonic_wait_registration_does_not_charge_the_clock_change_source() {
        let timer = TimerFd::try_new(TimerClock::Monotonic).unwrap();
        let mut context = Context::from_waker(core::task::Waker::noop());
        let registration = timer.register(&mut context, IoEvents::READABLE).unwrap();

        assert_eq!(registration.source_count(), 1);
    }

    #[test]
    fn relative_realtime_wait_does_not_charge_the_clock_change_source() {
        let timer = TimerFd::try_new(TimerClock::Realtime).unwrap();
        let mut context = Context::from_waker(core::task::Waker::noop());
        let registration = timer.register(&mut context, IoEvents::READABLE).unwrap();

        assert_eq!(registration.source_count(), 1);
    }

    #[test]
    fn cancelable_rearm_commits_but_reports_a_pending_cancellation() {
        let mut inner = TimerFdInner::new(21);
        assert!(!inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(10)),
            true,
            21,
        ));
        inner.refresh_at(Duration::from_secs(1), 22);
        assert!(inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(20)),
            true,
            22,
        ));
        assert!(!inner.cancelled);
        assert_eq!(inner.next_expiration, Some(Duration::from_secs(20)));
    }

    #[test]
    fn cancelable_disarm_succeeds_and_preserves_the_hidden_cancellation() {
        let mut inner = TimerFdInner::new(31);
        assert!(!inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(10)),
            true,
            31,
        ));
        inner.refresh_at(Duration::from_secs(1), 32);
        assert!(inner.cancelled);
        assert!(inner.cancellation_readable);

        assert!(!inner.rearm(AlarmClock::Realtime, Duration::ZERO, None, true, 32,));
        assert!(inner.cancelled);
        assert!(!inner.cancellation_readable);

        // Linux consumes that retained sentinel when a later nonzero
        // cancelable arm is installed, after committing the replacement.
        assert!(inner.rearm(
            AlarmClock::Realtime,
            Duration::ZERO,
            Some(Duration::from_secs(20)),
            true,
            32,
        ));
        assert!(!inner.cancelled);
        assert!(!inner.cancellation_readable);
        assert_eq!(inner.next_expiration, Some(Duration::from_secs(20)));
    }
}
