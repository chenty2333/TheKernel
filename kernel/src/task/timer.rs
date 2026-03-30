//! Time management module.

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, binary_heap::BinaryHeap},
    sync::Arc,
};
use core::{
    future::{Future, poll_fn},
    mem,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos, wall_time};
use axpoll::PollSet;
use axtask::{
    WeakAxTaskRef, current, register_timer_callback,
    future::block_on,
};
use event_listener::{Event, listener};
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use spin::Mutex;
use starry_signal::Signo;
use strum::FromRepr;

use crate::task::poll_timer;

fn time_value_from_nanos(nanos: usize) -> TimeValue {
    let secs = nanos as u64 / NANOS_PER_SEC;
    let nsecs = nanos as u64 - secs * NANOS_PER_SEC;
    TimeValue::new(secs, nsecs as u32)
}

/// Clock domain used by alarms.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum AlarmClock {
    Realtime,
    Monotonic,
}

impl AlarmClock {
    pub(crate) fn now(self) -> Duration {
        match self {
            AlarmClock::Realtime => wall_time(),
            AlarmClock::Monotonic => axhal::time::monotonic_time(),
        }
    }
}

/// The action to take when an alarm fires.
enum AlarmAction {
    /// Interrupt a task and poll its itimers.
    PollTask(WeakAxTaskRef),
    /// Wake a PollSet (used by timerfd).
    WakePollSet(Arc<PollSet>),
}

struct Entry {
    deadline: Duration,
    action: AlarmAction,
}
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimerKey {
    deadline: Duration,
    key: u64,
}

#[derive(Default)]
struct ClockTimerRuntime {
    next_key: u64,
    wheel: BTreeMap<TimerKey, Waker>,
}

impl ClockTimerRuntime {
    fn add(&mut self, now: Duration, deadline: Duration) -> Option<TimerKey> {
        if deadline <= now {
            return None;
        }

        let key = TimerKey {
            deadline,
            key: self.next_key,
        };
        self.wheel.insert(key, Waker::noop().clone());
        self.next_key += 1;
        Some(key)
    }

    fn poll(&mut self, key: &TimerKey, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(waker) = self.wheel.get_mut(key) {
            *waker = cx.waker().clone();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn cancel(&mut self, key: &TimerKey) {
        self.wheel.remove(key);
    }

    fn wake(&mut self, now: Duration) {
        if self.wheel.is_empty() {
            return;
        }

        let pending = self.wheel.split_off(&TimerKey {
            deadline: now,
            key: u64::MAX,
        });
        let expired = mem::replace(&mut self.wheel, pending);
        for (_, waker) in expired {
            waker.wake();
        }
    }
}

struct ClockTimerFuture {
    clock: AlarmClock,
    key: TimerKey,
}

impl Future for ClockTimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        timer_runtime(self.clock).lock().poll(&self.key, cx)
    }
}

impl Drop for ClockTimerFuture {
    fn drop(&mut self) {
        timer_runtime(self.clock).lock().cancel(&self.key);
    }
}

lazy_static! {
    static ref REALTIME_ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
    static ref MONOTONIC_ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
    static ref REALTIME_ALARM_EVENT: Event = Event::new();
    static ref MONOTONIC_ALARM_EVENT: Event = Event::new();
    static ref REALTIME_TIMER_RUNTIME: SpinNoIrq<ClockTimerRuntime> =
        SpinNoIrq::new(ClockTimerRuntime::default());
    static ref MONOTONIC_TIMER_RUNTIME: SpinNoIrq<ClockTimerRuntime> =
        SpinNoIrq::new(ClockTimerRuntime::default());
}

static CLOCK_TIMER_CALLBACK_REGISTERED: [AtomicBool; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicBool::new(false) }; axconfig::plat::MAX_CPU_NUM];

/// The type of interval timer.
#[repr(i32)]
#[allow(non_camel_case_types)]
#[derive(Eq, PartialEq, Debug, Clone, Copy, FromRepr)]
pub enum ITimerType {
    /// 统计系统实际运行时间
    Real    = 0,
    /// 统计用户态运行时间
    Virtual = 1,
    /// 统计进程的所有用户态/内核态运行时间
    Prof    = 2,
}

impl ITimerType {
    /// Returns the signal number associated with this timer type.
    pub fn signo(&self) -> Signo {
        match self {
            ITimerType::Real => Signo::SIGALRM,
            ITimerType::Virtual => Signo::SIGVTALRM,
            ITimerType::Prof => Signo::SIGPROF,
        }
    }
}

