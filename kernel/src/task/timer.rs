//! Time management module.

use alloc::{
    borrow::ToOwned,
    collections::binary_heap::BinaryHeap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::{Future, poll_fn},
    mem,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos};
use axpoll::PollSet;
use axtask::{
    TimerCallbackRegisterError, TimerCallbackToken, WeakAxTaskRef, cancel_timer_callback, current,
    future::{BlockOnError, block_on},
    register_timer_callback,
};
use event_listener::{Event, listener};
use kernel_guard::NoPreempt;
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use linux_raw_sys::general::{RLIM_INFINITY, RLIMIT_CPU, SI_TIMER};
use spin::Mutex;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use strum::FromRepr;

use super::{
    AsThread, ProcessData, poll_itimer_alarm, send_queued_signal_to_process_data,
    send_queued_signal_to_visible_thread,
};
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
        /// `None` selects Linux's default `sival_int = timerid` behavior.
        value: Option<usize>,
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
    signal_pending: bool,
    signal_retry_pending: bool,
    signal_token: u32,
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
            signal_retry_pending: false,
            signal_token: 0,
        }
    }

    fn begin_signal_delivery(&mut self, expirations: u128) -> Option<TimerSignalDelivery> {
        if self.signal_pending {
            let extra = expirations.min(i32::MAX as u128) as i32;
            self.overrun = self.overrun.saturating_add(extra);
            return None;
        }

        if self.signal_retry_pending {
            // The original expiry still needs a notification. Every expiry
            // observed while retrying is therefore an overrun of that event.
            // Keep the existing token and sleeping retry: replacing it here
            // would leave stale alarm entries behind for every short-period
            // expiry and could amplify one timer into an unbounded heap load.
            let extra = expirations.min(i32::MAX as u128) as i32;
            self.overrun = self.overrun.saturating_add(extra);
            return None;
        }

        self.overrun = expirations.saturating_sub(1).min(i32::MAX as u128) as i32;

        self.signal_token = next_posix_timer_signal_token();
        self.signal_pending = true;
        Some(TimerSignalDelivery {
            token: self.signal_token,
            overrun: self.overrun,
        })
    }

    fn fail_signal_delivery(&mut self, token: u32) -> bool {
        if !self.signal_pending || self.signal_token != token {
            return false;
        }
        self.signal_pending = false;
        self.signal_retry_pending = true;
        true
    }

    fn retry_signal_delivery(&mut self, token: u32) -> Option<TimerSignalDelivery> {
        if self.signal_pending || !self.signal_retry_pending || self.signal_token != token {
            return None;
        }
        self.signal_token = next_posix_timer_signal_token();
        self.signal_pending = true;
        Some(TimerSignalDelivery {
            token: self.signal_token,
            overrun: self.overrun,
        })
    }

    fn abandon_signal_delivery(&mut self, token: u32) -> bool {
        if !self.signal_pending || self.signal_token != token {
            return false;
        }
        self.signal_pending = false;
        self.signal_retry_pending = false;
        true
    }

    fn acknowledge_signal_delivery(&mut self, token: u32) -> bool {
        if !self.signal_pending || self.signal_token != token {
            return false;
        }
        self.signal_pending = false;
        self.signal_retry_pending = false;
        true
    }

    pub(crate) fn reset_signal_delivery(&mut self) {
        self.overrun = 0;
        self.signal_pending = false;
        self.signal_retry_pending = false;
        self.signal_token = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerSignalDelivery {
    token: u32,
    overrun: i32,
}

/// The action to take when an alarm fires.
enum AlarmAction {
    /// Interrupt a task and poll an itimer if the queued generation is current.
    PollITimer {
        task: WeakAxTaskRef,
        ty: ITimerType,
        sequence: u64,
    },
    /// Wake a PollSet (used by timerfd).
    WakePollSet(Arc<PollSet>),
    /// Deliver a POSIX timer event.
    PosixTimer {
        proc: Weak<ProcessData>,
        timerid: usize,
        sequence: u64,
    },
    /// Retry an admitted timer event after temporary sigqueue pressure.
    PosixTimerRetry {
        proc: Weak<ProcessData>,
        timerid: usize,
        token: u32,
        backoff: Duration,
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

const CLOCK_TIMER_CAPACITY: usize = 256;
// The alarm owner drives timerfd/POSIX/interval timers for the whole kernel.
// Keep one dedicated slot per clock domain so user futex/nanosleep pressure
// cannot make that owner exit when the general admission budget is full.
const CLOCK_TIMER_SYSTEM_RESERVE: usize = 1;
const CLOCK_TIMER_GENERAL_CAPACITY: usize = CLOCK_TIMER_CAPACITY - CLOCK_TIMER_SYSTEM_RESERVE;
const CLOCK_TIMER_WAKE_BATCH: usize = 16;
const CLOCK_TIMER_WAKE_BATCHES: usize = CLOCK_TIMER_CAPACITY.div_ceil(CLOCK_TIMER_WAKE_BATCH);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockTimerAdmission {
    General,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerKey {
    slot: usize,
    generation: u64,
}

struct ClockTimerSlot {
    deadline: Duration,
    generation: u64,
    occupied: bool,
    waker: Option<Waker>,
}

impl ClockTimerSlot {
    const fn new() -> Self {
        Self {
            deadline: Duration::ZERO,
            generation: 0,
            occupied: false,
            waker: None,
        }
    }
}

struct ClockTimerRuntime {
    slots: [ClockTimerSlot; CLOCK_TIMER_CAPACITY],
    next_general: usize,
    len: usize,
}

impl ClockTimerRuntime {
    const fn new() -> Self {
        Self {
            slots: [const { ClockTimerSlot::new() }; CLOCK_TIMER_CAPACITY],
            next_general: CLOCK_TIMER_SYSTEM_RESERVE,
            len: 0,
        }
    }

    fn reserve(&mut self, now: Duration, deadline: Duration) -> AxResult<Option<TimerKey>> {
        self.reserve_with_admission(now, deadline, ClockTimerAdmission::General)
    }

    fn reserve_system(&mut self, now: Duration, deadline: Duration) -> AxResult<Option<TimerKey>> {
        self.reserve_with_admission(now, deadline, ClockTimerAdmission::System)
    }

    fn reserve_with_admission(
        &mut self,
        now: Duration,
        deadline: Duration,
        admission: ClockTimerAdmission,
    ) -> AxResult<Option<TimerKey>> {
        if deadline <= now {
            return Ok(None);
        }

        let slot = match admission {
            ClockTimerAdmission::System => {
                let slots = &self.slots[..CLOCK_TIMER_SYSTEM_RESERVE];
                if let Some(slot) = slots
                    .iter()
                    .position(|entry| !entry.occupied && entry.generation < u64::MAX)
                {
                    slot
                } else if slots.iter().all(|entry| entry.occupied) {
                    return Err(AxError::ResourceBusy);
                } else {
                    return Err(AxError::OutOfRange);
                }
            }
            ClockTimerAdmission::General => {
                let start = self.next_general - CLOCK_TIMER_SYSTEM_RESERVE;
                let mut reusable = None;
                let mut has_free = false;
                for offset in 0..CLOCK_TIMER_GENERAL_CAPACITY {
                    let slot = CLOCK_TIMER_SYSTEM_RESERVE
                        + (start + offset) % CLOCK_TIMER_GENERAL_CAPACITY;
                    let entry = &self.slots[slot];
                    if !entry.occupied {
                        has_free = true;
                        if entry.generation < u64::MAX {
                            reusable = Some(slot);
                            break;
                        }
                    }
                }
                match reusable {
                    Some(slot) => slot,
                    None if has_free => return Err(AxError::OutOfRange),
                    None => return Err(AxError::ResourceBusy),
                }
            }
        };

        let entry = &mut self.slots[slot];
        entry.generation += 1;
        entry.deadline = deadline;
        entry.occupied = true;
        self.len += 1;
        if admission == ClockTimerAdmission::General {
            self.next_general = if slot + 1 == CLOCK_TIMER_CAPACITY {
                CLOCK_TIMER_SYSTEM_RESERVE
            } else {
                slot + 1
            };
        }
        let key = TimerKey {
            slot,
            generation: entry.generation,
        };
        Ok(Some(key))
    }

    fn is_live(&self, key: TimerKey) -> bool {
        self.slots
            .get(key.slot)
            .is_some_and(|entry| entry.occupied && entry.generation == key.generation)
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.slots
            .iter()
            .filter(|entry| entry.occupied)
            .map(|entry| entry.deadline)
            .min()
    }

    fn poll(
        &mut self,
        key: TimerKey,
        candidate: &Waker,
        owned: Waker,
    ) -> (Poll<()>, Option<Waker>) {
        if !self.is_live(key) {
            return (Poll::Ready(()), Some(owned));
        }

        let entry = &mut self.slots[key.slot];
        if entry
            .waker
            .as_ref()
            .is_some_and(|registered| registered.will_wake(candidate))
        {
            (Poll::Pending, Some(owned))
        } else {
            (Poll::Pending, entry.waker.replace(owned))
        }
    }

    fn cancel(&mut self, key: TimerKey) -> Option<Waker> {
        if !self.is_live(key) {
            return None;
        }
        let entry = &mut self.slots[key.slot];
        entry.occupied = false;
        self.len -= 1;
        if key.slot >= CLOCK_TIMER_SYSTEM_RESERVE {
            self.next_general = key.slot;
        }
        entry.waker.take()
    }

    fn drain_expired_batch(
        &mut self,
        now: Duration,
        pending: &mut [Option<Waker>; CLOCK_TIMER_WAKE_BATCH],
    ) -> usize {
        let mut count = 0;
        let mut first_general_refund = None;
        for (slot, entry) in self.slots.iter_mut().enumerate() {
            if entry.occupied && entry.deadline <= now {
                entry.occupied = false;
                self.len -= 1;
                pending[count] = entry.waker.take();
                if slot >= CLOCK_TIMER_SYSTEM_RESERVE && first_general_refund.is_none() {
                    first_general_refund = Some(slot);
                }
                count += 1;
                if count == CLOCK_TIMER_WAKE_BATCH {
                    break;
                }
            }
        }
        if let Some(slot) = first_general_refund {
            self.next_general = slot;
        }
        count
    }
}

/// An admitted clock sleep whose polling path only touches bounded IRQ-safe
/// timer and waker state.
#[must_use = "a prepared clock sleep must be polled or dropped to cancel it"]
pub(crate) struct PreparedClockSleep {
    clock: AlarmClock,
    key: Option<TimerKey>,
}

impl Future for PreparedClockSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(key) = self.key else {
            return Poll::Ready(());
        };

        // Clone and release wakers outside the IRQ-safe registry lock. The
        // locked section performs only a bounded slot lookup and replacement.
        let owned = cx.waker().clone();
        let (result, deferred) = timer_runtime(self.clock)
            .lock()
            .poll(key, cx.waker(), owned);
        drop(deferred);
        result
    }
}

impl Drop for PreparedClockSleep {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let deferred = timer_runtime(self.clock).lock().cancel(key);
            drop(deferred);
            update_clock_timer_deadline();
        }
    }
}

