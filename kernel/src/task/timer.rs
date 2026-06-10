//! Time management module.

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, binary_heap::BinaryHeap},
    sync::{Arc, Weak},
};
use core::{
    future::{Future, poll_fn},
    mem,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::LinuxError;
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos};
use axpoll::PollSet;
use axtask::{WeakAxTaskRef, current, future::block_on, register_timer_callback};
use event_listener::{Event, listener};
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use linux_raw_sys::general::SI_TIMER;
use spin::Mutex;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use strum::FromRepr;

use super::{ProcessData, poll_timer, send_signal_to_process, send_signal_to_visible_thread};
use crate::time::wall_time;

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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum PosixTimerClock {
    Realtime,
    Monotonic,
    Tai,
    ProcessCpu,
    ThreadCpu,
}

impl PosixTimerClock {
    pub(crate) fn alarm_clock(self) -> AlarmClock {
        match self {
            Self::Realtime | Self::Tai => AlarmClock::Realtime,
            Self::Monotonic | Self::ProcessCpu | Self::ThreadCpu => AlarmClock::Monotonic,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum PosixTimerNotify {
    None,
    Signal {
        signo: Signo,
        target_tid: Option<Pid>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct PosixTimer {
    pub clock: PosixTimerClock,
    pub notify: PosixTimerNotify,
    pub interval: Duration,
    pub deadline: Option<Duration>,
    pub sequence: u64,
    pub overrun: i32,
    pub signal_pending: bool,
}

impl PosixTimer {
    pub(crate) fn new(clock: PosixTimerClock, notify: PosixTimerNotify) -> Self {
        Self {
            clock,
            notify,
            interval: Duration::ZERO,
            deadline: None,
            sequence: 0,
            overrun: 0,
            signal_pending: false,
        }
    }
}

/// The action to take when an alarm fires.
enum AlarmAction {
    /// Interrupt a task and poll its itimers.
    PollTask(WeakAxTaskRef),
    /// Wake a PollSet (used by timerfd).
    WakePollSet(Arc<PollSet>),
    /// Deliver a POSIX timer event.
    PosixTimer {
        proc: Weak<ProcessData>,
        timerid: usize,
        sequence: u64,
    },
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

    fn next_deadline(&self) -> Option<Duration> {
        self.wheel.first_key_value().map(|(key, _)| key.deadline)
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
        if self.clock == AlarmClock::Monotonic {
            update_monotonic_timer_deadline();
        }
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
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
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
    last_cpu_ns: usize,
    last_wall_ns: usize,
    state: TimerState,
    paused_state: TimerState,
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
            last_cpu_ns: 0,
            last_wall_ns: 0,
            state: TimerState::None,
            paused_state: TimerState::None,
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
        let wall_delta = now_ns.saturating_sub(self.last_wall_ns);
        let cpu_delta = now_ns.saturating_sub(self.last_cpu_ns);
        match self.state {
            TimerState::User => {
                self.utime_ns += cpu_delta;
                self.update_itimer(ITimerType::Virtual, cpu_delta, &emitter);
                self.update_itimer(ITimerType::Prof, cpu_delta, &emitter);
            }
            TimerState::Kernel => {
                self.stime_ns += cpu_delta;
                self.update_itimer(ITimerType::Prof, cpu_delta, &emitter);
            }
            TimerState::None => {}
        }
        self.update_itimer(ITimerType::Real, wall_delta, &emitter);
        self.last_cpu_ns = now_ns;
        self.last_wall_ns = now_ns;
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.last_cpu_ns = monotonic_time_nanos() as usize;
        self.state = state;
    }

    /// Pauses CPU-time accounting while this thread is not running.
    pub fn pause_for_switch(&mut self, emitter: impl Fn(Signo)) {
        self.poll(emitter);
        self.paused_state = self.state;
        self.set_state(TimerState::None);
    }

    /// Resumes the CPU-time accounting state that was active before switch-out.
    pub fn resume_after_switch(&mut self) {
        let state = self.paused_state;
        self.paused_state = TimerState::None;
        self.set_state(state);
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
    if clock == AlarmClock::Monotonic {
        update_monotonic_timer_deadline();
    }
}

fn update_monotonic_timer_deadline() {
    let deadline = MONOTONIC_TIMER_RUNTIME.lock().next_deadline();
    axruntime::set_early_timer_deadline(deadline);
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

pub(crate) fn register_posix_timer_alarm(
    proc_data: &Arc<ProcessData>,
    timerid: usize,
    clock: AlarmClock,
    deadline: Duration,
    sequence: u64,
) {
    register_alarm(
        clock,
        deadline,
        AlarmAction::PosixTimer {
            proc: Arc::downgrade(proc_data),
            timerid,
            sequence,
        },
    );
}

fn saturating_duration_mul(duration: Duration, count: u128) -> Duration {
    let nanos = duration
        .as_nanos()
        .saturating_mul(count)
        .min(u64::MAX as u128) as u64;
    Duration::from_nanos(nanos)
}

fn posix_timer_signal_info(signo: Signo, timerid: usize, overrun: i32) -> SignalInfo {
    let mut info = SignalInfo::new_kernel(signo);
    info.set_code(SI_TIMER);
    info.0
        .__bindgen_anon_1
        .__bindgen_anon_1
        ._sifields
        ._timer
        ._tid = timerid as _;
    info.0
        .__bindgen_anon_1
        .__bindgen_anon_1
        ._sifields
        ._timer
        ._overrun = overrun;
    info
}

fn timer_signal_id(sig: &SignalInfo) -> Option<usize> {
    if sig.code() != SI_TIMER {
        return None;
    }

    let raw_timerid = unsafe {
        sig.0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._timer
            ._tid
    };
    usize::try_from(raw_timerid).ok()
}

pub(crate) fn acknowledge_posix_timer_signal(proc_data: &ProcessData, sig: &SignalInfo) {
    let Some(timerid) = timer_signal_id(sig) else {
        return;
    };

    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return;
    };
    timer.signal_pending = false;
}

fn fire_posix_timer(proc_data: Arc<ProcessData>, timerid: usize, sequence: u64) {
    let (notify, overrun, should_deliver, next) = {
        let mut timers = proc_data.posix_timers.lock();
        let Some(Some(timer)) = timers.get_mut(timerid) else {
            return;
        };
        if timer.sequence != sequence {
            return;
        }
        let Some(deadline) = timer.deadline else {
            return;
        };

        let now = timer.clock.alarm_clock().now();
        if now < deadline {
            return;
        }

        let expirations = if timer.interval.is_zero() {
            1_u128
        } else {
            let elapsed = now.saturating_sub(deadline).as_nanos();
            let interval = timer.interval.as_nanos().max(1);
            1_u128.saturating_add(elapsed / interval)
        };
        let should_deliver = !timer.signal_pending;
        if should_deliver {
            timer.overrun = expirations.saturating_sub(1).min(i32::MAX as u128) as i32;
            timer.signal_pending = true;
        } else {
            let extra = expirations.min(i32::MAX as u128) as i32;
            timer.overrun = timer.overrun.saturating_add(extra);
        }
        let overrun = timer.overrun;

        let notify = timer.notify;
        let next = if timer.interval.is_zero() {
            timer.deadline = None;
            timer.sequence = timer.sequence.wrapping_add(1);
            None
        } else {
            let next_deadline = deadline
                .checked_add(saturating_duration_mul(timer.interval, expirations))
                .unwrap_or(Duration::MAX);
            timer.deadline = Some(next_deadline);
            timer.sequence = timer.sequence.wrapping_add(1);
            Some((timer.clock.alarm_clock(), next_deadline, timer.sequence))
        };
        (notify, overrun, should_deliver, next)
    };

    if let Some((clock, deadline, next_sequence)) = next {
        register_posix_timer_alarm(&proc_data, timerid, clock, deadline, next_sequence);
    }

    if !should_deliver {
        return;
    }

    let (signo, target_tid) = match notify {
        PosixTimerNotify::None => return,
        PosixTimerNotify::Signal { signo, target_tid } => (signo, target_tid),
    };

    let siginfo = posix_timer_signal_info(signo, timerid, overrun);
    let result = if let Some(tid) = target_tid {
        send_signal_to_visible_thread(Some(proc_data.proc.pid()), tid, Some(siginfo))
    } else {
        send_signal_to_process(proc_data.proc.pid(), Some(siginfo))
    };

    if let Err(err) = result
        && LinuxError::from(err) != LinuxError::ESRCH
    {
        warn!("failed to deliver POSIX timer signal: {err:?}");
    }
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
            AlarmAction::PosixTimer {
                proc,
                timerid,
                sequence,
            } => {
                if let Some(proc_data) = proc.upgrade() {
                    fire_posix_timer(proc_data, timerid, sequence);
                }
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
    if clock == AlarmClock::Monotonic {
        update_monotonic_timer_deadline();
    }
    if let Some(key) = key {
        ClockTimerFuture { clock, key }.await;
    }
}
