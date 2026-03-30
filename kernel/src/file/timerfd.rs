use alloc::{borrow::Cow, sync::Arc};
use core::{
    convert::TryFrom,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::AxError;
use axhal::time::{monotonic_time, wall_time};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::future::{block_on, poll_io};
use spin::Mutex;

use crate::{
    file::{FileLike, IoDst, IoSrc},
    task::{AlarmClock, register_pollset_alarm},
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum TimerClock {
    Realtime,
    Monotonic,
}

impl TimerClock {
    fn now(self) -> Duration {
        match self {
            Self::Realtime => wall_time(),
            Self::Monotonic => monotonic_time(),
        }
    }

    fn alarm_clock(self) -> AlarmClock {
        match self {
            Self::Realtime => AlarmClock::Realtime,
            Self::Monotonic => AlarmClock::Monotonic,
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
}

impl TimerFdInner {
    fn new() -> Self {
        Self {
            expirations: 0,
            interval: Duration::ZERO,
            next_expiration: None,
        }
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
    inner: Mutex<TimerFdInner>,
    non_blocking: AtomicBool,
    poll_rx: Arc<PollSet>,
}

impl TimerFd {
    pub fn new(clock: TimerClock) -> Arc<Self> {
        Arc::new(Self {
            clock,
            inner: Mutex::new(TimerFdInner::new()),
            non_blocking: AtomicBool::new(false),
            poll_rx: Arc::default(),
        })
    }

    /// Arms or disarms the timer. Returns the old (interval, value) setting.
    pub fn settime(
        &self,
        absolute: bool,
        interval: Duration,
        value: Duration,
    ) -> (Duration, Duration) {
        let mut inner = self.inner.lock();
        let now = self.clock.now();

        // Capture old state before modifying.
        inner.update_expirations(now);
        let old_interval = inner.interval;
        let old_value = inner
            .next_expiration
            .map(|exp| exp.saturating_sub(now))
            .unwrap_or(Duration::ZERO);

        // Reset expiration counter on re-arm.
        inner.expirations = 0;

        if value.is_zero() {
            // Disarm the timer.
            inner.interval = Duration::ZERO;
            inner.next_expiration = None;
        } else {
            inner.interval = interval;
            let deadline = if absolute {
                value
            } else {
                now.checked_add(value).unwrap_or(Duration::MAX)
            };
            inner.next_expiration = Some(deadline);

            // Register with the alarm system so epoll/poll can be woken.
            register_pollset_alarm(self.clock.alarm_clock(), deadline, self.poll_rx.clone());
        }

        (old_interval, old_value)
    }

    /// Returns the current (interval, time-until-next-expiration).
    pub fn gettime(&self) -> (Duration, Duration) {
        let mut inner = self.inner.lock();
        let now = self.clock.now();
        inner.update_expirations(now);

        let value = inner
            .next_expiration
            .map(|exp| exp.saturating_sub(now))
            .unwrap_or(Duration::ZERO);
        (inner.interval, value)
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.inner.lock().next_expiration
    }
}

impl FileLike for TimerFd {
    fn read(&self, dst: &mut IoDst) -> axio::Result<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(AxError::InvalidInput);
        }

        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let mut inner = self.inner.lock();
            let now = self.clock.now();
            inner.update_expirations(now);

            if inner.expirations > 0 {
                let count = inner.expirations;
                inner.expirations = 0;

                // If repeating, register the next alarm.
                if let Some(deadline) = inner.next_expiration {
                    register_pollset_alarm(
                        self.clock.alarm_clock(),
                        deadline,
                        self.poll_rx.clone(),
                    );
                }

                dst.write(&count.to_ne_bytes())?;
                Ok(size_of::<u64>())
            } else {
                Err(AxError::WouldBlock)
            }
        }))
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

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[timerfd]".into()
    }
}

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let mut inner = self.inner.lock();
        let now = self.clock.now();
        inner.update_expirations(now);

        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, inner.expirations > 0);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
            // Ensure we get woken when the timer fires.
            if let Some(deadline) = self.next_deadline() {
                register_pollset_alarm(self.clock.alarm_clock(), deadline, self.poll_rx.clone());
            }
        }
    }
}