lazy_static! {
    static ref REALTIME_ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
    static ref MONOTONIC_ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
    static ref REALTIME_ALARM_EVENT: Event = Event::new();
    static ref MONOTONIC_ALARM_EVENT: Event = Event::new();
    static ref REALTIME_TIMER_RUNTIME: SpinNoIrq<ClockTimerRuntime> =
        SpinNoIrq::new(ClockTimerRuntime::new());
    static ref MONOTONIC_TIMER_RUNTIME: SpinNoIrq<ClockTimerRuntime> =
        SpinNoIrq::new(ClockTimerRuntime::new());
}

static CLOCK_TIMER_CALLBACK_TOKENS: [SpinNoIrq<Option<TimerCallbackToken>>;
    axconfig::plat::MAX_CPU_NUM] = [const { SpinNoIrq::new(None) }; axconfig::plat::MAX_CPU_NUM];

static NEXT_POSIX_TIMER_SIGNAL_TOKEN: AtomicU32 = AtomicU32::new(1);
const POSIX_TIMER_RETRY_INITIAL: Duration = Duration::from_millis(1);
const POSIX_TIMER_RETRY_MAX: Duration = Duration::from_secs(1);

fn next_posix_timer_signal_token() -> u32 {
    loop {
        let current = NEXT_POSIX_TIMER_SIGNAL_TOKEN.load(Ordering::Relaxed);
        let token = current.max(1);
        let next = token.wrapping_add(1).max(1);
        if NEXT_POSIX_TIMER_SIGNAL_TOKEN
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return token;
        }
    }
}