#[derive(Default)]
struct ITimer {
    interval_ns: usize,
    remained_ns: usize,
}

impl ITimer {
    pub fn new(interval_ns: usize, remained_ns: usize) -> Self {
        let result = Self {
            interval_ns,
            remained_ns,
        };
        result.renew_timer();
        result
    }

    pub fn update(&mut self, delta: usize) -> bool {
        if self.remained_ns == 0 {
            return false;
        }
        if self.remained_ns > delta {
            self.remained_ns -= delta;
            false
        } else {
            self.remained_ns = self.interval_ns;
            self.renew_timer();
            true
        }
    }

    pub fn renew_timer(&self) {
        if self.remained_ns > 0 {
            let deadline = wall_time()
                .checked_add(Duration::from_nanos(self.remained_ns as u64))
                .unwrap_or(Duration::MAX);
            register_alarm(
                AlarmClock::Realtime,
                deadline,
                AlarmAction::PollTask(Arc::downgrade(&current())),
            );
        }
    }
}

/// Represents the state of the timer.
#[derive(Debug)]
pub enum TimerState {
    /// Fallback state.
    None,
    /// The timer is running in user space.
    User,
    /// The timer is running in kernel space.
    Kernel,
}

// TODO(mivik): preempting does not change the timer state currently
/// A manager for time-related operations.
pub struct TimeManager {
    utime_ns: usize,
    stime_ns: usize,
    last_wall_ns: usize,
    state: TimerState,
    itimers: [ITimer; 3],
}

impl Default for TimeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeManager {
    pub(crate) fn new() -> Self {
        Self {
            utime_ns: 0,
            stime_ns: 0,
            last_wall_ns: 0,
            state: TimerState::None,
            itimers: Default::default(),
        }
    }

    /// Returns the current user time and system time as a tuple of `TimeValue`.
    pub fn output(&self) -> (TimeValue, TimeValue) {
        let utime = time_value_from_nanos(self.utime_ns);
        let stime = time_value_from_nanos(self.stime_ns);
        (utime, stime)
    }

    /// Polls the time manager to update the timers and emit signals if
    /// necessary.
    pub fn poll(&mut self, emitter: impl Fn(Signo)) {
        let now_ns = monotonic_time_nanos() as usize;
        let delta = now_ns - self.last_wall_ns;
        match self.state {
            TimerState::User => {
                self.utime_ns += delta;
                self.update_itimer(ITimerType::Virtual, delta, &emitter);
                self.update_itimer(ITimerType::Prof, delta, &emitter);
            }
            TimerState::Kernel => {
                self.stime_ns += delta;
                self.update_itimer(ITimerType::Prof, delta, &emitter);
            }
            TimerState::None => {}
        }
        self.update_itimer(ITimerType::Real, delta, &emitter);
        self.last_wall_ns = now_ns;
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.state = state;
    }

    /// Sets the interval timer of the specified type with the given interval
    /// and remaining time.
    pub fn set_itimer(
        &mut self,
        ty: ITimerType,
        interval_ns: usize,
        remained_ns: usize,
    ) -> (TimeValue, TimeValue) {
        let old = mem::replace(
            &mut self.itimers[ty as usize],
            ITimer::new(interval_ns, remained_ns),
        );
        (
            time_value_from_nanos(old.interval_ns),
            time_value_from_nanos(old.remained_ns),
        )
    }

    /// Gets the current interval and remaining time.
    pub fn get_itimer(&self, ty: ITimerType) -> (TimeValue, TimeValue) {
        let itimer = &self.itimers[ty as usize];
        (
            time_value_from_nanos(itimer.interval_ns),
            time_value_from_nanos(itimer.remained_ns),
        )
    }

    fn update_itimer(&mut self, ty: ITimerType, delta: usize, emitter: impl Fn(Signo)) {
        if self.itimers[ty as usize].update(delta) {
            emitter(ty.signo());
        }
    }
}

enum AlarmWait {
    DeadlineReached,
    NewTimer,
}

async fn alarm_task(clock: AlarmClock) {
    loop {
        // Register before inspecting the queues so a newly inserted earlier
        // deadline cannot race past us and get delayed until a stale timeout.
        listener!(alarm_event(clock) => listener);

        if process_due(clock) {
            continue;
        }

        let Some(deadline) = queue_deadline(clock) else {
            listener.await;
            continue;
        };

        let _ = wait_until_or_alarm(clock, deadline, listener).await;
    }
}