// Set (sticky) when any process sets RLIMIT_CPU to a finite value, so the
// per-fault/per-syscall update_rlimit_cpu can skip the current()+rlim path.
pub static ANY_RLIMIT_CPU_SET: AtomicBool = AtomicBool::new(false);

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
    sequence: u64,
}

#[derive(Debug, Default)]
struct CpuLimitState {
    soft_signal_sent: bool,
}

impl CpuLimitState {
    fn reset_if_below_soft(&mut self, total_cpu_ns: usize, soft_secs: u64) {
        if soft_secs == RLIM_INFINITY as i64 as u64 {
            self.soft_signal_sent = false;
            return;
        }
        if total_cpu_ns < secs_to_nanos(soft_secs) {
            self.soft_signal_sent = false;
        }
    }
}

fn secs_to_nanos(secs: u64) -> usize {
    secs.saturating_mul(NANOS_PER_SEC)
        .try_into()
        .unwrap_or(usize::MAX)
}

impl ITimer {
    pub fn new(interval_ns: usize, remained_ns: usize, sequence: u64) -> Self {
        Self {
            interval_ns,
            remained_ns,
            sequence,
        }
    }

    pub fn update(&mut self, ty: ITimerType, delta: usize) -> bool {
        if self.remained_ns == 0 {
            return false;
        }
        if self.remained_ns > delta {
            self.remained_ns -= delta;
            false
        } else {
            self.remained_ns = self.interval_ns;
            self.renew_timer(ty);
            true
        }
    }

    pub fn renew_timer(&self, ty: ITimerType) {
        if self.remained_ns > 0 {
            let deadline = wall_time()
                .checked_add(Duration::from_nanos(self.remained_ns as u64))
                .unwrap_or(Duration::MAX);
            register_alarm(
                AlarmClock::Realtime,
                deadline,
                AlarmAction::PollITimer {
                    task: Arc::downgrade(&current()),
                    ty,
                    sequence: self.sequence,
                },
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
    cpu_limit: CpuLimitState,
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
            cpu_limit: CpuLimitState::default(),
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
    pub fn poll(&mut self, signals: &mut Vec<Signo>) {
        let now_ns = monotonic_time_nanos() as usize;
        let wall_delta = now_ns.saturating_sub(self.last_wall_ns);
        let cpu_delta = now_ns.saturating_sub(self.last_cpu_ns);
        match self.state {
            TimerState::User => {
                self.utime_ns += cpu_delta;
                self.update_itimer(ITimerType::Virtual, cpu_delta, signals);
                self.update_itimer(ITimerType::Prof, cpu_delta, signals);
            }
            TimerState::Kernel => {
                self.stime_ns += cpu_delta;
                self.update_itimer(ITimerType::Prof, cpu_delta, signals);
            }
            TimerState::None => {}
        }
        self.update_itimer(ITimerType::Real, wall_delta, signals);
        self.update_rlimit_cpu(signals);
        self.last_cpu_ns = now_ns;
        self.last_wall_ns = now_ns;
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.last_cpu_ns = monotonic_time_nanos() as usize;
        self.state = state;
    }

    /// Pauses CPU-time accounting while this thread is not running.
    pub fn pause_for_switch(&mut self, signals: &mut Vec<Signo>) {
        self.poll(signals);
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
        let index = ty as usize;
        let sequence = self.itimers[index].sequence.wrapping_add(1);
        let old = mem::replace(
            &mut self.itimers[index],
            ITimer::new(interval_ns, remained_ns, sequence),
        );
        self.itimers[index].renew_timer(ty);
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

    fn update_itimer(&mut self, ty: ITimerType, delta: usize, signals: &mut Vec<Signo>) {
        if self.itimers[ty as usize].update(ty, delta) {
            signals.push(ty.signo());
        }
    }

    pub fn itimer_sequence_matches(&self, ty: ITimerType, sequence: u64) -> bool {
        self.itimers[ty as usize].sequence == sequence
    }

    fn update_rlimit_cpu(&mut self, signals: &mut Vec<Signo>) {
        // Common case: no process has ever set RLIMIT_CPU to a finite value, so
        // there is nothing to check. Skip the current() + proc_data.clone() +
        // rlim.read() path that would otherwise run twice per fault/syscall via
        // set_timer_state. The flag is sticky (once a CPU limit is set, always
        // check) so rlimit users stay correct.
        if !ANY_RLIMIT_CPU_SET.load(Ordering::Relaxed) {
            return;
        }
        let curr = current();
        let Some(thread) = curr.try_as_thread() else {
            return;
        };
        let proc_data = thread.proc_data.clone();
        let (soft_limit, hard_limit) = {
            let limits = proc_data.rlim.read();
            let limit = &limits[RLIMIT_CPU];
            (limit.current, limit.max)
        };
        let total = self.utime_ns.saturating_add(self.stime_ns);

        self.cpu_limit.reset_if_below_soft(total, soft_limit);

        if hard_limit != RLIM_INFINITY as i64 as u64 && total >= secs_to_nanos(hard_limit) {
            signals.push(Signo::SIGKILL);
            return;
        }
        if soft_limit != RLIM_INFINITY as i64 as u64
            && total >= secs_to_nanos(soft_limit)
            && !self.cpu_limit.soft_signal_sent
        {
            self.cpu_limit.soft_signal_sent = true;
            signals.push(Signo::SIGXCPU);
        }
    }
}

enum AlarmWait {
    DeadlineReached,
    NewTimer,
}

fn alarm_task(clock: AlarmClock) -> Result<AxResult<()>, BlockOnError> {
    loop {
        // Register before inspecting the queues so a newly inserted earlier
        // deadline cannot race past us and get delayed until a stale timeout.
        listener!(alarm_event(clock) => listener);

        if process_due(clock) {
            axtask::resched_if_needed();
            continue;
        }

        let Some(deadline) = queue_deadline(clock) else {
            block_on(listener)?;
            continue;
        };

        let mut sleeper = match prepare_alarm_clock_sleep(clock, deadline) {
            Ok(sleeper) => sleeper,
            Err(error) => return Ok(Err(error)),
        };
        match block_on(wait_until_or_alarm(&mut sleeper, listener)) {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Ok(Err(error)),
            Err(error) => return Err(error),
        }
    }
}

/// Spawns the alarm task.
pub fn spawn_alarm_task() -> Result<(), axerrno::AxError> {
    info!("Initialize alarm...");
    ensure_clock_timer_runtime()?;
    axtask::spawn_raw(
        || match alarm_task(AlarmClock::Realtime) {
            Ok(Ok(())) => error!("Realtime alarm worker ended unexpectedly"),
            Ok(Err(error)) => error!("Realtime alarm timer stopped: {error}"),
            Err(error) => {
                error!("Realtime alarm worker stopped: {error}");
            }
        },
        "alarm_realtime".to_owned(),
        axconfig::TASK_STACK_SIZE,
    )?;
    axtask::spawn_raw(
        || match alarm_task(AlarmClock::Monotonic) {
            Ok(Ok(())) => error!("Monotonic alarm worker ended unexpectedly"),
            Ok(Err(error)) => error!("Monotonic alarm timer stopped: {error}"),
            Err(error) => {
                error!("Monotonic alarm worker stopped: {error}");
            }
        },
        "alarm_monotonic".to_owned(),
        axconfig::TASK_STACK_SIZE,
    )?;
    Ok(())
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

fn map_timer_callback_register_error(error: TimerCallbackRegisterError) -> AxError {
    match error {
        TimerCallbackRegisterError::NoMemory => AxError::NoMemory,
        TimerCallbackRegisterError::CapacityExhausted => AxError::ResourceBusy,
        TimerCallbackRegisterError::TokenSpaceExhausted => AxError::OutOfRange,
    }
}

fn retain_first_callback_token<T>(slot: &mut Option<T>, token: T) -> Option<T> {
    if slot.is_none() {
        *slot = Some(token);
        None
    } else {
        Some(token)
    }
}

fn ensure_clock_timer_runtime() -> AxResult<()> {
    // Keep the outer per-CPU token slot and axtask's internally sampled owner
    // CPU identical even if this task's affinity changes concurrently.
    let _cpu_guard = NoPreempt::new();
    let cpu_id = axhal::percpu::this_cpu_id();
    let owner = &CLOCK_TIMER_CALLBACK_TOKENS[cpu_id];
    if owner.lock().is_some() {
        return Ok(());
    }

    let token = match register_timer_callback(|_| {
        wake_clock_timers(AlarmClock::Realtime);
        wake_clock_timers(AlarmClock::Monotonic);
    }) {
        Ok(token) => token,
        Err(error) => {
            // A concurrent caller may have completed registration while this
            // call was rejected. In that case the current CPU already has the
            // exact persistent owner and no failure needs to escape.
            if owner.lock().is_some() {
                return Ok(());
            }
            return Err(map_timer_callback_register_error(error));
        }
    };

    let duplicate = {
        let mut retained = owner.lock();
        retain_first_callback_token(&mut retained, token)
    };
    if let Some(duplicate) = duplicate
        && !cancel_timer_callback(duplicate)
    {
        error!("failed to cancel duplicate clock timer callback on CPU {cpu_id}");
        return Err(AxError::BadState);
    }

    Ok(())
}

#[cfg(test)]
mod clock_timer_callback_tests {
    use core::task::Waker;

    use super::*;

    #[test]
    fn first_callback_token_is_retained_and_duplicates_are_returned() {
        let mut owner = None;
        assert_eq!(retain_first_callback_token(&mut owner, 7_u8), None);
        assert_eq!(owner, Some(7));
        assert_eq!(retain_first_callback_token(&mut owner, 11_u8), Some(11));
        assert_eq!(owner, Some(7));
    }

    #[test]
    fn callback_registration_failures_keep_distinct_kernel_errors() {
        assert_eq!(
            map_timer_callback_register_error(TimerCallbackRegisterError::NoMemory),
            AxError::NoMemory
        );
        assert_eq!(
            map_timer_callback_register_error(TimerCallbackRegisterError::CapacityExhausted),
            AxError::ResourceBusy
        );
        assert_eq!(
            map_timer_callback_register_error(TimerCallbackRegisterError::TokenSpaceExhausted),
            AxError::OutOfRange
        );
    }

    #[test]
    fn elapsed_clock_sleep_never_consumes_a_slot() {
        let mut runtime = ClockTimerRuntime::new();
        assert_eq!(
            runtime.reserve(Duration::from_secs(5), Duration::from_secs(5)),
            Ok(None)
        );
        assert_eq!(runtime.len, 0);
        assert_eq!(runtime.next_deadline(), None);
    }

    #[test]
    fn cancelling_clock_sleep_refunds_exactly_one_slot() {
        let mut runtime = ClockTimerRuntime::new();
        let key = runtime
            .reserve(Duration::ZERO, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(runtime.len, 1);

        assert!(runtime.cancel(key).is_none());
        assert_eq!(runtime.len, 0);
        assert!(runtime.cancel(key).is_none());
        assert_eq!(runtime.len, 0);

        assert!(
            runtime
                .reserve(Duration::ZERO, Duration::from_secs(2))
                .unwrap()
                .is_some()
        );
        assert_eq!(runtime.len, 1);
    }

    #[test]
    fn clock_sleep_admission_is_bounded_and_refundable() {
        let mut runtime = ClockTimerRuntime::new();
        let mut first = None;
        for index in 0..CLOCK_TIMER_GENERAL_CAPACITY {
            let key = runtime
                .reserve(Duration::ZERO, Duration::from_secs(index as u64 + 1))
                .unwrap()
                .unwrap();
            first.get_or_insert(key);
        }
        assert!(matches!(
            runtime.reserve(Duration::ZERO, Duration::from_secs(u64::MAX)),
            Err(AxError::ResourceBusy)
        ));

        let system = runtime
            .reserve_system(Duration::ZERO, Duration::from_secs(u64::MAX))
            .unwrap()
            .unwrap();
        assert!(matches!(
            runtime.reserve_system(Duration::ZERO, Duration::from_secs(u64::MAX)),
            Err(AxError::ResourceBusy)
        ));
        assert_eq!(runtime.len, CLOCK_TIMER_CAPACITY);

        runtime.cancel(first.unwrap());
        assert!(
            runtime
                .reserve(Duration::ZERO, Duration::from_secs(u64::MAX))
                .unwrap()
                .is_some()
        );
        assert_eq!(runtime.len, CLOCK_TIMER_CAPACITY);
        assert!(runtime.is_live(system));
    }

    #[test]
    fn expired_slots_release_wakers_outside_the_runtime() {
        let mut runtime = ClockTimerRuntime::new();
        let key = runtime
            .reserve(Duration::ZERO, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let waker = Waker::noop().clone();
        assert_eq!(runtime.poll(key, Waker::noop(), waker).0, Poll::Pending);

        let mut pending = [const { None }; CLOCK_TIMER_WAKE_BATCH];
        assert_eq!(
            runtime.drain_expired_batch(Duration::from_secs(1), &mut pending),
            0
        );
        assert_eq!(
            runtime.drain_expired_batch(Duration::from_secs(2), &mut pending),
            1
        );
        assert_eq!(runtime.len, 0);
        assert_eq!(pending.into_iter().flatten().count(), 1);
    }

    #[test]
    fn expired_clock_sleeps_drain_in_fixed_irq_batches() {
        let mut runtime = ClockTimerRuntime::new();
        for _ in 0..CLOCK_TIMER_WAKE_BATCH + 1 {
            runtime
                .reserve(Duration::ZERO, Duration::from_secs(1))
                .unwrap()
                .unwrap();
        }

        let mut first = [const { None }; CLOCK_TIMER_WAKE_BATCH];
        assert_eq!(
            runtime.drain_expired_batch(Duration::from_secs(1), &mut first),
            CLOCK_TIMER_WAKE_BATCH
        );
        assert_eq!(runtime.len, 1);

        let mut second = [const { None }; CLOCK_TIMER_WAKE_BATCH];
        assert_eq!(
            runtime.drain_expired_batch(Duration::from_secs(1), &mut second),
            1
        );
        assert_eq!(runtime.len, 0);
    }
}

fn wake_clock_timers(clock: AlarmClock) {
    let now = clock.now();
    let mut woke = false;
    for _ in 0..CLOCK_TIMER_WAKE_BATCHES {
        let mut pending = [const { None }; CLOCK_TIMER_WAKE_BATCH];
        let count = timer_runtime(clock)
            .lock()
            .drain_expired_batch(now, &mut pending);
        if count == 0 {
            break;
        }
        woke = true;
        for waker in pending.into_iter().flatten() {
            waker.wake();
        }
        if count < CLOCK_TIMER_WAKE_BATCH {
            break;
        }
    }
    if woke {
        axtask::request_resched_current();
    }
    update_clock_timer_deadline();
}

fn realtime_deadline_as_monotonic(deadline: Duration) -> Duration {
    let realtime_now = AlarmClock::Realtime.now();
    let monotonic_now = AlarmClock::Monotonic.now();
    if deadline <= realtime_now {
        monotonic_now
    } else {
        monotonic_now
            .checked_add(deadline - realtime_now)
            .unwrap_or(Duration::MAX)
    }
}

fn update_clock_timer_deadline() {
    let realtime_deadline = REALTIME_TIMER_RUNTIME
        .lock()
        .next_deadline()
        .map(realtime_deadline_as_monotonic);
    let monotonic_deadline = MONOTONIC_TIMER_RUNTIME.lock().next_deadline();
    let deadline = match (realtime_deadline, monotonic_deadline) {
        (Some(real), Some(mono)) => Some(real.min(mono)),
        (Some(real), None) => Some(real),
        (None, Some(mono)) => Some(mono),
        (None, None) => None,
    };
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

fn posix_timer_signal_info(
    signo: Signo,
    timerid: usize,
    overrun: i32,
    value: usize,
    token: u32,
) -> SignalInfo {
    let mut info = SignalInfo::new_kernel(signo);
    info.set_code(SI_TIMER);
    let timer = unsafe { &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._timer };
    timer._tid = timerid as _;
    timer._overrun = overrun;
    timer._sigval = linux_raw_sys::general::sigval_t {
        sival_ptr: value as *mut linux_raw_sys::ctypes::c_void,
    };
    timer._sys_private = token as i32;
    info
}

fn timer_signal_identity(sig: &SignalInfo) -> Option<(usize, u32)> {
    if sig.code() != SI_TIMER {
        return None;
    }

    let timer = unsafe { sig.0.__bindgen_anon_1.__bindgen_anon_1._sifields._timer };
    Some((usize::try_from(timer._tid).ok()?, timer._sys_private as u32))
}

pub(crate) fn acknowledge_posix_timer_signal(proc_data: &ProcessData, sig: &SignalInfo) {
    let Some((timerid, token)) = timer_signal_identity(sig) else {
        return;
    };

    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return;
    };
    timer.acknowledge_signal_delivery(token);
}

fn fail_posix_timer_signal(proc_data: &ProcessData, timerid: usize, token: u32) -> bool {
    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return false;
    };
    timer.fail_signal_delivery(token)
}

fn abandon_posix_timer_signal(proc_data: &ProcessData, timerid: usize, token: u32) {
    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return;
    };
    timer.abandon_signal_delivery(token);
}

fn register_posix_timer_retry(
    proc_data: &Arc<ProcessData>,
    timerid: usize,
    token: u32,
    backoff: Duration,
) {
    let deadline = AlarmClock::Monotonic
        .now()
        .checked_add(backoff)
        .unwrap_or(Duration::MAX);
    register_alarm(
        AlarmClock::Monotonic,
        deadline,
        AlarmAction::PosixTimerRetry {
            proc: Arc::downgrade(proc_data),
            timerid,
            token,
            backoff,
        },
    );
}

fn next_posix_timer_retry_backoff(backoff: Duration) -> Duration {
    backoff.saturating_mul(2).min(POSIX_TIMER_RETRY_MAX)
}

fn deliver_posix_timer_signal(
    proc_data: &Arc<ProcessData>,
    timerid: usize,
    notify: PosixTimerNotify,
    delivery: TimerSignalDelivery,
    retry_backoff: Duration,
) {
    let (signo, target_tid, value) = match notify {
        PosixTimerNotify::None => return,
        PosixTimerNotify::Signal {
            signo,
            target_tid,
            value,
        } => (signo, target_tid, value.unwrap_or(timerid)),
    };

    let siginfo = posix_timer_signal_info(signo, timerid, delivery.overrun, value, delivery.token);
    let result = if let Some(tid) = target_tid {
        send_queued_signal_to_visible_thread(Some(proc_data.proc.pid()), tid, Some(siginfo))
    } else {
        send_queued_signal_to_process_data(proc_data, Some(siginfo))
    };

    match result {
        Ok(true) => {}
        Ok(false) => {
            // Ignore and standard-signal coalescing own no timer record. They
            // are semantic consumption, not queue-pressure failures.
            abandon_posix_timer_signal(proc_data, timerid, delivery.token);
        }
        Err(err) => {
            let linux_error = LinuxError::from(err);
            if linux_error == LinuxError::ESRCH {
                abandon_posix_timer_signal(proc_data, timerid, delivery.token);
                return;
            }

            // Admission/allocation failure retains the exact timer event and
            // schedules one sleeping retry. Periodic expiries merge their
            // overruns into that generation; rearm/delete invalidate it.
            if fail_posix_timer_signal(proc_data, timerid, delivery.token) {
                register_posix_timer_retry(proc_data, timerid, delivery.token, retry_backoff);
            }
            if linux_error != LinuxError::EAGAIN {
                warn!("failed to deliver POSIX timer signal: {err:?}");
            }
        }
    }
}

fn retry_posix_timer_signal(
    proc_data: Arc<ProcessData>,
    timerid: usize,
    token: u32,
    backoff: Duration,
) {
    let (notify, delivery) = {
        let mut timers = proc_data.posix_timers.lock();
        let Some(Some(timer)) = timers.get_mut(timerid) else {
            return;
        };
        let notify = timer.notify;
        let Some(delivery) = timer.retry_signal_delivery(token) else {
            return;
        };
        (notify, delivery)
    };
    deliver_posix_timer_signal(
        &proc_data,
        timerid,
        notify,
        delivery,
        next_posix_timer_retry_backoff(backoff),
    );
}

fn fire_posix_timer(proc_data: Arc<ProcessData>, timerid: usize, sequence: u64) {
    let (notify, delivery, next) = {
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
        let notify = timer.notify;
        let delivery = match notify {
            PosixTimerNotify::None => None,
            PosixTimerNotify::Signal { .. } => timer.begin_signal_delivery(expirations),
        };

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
        (notify, delivery, next)
    };

    if let Some((clock, deadline, next_sequence)) = next {
        register_posix_timer_alarm(&proc_data, timerid, clock, deadline, next_sequence);
    }

    let Some(delivery) = delivery else { return };

    deliver_posix_timer_signal(
        &proc_data,
        timerid,
        notify,
        delivery,
        POSIX_TIMER_RETRY_INITIAL,
    );
}

#[cfg(test)]
mod posix_timer_signal_tests {
    use super::*;

    fn timer() -> PosixTimer {
        PosixTimer::new(
            PosixTimerClock::Monotonic,
            PosixTimerNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: Some(7),
            },
        )
    }

    #[test]
    fn failed_admission_retries_without_losing_overrun() {
        let mut timer = timer();
        let first = timer.begin_signal_delivery(1).unwrap();
        assert_eq!(first.overrun, 0);
        assert!(timer.fail_signal_delivery(first.token));

        assert!(timer.begin_signal_delivery(2).is_none());
        let retry = timer.retry_signal_delivery(first.token).unwrap();
        assert_eq!(retry.overrun, 2);
        assert!(!timer.acknowledge_signal_delivery(first.token));
        assert!(timer.acknowledge_signal_delivery(retry.token));
    }

    #[test]
    fn one_shot_deferred_retry_retains_the_original_event() {
        let mut timer = timer();
        let first = timer.begin_signal_delivery(1).unwrap();
        assert!(timer.fail_signal_delivery(first.token));

        let retry = timer.retry_signal_delivery(first.token).unwrap();
        assert_eq!(retry.overrun, 0);
        assert_ne!(retry.token, first.token);
        assert!(timer.acknowledge_signal_delivery(retry.token));
    }

    #[test]
    fn pending_delivery_accumulates_and_stale_tokens_cannot_clear_it() {
        let mut timer = timer();
        let delivery = timer.begin_signal_delivery(3).unwrap();
        assert_eq!(delivery.overrun, 2);
        assert!(timer.begin_signal_delivery(4).is_none());
        assert_eq!(timer.overrun, 6);
        assert!(!timer.fail_signal_delivery(delivery.token.wrapping_add(1)));
        assert!(timer.signal_pending);
    }

    #[test]
    fn rearm_invalidates_a_deferred_one_shot_retry() {
        let mut timer = timer();
        let first = timer.begin_signal_delivery(1).unwrap();
        assert!(timer.fail_signal_delivery(first.token));

        timer.reset_signal_delivery();
        assert!(timer.retry_signal_delivery(first.token).is_none());
        assert!(!timer.signal_pending);
        assert!(!timer.signal_retry_pending);
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
            AlarmAction::PollITimer { task, ty, sequence } => {
                if let Some(task) = task.upgrade() {
                    poll_itimer_alarm(&task, ty, sequence);
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
            AlarmAction::PosixTimerRetry {
                proc,
                timerid,
                token,
                backoff,
            } => {
                if let Some(proc_data) = proc.upgrade() {
                    retry_posix_timer_signal(proc_data, timerid, token, backoff);
                }
            }
        }
    }
    progressed
}

async fn wait_until_or_alarm<L>(
    sleeper: &mut PreparedClockSleep,
    mut listener: L,
) -> AxResult<AlarmWait>
where
    L: Future<Output = ()> + Unpin,
{
    poll_fn(|cx| {
        if Pin::new(&mut listener).poll(cx).is_ready() {
            return Poll::Ready(Ok(AlarmWait::NewTimer));
        }
        match Pin::new(&mut *sleeper).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Ok(AlarmWait::DeadlineReached)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Admits a clock-domain sleep before a synchronous block session starts.
///
/// Callback registration, bounded slot admission, and hardware deadline
/// publication may fail or touch non-wait state, so they deliberately happen
/// here rather than in [`Future::poll`]. The returned future only updates its
/// pre-admitted IRQ-safe slot and can be borrowed by `block_on` callers.
pub(crate) fn prepare_clock_sleep(
    clock: AlarmClock,
    deadline: Duration,
) -> AxResult<PreparedClockSleep> {
    prepare_clock_sleep_with_admission(clock, deadline, ClockTimerAdmission::General)
}

fn prepare_alarm_clock_sleep(
    clock: AlarmClock,
    deadline: Duration,
) -> AxResult<PreparedClockSleep> {
    prepare_clock_sleep_with_admission(clock, deadline, ClockTimerAdmission::System)
}

fn prepare_clock_sleep_with_admission(
    clock: AlarmClock,
    deadline: Duration,
    admission: ClockTimerAdmission,
) -> AxResult<PreparedClockSleep> {
    let now = clock.now();
    if deadline <= now {
        return Ok(PreparedClockSleep { clock, key: None });
    }
    if let Err(error) = ensure_clock_timer_runtime() {
        // Completion wins if the deadline elapsed while callback admission was
        // attempted. Otherwise preserve the exact construction failure.
        if deadline <= clock.now() {
            return Ok(PreparedClockSleep { clock, key: None });
        }
        return Err(error);
    }

    let now = clock.now();
    let reservation = {
        let mut runtime = timer_runtime(clock).lock();
        match admission {
            ClockTimerAdmission::General => runtime.reserve(now, deadline),
            ClockTimerAdmission::System => runtime.reserve_system(now, deadline),
        }
    };
    let key = match reservation {
        Ok(key) => key,
        Err(_) if deadline <= clock.now() => None,
        Err(error) => return Err(error),
    };
    update_clock_timer_deadline();
    Ok(PreparedClockSleep { clock, key })
}