/// Spawns the alarm task.
pub fn spawn_alarm_task() {
    info!("Initialize alarm...");
    ensure_clock_timer_runtime();
    axtask::spawn_raw(
        || block_on(alarm_task(AlarmClock::Realtime)),
        "alarm_realtime".to_owned(),
        axconfig::TASK_STACK_SIZE,
    );
    axtask::spawn_raw(
        || block_on(alarm_task(AlarmClock::Monotonic)),
        "alarm_monotonic".to_owned(),
        axconfig::TASK_STACK_SIZE,
    );
}

fn alarm_list(clock: AlarmClock) -> &'static Mutex<BinaryHeap<Entry>> {
    match clock {
        AlarmClock::Realtime => &REALTIME_ALARM_LIST,
        AlarmClock::Monotonic => &MONOTONIC_ALARM_LIST,
    }
}

fn alarm_event(clock: AlarmClock) -> &'static Event {
    match clock {
        AlarmClock::Realtime => &REALTIME_ALARM_EVENT,
        AlarmClock::Monotonic => &MONOTONIC_ALARM_EVENT,
    }
}

fn timer_runtime(clock: AlarmClock) -> &'static SpinNoIrq<ClockTimerRuntime> {
    match clock {
        AlarmClock::Realtime => &REALTIME_TIMER_RUNTIME,
        AlarmClock::Monotonic => &MONOTONIC_TIMER_RUNTIME,
    }
}

fn ensure_clock_timer_runtime() {
    let cpu_id = axhal::percpu::this_cpu_id();
    if CLOCK_TIMER_CALLBACK_REGISTERED[cpu_id]
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        register_timer_callback(|_| {
            wake_clock_timers(AlarmClock::Realtime);
            wake_clock_timers(AlarmClock::Monotonic);
        });
    }
}

fn wake_clock_timers(clock: AlarmClock) {
    timer_runtime(clock).lock().wake(clock.now());
}

fn register_alarm(clock: AlarmClock, deadline: Duration, action: AlarmAction) {
    let list = alarm_list(clock);
    let mut guard = list.lock();
    let should_wake = guard.peek().is_none_or(|it| it.deadline > deadline);
    guard.push(Entry { deadline, action });
    drop(guard);
    if should_wake {
        alarm_event(clock).notify(1);
    }
}

/// Registers a one-shot alarm that wakes the given [`PollSet`] at the specified
/// deadline in the selected clock domain. Used by timerfd to get notified when
/// the timer expires.
pub fn register_pollset_alarm(clock: AlarmClock, deadline: Duration, poll_set: Arc<PollSet>) {
    register_alarm(clock, deadline, AlarmAction::WakePollSet(poll_set));
}

fn queue_deadline(clock: AlarmClock) -> Option<Duration> {
    let list = alarm_list(clock);
    let guard = list.lock();
    Some(guard.peek()?.deadline)
}

fn pop_due(clock: AlarmClock) -> Option<AlarmAction> {
    let list = alarm_list(clock);
    let mut guard = list.lock();
    let now = clock.now();
    if guard.peek().is_some_and(|entry| entry.deadline <= now) {
        guard.pop().map(|entry| entry.action)
    } else {
        None
    }
}

fn process_due(clock: AlarmClock) -> bool {
    let mut progressed = false;
    while let Some(action) = pop_due(clock) {
        progressed = true;
        match action {
            AlarmAction::PollTask(weak_task) => {
                if let Some(task) = weak_task.upgrade() {
                    poll_timer(&task);
                }
            }
            AlarmAction::WakePollSet(poll_set) => {
                poll_set.wake();
            }
        }
    }
    progressed
}

async fn wait_until_or_alarm<L>(clock: AlarmClock, deadline: Duration, mut listener: L) -> AlarmWait
where
    L: Future<Output = ()> + Unpin,
{
    let mut sleeper = core::pin::pin!(sleep_until_clock(clock, deadline));
    poll_fn(|cx| {
        if Pin::new(&mut listener).poll(cx).is_ready() {
            return Poll::Ready(AlarmWait::NewTimer);
        }
        if sleeper.as_mut().poll(cx).is_ready() {
            return Poll::Ready(AlarmWait::DeadlineReached);
        }
        Poll::Pending
    })
    .await
}

pub async fn sleep_until_clock(clock: AlarmClock, deadline: Duration) {
    ensure_clock_timer_runtime();
    let key = timer_runtime(clock).lock().add(clock.now(), deadline);
    if let Some(key) = key {
        ClockTimerFuture { clock, key }.await;
    }
}
