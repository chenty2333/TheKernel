//! Time management module.

use alloc::{
    borrow::ToOwned,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    cell::UnsafeCell,
    future::{Future, poll_fn},
    mem::MaybeUninit,
    pin::Pin,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos};
use axpoll::PollSet;
use axtask::{
    AxTaskRef, TimerCallbackRegisterError, TimerCallbackToken, cancel_timer_callback,
    future::{BlockOnError, block_on},
    register_timer_callback,
};
use event_listener::{Event, listener};
use kernel_guard::NoPreempt;
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use linux_raw_sys::general::{RLIM_INFINITY, RLIMIT_CPU, SI_TIMER};
use spin::Once;
use strum::FromRepr;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{SignalInfo, SignalTimerPayload, Signo};

use super::{
    AsThread, ProcessData, TaskUsage, send_queued_signal_to_process_data,
    send_queued_signal_to_visible_thread, send_signal_to_process_data, try_processes,
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
    /// Thread-group CPU-time clock.  Expiry is evaluated by the existing
    /// deferred accounting worker, never by a wall-clock alarm.
    ProcessCpu,
    /// CPU-time clock of the thread which created the timer.  POSIX timers
    /// remain owned by the process, while this immutable kernel TID pins the
    /// clock source for the timer lifetime.
    ThreadCpu,
}

impl PosixTimerClock {
    pub(crate) fn absolute_alarm_clock(self) -> AlarmClock {
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

pub(crate) struct PosixTimer {
    /// A timer ID is reserved before it is copied to userspace.  Other
    /// threads must not operate on that slot until the successful copy has
    /// published it.
    published: bool,
    /// Clock advertised by `timer_create(2)`.
    pub clock: PosixTimerClock,
    /// Clock basis of the currently armed host deadline.
    pub effective_clock: AlarmClock,
    pub notify: PosixTimerNotify,
    pub interval: Duration,
    /// The CLOCK_TAI deadline as supplied by userspace for an absolute arm.
    ///
    /// `deadline` below is always expressed in the backing alarm clock.  TAI
    /// is backed by realtime, but its UTC offset is mutable through
    /// `ADJ_TAI`; retaining the advertised-domain deadline lets that commit
    /// rebase an already armed timer without treating a relative timer as an
    /// absolute one.
    tai_deadline: Option<Duration>,
    /// Timex generation used to derive `deadline` from `tai_deadline`.
    tai_offset_generation: u64,
    pub deadline: Option<Duration>,
    /// Absolute threshold in the advertised CPU-clock domain.  This is kept
    /// separate from `deadline`: CPU time does not advance while a task is
    /// descheduled and therefore cannot be represented by AlarmClock.
    cpu_deadline_ns: Option<u64>,
    /// Immutable creator thread for CLOCK_THREAD_CPUTIME_ID timers.
    cpu_target_task: Option<AxTaskRef>,
    /// Foreign process clock owner for an encoded CPU clock. `None` denotes
    /// the timer-owning process itself and avoids a self-reference cycle.
    cpu_target_process: Option<Weak<ProcessData>>,
    cpu_target_process_pid: Option<Pid>,
    pub sequence: u64,
    pub overrun: i32,
    signal_pending: bool,
    signal_retry_pending: bool,
    signal_token: u32,
    main_alarm: AlarmToken,
    retry_alarm: Option<AlarmToken>,
}

impl core::fmt::Debug for PosixTimer {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PosixTimer")
            .field("published", &self.published)
            .field("clock", &self.clock)
            .field("effective_clock", &self.effective_clock)
            .field("notify", &self.notify)
            .field("interval", &self.interval)
            .field("tai_deadline", &self.tai_deadline)
            .field("tai_offset_generation", &self.tai_offset_generation)
            .field("deadline", &self.deadline)
            .field("cpu_deadline_ns", &self.cpu_deadline_ns)
            .field(
                "cpu_target_task",
                &self.cpu_target_task.as_ref().map(|_| "task"),
            )
            .field("cpu_target_process_pid", &self.cpu_target_process_pid)
            .field("sequence", &self.sequence)
            .field("overrun", &self.overrun)
            .field("signal_pending", &self.signal_pending)
            .field("signal_retry_pending", &self.signal_retry_pending)
            .field("signal_token", &self.signal_token)
            .finish()
    }
}

impl PosixTimer {
    pub(crate) fn try_new(
        clock: PosixTimerClock,
        notify: PosixTimerNotify,
        cpu_target_task: Option<AxTaskRef>,
        cpu_target_process: Option<Weak<ProcessData>>,
        cpu_target_process_pid: Option<Pid>,
    ) -> Result<Self, AlarmTokenReserveError> {
        let main_alarm = AlarmToken::try_new()?;
        let retry_alarm = match notify {
            PosixTimerNotify::None => None,
            PosixTimerNotify::Signal { .. } => Some(AlarmToken::try_new()?),
        };
        Ok(Self {
            published: false,
            clock,
            effective_clock: clock.absolute_alarm_clock(),
            notify,
            interval: Duration::ZERO,
            tai_deadline: None,
            tai_offset_generation: 0,
            deadline: None,
            cpu_deadline_ns: None,
            cpu_target_task,
            cpu_target_process,
            cpu_target_process_pid,
            sequence: 0,
            overrun: 0,
            signal_pending: false,
            signal_retry_pending: false,
            signal_token: 0,
            main_alarm,
            retry_alarm,
        })
    }

    pub(crate) fn is_published(&self) -> bool {
        self.published
    }

    pub(crate) const fn is_cpu_clock(&self) -> bool {
        matches!(
            self.clock,
            PosixTimerClock::ProcessCpu | PosixTimerClock::ThreadCpu
        )
    }

    fn cpu_now_ns(&self, owner: &ProcessData) -> Option<u64> {
        match self.clock {
            PosixTimerClock::ProcessCpu => match self.cpu_target_process_pid {
                None => Some(process_cpu_usage(owner).total_ns),
                Some(_) => self
                    .cpu_target_process
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .map(|target| process_cpu_usage(&target).total_ns),
            },
            PosixTimerClock::ThreadCpu => self.cpu_target_task.as_ref().and_then(|task| {
                let thread = task.try_as_thread()?;
                let usage = TaskUsage::from_thread(thread);
                Some(usage.utime_ns.saturating_add(usage.stime_ns))
            }),
            _ => None,
        }
    }

    pub(crate) fn arm_cpu(
        &mut self,
        owner: &ProcessData,
        absolute: bool,
        value: Duration,
    ) -> AxResult<()> {
        debug_assert!(self.is_cpu_clock());
        self.tai_deadline = None;
        self.deadline = None;
        self.cpu_deadline_ns = if value.is_zero() {
            None
        } else if absolute {
            Some(value.as_nanos().min(u64::MAX as u128) as u64)
        } else {
            let now = self.cpu_now_ns(owner).ok_or(AxError::NoSuchProcess)?;
            Some(now.saturating_add(value.as_nanos().min(u64::MAX as u128) as u64))
        };
        Ok(())
    }

    pub(crate) fn remaining(&self, owner: &ProcessData) -> Duration {
        if let Some(deadline) = self.cpu_deadline_ns {
            let now = self.cpu_now_ns(owner).unwrap_or(u64::MAX);
            return Duration::from_nanos(deadline.saturating_sub(now));
        }
        if let Some(deadline) = self.tai_deadline {
            return deadline.saturating_sub(crate::syscall::tai_time());
        }
        self.deadline
            .map(|deadline| deadline.saturating_sub(self.effective_clock.now()))
            .unwrap_or(Duration::ZERO)
    }

    fn timer_now(&self) -> Duration {
        if self.tai_deadline.is_some() {
            crate::syscall::tai_time()
        } else {
            self.effective_clock.now()
        }
    }

    /// Records the advertised TAI deadline together with the exact timex
    /// generation that supplied its realtime projection.  The caller owns
    /// `posix_timers`, so this stays paired with the main-alarm publication.
    pub(crate) fn set_tai_absolute_deadline(
        &mut self,
        deadline: Option<Duration>,
        generation: u64,
    ) {
        self.tai_deadline = deadline;
        self.tai_offset_generation = deadline.map_or(0, |_| generation);
    }

    fn rebase_tai_absolute_deadline(
        &mut self,
        generation: u64,
        offset_seconds: i64,
    ) -> Option<Duration> {
        let deadline = self.tai_deadline?;
        if self.tai_offset_generation >= generation {
            return None;
        }
        self.tai_offset_generation = generation;
        Some(tai_deadline_as_realtime(deadline, offset_seconds))
    }

    /// Arms/disarms an accounting-clock timer.  Returns an expiry delivery
    /// when this accounting sample crossed the threshold.
    fn evaluate_cpu(&mut self, owner: &ProcessData) -> Option<TimerSignalDelivery> {
        let deadline = self.cpu_deadline_ns?;
        let now = self.cpu_now_ns(owner)?;
        let terminal_thread = matches!(self.clock, PosixTimerClock::ThreadCpu)
            && self.cpu_target_task.as_ref().is_none_or(|task| {
                task.try_as_thread()
                    .is_none_or(|thread| thread.pending_exit())
            });
        if now < deadline {
            // A thread CPU clock cannot advance after terminal teardown.
            // Keep the final published usage observable through gettime only
            // until this worker pass, then retire an unreachable threshold.
            if terminal_thread {
                self.cpu_deadline_ns = None;
            }
            return None;
        }
        let expirations = if self.interval.is_zero() {
            self.cpu_deadline_ns = None;
            1
        } else {
            let interval = self.interval.as_nanos().min(u64::MAX as u128) as u64;
            let expirations = now
                .saturating_sub(deadline)
                .saturating_div(interval.max(1))
                .saturating_add(1);
            self.cpu_deadline_ns =
                Some(deadline.saturating_add(interval.saturating_mul(expirations)));
            expirations as u128
        };
        let delivery = self.begin_signal_delivery(expirations);
        if terminal_thread {
            self.cpu_deadline_ns = None;
        }
        delivery
    }

    pub(crate) fn publish(&mut self) {
        debug_assert!(!self.published, "POSIX timer published twice");
        self.published = true;
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

    pub(crate) fn reset_signal_delivery(&mut self) -> AlarmPublication {
        self.overrun = 0;
        self.signal_pending = false;
        self.signal_retry_pending = false;
        self.signal_token = 0;
        self.retry_alarm
            .as_ref()
            .map_or_else(AlarmPublication::empty, AlarmToken::prepare_disarm)
    }

    pub(crate) fn prepare_main_alarm(
        &self,
        proc_data: &Arc<ProcessData>,
        timerid: usize,
        clock: AlarmClock,
        deadline: Duration,
        sequence: u64,
    ) -> AlarmPublication {
        self.main_alarm.prepare_arm(
            clock,
            deadline,
            AlarmAction::PosixTimer {
                proc: Arc::downgrade(proc_data),
                timerid,
                sequence,
            },
        )
    }

    pub(crate) fn prepare_main_disarm(&self) -> AlarmPublication {
        self.main_alarm.prepare_disarm()
    }

    fn main_alarm_matches(&self, owner: AlarmSlotKey) -> bool {
        self.main_alarm.matches(owner)
    }

    fn retry_alarm_matches(&self, owner: AlarmSlotKey) -> bool {
        self.retry_alarm
            .as_ref()
            .is_some_and(|alarm| alarm.matches(owner))
    }

    fn prepare_retry_alarm(
        &self,
        proc_data: &Arc<ProcessData>,
        timerid: usize,
        token: u32,
        backoff: Duration,
    ) -> Option<AlarmPublication> {
        let alarm = self.retry_alarm.as_ref()?;
        let deadline = AlarmClock::Monotonic
            .now()
            .checked_add(backoff)
            .unwrap_or(Duration::MAX);
        Some(alarm.prepare_arm(
            AlarmClock::Monotonic,
            deadline,
            AlarmAction::PosixTimerRetry {
                proc: Arc::downgrade(proc_data),
                timerid,
                token,
                backoff,
            },
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimerSignalDelivery {
    token: u32,
    overrun: i32,
}

/// The action to take when an alarm fires.
enum AlarmAction {
    /// Complete the process-wide real interval timer if this arm is current.
    ProcessITimerReal {
        proc: Weak<ProcessData>,
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

const ALARM_TOKEN_CAPACITY: usize = 4096;
const ALARM_HEAP_INDEX_NONE: usize = usize::MAX;
const ALARM_DISPATCH_BATCH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AlarmSlotKey {
    slot: usize,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlarmTokenReserveError {
    CapacityExhausted,
    TokenSpaceExhausted,
}

struct AlarmSlot {
    generation: u64,
    leased: bool,
    active: bool,
    clock: AlarmClock,
    deadline: Duration,
    action: Option<AlarmAction>,
    heap_index: usize,
}

impl AlarmSlot {
    const fn new() -> Self {
        Self {
            generation: 0,
            leased: false,
            active: false,
            clock: AlarmClock::Monotonic,
            deadline: Duration::ZERO,
            action: None,
            heap_index: ALARM_HEAP_INDEX_NONE,
        }
    }
}

#[derive(Clone, Copy)]
struct AlarmHeapNode {
    slot: usize,
    deadline: Duration,
}

impl AlarmHeapNode {
    const EMPTY: Self = Self {
        slot: 0,
        deadline: Duration::ZERO,
    };

    fn precedes(self, other: Self) -> bool {
        self.deadline < other.deadline
            || (self.deadline == other.deadline && self.slot < other.slot)
    }
}

/// A fixed-capacity indexed min-heap. Slot back-pointers make cancellation and
/// rearm `O(log N)` without leaving lazy tombstones behind.
struct AlarmHeap<const CAPACITY: usize> {
    nodes: [AlarmHeapNode; CAPACITY],
    len: usize,
}

impl<const CAPACITY: usize> AlarmHeap<CAPACITY> {
    const fn new() -> Self {
        Self {
            nodes: [AlarmHeapNode::EMPTY; CAPACITY],
            len: 0,
        }
    }

    fn peek_deadline(&self) -> Option<Duration> {
        (self.len != 0).then_some(self.nodes[0].deadline)
    }

    fn swap_nodes(&mut self, left: usize, right: usize, slots: &mut [AlarmSlot]) {
        self.nodes.swap(left, right);
        slots[self.nodes[left].slot].heap_index = left;
        slots[self.nodes[right].slot].heap_index = right;
    }

    fn sift_up(&mut self, mut index: usize, slots: &mut [AlarmSlot]) {
        while index != 0 {
            let parent = (index - 1) / 2;
            if !self.nodes[index].precedes(self.nodes[parent]) {
                break;
            }
            self.swap_nodes(index, parent, slots);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize, slots: &mut [AlarmSlot]) {
        loop {
            let left = index * 2 + 1;
            if left >= self.len {
                break;
            }
            let right = left + 1;
            let next = if right < self.len && self.nodes[right].precedes(self.nodes[left]) {
                right
            } else {
                left
            };
            if !self.nodes[next].precedes(self.nodes[index]) {
                break;
            }
            self.swap_nodes(index, next, slots);
            index = next;
        }
    }

    fn insert(&mut self, slot: usize, deadline: Duration, slots: &mut [AlarmSlot]) {
        debug_assert!(self.len < CAPACITY);
        debug_assert_eq!(slots[slot].heap_index, ALARM_HEAP_INDEX_NONE);
        let index = self.len;
        self.len += 1;
        self.nodes[index] = AlarmHeapNode { slot, deadline };
        slots[slot].heap_index = index;
        self.sift_up(index, slots);
    }

    fn remove_at(&mut self, index: usize, slots: &mut [AlarmSlot]) -> usize {
        debug_assert!(index < self.len);
        let removed = self.nodes[index].slot;
        self.len -= 1;
        if index != self.len {
            self.nodes[index] = self.nodes[self.len];
            slots[self.nodes[index].slot].heap_index = index;
            if index != 0 && self.nodes[index].precedes(self.nodes[(index - 1) / 2]) {
                self.sift_up(index, slots);
            } else {
                self.sift_down(index, slots);
            }
        }
        self.nodes[self.len] = AlarmHeapNode::EMPTY;
        slots[removed].heap_index = ALARM_HEAP_INDEX_NONE;
        removed
    }

    fn pop_min(&mut self, slots: &mut [AlarmSlot]) -> Option<usize> {
        (self.len != 0).then(|| self.remove_at(0, slots))
    }
}

struct AlarmDispatch {
    owner: AlarmSlotKey,
    action: AlarmAction,
}

struct AlarmRegistry<const CAPACITY: usize> {
    slots: [AlarmSlot; CAPACITY],
    realtime: AlarmHeap<CAPACITY>,
    monotonic: AlarmHeap<CAPACITY>,
    next_free: usize,
}

impl<const CAPACITY: usize> AlarmRegistry<CAPACITY> {
    const fn new() -> Self {
        Self {
            slots: [const { AlarmSlot::new() }; CAPACITY],
            realtime: AlarmHeap::new(),
            monotonic: AlarmHeap::new(),
            next_free: 0,
        }
    }

    fn heap(&self, clock: AlarmClock) -> &AlarmHeap<CAPACITY> {
        match clock {
            AlarmClock::Realtime => &self.realtime,
            AlarmClock::Monotonic => &self.monotonic,
        }
    }

    fn heap_mut(&mut self, clock: AlarmClock) -> (&mut AlarmHeap<CAPACITY>, &mut [AlarmSlot]) {
        match clock {
            AlarmClock::Realtime => (&mut self.realtime, &mut self.slots),
            AlarmClock::Monotonic => (&mut self.monotonic, &mut self.slots),
        }
    }

    fn is_live(&self, key: AlarmSlotKey) -> bool {
        self.slots
            .get(key.slot)
            .is_some_and(|slot| slot.leased && slot.generation == key.generation)
    }

    fn reserve(&mut self) -> Result<AlarmSlotKey, AlarmTokenReserveError> {
        let mut saw_retired_slot = false;
        for offset in 0..CAPACITY {
            let slot_index = (self.next_free + offset) % CAPACITY;
            let slot = &mut self.slots[slot_index];
            if slot.leased {
                continue;
            }
            if slot.generation == u64::MAX {
                saw_retired_slot = true;
                continue;
            }

            slot.generation += 1;
            slot.leased = true;
            slot.active = false;
            slot.action = None;
            slot.heap_index = ALARM_HEAP_INDEX_NONE;
            self.next_free = (slot_index + 1) % CAPACITY;
            return Ok(AlarmSlotKey {
                slot: slot_index,
                generation: slot.generation,
            });
        }

        if saw_retired_slot {
            Err(AlarmTokenReserveError::TokenSpaceExhausted)
        } else {
            Err(AlarmTokenReserveError::CapacityExhausted)
        }
    }

    fn remove_active(&mut self, slot_index: usize) -> Option<AlarmAction> {
        if !self.slots[slot_index].active {
            return None;
        }
        let clock = self.slots[slot_index].clock;
        let heap_index = self.slots[slot_index].heap_index;
        let (heap, slots) = self.heap_mut(clock);
        let removed = heap.remove_at(heap_index, slots);
        debug_assert_eq!(removed, slot_index);
        let slot = &mut self.slots[slot_index];
        slot.active = false;
        slot.action.take()
    }

    fn arm(
        &mut self,
        key: AlarmSlotKey,
        clock: AlarmClock,
        deadline: Duration,
        action: AlarmAction,
    ) -> Result<(Option<AlarmAction>, bool), AlarmAction> {
        if !self.is_live(key) {
            return Err(action);
        }

        let prior_deadline = self.heap(clock).peek_deadline();
        let retired = self.remove_active(key.slot);
        {
            let slot = &mut self.slots[key.slot];
            slot.clock = clock;
            slot.deadline = deadline;
            slot.action = Some(action);
            slot.active = true;
        }
        let (heap, slots) = self.heap_mut(clock);
        heap.insert(key.slot, deadline, slots);
        Ok((retired, prior_deadline.is_none_or(|prior| deadline < prior)))
    }

    fn disarm(&mut self, key: AlarmSlotKey) -> Option<AlarmAction> {
        self.is_live(key)
            .then(|| self.remove_active(key.slot))
            .flatten()
    }

    fn release(&mut self, key: AlarmSlotKey) -> Option<AlarmAction> {
        if !self.is_live(key) {
            return None;
        }
        let retired = self.remove_active(key.slot);
        let slot = &mut self.slots[key.slot];
        slot.leased = false;
        slot.active = false;
        slot.action = None;
        slot.heap_index = ALARM_HEAP_INDEX_NONE;
        self.next_free = key.slot;
        retired
    }

    fn next_deadline(&self, clock: AlarmClock) -> Option<Duration> {
        self.heap(clock).peek_deadline()
    }

    fn take_due_batch(
        &mut self,
        clock: AlarmClock,
        now: Duration,
        pending: &mut [Option<AlarmDispatch>; ALARM_DISPATCH_BATCH],
    ) -> usize {
        let mut count = 0;
        while count < pending.len()
            && self
                .heap(clock)
                .peek_deadline()
                .is_some_and(|deadline| deadline <= now)
        {
            let Some(slot_index) = ({
                let (heap, slots) = self.heap_mut(clock);
                heap.pop_min(slots)
            }) else {
                debug_assert!(false, "alarm heap lost its published root");
                break;
            };
            let slot = &mut self.slots[slot_index];
            slot.active = false;
            let Some(action) = slot.action.take() else {
                debug_assert!(false, "active alarm heap slot has no action");
                continue;
            };
            pending[count] = Some(AlarmDispatch {
                owner: AlarmSlotKey {
                    slot: slot_index,
                    generation: slot.generation,
                },
                action,
            });
            count += 1;
        }
        count
    }
}

static ALARM_REGISTRY: SpinNoIrq<AlarmRegistry<ALARM_TOKEN_CAPACITY>> =
    SpinNoIrq::new(AlarmRegistry::new());

/// Persistent ownership of one bounded alarm slot. Rearm never performs a new
/// admission, so an already-created periodic timer cannot lose its next event
/// merely because unrelated timers filled the registry.
#[derive(Debug)]
pub(crate) struct AlarmToken {
    key: AlarmSlotKey,
}

impl AlarmToken {
    pub(crate) fn try_new() -> Result<Self, AlarmTokenReserveError> {
        ALARM_REGISTRY.lock().reserve().map(|key| Self { key })
    }

    fn matches(&self, key: AlarmSlotKey) -> bool {
        self.key == key
    }

    fn prepare_arm(
        &self,
        clock: AlarmClock,
        deadline: Duration,
        action: AlarmAction,
    ) -> AlarmPublication {
        let result = ALARM_REGISTRY.lock().arm(self.key, clock, deadline, action);
        match result {
            Ok((retired, should_notify)) => AlarmPublication {
                retired,
                notify: should_notify.then_some(clock),
            },
            Err(action) => {
                debug_assert!(false, "live AlarmToken lost its registry slot");
                AlarmPublication {
                    retired: Some(action),
                    notify: None,
                }
            }
        }
    }

    pub(crate) fn prepare_disarm(&self) -> AlarmPublication {
        let retired = ALARM_REGISTRY.lock().disarm(self.key);
        AlarmPublication {
            retired,
            notify: None,
        }
    }
}

impl Drop for AlarmToken {
    fn drop(&mut self) {
        // Move the action out while locked, but release its Arc/Weak payload
        // only after the IRQ-safe registry guard is gone.
        let retired = ALARM_REGISTRY.lock().release(self.key);
        drop(retired);
    }
}

/// Deferred side effects of an alarm mutation. Callers may update the bounded
/// registry while holding their owner lock, then publish only after that lock
/// is released. Registry code itself never drops actions or wakes consumers.
#[must_use = "alarm mutations must be published after releasing owner locks"]
pub(crate) struct AlarmPublication {
    retired: Option<AlarmAction>,
    notify: Option<AlarmClock>,
}

impl AlarmPublication {
    const fn empty() -> Self {
        Self {
            retired: None,
            notify: None,
        }
    }

    pub(crate) fn publish(mut self) {
        let retired = self.retired.take();
        let notify = self.notify.take();
        drop(retired);
        if let Some(clock) = notify {
            alarm_event(clock).notify(1);
        }
    }
}

#[cfg(test)]
mod alarm_registry_tests {
    use super::*;

    fn wake_action(source: &Arc<PollSet>) -> AlarmAction {
        AlarmAction::WakePollSet(source.clone())
    }

    #[test]
    fn admission_refunds_slots_and_never_reuses_a_generation() {
        let mut registry = AlarmRegistry::<2>::new();
        let first = registry.reserve().unwrap();
        let second = registry.reserve().unwrap();
        assert_eq!(
            registry.reserve(),
            Err(AlarmTokenReserveError::CapacityExhausted)
        );

        assert!(registry.release(first).is_none());
        let replacement = registry.reserve().unwrap();
        assert_eq!(replacement.slot, first.slot);
        assert!(replacement.generation > first.generation);
        assert!(!registry.is_live(first));
        assert!(registry.is_live(replacement));

        assert!(registry.release(second).is_none());
        assert!(registry.release(replacement).is_none());
    }

    #[test]
    fn exhausted_generation_retires_the_slot_instead_of_wrapping() {
        let mut registry = AlarmRegistry::<1>::new();
        registry.slots[0].generation = u64::MAX;
        assert_eq!(
            registry.reserve(),
            Err(AlarmTokenReserveError::TokenSpaceExhausted)
        );
        assert!(!registry.slots[0].leased);
    }

    #[test]
    fn far_future_rearm_stays_one_node_and_moves_between_clock_heaps() {
        let mut registry = AlarmRegistry::<2>::new();
        let owner = registry.reserve().unwrap();
        let blocker = registry.reserve().unwrap();
        let source = Arc::new(PollSet::new());
        assert_eq!(
            registry.reserve(),
            Err(AlarmTokenReserveError::CapacityExhausted)
        );

        for iteration in 0..100_000_u64 {
            let clock = if iteration & 1 == 0 {
                AlarmClock::Realtime
            } else {
                AlarmClock::Monotonic
            };
            let deadline = Duration::from_secs(1_000_000 + iteration);
            let (retired, _) = registry
                .arm(owner, clock, deadline, wake_action(&source))
                .unwrap_or_else(|_| panic!("live lease rejected rearm"));
            drop(retired);
            assert_eq!(registry.realtime.len + registry.monotonic.len, 1);
            assert_eq!(registry.next_deadline(clock), Some(deadline));
        }

        let retired = registry.disarm(owner);
        assert!(retired.is_some());
        drop(retired);
        assert_eq!(registry.realtime.len + registry.monotonic.len, 0);
        assert!(registry.release(owner).is_none());
        assert!(registry.release(blocker).is_none());
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn rearm_removes_the_previous_heap_node_before_inserting() {
        let mut registry = AlarmRegistry::<2>::new();
        let owner = registry.reserve().unwrap();
        let source = Arc::new(PollSet::new());

        registry
            .arm(
                owner,
                AlarmClock::Monotonic,
                Duration::from_secs(1),
                wake_action(&source),
            )
            .unwrap_or_else(|_| panic!("live lease rejected arm"));
        let (retired, _) = registry
            .arm(
                owner,
                AlarmClock::Monotonic,
                Duration::from_secs(2),
                wake_action(&source),
            )
            .unwrap_or_else(|_| panic!("live lease rejected rearm"));

        assert!(retired.is_some());
        assert_eq!(registry.monotonic.len, 1);
        assert_eq!(
            registry.next_deadline(AlarmClock::Monotonic),
            Some(Duration::from_secs(2))
        );

        drop(retired);
        // The rearm kept the owner leased with the 2s alarm active, so the
        // lease release retires exactly that alarm; a second release finds
        // nothing left.
        let released = registry.release(owner);
        assert!(released.is_some());
        drop(released);
        assert_eq!(registry.realtime.len + registry.monotonic.len, 0);
        assert!(registry.release(owner).is_none());
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn due_dispatch_identity_cannot_cross_delete_and_slot_reuse() {
        let mut registry = AlarmRegistry::<1>::new();
        let old_owner = registry.reserve().unwrap();
        let source = Arc::new(PollSet::new());
        registry
            .arm(
                old_owner,
                AlarmClock::Monotonic,
                Duration::from_secs(1),
                wake_action(&source),
            )
            .unwrap_or_else(|_| panic!("live lease rejected arm"));

        let mut pending = [const { None }; ALARM_DISPATCH_BATCH];
        assert_eq!(
            registry.take_due_batch(AlarmClock::Monotonic, Duration::from_secs(1), &mut pending,),
            1
        );
        let dispatch = pending[0].take().unwrap();
        assert_eq!(dispatch.owner, old_owner);

        assert!(registry.release(old_owner).is_none());
        let new_owner = registry.reserve().unwrap();
        assert_eq!(new_owner.slot, old_owner.slot);
        assert_ne!(new_owner.generation, old_owner.generation);
        assert!(!registry.is_live(dispatch.owner));
        assert!(registry.is_live(new_owner));

        drop(dispatch);
        assert!(registry.release(new_owner).is_none());
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn due_dispatch_stops_after_one_fixed_batch() {
        const COUNT: usize = ALARM_DISPATCH_BATCH + 1;
        let mut registry = AlarmRegistry::<COUNT>::new();
        let source = Arc::new(PollSet::new());
        let mut owners = Vec::new();
        for _ in 0..COUNT {
            let owner = registry.reserve().unwrap();
            registry
                .arm(
                    owner,
                    AlarmClock::Monotonic,
                    Duration::from_secs(1),
                    wake_action(&source),
                )
                .unwrap_or_else(|_| panic!("live lease rejected arm"));
            owners.push(owner);
        }

        let mut first = [const { None }; ALARM_DISPATCH_BATCH];
        assert_eq!(
            registry.take_due_batch(AlarmClock::Monotonic, Duration::from_secs(1), &mut first),
            ALARM_DISPATCH_BATCH
        );
        assert_eq!(
            registry.next_deadline(AlarmClock::Monotonic),
            Some(Duration::from_secs(1))
        );

        let mut second = [const { None }; ALARM_DISPATCH_BATCH];
        assert_eq!(
            registry.take_due_batch(AlarmClock::Monotonic, Duration::from_secs(1), &mut second),
            1
        );
        assert_eq!(registry.next_deadline(AlarmClock::Monotonic), None);

        drop(first);
        drop(second);
        for owner in owners {
            assert!(registry.release(owner).is_none());
        }
        assert_eq!(Arc::strong_count(&source), 1);
    }
}

const CLOCK_TIMER_CAPACITY: usize = 256;
// The alarm owner drives timerfd/POSIX/interval timers for the whole kernel.
// Keep one dedicated slot in every per-CPU clock shard so user
// futex/nanosleep pressure cannot make that owner exit when a shard's general
// admission budget is full.
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
struct TimerSlotKey {
    slot: usize,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClockTimerToken {
    owner_cpu: usize,
    key: TimerSlotKey,
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

    fn reserve(&mut self, now: Duration, deadline: Duration) -> AxResult<Option<TimerSlotKey>> {
        self.reserve_with_admission(now, deadline, ClockTimerAdmission::General)
    }

    fn reserve_system(
        &mut self,
        now: Duration,
        deadline: Duration,
    ) -> AxResult<Option<TimerSlotKey>> {
        self.reserve_with_admission(now, deadline, ClockTimerAdmission::System)
    }

    fn reserve_with_admission(
        &mut self,
        now: Duration,
        deadline: Duration,
        admission: ClockTimerAdmission,
    ) -> AxResult<Option<TimerSlotKey>> {
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
        let key = TimerSlotKey {
            slot,
            generation: entry.generation,
        };
        Ok(Some(key))
    }

    fn is_live(&self, key: TimerSlotKey) -> bool {
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
        key: TimerSlotKey,
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

    fn cancel(&mut self, key: TimerSlotKey) -> Option<Waker> {
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
    token: Option<ClockTimerToken>,
}

impl Future for PreparedClockSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(token) = self.token else {
            return Poll::Ready(());
        };

        // Clone and release wakers outside the IRQ-safe registry lock. The
        // locked section performs only a bounded slot lookup and replacement.
        let owned = cx.waker().clone();
        let (result, deferred) =
            timer_runtime(self.clock, token.owner_cpu)
                .lock()
                .poll(token.key, cx.waker(), owned);
        drop(deferred);
        result
    }
}

impl Drop for PreparedClockSleep {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // Cancellation is safe from a migrated task: the token routes to
            // the owning CPU's shard and that shard's spin lock serializes
            // with its timer callback. Only the owning CPU may reprogram its
            // local hardware deadline. A remote cancellation can leave one
            // stale early interrupt, but cannot delay a later live deadline.
            let _cpu_guard = NoPreempt::new();
            let current_cpu = axhal::percpu::this_cpu_id();
            let deferred = timer_runtime(self.clock, token.owner_cpu)
                .lock()
                .cancel(token.key);
            drop(deferred);
            if current_cpu == token.owner_cpu {
                update_clock_timer_deadline(token.owner_cpu);
            }
        }
    }
}

lazy_static! {
    static ref REALTIME_ALARM_EVENT: Event = Event::new();
    static ref MONOTONIC_ALARM_EVENT: Event = Event::new();
}

// CPU-clock sleepers cannot be backed by a wall-clock deadline: a task which
// is not consuming CPU must remain asleep indefinitely.  Keep their wake
// registrations bounded and allocation-free in the accounting producer path.
const CPU_CLOCK_SLEEP_WAITER_CAPACITY: usize = 64;
static CPU_CLOCK_SLEEP_WAITERS: PollSet<CPU_CLOCK_SLEEP_WAITER_CAPACITY> = PollSet::new();

/// Returns the bounded readiness source notified after CPU accounting advances
/// or a thread begins terminal teardown. Callers retain their target and
/// predicate; the source intentionally broadcasts rather than keeping
/// unbounded per-task timer lists in IRQ accounting paths.
pub(crate) fn cpu_clock_sleep_waiters() -> &'static PollSet<CPU_CLOCK_SLEEP_WAITER_CAPACITY> {
    &CPU_CLOCK_SLEEP_WAITERS
}

/// Wakes CPU-clock sleepers to re-evaluate their pinned target.  This is
/// called only after the new accounting snapshot is published, so observers
/// cannot wake and read an older target value.
pub(crate) fn notify_cpu_clock_sleepers() {
    if !CPU_CLOCK_SLEEP_WAITERS.is_empty() {
        CPU_CLOCK_SLEEP_WAITERS.wake();
    }
}

static REALTIME_TIMER_RUNTIMES: [SpinNoIrq<ClockTimerRuntime>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(ClockTimerRuntime::new()) }; axconfig::plat::MAX_CPU_NUM];
static MONOTONIC_TIMER_RUNTIMES: [SpinNoIrq<ClockTimerRuntime>; axconfig::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(ClockTimerRuntime::new()) }; axconfig::plat::MAX_CPU_NUM];

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

    fn process_cpu_now(self, usage: ProcessCpuUsage) -> Option<u64> {
        match self {
            ITimerType::Real => None,
            ITimerType::Virtual => Some(usage.virtual_ns),
            ITimerType::Prof => Some(usage.prof_ns),
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessTimerCharge {
    user_ns: usize,
    system_ns: usize,
}

impl ProcessTimerCharge {
    fn total_ns(&self) -> usize {
        self.user_ns.saturating_add(self.system_ns)
    }

    fn is_empty(&self) -> bool {
        self.user_ns == 0 && self.system_ns == 0
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct ProcessCpuUsage {
    virtual_ns: u64,
    prof_ns: u64,
    total_ns: u64,
}

fn add_process_cpu_nonwrapping(
    counter: &core::sync::atomic::AtomicU64,
    overflowed: &core::sync::atomic::AtomicBool,
    delta: usize,
) {
    let delta = u64::try_from(delta).unwrap_or(u64::MAX);
    if delta == 0 {
        return;
    }
    if overflowed.load(Ordering::Acquire) {
        return;
    }

    // One fixed atomic operation keeps the timer-IRQ producer bounded. The
    // physical word may wrap at the overflow edge, but the durable marker is
    // the logical high bit: every reader maps the whole clock domain to MAX
    // after observing it, so no generation is ever reused.
    let previous = counter.fetch_add(delta, Ordering::AcqRel);
    if previous.checked_add(delta).is_none() {
        overflowed.store(true, Ordering::Release);
    }
}

fn account_eligible_process_cpu(
    epoch: &core::sync::atomic::AtomicU64,
    writers: &core::sync::atomic::AtomicUsize,
    clock_ns: &core::sync::atomic::AtomicU64,
    overflowed: &core::sync::atomic::AtomicBool,
    local_epoch: &mut u64,
    delta_ns: usize,
) {
    if delta_ns == 0 {
        return;
    }
    let observed = epoch.load(Ordering::SeqCst);
    if observed & 1 != 0 {
        // An arm transition owns the cutoff. Conservatively omit this
        // crossing interval; CPU timers may be late, but never consume time
        // that began before their new generation.
        return;
    }

    // Epoch and writer admission are separate words. These four crossing
    // operations deliberately share the SeqCst order with the arming side so
    // the writer and armer cannot both miss one another (store buffering).
    writers.fetch_add(1, Ordering::SeqCst);
    let stable = epoch.load(Ordering::SeqCst);
    if stable == observed && stable & 1 == 0 {
        if *local_epoch == stable {
            add_process_cpu_nonwrapping(clock_ns, overflowed, delta_ns);
        } else {
            // The interval began under an older generation. Rebase the local
            // cursor without charging it to the newly armed timer.
            *local_epoch = stable;
        }
    }
    writers.fetch_sub(1, Ordering::SeqCst);
}

fn account_process_cpu(
    proc_data: &ProcessData,
    charge: ProcessTimerCharge,
    virtual_epoch: &mut u64,
    prof_epoch: &mut u64,
) {
    if charge.is_empty() {
        return;
    }

    add_process_cpu_nonwrapping(
        &proc_data.process_cpu_total_ns,
        &proc_data.process_cpu_accounting_overflowed,
        charge.total_ns(),
    );
    let armed = proc_data.process_itimer_cpu_armed.load(Ordering::Acquire);
    if armed & PROCESS_ITIMER_VIRTUAL_PENDING != 0 {
        account_eligible_process_cpu(
            &proc_data.process_itimer_virtual_epoch,
            &proc_data.process_itimer_virtual_writers,
            &proc_data.process_itimer_virtual_clock_ns,
            &proc_data.process_cpu_accounting_overflowed,
            virtual_epoch,
            charge.user_ns,
        );
    }
    if armed & PROCESS_ITIMER_PROF_PENDING != 0 {
        account_eligible_process_cpu(
            &proc_data.process_itimer_prof_epoch,
            &proc_data.process_itimer_prof_writers,
            &proc_data.process_itimer_prof_clock_ns,
            &proc_data.process_cpu_accounting_overflowed,
            prof_epoch,
            charge.total_ns(),
        );
    }
    notify_foreign_cpu_timer_owners(proc_data);
}

/// IRQ-side RCU read section for foreign CPU-clock timer owners. Publishers
/// retain one Arc raw count in each live node; an even generation is stable,
/// odd is being retired/replaced. No allocation or lock is taken here.
fn notify_foreign_cpu_timer_owners(target: &ProcessData) {
    for node in &target.foreign_cpu_timer_subscribers.nodes {
        let before = node.generation.load(Ordering::Acquire);
        if before == 0 || before & 1 != 0 {
            continue;
        }
        let owner = node.owner.load(Ordering::Acquire);
        if owner.is_null() {
            continue;
        }
        // SAFETY: a stable node owns one raw Arc count until its publisher
        // flips generation odd, clears owner, and waits for IRQ readers.
        unsafe {
            Arc::increment_strong_count(owner);
        }
        if node.generation.load(Ordering::Acquire) != before {
            unsafe {
                drop(Arc::from_raw(owner));
            }
            continue;
        }
        let owner = unsafe { Arc::from_raw(owner) };
        if let Some(cpu) = request_process_cpu_evaluation(&owner) {
            crate::deferred_work::wake_process_timer_worker(cpu);
        }
    }
}

fn process_cpu_usage(proc_data: &ProcessData) -> ProcessCpuUsage {
    if proc_data
        .process_cpu_accounting_overflowed
        .load(Ordering::Acquire)
    {
        return ProcessCpuUsage {
            virtual_ns: u64::MAX,
            prof_ns: u64::MAX,
            total_ns: u64::MAX,
        };
    }
    ProcessCpuUsage {
        virtual_ns: proc_data
            .process_itimer_virtual_clock_ns
            .load(Ordering::Acquire),
        prof_ns: proc_data
            .process_itimer_prof_clock_ns
            .load(Ordering::Acquire),
        total_ns: proc_data.process_cpu_total_ns.load(Ordering::Relaxed),
    }
}

/// Republishes whether POSIX CPU-clock timers require accounting-worker
/// wakeups.  The timer owner calls this only after releasing `posix_timers`;
/// the worker samples the inverse lock order only after it has completed the
/// interval-timer evaluation, so no lock cycle is introduced.
pub(crate) fn refresh_posix_cpu_timer_armed(proc_data: &ProcessData) {
    let posix_armed = proc_data.posix_timers.lock().iter().flatten().any(|timer| {
        timer.is_published() && timer.is_cpu_clock() && timer.cpu_deadline_ns.is_some()
    });
    let interval_armed = proc_data.process_itimers.lock().cpu_armed_mask();
    proc_data.process_itimer_cpu_armed.store(
        interval_armed
            | if posix_armed {
                PROCESS_POSIX_CPU_ARMED
            } else {
                0
            },
        Ordering::Release,
    );
}

const PROCESS_ITIMER_VIRTUAL_PENDING: u8 = 1 << 0;
const PROCESS_ITIMER_PROF_PENDING: u8 = 1 << 1;
const PROCESS_RLIMIT_CPU_SOFT_PENDING: u8 = 1 << 2;
const PROCESS_RLIMIT_CPU_HARD_PENDING: u8 = 1 << 3;
const PROCESS_CPU_EVALUATE_PENDING: u8 = 1 << 4;
/// At least one POSIX CPU-clock timer is armed.  This is an admission bit,
/// not a signal bit: accounting only uses it to queue the process worker.
const PROCESS_POSIX_CPU_ARMED: u8 = 1 << 5;
const PROCESS_ITIMER_CPU_ARMED_MASK: u8 =
    PROCESS_ITIMER_VIRTUAL_PENDING | PROCESS_ITIMER_PROF_PENDING | PROCESS_POSIX_CPU_ARMED;
const PROCESS_CPU_POLICY_PENDING_MASK: u8 = PROCESS_ITIMER_CPU_ARMED_MASK
    | PROCESS_RLIMIT_CPU_SOFT_PENDING
    | PROCESS_RLIMIT_CPU_HARD_PENDING
    | PROCESS_CPU_EVALUATE_PENDING;
const PROCESS_ITIMER_WORK_BATCH: usize = 16;
/// Fixed foreign CPU-clock subscriber capacity per target process.  Nodes are
/// embedded in ProcessData, so accounting IRQ publication never allocates.
pub(crate) const FOREIGN_CPU_TIMER_SUBSCRIBERS: usize = 32;
pub(crate) struct ForeignCpuTimerSubscriber {
    pub(crate) generation: AtomicU64,
    pub(crate) owner: AtomicPtr<ProcessData>,
}
impl ForeignCpuTimerSubscriber {
    pub(crate) const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            owner: AtomicPtr::new(ptr::null_mut()),
        }
    }
}
pub(crate) struct ForeignCpuTimerSubscriberPool {
    pub(crate) epoch: AtomicU64,
    pub(crate) nodes: [ForeignCpuTimerSubscriber; FOREIGN_CPU_TIMER_SUBSCRIBERS],
}
impl ForeignCpuTimerSubscriberPool {
    pub(crate) const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            nodes: [const { ForeignCpuTimerSubscriber::new() }; FOREIGN_CPU_TIMER_SUBSCRIBERS],
        }
    }
}
pub(crate) fn publish_foreign_cpu_timer_owner(
    target: &ProcessData,
    owner: &Arc<ProcessData>,
) -> AxResult<usize> {
    for (index, node) in target
        .foreign_cpu_timer_subscribers
        .nodes
        .iter()
        .enumerate()
    {
        if node
            .owner
            .compare_exchange(
                ptr::null_mut(),
                Arc::as_ptr(owner).cast_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let generation = target
                .foreign_cpu_timer_subscribers
                .epoch
                .fetch_add(2, Ordering::AcqRel)
                .saturating_add(2)
                & !1;
            let retained = Arc::into_raw(owner.clone()).cast_mut();
            node.owner.store(retained, Ordering::Release);
            node.generation.store(generation.max(2), Ordering::Release);
            return Ok(index);
        }
    }
    Err(AxError::WouldBlock)
}
pub(crate) fn retire_foreign_cpu_timer_owner(target: &ProcessData, slot: usize) {
    let Some(node) = target.foreign_cpu_timer_subscribers.nodes.get(slot) else {
        return;
    };
    let generation = node.generation.load(Ordering::Acquire);
    node.generation.store(generation | 1, Ordering::Release);
    let owner = node.owner.swap(ptr::null_mut(), Ordering::AcqRel);
    // IRQ readers validate generation after retaining their temporary Arc;
    // this release point prevents new readers from observing this raw owner.
    node.generation.store(0, Ordering::Release);
    if !owner.is_null() {
        unsafe {
            drop(Arc::from_raw(owner));
        }
    }
}
/// Intrusive node for one bounded process-timer MPSC ingress. A queued
/// process retains one strong `Arc` in `owner`; the queue's single consumer
/// converts that raw strong reference back into an `Arc` after fully unlinking
/// the node. Permanent per-CPU stubs have a null owner.
pub(crate) struct ProcessITimerWorkNode {
    next: AtomicPtr<ProcessITimerWorkNode>,
    owner: AtomicPtr<ProcessData>,
}

impl ProcessITimerWorkNode {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            owner: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

const PROCESS_ITIMER_CPU_COUNT: usize = axconfig::plat::MAX_CPU_NUM;

// Queue ownership is permanent for the x86_64-only kernel. The current
// project has no CPU hotplug path, so a node published to CPU N's queue is
// normally consumed by CPU N's pinned worker; if that worker fails, the same
// fixed cursor may be handed to one bounded fallback task context. Keeping
// stubs and tails in fixed storage makes IRQ/task-context publication
// allocation free and gives every producer its own tail cache line domain.
static PROCESS_ITIMER_WORK_STUBS: [ProcessITimerWorkNode; PROCESS_ITIMER_CPU_COUNT] =
    [const { ProcessITimerWorkNode::new() }; PROCESS_ITIMER_CPU_COUNT];
static PROCESS_ITIMER_WORK_TAILS: [AtomicPtr<ProcessITimerWorkNode>; PROCESS_ITIMER_CPU_COUNT] =
    [const { AtomicPtr::new(ptr::null_mut()) }; PROCESS_ITIMER_CPU_COUNT];
static PROCESS_ITIMER_WORK_QUEUES_INIT: Once = Once::new();
static PROCESS_ITIMER_WORK_PUBLISHED: AtomicUsize = AtomicUsize::new(0);
static PROCESS_ITIMER_WORK_DRAINED: AtomicUsize = AtomicUsize::new(0);
static PROCESS_ITIMER_WORK_LAST_NODE_CLEANUPS: AtomicUsize = AtomicUsize::new(0);
static PROCESS_ITIMER_WORK_PRODUCER_LINK_GAPS: AtomicUsize = AtomicUsize::new(0);

const PROCESS_ITIMER_CONSUMER_FREE: u8 = 0;
const PROCESS_ITIMER_CONSUMER_WORKER: u8 = 1;
const PROCESS_ITIMER_CONSUMER_FALLBACK: u8 = 2;

struct ProcessITimerConsumerSlot {
    owner: AtomicU8,
    cursor: UnsafeCell<MaybeUninit<ProcessITimerWorkConsumer>>,
}

// The owner token is the synchronization boundary. Exactly one task context
// may dereference a slot cursor at a time; the fixed storage itself never
// moves, so a worker can release it after an error and a fallback (possibly
// running on another CPU) can acquire the same cursor without rebuilding the
// queue position from the stub.
unsafe impl Sync for ProcessITimerConsumerSlot {}

impl ProcessITimerConsumerSlot {
    const fn new() -> Self {
        Self {
            owner: AtomicU8::new(PROCESS_ITIMER_CONSUMER_FREE),
            cursor: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

static PROCESS_ITIMER_CONSUMER_SLOTS: [ProcessITimerConsumerSlot; PROCESS_ITIMER_CPU_COUNT] =
    [const { ProcessITimerConsumerSlot::new() }; PROCESS_ITIMER_CPU_COUNT];
static PROCESS_ITIMER_CONSUMERS_INIT: Once = Once::new();

fn ensure_process_itimer_work_queues() {
    PROCESS_ITIMER_WORK_QUEUES_INIT.call_once(|| {
        for cpu in 0..PROCESS_ITIMER_CPU_COUNT {
            let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[cpu]).cast_mut();
            PROCESS_ITIMER_WORK_STUBS[cpu]
                .next
                .store(ptr::null_mut(), Ordering::Relaxed);
            PROCESS_ITIMER_WORK_STUBS[cpu]
                .owner
                .store(ptr::null_mut(), Ordering::Relaxed);
            PROCESS_ITIMER_WORK_TAILS[cpu].store(stub, Ordering::Relaxed);
        }
    });
}

fn process_itimer_consumer_from_cpu(cpu: usize) -> ProcessITimerWorkConsumer {
    debug_assert!(cpu < PROCESS_ITIMER_CPU_COUNT);
    ProcessITimerWorkConsumer {
        cpu,
        head: ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[cpu]).cast_mut(),
    }
}

fn ensure_process_itimer_consumers() {
    ensure_process_itimer_work_queues();
    PROCESS_ITIMER_CONSUMERS_INIT.call_once(|| {
        for (cpu, slot) in PROCESS_ITIMER_CONSUMER_SLOTS.iter().enumerate() {
            // SAFETY: `call_once` exclusively initializes each never-read
            // slot. The owner token remains FREE until this write completes.
            unsafe {
                (*slot.cursor.get()) = MaybeUninit::new(process_itimer_consumer_from_cpu(cpu));
            }
        }
    });
}

fn acquire_process_itimer_consumer(cpu: usize, owner: u8) -> bool {
    ensure_process_itimer_consumers();
    debug_assert!(cpu < PROCESS_ITIMER_CPU_COUNT);
    PROCESS_ITIMER_CONSUMER_SLOTS[cpu]
        .owner
        .compare_exchange(
            PROCESS_ITIMER_CONSUMER_FREE,
            owner,
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .is_ok()
}

fn release_process_itimer_consumer(cpu: usize, owner: u8) {
    debug_assert!(cpu < PROCESS_ITIMER_CPU_COUNT);
    let result = PROCESS_ITIMER_CONSUMER_SLOTS[cpu].owner.compare_exchange(
        owner,
        PROCESS_ITIMER_CONSUMER_FREE,
        Ordering::Release,
        Ordering::Relaxed,
    );
    debug_assert!(result.is_ok());
}

unsafe fn process_itimer_consumer_mut(cpu: usize) -> &'static mut ProcessITimerWorkConsumer {
    // SAFETY: callers hold the slot's owner token, which serializes this
    // mutable access between the bound worker and the fallback consumer.
    unsafe { (*PROCESS_ITIMER_CONSUMER_SLOTS[cpu].cursor.get()).assume_init_mut() }
}

pub(crate) fn acquire_process_itimer_worker_consumer(cpu: usize) -> bool {
    acquire_process_itimer_consumer(cpu, PROCESS_ITIMER_CONSUMER_WORKER)
}

pub(crate) fn acquire_process_itimer_fallback_consumer(cpu: usize) -> bool {
    acquire_process_itimer_consumer(cpu, PROCESS_ITIMER_CONSUMER_FALLBACK)
}

pub(crate) fn release_process_itimer_worker_consumer(cpu: usize) {
    release_process_itimer_consumer(cpu, PROCESS_ITIMER_CONSUMER_WORKER);
}

pub(crate) fn release_process_itimer_fallback_consumer(cpu: usize) {
    release_process_itimer_consumer(cpu, PROCESS_ITIMER_CONSUMER_FALLBACK);
}

pub(crate) fn process_itimer_consumer_has_pending(cpu: usize) -> bool {
    // SAFETY: the worker/fallback owner token is held by the caller.
    unsafe { process_itimer_consumer_mut(cpu).has_pending() }
}

pub(crate) fn process_itimer_consumer_is_quiescent(cpu: usize) -> bool {
    // SAFETY: the worker/fallback owner token is held by the caller.
    unsafe { process_itimer_consumer_mut(cpu).is_quiescent() }
}

pub(crate) fn drain_process_itimer_batch(cpu: usize) -> usize {
    // SAFETY: the worker/fallback owner token is held by the caller.
    unsafe { process_itimer_consumer_mut(cpu).drain_batch() }
}

/// Initializes the fixed per-CPU ingress before user tasks are published.
/// `Once` also keeps direct unit-test and early teardown callers safe if they
/// exercise the producer before the normal deferred-work init sequence.
pub(crate) fn init_process_itimer_work_queues() {
    ensure_process_itimer_consumers();
}

#[inline]
fn process_itimer_owner_cpu() -> usize {
    let cpu = axhal::percpu::this_cpu_id();
    debug_assert!(cpu < PROCESS_ITIMER_CPU_COUNT);
    cpu
}

#[inline]
fn process_itimer_owner_from_token_for_cpu_count(token: usize, cpu_count: usize) -> Option<usize> {
    let cpu = token.checked_sub(1)?;
    (cpu < cpu_count).then_some(cpu)
}

#[inline]
fn process_itimer_owner_from_token(token: usize) -> Option<usize> {
    // This word is only ever published by `publish_process_itimer_work`, but
    // treat a stale/corrupt value as an absent wake target in release builds
    // as well.  Returning an out-of-range CPU here would turn a harmless
    // coalesced request into an indexing panic in the wake path.
    process_itimer_owner_from_token_for_cpu_count(token, PROCESS_ITIMER_CPU_COUNT)
}

fn rebase_process_cpu_timer_clock(
    owner: &ProcessData,
    ty: ITimerType,
) -> Result<(u64, u64), ProcessITimerSetAttemptError> {
    let (epoch, writers, clock_ns) = match ty {
        ITimerType::Real => return Ok((0, 0)),
        ITimerType::Virtual => (
            &owner.process_itimer_virtual_epoch,
            &owner.process_itimer_virtual_writers,
            &owner.process_itimer_virtual_clock_ns,
        ),
        ITimerType::Prof => (
            &owner.process_itimer_prof_epoch,
            &owner.process_itimer_prof_writers,
            &owner.process_itimer_prof_clock_ns,
        ),
    };

    let current = epoch.load(Ordering::SeqCst);
    debug_assert_eq!(
        current & 1,
        0,
        "CPU timer epoch owner observed a transition"
    );
    let next = current
        .checked_add(2)
        .ok_or(ProcessITimerSetAttemptError::Kernel(AxError::OutOfRange))?;

    // The odd epoch closes admission for new IRQ writers. Writers already
    // admitted under `current` retire in a fixed, allocation-free section;
    // after they drain, the sampled clock is an exact arm cutoff.
    epoch.store(current + 1, Ordering::SeqCst);
    while writers.load(Ordering::SeqCst) != 0 {
        core::hint::spin_loop();
    }
    let baseline_ns = clock_ns.load(Ordering::Acquire);
    epoch.store(next, Ordering::SeqCst);
    Ok((baseline_ns, next))
}

#[derive(Debug)]
struct ProcessITimer {
    interval_ns: usize,
    remaining_ns: usize,
    cpu_deadline_ns: Option<u64>,
    sequence: u64,
}

impl ProcessITimer {
    const fn new() -> Self {
        Self {
            interval_ns: 0,
            remaining_ns: 0,
            cpu_deadline_ns: None,
            sequence: 0,
        }
    }

    fn evaluate_cpu(&mut self, now_ns: u64) -> bool {
        let Some(deadline_ns) = self.cpu_deadline_ns else {
            return false;
        };
        if now_ns < deadline_ns {
            return false;
        }

        if self.interval_ns == 0 {
            self.cpu_deadline_ns = None;
        } else {
            let interval_ns = self.interval_ns as u64;
            let expirations = now_ns
                .saturating_sub(deadline_ns)
                .saturating_div(interval_ns.max(1))
                .saturating_add(1);
            self.cpu_deadline_ns =
                Some(deadline_ns.saturating_add(interval_ns.saturating_mul(expirations)));
        }
        true
    }
}

/// Linux interval timers are shared by a thread group. The real timer lazily
/// admits one alarm lease on its first arm and then retains that lease until
/// process teardown; virtual/profiling timers consume aggregate per-thread
/// CPU-accounting deltas and never occupy a wall-clock alarm slot.
pub(crate) struct ProcessITimers {
    timers: [ProcessITimer; 3],
    real_deadline: Option<Duration>,
    real_alarm: Option<AlarmToken>,
}

impl ProcessITimers {
    pub(crate) const fn new() -> Self {
        Self {
            timers: [
                ProcessITimer::new(),
                ProcessITimer::new(),
                ProcessITimer::new(),
            ],
            real_deadline: None,
            real_alarm: None,
        }
    }

    fn cpu_remaining_at(timer: &ProcessITimer, now_ns: u64) -> usize {
        timer
            .cpu_deadline_ns
            .map(|deadline_ns| {
                if deadline_ns <= now_ns {
                    // Linux keeps an expired-but-not-yet-consumed CPU timer
                    // visibly armed by returning TICK_NSEC rather than zero.
                    (NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64).max(1)
                } else {
                    deadline_ns - now_ns
                }
            })
            .unwrap_or(0)
            .min(usize::MAX as u64) as usize
    }

    fn cpu_value_at(&self, ty: ITimerType, now_ns: u64) -> (TimeValue, TimeValue) {
        let timer = &self.timers[ty as usize];
        (
            time_value_from_nanos(timer.interval_ns),
            time_value_from_nanos(Self::cpu_remaining_at(timer, now_ns)),
        )
    }

    fn get(&self, owner: &ProcessData, ty: ITimerType) -> (TimeValue, TimeValue) {
        let timer = &self.timers[ty as usize];
        let remaining_ns = if ty == ITimerType::Real {
            self.real_deadline
                .map(|deadline| {
                    deadline
                        .saturating_sub(AlarmClock::Monotonic.now())
                        .as_nanos()
                        .min(usize::MAX as u128) as usize
                })
                .unwrap_or(0)
        } else {
            let now_ns = ty
                .process_cpu_now(process_cpu_usage(owner))
                .expect("CPU timer has a process CPU clock");
            Self::cpu_remaining_at(timer, now_ns)
        };
        (
            time_value_from_nanos(timer.interval_ns),
            time_value_from_nanos(remaining_ns),
        )
    }

    fn try_set(
        &mut self,
        owner: &Arc<ProcessData>,
        ty: ITimerType,
        interval_ns: usize,
        remaining_ns: usize,
        admitted_alarm: &mut Option<AlarmToken>,
    ) -> Result<ProcessITimerSetOutcome, ProcessITimerSetAttemptError> {
        let index = ty as usize;
        let sequence = self.timers[index]
            .sequence
            .checked_add(1)
            .ok_or(ProcessITimerSetAttemptError::Kernel(AxError::OutOfRange))?;
        let (old, cpu_deadline_ns, cpu_epoch) = if ty == ITimerType::Real {
            (self.get(owner, ty), None, None)
        } else {
            let requested_ns = u64::try_from(remaining_ns)
                .map_err(|_| ProcessITimerSetAttemptError::Kernel(AxError::OutOfRange))?;
            let (baseline_ns, epoch) = rebase_process_cpu_timer_clock(owner, ty)?;
            let old = self.cpu_value_at(ty, baseline_ns);
            let deadline = (remaining_ns != 0).then(|| baseline_ns.saturating_add(requested_ns));
            (old, deadline, Some(epoch))
        };

        if ty == ITimerType::Real && remaining_ns != 0 && self.real_alarm.is_none() {
            let Some(alarm) = admitted_alarm.take() else {
                return Err(ProcessITimerSetAttemptError::NeedAlarmToken);
            };
            self.real_alarm = Some(alarm);
        }

        let timer = &mut self.timers[index];
        timer.interval_ns = interval_ns;
        timer.remaining_ns = remaining_ns;
        timer.cpu_deadline_ns = cpu_deadline_ns;
        timer.sequence = sequence;
        owner
            .process_itimer_cpu_armed
            .store(self.cpu_armed_mask(), Ordering::Release);

        let publication = if ty != ITimerType::Real {
            AlarmPublication::empty()
        } else if remaining_ns == 0 {
            self.real_deadline = None;
            self.real_alarm
                .as_ref()
                .map_or_else(AlarmPublication::empty, AlarmToken::prepare_disarm)
        } else {
            let deadline = AlarmClock::Monotonic
                .now()
                .checked_add(Duration::from_nanos(remaining_ns as u64))
                .unwrap_or(Duration::MAX);
            self.real_deadline = Some(deadline);
            self.real_alarm
                .as_ref()
                .expect("armed process itimer owns an alarm lease")
                .prepare_arm(
                    AlarmClock::Monotonic,
                    deadline,
                    AlarmAction::ProcessITimerReal {
                        proc: Arc::downgrade(owner),
                        sequence,
                    },
                )
        };

        Ok(ProcessITimerSetOutcome {
            old,
            publication,
            cpu_epoch,
        })
    }

    fn cpu_armed_mask(&self) -> u8 {
        let mut mask = 0;
        if self.timers[ITimerType::Virtual as usize]
            .cpu_deadline_ns
            .is_some()
        {
            mask |= PROCESS_ITIMER_VIRTUAL_PENDING;
        }
        if self.timers[ITimerType::Prof as usize]
            .cpu_deadline_ns
            .is_some()
        {
            mask |= PROCESS_ITIMER_PROF_PENDING;
        }
        mask
    }

    fn evaluate_cpu(&mut self, usage: ProcessCpuUsage) -> ProcessITimerSignals {
        ProcessITimerSignals {
            virtual_expired: self.timers[ITimerType::Virtual as usize]
                .evaluate_cpu(usage.virtual_ns),
            prof_expired: self.timers[ITimerType::Prof as usize].evaluate_cpu(usage.prof_ns),
        }
    }

    fn prepare_real_fire(
        &mut self,
        owner: &Arc<ProcessData>,
        alarm_owner: AlarmSlotKey,
        sequence: u64,
    ) -> Option<ProcessITimerFireOutcome> {
        let alarm = self.real_alarm.as_ref()?;
        let timer = &mut self.timers[ITimerType::Real as usize];
        if !alarm.matches(alarm_owner) || timer.sequence != sequence {
            return None;
        }
        let deadline = self.real_deadline?;
        let now = AlarmClock::Monotonic.now();
        if now < deadline {
            return None;
        }

        if timer.interval_ns == 0 {
            timer.remaining_ns = 0;
            self.real_deadline = None;
            return Some(ProcessITimerFireOutcome {
                publication: AlarmPublication::empty(),
            });
        }

        let interval = Duration::from_nanos(timer.interval_ns as u64);
        let elapsed = now.saturating_sub(deadline).as_nanos();
        let expirations = 1_u128.saturating_add(elapsed / interval.as_nanos().max(1));
        let next_deadline = deadline
            .checked_add(saturating_duration_mul(interval, expirations))
            .unwrap_or(Duration::MAX);
        timer.remaining_ns = timer.interval_ns;
        self.real_deadline = Some(next_deadline);
        let publication = self
            .real_alarm
            .as_ref()
            .expect("periodic process itimer retains its alarm lease")
            .prepare_arm(
                AlarmClock::Monotonic,
                next_deadline,
                AlarmAction::ProcessITimerReal {
                    proc: Arc::downgrade(owner),
                    sequence,
                },
            );
        Some(ProcessITimerFireOutcome { publication })
    }
}

#[cfg(test)]
mod process_itimer_tests {
    use alloc::vec;

    use super::*;

    fn arm_cpu_timer(timers: &mut ProcessITimers, ty: ITimerType, interval: usize, deadline: u64) {
        let timer = &mut timers.timers[ty as usize];
        timer.interval_ns = interval;
        timer.cpu_deadline_ns = Some(deadline);
    }

    #[test]
    fn virtual_consumes_only_user_time_and_prof_consumes_both() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Virtual, 0, 5);
        arm_cpu_timer(&mut timers, ITimerType::Prof, 0, 5);

        let signals = timers.evaluate_cpu(ProcessCpuUsage {
            virtual_ns: 0,
            prof_ns: 5,
            total_ns: 5,
        });
        assert_eq!(
            signals,
            ProcessITimerSignals {
                virtual_expired: false,
                prof_expired: true,
            }
        );
        assert_eq!(
            timers.timers[ITimerType::Virtual as usize].cpu_deadline_ns,
            Some(5)
        );
        assert_eq!(
            timers.timers[ITimerType::Prof as usize].cpu_deadline_ns,
            None
        );

        let signals = timers.evaluate_cpu(ProcessCpuUsage {
            virtual_ns: 5,
            prof_ns: 10,
            total_ns: 10,
        });
        assert_eq!(
            signals,
            ProcessITimerSignals {
                virtual_expired: true,
                prof_expired: false,
            }
        );
        assert_eq!(timers.cpu_armed_mask(), 0);
    }

    #[test]
    fn periodic_cpu_timer_carries_overshoot_into_one_future_period() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Virtual, 10, 4);

        let signals = timers.evaluate_cpu(ProcessCpuUsage {
            virtual_ns: 27,
            prof_ns: 27,
            total_ns: 27,
        });
        assert!(signals.virtual_expired);
        assert!(!signals.prof_expired);
        assert_eq!(
            timers.timers[ITimerType::Virtual as usize].cpu_deadline_ns,
            Some(34)
        );
        assert_eq!(timers.cpu_armed_mask(), PROCESS_ITIMER_VIRTUAL_PENDING);
    }

    #[test]
    fn cpu_timer_one_shot_disarms_after_exact_threshold() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Prof, 0, 8);

        assert!(
            !timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: 3,
                    prof_ns: 7,
                    total_ns: 7,
                })
                .prof_expired
        );
        assert_eq!(
            timers.timers[ITimerType::Prof as usize].cpu_deadline_ns,
            Some(8)
        );
        assert!(
            timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: 3,
                    prof_ns: 8,
                    total_ns: 8,
                })
                .prof_expired
        );
        assert_eq!(
            timers.timers[ITimerType::Prof as usize].cpu_deadline_ns,
            None
        );
        assert!(
            !timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: u64::MAX,
                    prof_ns: u64::MAX,
                    total_ns: u64::MAX,
                })
                .prof_expired
        );
    }

    #[test]
    fn absolute_cpu_deadline_does_not_consume_pre_arm_usage() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Virtual, 0, 105);

        assert!(
            !timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: 104,
                    prof_ns: 104,
                    total_ns: 104,
                })
                .virtual_expired
        );
        assert!(
            timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: 105,
                    prof_ns: 105,
                    total_ns: 105,
                })
                .virtual_expired
        );
    }

    #[test]
    fn eligible_clock_rebases_the_first_cross_generation_interval() {
        let epoch = core::sync::atomic::AtomicU64::new(0);
        let writers = core::sync::atomic::AtomicUsize::new(0);
        let clock_ns = core::sync::atomic::AtomicU64::new(0);
        let overflowed = core::sync::atomic::AtomicBool::new(false);
        let mut local_epoch = 0;

        account_eligible_process_cpu(
            &epoch,
            &writers,
            &clock_ns,
            &overflowed,
            &mut local_epoch,
            5,
        );
        assert_eq!(clock_ns.load(Ordering::Relaxed), 5);

        epoch.store(2, Ordering::Release);
        account_eligible_process_cpu(
            &epoch,
            &writers,
            &clock_ns,
            &overflowed,
            &mut local_epoch,
            7,
        );
        assert_eq!(local_epoch, 2);
        assert_eq!(clock_ns.load(Ordering::Relaxed), 5);

        account_eligible_process_cpu(
            &epoch,
            &writers,
            &clock_ns,
            &overflowed,
            &mut local_epoch,
            3,
        );
        assert_eq!(clock_ns.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn two_poll_arm_cutoff_charges_lifetime_without_consuming_new_prof_timer() {
        let epoch = core::sync::atomic::AtomicU64::new(0);
        let writers = core::sync::atomic::AtomicUsize::new(0);
        let eligible_ns = core::sync::atomic::AtomicU64::new(0);
        let lifetime_ns = core::sync::atomic::AtomicU64::new(0);
        let overflowed = core::sync::atomic::AtomicBool::new(false);
        let mut local_epoch = 0;

        // The pre-arm poll closes the interval that began before the syscall.
        add_process_cpu_nonwrapping(&lifetime_ns, &overflowed, 5);
        account_eligible_process_cpu(
            &epoch,
            &writers,
            &eligible_ns,
            &overflowed,
            &mut local_epoch,
            5,
        );

        // Arming publishes a fresh stable generation. The second poll still
        // charges lifetime CPU, but rebases its local cursor instead of
        // relabeling the crossing interval as newly eligible PROF time.
        epoch.store(2, Ordering::SeqCst);
        add_process_cpu_nonwrapping(&lifetime_ns, &overflowed, 7);
        account_eligible_process_cpu(
            &epoch,
            &writers,
            &eligible_ns,
            &overflowed,
            &mut local_epoch,
            7,
        );
        assert_eq!(lifetime_ns.load(Ordering::Relaxed), 12);
        assert_eq!(eligible_ns.load(Ordering::Relaxed), 5);
        assert_eq!(local_epoch, 2);

        // Only the next interval belongs to the newly armed generation.
        add_process_cpu_nonwrapping(&lifetime_ns, &overflowed, 3);
        account_eligible_process_cpu(
            &epoch,
            &writers,
            &eligible_ns,
            &overflowed,
            &mut local_epoch,
            3,
        );
        assert_eq!(lifetime_ns.load(Ordering::Relaxed), 15);
        assert_eq!(eligible_ns.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn odd_arm_epoch_closes_writer_admission_without_relabeling_debt() {
        let epoch = core::sync::atomic::AtomicU64::new(1);
        let writers = core::sync::atomic::AtomicUsize::new(0);
        let clock_ns = core::sync::atomic::AtomicU64::new(0);
        let overflowed = core::sync::atomic::AtomicBool::new(false);
        let mut local_epoch = 0;

        account_eligible_process_cpu(
            &epoch,
            &writers,
            &clock_ns,
            &overflowed,
            &mut local_epoch,
            7,
        );
        assert_eq!(local_epoch, 0);
        assert_eq!(clock_ns.load(Ordering::Relaxed), 0);

        epoch.store(2, Ordering::Release);
        account_eligible_process_cpu(
            &epoch,
            &writers,
            &clock_ns,
            &overflowed,
            &mut local_epoch,
            7,
        );
        assert_eq!(local_epoch, 2);
        assert_eq!(clock_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn seq_cst_arm_gate_excludes_every_old_writer_interleaving() {
        #[derive(Clone, Copy)]
        struct Model {
            epoch: u64,
            writers: usize,
            writer_observed: u64,
            local_epoch: u64,
            baseline_taken: bool,
            writer_step: u8,
            armer_step: u8,
        }

        fn explore(model: Model, completed: &mut usize) {
            if model.writer_step == 4 && model.armer_step == 4 {
                *completed += 1;
                return;
            }

            if model.writer_step < 4 {
                let mut next = model;
                match next.writer_step {
                    0 => next.writer_observed = next.epoch,
                    1 => next.writers += 1,
                    2 => {
                        if next.epoch == next.writer_observed
                            && next.epoch & 1 == 0
                            && next.local_epoch == next.epoch
                        {
                            assert!(
                                !next.baseline_taken,
                                "an old-generation writer published after the arm baseline"
                            );
                        } else if next.epoch & 1 == 0 {
                            next.local_epoch = next.epoch;
                        }
                    }
                    3 => next.writers -= 1,
                    _ => unreachable!(),
                }
                next.writer_step += 1;
                explore(next, completed);
            }

            if model.armer_step < 4 && (model.armer_step != 1 || model.writers == 0) {
                let mut next = model;
                match next.armer_step {
                    0 => next.epoch = 1,
                    1 => {}
                    2 => next.baseline_taken = true,
                    3 => next.epoch = 2,
                    _ => unreachable!(),
                }
                next.armer_step += 1;
                explore(next, completed);
            }
        }

        let mut completed = 0;
        explore(
            Model {
                epoch: 0,
                writers: 0,
                writer_observed: 0,
                local_epoch: 0,
                baseline_taken: false,
                writer_step: 0,
                armer_step: 0,
            },
            &mut completed,
        );
        assert!(completed != 0);
    }

    #[test]
    fn expired_cpu_timer_stays_visibly_armed_until_worker_consumes_it() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Prof, 0, 8);
        let (_, remaining) = timers.cpu_value_at(ITimerType::Prof, 8);
        assert_eq!(
            remaining.as_nanos(),
            (NANOS_PER_SEC / axconfig::TICKS_PER_SEC as u64).max(1) as u128
        );

        assert!(
            timers
                .evaluate_cpu(ProcessCpuUsage {
                    virtual_ns: 0,
                    prof_ns: 8,
                    total_ns: 8,
                })
                .prof_expired
        );
        let (_, remaining) = timers.cpu_value_at(ITimerType::Prof, 8);
        assert_eq!(remaining.as_nanos(), 0);
    }

    #[test]
    fn seq_cst_pending_handoff_never_strands_the_last_request() {
        #[derive(Clone, Copy)]
        struct Model {
            pending: bool,
            queued: bool,
            consumer_observed_pending: bool,
            producer_step: u8,
            consumer_step: u8,
        }

        fn explore(model: Model, completed: &mut usize) {
            if model.producer_step == 2 && model.consumer_step == 3 {
                *completed += 1;
                assert!(
                    !model.pending || model.queued,
                    "a published CPU-policy request was left without a queue owner"
                );
                return;
            }

            if model.producer_step < 2 {
                let mut next = model;
                match next.producer_step {
                    // pending.fetch_or(..., SeqCst)
                    0 => next.pending = true,
                    // queued.compare_exchange(..., SeqCst, SeqCst)
                    1 if !next.queued => next.queued = true,
                    1 => {}
                    _ => unreachable!(),
                }
                next.producer_step += 1;
                explore(next, completed);
            }

            if model.consumer_step < 3 {
                let mut next = model;
                match next.consumer_step {
                    // queued.store(false, SeqCst)
                    0 => next.queued = false,
                    // pending.load(SeqCst)
                    1 => next.consumer_observed_pending = next.pending,
                    // publish_process_itimer_work() after a positive recheck
                    2 if next.consumer_observed_pending && !next.queued => next.queued = true,
                    2 => {}
                    _ => unreachable!(),
                }
                next.consumer_step += 1;
                explore(next, completed);
            }
        }

        let mut completed = 0;
        explore(
            Model {
                // The worker already popped the old queue node and swapped its
                // old pending bits. A concurrent producer now races the final
                // queued=false / pending recheck handoff.
                pending: false,
                queued: true,
                consumer_observed_pending: false,
                producer_step: 0,
                consumer_step: 0,
            },
            &mut completed,
        );
        assert_eq!(completed, 10);
    }

    #[test]
    fn per_cpu_owner_queues_start_with_isolated_stub_and_tail_pairs() {
        ensure_process_itimer_work_queues();
        let cpu_count = PROCESS_ITIMER_CPU_COUNT.min(2);
        for cpu in 0..cpu_count {
            let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[cpu]).cast_mut();
            assert_eq!(PROCESS_ITIMER_WORK_TAILS[cpu].load(Ordering::Acquire), stub);
            assert!(!has_deferred_process_itimer_work_on_cpu(cpu));
            assert!(
                PROCESS_ITIMER_WORK_STUBS[cpu]
                    .next
                    .load(Ordering::Acquire)
                    .is_null()
            );
            assert!(
                PROCESS_ITIMER_WORK_STUBS[cpu]
                    .owner
                    .load(Ordering::Acquire)
                    .is_null()
            );
        }
        if let Some((first, rest)) = PROCESS_ITIMER_WORK_STUBS.split_first()
            && let Some(second) = rest.first()
        {
            assert_ne!(ptr::from_ref(first), ptr::from_ref(second));
        }
    }

    #[test]
    fn process_timer_policy_drain_is_bounded_to_one_fixed_batch() {
        assert_eq!(PROCESS_ITIMER_WORK_BATCH, 16);
    }

    #[test]
    fn consumer_owner_token_handoff_is_exclusive() {
        ensure_process_itimer_consumers();
        assert!(acquire_process_itimer_worker_consumer(0));
        assert!(!acquire_process_itimer_fallback_consumer(0));
        // The ownership token protects a fixed cursor, not a consumer value
        // rebuilt from the stub. Preserve the exact cursor address/state
        // across the worker -> fallback handoff.
        let worker_head = unsafe { process_itimer_consumer_mut(0).head };
        release_process_itimer_worker_consumer(0);
        assert!(acquire_process_itimer_fallback_consumer(0));
        let fallback_head = unsafe { process_itimer_consumer_mut(0).head };
        assert_eq!(fallback_head, worker_head);
        release_process_itimer_fallback_consumer(0);
    }

    #[test]
    fn queued_owner_token_keeps_wake_target_across_producer_migration() {
        // CPU 2 owns the queued node (the public token is CPU + 1). The
        // producer may migrate before its caller performs the wake, but the
        // readback remains the queue owner's exact CPU rather than the
        // producer's current CPU. Use a synthetic topology because host
        // tests configure only one production CPU.
        const SYNTHETIC_CPU_COUNT: usize = 4;
        let owner_token = core::sync::atomic::AtomicUsize::new(2 + 1);
        let wake_cpu = process_itimer_owner_from_token_for_cpu_count(
            owner_token.load(Ordering::Acquire),
            SYNTHETIC_CPU_COUNT,
        );
        let producer_cpu_after_migration = 0;
        assert_eq!(wake_cpu, Some(2));
        assert_ne!(producer_cpu_after_migration, wake_cpu.unwrap());
        owner_token.store(0, Ordering::Release);
        assert_eq!(
            process_itimer_owner_from_token(owner_token.load(Ordering::Acquire)),
            None
        );
        assert_eq!(
            process_itimer_owner_from_token(PROCESS_ITIMER_CPU_COUNT + 1),
            None
        );
    }

    #[derive(Debug)]
    struct QueueCursorModel {
        next: Vec<Option<usize>>,
        owner: Vec<bool>,
        head: usize,
        tail: usize,
    }

    impl QueueCursorModel {
        const STUB: usize = 0;

        fn new(node_count: usize) -> Self {
            Self {
                next: vec![None; node_count + 1],
                owner: vec![false; node_count + 1],
                head: Self::STUB,
                tail: Self::STUB,
            }
        }

        fn publish(&mut self, node: usize) {
            assert!(!self.owner[node]);
            self.owner[node] = true;
            self.next[node] = None;
            let previous = self.tail;
            self.tail = node;
            self.next[previous] = Some(node);
        }

        fn pop(&mut self) -> Option<usize> {
            let mut head = self.head;
            let mut next = self.next[head];
            if head == Self::STUB {
                let node = next?;
                self.head = node;
                head = node;
                next = self.next[head];
            }
            if next.is_none() {
                if head != self.tail {
                    // Producer tail-link gap: preserve the exact cursor.
                    return None;
                }
                self.next[Self::STUB] = None;
                let previous = self.tail;
                self.tail = Self::STUB;
                self.next[previous] = Some(Self::STUB);
                next = self.next[head];
                next?;
            }
            self.head = next.expect("linked queue node must have a successor");
            assert!(self.owner[head]);
            self.owner[head] = false;
            Some(head)
        }

        fn drain_batch(&mut self) -> usize {
            let mut drained = 0;
            while drained < PROCESS_ITIMER_WORK_BATCH {
                if self.pop().is_none() {
                    break;
                }
                drained += 1;
            }
            drained
        }

        fn raw_work_pending(&self) -> bool {
            self.tail != Self::STUB || self.next[Self::STUB].is_some()
        }

        fn all_released(&self) -> bool {
            self.owner.iter().skip(1).all(|owner| !owner)
                && self.head == Self::STUB
                && self.tail == Self::STUB
        }
    }

    #[test]
    fn persistent_cursor_drains_more_than_two_batches_after_failure() {
        let mut queue = QueueCursorModel::new(40);
        for node in 1..=40 {
            queue.publish(node);
        }
        let mut consumer_owner = PROCESS_ITIMER_CONSUMER_WORKER;
        assert_eq!(consumer_owner, PROCESS_ITIMER_CONSUMER_WORKER);
        assert_eq!(queue.drain_batch(), 16);
        assert_ne!(queue.head, QueueCursorModel::STUB);
        // A failed worker hands this exact non-stub cursor to fallback;
        // rebuilding a fresh consumer from STUB here would revisit the first
        // batch whose owners have already been cleared.
        consumer_owner = PROCESS_ITIMER_CONSUMER_FALLBACK;
        assert_eq!(consumer_owner, PROCESS_ITIMER_CONSUMER_FALLBACK);
        assert_eq!(queue.drain_batch(), 16);
        assert_eq!(queue.drain_batch(), 8);
        assert_eq!(queue.drain_batch(), 0);
        assert!(queue.all_released());
    }

    #[test]
    fn failed_consumer_keeps_nonstub_cursor_during_producer_link_gap() {
        let mut queue = QueueCursorModel::new(2);
        queue.publish(1);
        // Consumer detached node 1; producer has swapped node 2 into tail but
        // has not yet published node 1's successor.
        queue.head = 1;
        queue.next[1] = None;
        queue.owner[2] = true;
        queue.tail = 2;
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.head, 1);
        // Failure releases only ownership, so fallback resumes this cursor.
        queue.next[1] = Some(2);
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
        assert!(queue.all_released());
    }

    #[test]
    fn failed_worker_scans_cursor_after_stub_tail_masks_late_link() {
        let mut queue = QueueCursorModel::new(2);
        queue.publish(1);
        // The worker advanced to node 1 while a producer swapped node 2 but
        // had not linked node 1 -> node 2 yet.
        queue.head = 1;
        queue.next[1] = None;
        queue.owner[2] = true;
        queue.tail = 2;
        assert!(queue.raw_work_pending());

        // The failed worker's last consumer step inserts the permanent stub
        // after observing the producer tail. The producer then completes the
        // old predecessor link. The raw tail/stub predicate is now empty even
        // though the persistent cursor still has both nodes to consume.
        queue.next[QueueCursorModel::STUB] = None;
        queue.tail = QueueCursorModel::STUB;
        queue.next[2] = Some(QueueCursorModel::STUB);
        queue.next[1] = Some(2);
        assert!(!queue.raw_work_pending());

        // A failed-worker fallback must scan the cursor/generation latch,
        // rather than trust that false predicate.
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
        assert!(queue.all_released());
    }

    #[derive(Debug, Default)]
    struct DeliveryOwnershipModel {
        owner: bool,
        queued: bool,
        pending: bool,
        delivered: Vec<u8>,
    }

    impl DeliveryOwnershipModel {
        fn publish_initial(&mut self) {
            assert!(!self.owner);
            self.owner = true;
            self.queued = true;
            self.pending = true;
        }

        fn begin_batch(&mut self) -> bool {
            assert!(self.owner);
            core::mem::replace(&mut self.pending, false)
        }

        fn producer_request_during_delivery(&mut self) -> bool {
            self.pending = true;
            // The live owner coalesces the request; it must not publish a
            // second delivery node while the first signal batch is in flight.
            !self.owner
        }

        fn try_begin_concurrent_fallback(&mut self) -> bool {
            if self.owner {
                return false;
            }
            self.owner = true;
            self.queued = true;
            true
        }

        fn send(&mut self, signal: u8) {
            assert!(self.owner);
            self.delivered.push(signal);
        }

        fn finish_batch_and_requeue(&mut self) {
            self.queued = false;
            self.owner = false;
            if self.pending {
                self.owner = true;
                self.queued = true;
            }
        }
    }

    #[test]
    fn delivery_owner_blocks_concurrent_fallback_until_signal_batch_finishes() {
        let mut model = DeliveryOwnershipModel::default();
        model.publish_initial();
        assert!(model.begin_batch());

        // A producer races while the worker/fallback is delivering the
        // snapshot. It records coalesced work, but cannot acquire delivery
        // ownership or reorder the signal batch.
        assert!(!model.producer_request_during_delivery());
        assert!(!model.try_begin_concurrent_fallback());
        model.send(1);
        model.send(2);
        assert_eq!(model.delivered, vec![1, 2]);

        // Only after delivery completes may the coalesced request be
        // republished and become eligible for the next consumer pass.
        model.finish_batch_and_requeue();
        assert!(model.owner);
        assert!(model.queued);
        assert!(model.begin_batch());
    }

    #[test]
    fn independent_owner_queues_do_not_cross_consume_or_retain_nodes() {
        let mut cpu0 = QueueCursorModel::new(2);
        let mut cpu1 = QueueCursorModel::new(2);
        cpu0.publish(1);
        cpu1.publish(1);
        assert_eq!(cpu0.drain_batch(), 1);
        assert!(cpu1.owner[1]);
        assert_eq!(cpu1.drain_batch(), 1);
        assert!(cpu0.all_released());
        assert!(cpu1.all_released());
    }

    #[test]
    fn process_cpu_overflow_marker_closes_the_logical_clock_domain() {
        let counter = core::sync::atomic::AtomicU64::new(u64::MAX - 1);
        let overflowed = core::sync::atomic::AtomicBool::new(false);
        add_process_cpu_nonwrapping(&counter, &overflowed, 2);
        assert!(overflowed.load(Ordering::Acquire));
        let wrapped = counter.load(Ordering::Relaxed);
        add_process_cpu_nonwrapping(&counter, &overflowed, 7);
        assert_eq!(counter.load(Ordering::Relaxed), wrapped);
    }

    #[test]
    fn rlimit_soft_transition_is_visible_and_repeats_each_cpu_second() {
        let infinity = RLIM_INFINITY as i64 as u64;
        assert_eq!(
            process_cpu_limit_transition(NANOS_PER_SEC - 1, 1, 4),
            ProcessCpuLimitTransition::None
        );
        assert_eq!(
            process_cpu_limit_transition(NANOS_PER_SEC, 1, 4),
            ProcessCpuLimitTransition::Soft { next_limit: 2 }
        );
        assert_eq!(
            process_cpu_limit_transition(2 * NANOS_PER_SEC, 2, 4),
            ProcessCpuLimitTransition::Soft { next_limit: 3 }
        );
        assert_eq!(
            process_cpu_limit_transition(u64::MAX, infinity, infinity),
            ProcessCpuLimitTransition::None
        );
    }

    #[test]
    fn rlimit_hard_transition_has_priority_and_large_soft_steps_stay_bounded() {
        assert_eq!(
            process_cpu_limit_transition(NANOS_PER_SEC, 1, 1),
            ProcessCpuLimitTransition::Hard
        );
        assert_eq!(
            process_cpu_limit_transition(3 * NANOS_PER_SEC, 1, 4),
            ProcessCpuLimitTransition::Soft { next_limit: 2 }
        );
        assert_eq!(
            process_cpu_limit_transition(0, 0, 2),
            ProcessCpuLimitTransition::Soft { next_limit: 1 }
        );
        assert_eq!(
            process_cpu_limit_transition(0, 0, 0),
            ProcessCpuLimitTransition::Hard
        );
    }
}

enum ProcessITimerSetAttemptError {
    NeedAlarmToken,
    Kernel(AxError),
}

struct ProcessITimerSetOutcome {
    old: (TimeValue, TimeValue),
    publication: AlarmPublication,
    cpu_epoch: Option<u64>,
}

struct ProcessITimerFireOutcome {
    publication: AlarmPublication,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct ProcessITimerSignals {
    virtual_expired: bool,
    prof_expired: bool,
}

pub(crate) fn get_process_itimer(
    proc_data: &ProcessData,
    ty: ITimerType,
) -> (TimeValue, TimeValue) {
    proc_data.process_itimers.lock().get(proc_data, ty)
}

pub(crate) fn set_process_itimer(
    proc_data: &Arc<ProcessData>,
    ty: ITimerType,
    interval_ns: usize,
    remaining_ns: usize,
) -> AxResult<((TimeValue, TimeValue), Option<u64>)> {
    let mut admitted_alarm = None;
    loop {
        let attempt = proc_data.process_itimers.lock().try_set(
            proc_data,
            ty,
            interval_ns,
            remaining_ns,
            &mut admitted_alarm,
        );
        match attempt {
            Ok(outcome) => {
                outcome.publication.publish();
                drop(admitted_alarm);
                return Ok((outcome.old, outcome.cpu_epoch));
            }
            Err(ProcessITimerSetAttemptError::NeedAlarmToken) => {
                debug_assert!(admitted_alarm.is_none());
                admitted_alarm = Some(AlarmToken::try_new().map_err(|error| match error {
                    AlarmTokenReserveError::CapacityExhausted => AxError::WouldBlock,
                    AlarmTokenReserveError::TokenSpaceExhausted => AxError::OutOfRange,
                })?);
            }
            Err(ProcessITimerSetAttemptError::Kernel(error)) => {
                drop(admitted_alarm);
                return Err(error);
            }
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ProcessCpuPolicyEvaluation {
    signals: u8,
    posix_deliveries: Vec<(usize, PosixTimerNotify, TimerSignalDelivery)>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ProcessCpuLimitTransition {
    None,
    Soft { next_limit: u64 },
    Hard,
}

fn process_cpu_limit_transition(
    total_ns: u64,
    soft_limit: u64,
    hard_limit: u64,
) -> ProcessCpuLimitTransition {
    if hard_limit != RLIM_INFINITY as i64 as u64 && total_ns >= cpu_limit_threshold_ns(hard_limit) {
        return ProcessCpuLimitTransition::Hard;
    }
    if soft_limit == RLIM_INFINITY as i64 as u64 || total_ns < cpu_limit_threshold_ns(soft_limit) {
        return ProcessCpuLimitTransition::None;
    }

    let next_limit = soft_limit
        .checked_add(1)
        .unwrap_or(RLIM_INFINITY as i64 as u64)
        .min(hard_limit);
    ProcessCpuLimitTransition::Soft { next_limit }
}

fn evaluate_process_cpu_policy(proc_data: &Arc<ProcessData>) -> ProcessCpuPolicyEvaluation {
    let timer_armed = proc_data.process_itimer_cpu_armed.load(Ordering::Acquire) != 0;
    let rlimit_active = proc_data.process_rlimit_cpu_active.load(Ordering::Acquire);
    if !timer_armed && !rlimit_active {
        return ProcessCpuPolicyEvaluation::default();
    }

    let usage = process_cpu_usage(proc_data);
    let (signals, posix_deliveries) = if timer_armed {
        let mut timers = proc_data.process_itimers.lock();
        let signals = timers.evaluate_cpu(usage);
        let interval_armed = timers.cpu_armed_mask();
        drop(timers);
        let mut posix_deliveries = Vec::new();
        let mut posix_cpu_armed = false;
        {
            let mut timers = proc_data.posix_timers.lock();
            for (timerid, timer) in timers.iter_mut().enumerate() {
                let Some(timer) = timer.as_mut().filter(|timer| timer.is_published()) else {
                    continue;
                };
                if !timer.is_cpu_clock() {
                    continue;
                }
                if let Some(delivery) = timer.evaluate_cpu(proc_data) {
                    posix_deliveries.push((timerid, timer.notify, delivery));
                }
                posix_cpu_armed |= timer.cpu_deadline_ns.is_some();
            }
        }
        // Publish the fast-path mask while still holding the state owner lock.
        // The POSIX timer owner is separately serialized; both masks describe
        // state sampled during this worker pass, and the next mutation queues
        // a fresh evaluation after publication.
        proc_data.process_itimer_cpu_armed.store(
            interval_armed
                | if posix_cpu_armed {
                    PROCESS_POSIX_CPU_ARMED
                } else {
                    0
                },
            Ordering::Release,
        );
        (signals, posix_deliveries)
    } else {
        (ProcessITimerSignals::default(), Vec::new())
    };

    let mut pending = 0;
    if signals.virtual_expired {
        pending |= PROCESS_ITIMER_VIRTUAL_PENDING;
    }
    if signals.prof_expired {
        pending |= PROCESS_ITIMER_PROF_PENDING;
    }

    if rlimit_active {
        let mut limits = proc_data.rlim.write();
        let limit = &mut limits[RLIMIT_CPU];
        let soft_limit = limit.current;
        let hard_limit = limit.max;
        match process_cpu_limit_transition(usage.total_ns, soft_limit, hard_limit) {
            ProcessCpuLimitTransition::Hard => {
                // SIGKILL is a terminal process-directed publication. Disable
                // further evaluations before releasing the canonical rlimit
                // owner so concurrent accounting boundaries cannot duplicate its
                // ownership.
                proc_data
                    .process_rlimit_cpu_active
                    .store(false, Ordering::Release);
                pending |= PROCESS_RLIMIT_CPU_HARD_PENDING;
            }
            ProcessCpuLimitTransition::None => {
                if soft_limit == RLIM_INFINITY as i64 as u64 {
                    proc_data
                        .process_rlimit_cpu_active
                        .store(false, Ordering::Release);
                }
            }
            ProcessCpuLimitTransition::Soft { next_limit } => {
                // Linux exposes this transition through getrlimit(2): after each
                // soft crossing, rlim_cur itself advances by one CPU second and
                // remains the single canonical threshold for the next crossing.
                limit.current = next_limit;
                pending |= PROCESS_RLIMIT_CPU_SOFT_PENDING;
            }
        }
    }

    ProcessCpuPolicyEvaluation {
        signals: pending,
        posix_deliveries,
    }
}

/// Publishes a coalescible evaluation request after CPU-clock counters have
/// advanced. The producer path is allocation-free and contains no Linux
/// signal or rlimit policy; the dedicated worker is the only evaluator.
pub(crate) fn request_process_cpu_evaluation(proc_data: &Arc<ProcessData>) -> Option<usize> {
    if proc_data.process_itimer_cpu_armed.load(Ordering::Acquire) != 0
        || proc_data.process_rlimit_cpu_active.load(Ordering::Acquire)
    {
        // Pending publication and queue ownership share the worker's SeqCst
        // handoff order; either side must observe responsibility for the last
        // request even though the state spans pending, queued, and owner-CPU
        // atomic words.
        proc_data
            .process_itimer_pending
            .fetch_or(PROCESS_CPU_EVALUATE_PENDING, Ordering::SeqCst);
        publish_process_itimer_work(proc_data)
    } else {
        process_itimer_owner_from_token(
            proc_data
                .process_itimer_work_owner_cpu
                .load(Ordering::Acquire),
        )
    }
}

fn publish_process_itimer_work(proc_data: &Arc<ProcessData>) -> Option<usize> {
    // Prevent a task-context producer from being descheduled inside the
    // two-instruction MPSC link publication. IRQ callers are already in this
    // state. No lock or allocation is used here; the owner-token CAS is a
    // bounded handoff between the producer and the current queue consumer.
    let _guard = NoPreempt::new();
    let cpu = process_itimer_owner_cpu();
    let encoded_cpu = cpu
        .checked_add(1)
        .expect("process timer owner CPU encoding overflow");
    loop {
        let owner = proc_data
            .process_itimer_work_owner_cpu
            .load(Ordering::SeqCst);
        if owner != 0 {
            // The queued token already has an exact owner. Returning that
            // token lets the caller wake the right CPU even if it migrates
            // after this function returns.
            return process_itimer_owner_from_token(owner);
        }
        if proc_data
            .process_itimer_work_owner_cpu
            .compare_exchange(0, encoded_cpu, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            break;
        }
    }
    // Keep the original SeqCst queued transition as the compact mirror used
    // by teardown/diagnostics. The owner CPU token is the deduplication and
    // wake-target state; under the handoff invariant this CAS always wins.
    if proc_data
        .process_itimer_work_queued
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // A stale mirror must not strand the owner token. Preserve the
        // publication and repair the mirror before linking the node.
        proc_data
            .process_itimer_work_queued
            .store(true, Ordering::SeqCst);
    }
    // Arc cloning is one bounded refcount operation and allocates nothing.
    // The raw strong reference is transferred to the queue and reconstructed
    // exactly once by the single consumer.
    let owner = Arc::into_raw(proc_data.clone()).cast_mut();

    ensure_process_itimer_work_queues();
    let node = &proc_data.process_itimer_work_node;
    node.owner.store(owner, Ordering::Relaxed);
    node.next.store(ptr::null_mut(), Ordering::Relaxed);
    let node = ptr::from_ref(node).cast_mut();
    let previous = PROCESS_ITIMER_WORK_TAILS[cpu].swap(node, Ordering::AcqRel);
    // Release is the publication point. The caller wakes the worker only after
    // this store, so CPU `cpu`'s consumer may return NotReady on the transient
    // tail gap without losing progress.
    unsafe { &*previous }.next.store(node, Ordering::Release);
    PROCESS_ITIMER_WORK_PUBLISHED.fetch_add(1, Ordering::Relaxed);
    Some(cpu)
}

pub(crate) fn has_deferred_process_itimer_work_on_cpu(cpu: usize) -> bool {
    debug_assert!(cpu < PROCESS_ITIMER_CPU_COUNT);
    ensure_process_itimer_work_queues();
    let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[cpu]).cast_mut();
    let tail = PROCESS_ITIMER_WORK_TAILS[cpu].load(Ordering::Acquire);
    tail != stub
        || !PROCESS_ITIMER_WORK_STUBS[cpu]
            .next
            .load(Ordering::Acquire)
            .is_null()
}

pub(crate) fn has_deferred_process_itimer_work() -> bool {
    has_deferred_process_itimer_work_on_cpu(process_itimer_owner_cpu())
}

/// Single-consumer state for one bounded FIFO process-timer MPSC ingress.
pub(crate) struct ProcessITimerWorkConsumer {
    cpu: usize,
    head: *mut ProcessITimerWorkNode,
}

impl ProcessITimerWorkConsumer {
    fn is_quiescent(&self) -> bool {
        let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[self.cpu]).cast_mut();
        self.head == stub
            && PROCESS_ITIMER_WORK_TAILS[self.cpu].load(Ordering::Acquire) == stub
            && unsafe { &*self.head }
                .next
                .load(Ordering::Acquire)
                .is_null()
    }

    pub(crate) fn has_pending(&self) -> bool {
        let tail = PROCESS_ITIMER_WORK_TAILS[self.cpu].load(Ordering::Acquire);
        // SAFETY: `head` is either the permanent stub or a process node whose
        // queue-owned Arc remains retained until pop advances past it.
        let next = unsafe { &*self.head }.next.load(Ordering::Acquire);
        if !next.is_null() {
            return true;
        }
        let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[self.cpu]).cast_mut();
        // `head != tail && next == null` is the bounded producer link gap.
        // Report NotReady and rely on the producer's post-link wake; the
        // register-then-check worker wait closes the wake-before-sleep race.
        if self.head != stub && self.head == tail {
            // The consumer is parked on the final node before it links the
            // permanent stub back into the FIFO. This is local cleanup, not a
            // producer tail-link gap. Count the cleanup only when `pop`
            // actually inserts the stub; `has_pending` can observe a stale
            // tail snapshot while a producer is already linking a new node.
            true
        } else {
            false
        }
    }

    fn push_stub(&self) -> *mut ProcessITimerWorkNode {
        let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[self.cpu]).cast_mut();
        PROCESS_ITIMER_WORK_STUBS[self.cpu]
            .next
            .store(ptr::null_mut(), Ordering::Relaxed);
        let previous = PROCESS_ITIMER_WORK_TAILS[self.cpu].swap(stub, Ordering::AcqRel);
        // SAFETY: the single consumer owns `self.head`. If a producer wins
        // the tail swap concurrently, `previous` is that producer's node and
        // remains queue-owned until the producer links it; linking the stub
        // here preserves the same intrusive-chain lifetime rule.
        unsafe { &*previous }.next.store(stub, Ordering::Release);
        previous
    }

    fn pop(&mut self) -> Option<Arc<ProcessData>> {
        let stub = ptr::from_ref(&PROCESS_ITIMER_WORK_STUBS[self.cpu]).cast_mut();
        let mut head = self.head;
        // SAFETY: see `has_pending`; the single consumer owns `head` updates.
        let mut next = unsafe { &*head }.next.load(Ordering::Acquire);
        if head == stub {
            if next.is_null() {
                return None;
            }
            self.head = next;
            head = next;
            next = unsafe { &*head }.next.load(Ordering::Acquire);
        }

        if next.is_null() {
            if head != PROCESS_ITIMER_WORK_TAILS[self.cpu].load(Ordering::Acquire) {
                // A producer has swapped the tail but has not yet linked its
                // predecessor. It will publish next with Release and wake us.
                PROCESS_ITIMER_WORK_PRODUCER_LINK_GAPS.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            // The tail check above is only a snapshot. A producer may swap a
            // new node between that load and the stub insertion. Returning
            // the predecessor lets us classify that race as a producer link
            // gap rather than charging it to final-node cleanup. This keeps
            // the diagnostics meaningful without changing the queue's
            // lock-free progress rule.
            let previous = self.push_stub();
            if previous == head {
                PROCESS_ITIMER_WORK_LAST_NODE_CLEANUPS.fetch_add(1, Ordering::Relaxed);
            } else {
                PROCESS_ITIMER_WORK_PRODUCER_LINK_GAPS.fetch_add(1, Ordering::Relaxed);
            }
            next = unsafe { &*head }.next.load(Ordering::Acquire);
            if next.is_null() {
                return None;
            }
        }

        self.head = next;
        let owner = unsafe { &*head }
            .owner
            .swap(ptr::null_mut(), Ordering::AcqRel);
        assert!(!owner.is_null(), "process timer queue node lost its owner");
        // SAFETY: publish transferred exactly one strong count into `owner`,
        // and advancing `head` has now fully unlinked this reusable node.
        Some(unsafe { Arc::from_raw(owner) })
    }

    pub(crate) fn drain_batch(&mut self) -> usize {
        let mut drained = 0;
        while drained < PROCESS_ITIMER_WORK_BATCH {
            let Some(proc_data) = self.pop() else {
                break;
            };
            let mut pending = proc_data.process_itimer_pending.swap(0, Ordering::SeqCst);
            let mut posix_deliveries = Vec::new();
            if pending & PROCESS_CPU_EVALUATE_PENDING != 0 {
                let evaluation = evaluate_process_cpu_policy(&proc_data);
                pending = (pending & !PROCESS_CPU_EVALUATE_PENDING) | evaluation.signals;
                posix_deliveries = evaluation.posix_deliveries;
            }
            if pending & PROCESS_RLIMIT_CPU_HARD_PENDING != 0 {
                // A terminal hard crossing owns this worker pass. Do not
                // publish a soft signal prepared from the same aggregate
                // snapshot.
                pending &= !PROCESS_RLIMIT_CPU_SOFT_PENDING;
            }
            if pending & PROCESS_RLIMIT_CPU_HARD_PENDING != 0 {
                // Publish the terminal process-directed signal before any
                // catchable timer signal from the same aggregate snapshot.
                let _ = send_signal_to_process_data(
                    &proc_data,
                    Some(SignalInfo::new_kernel(Signo::SIGKILL)),
                );
            }
            if pending & PROCESS_ITIMER_VIRTUAL_PENDING != 0 {
                let _ = send_signal_to_process_data(
                    &proc_data,
                    Some(SignalInfo::new_kernel(ITimerType::Virtual.signo())),
                );
            }
            if pending & PROCESS_ITIMER_PROF_PENDING != 0 {
                let _ = send_signal_to_process_data(
                    &proc_data,
                    Some(SignalInfo::new_kernel(ITimerType::Prof.signo())),
                );
            }
            if pending & PROCESS_RLIMIT_CPU_SOFT_PENDING != 0 {
                let _ = send_signal_to_process_data(
                    &proc_data,
                    Some(SignalInfo::new_kernel(Signo::SIGXCPU)),
                );
            }
            for (timerid, notify, delivery) in posix_deliveries {
                deliver_posix_timer_signal(
                    &proc_data,
                    timerid,
                    notify,
                    delivery,
                    POSIX_TIMER_RETRY_INITIAL,
                );
            }
            debug_assert_eq!(pending & !PROCESS_CPU_POLICY_PENDING_MASK, 0);

            // Keep the ProcessData delivery owner until every signal derived
            // from this snapshot has been sent. A producer may set pending
            // while delivery is in progress, but it observes the owner token
            // and coalesces into the post-delivery requeue instead of
            // allowing another worker/fallback to deliver this process out of
            // order.
            proc_data
                .process_itimer_work_queued
                .store(false, Ordering::SeqCst);
            proc_data
                .process_itimer_work_owner_cpu
                .store(0, Ordering::SeqCst);
            // These words form one linearized handoff with the producer's
            // SeqCst pending publication, queued mirror, and owner-CPU token.
            // Either this recheck republishes the process, or the producer
            // observes the cleared owner and owns publication itself; the
            // last request cannot be stranded.
            if proc_data.process_itimer_pending.load(Ordering::SeqCst) != 0 {
                // A fallback may be draining a failed queue from a different
                // CPU. Re-publication can therefore select a new queue; wake
                // the exact token returned by that publication instead of
                // assuming the current consumer remains runnable.
                if let Some(cpu) = publish_process_itimer_work(&proc_data) {
                    crate::deferred_work::wake_process_timer_worker(cpu);
                }
            }
            PROCESS_ITIMER_WORK_DRAINED.fetch_add(1, Ordering::Relaxed);
            drained += 1;
        }
        drained
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct ProcessITimerWorkStats {
    pub published: usize,
    pub drained: usize,
    /// Number of times the consumer inserted the permanent stub after
    /// confirming that its current head was the queue tail. This excludes
    /// producer tail/link races observed during the same transition.
    pub last_node_cleanups: usize,
    /// Number of observations in which a producer had swapped the tail but
    /// had not yet linked the predecessor's `next` pointer. This includes a
    /// race discovered while inserting the permanent stub.
    pub producer_link_gaps: usize,
}

pub(crate) fn process_itimer_work_stats() -> ProcessITimerWorkStats {
    ProcessITimerWorkStats {
        published: PROCESS_ITIMER_WORK_PUBLISHED.load(Ordering::Relaxed),
        drained: PROCESS_ITIMER_WORK_DRAINED.load(Ordering::Relaxed),
        last_node_cleanups: PROCESS_ITIMER_WORK_LAST_NODE_CLEANUPS.load(Ordering::Relaxed),
        producer_link_gaps: PROCESS_ITIMER_WORK_PRODUCER_LINK_GAPS.load(Ordering::Relaxed),
    }
}

fn fire_process_itimer_real(proc_data: Arc<ProcessData>, alarm_owner: AlarmSlotKey, sequence: u64) {
    let Some(outcome) =
        proc_data
            .process_itimers
            .lock()
            .prepare_real_fire(&proc_data, alarm_owner, sequence)
    else {
        return;
    };
    outcome.publication.publish();
    let _ = send_signal_to_process_data(&proc_data, Some(SignalInfo::new_kernel(Signo::SIGALRM)));
}

fn cpu_limit_threshold_ns(secs: u64) -> u64 {
    secs.saturating_mul(NANOS_PER_SEC)
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
    state: TimerState,
    paused_state: TimerState,
    virtual_epoch: u64,
    prof_epoch: u64,
}

impl TimeManager {
    pub(crate) fn new(proc_data: &ProcessData) -> Self {
        Self {
            utime_ns: 0,
            stime_ns: 0,
            last_cpu_ns: 0,
            state: TimerState::None,
            paused_state: TimerState::None,
            virtual_epoch: proc_data
                .process_itimer_virtual_epoch
                .load(Ordering::Acquire),
            prof_epoch: proc_data.process_itimer_prof_epoch.load(Ordering::Acquire),
        }
    }

    /// Returns the current user time and system time as a tuple of `TimeValue`.
    pub fn output(&self) -> (TimeValue, TimeValue) {
        let utime = time_value_from_nanos(self.utime_ns);
        let stime = time_value_from_nanos(self.stime_ns);
        (utime, stime)
    }

    fn account_and_publish(&mut self, proc_data: &ProcessData) {
        let now_ns = monotonic_time_nanos() as usize;
        let cpu_delta = now_ns.saturating_sub(self.last_cpu_ns);
        let charge = match self.state {
            TimerState::User => {
                self.utime_ns = self.utime_ns.saturating_add(cpu_delta);
                ProcessTimerCharge {
                    user_ns: cpu_delta,
                    system_ns: 0,
                }
            }
            TimerState::Kernel => {
                self.stime_ns = self.stime_ns.saturating_add(cpu_delta);
                ProcessTimerCharge {
                    user_ns: 0,
                    system_ns: cpu_delta,
                }
            }
            TimerState::None => ProcessTimerCharge::default(),
        };
        self.last_cpu_ns = now_ns;
        account_process_cpu(
            proc_data,
            charge,
            &mut self.virtual_epoch,
            &mut self.prof_epoch,
        );
    }

    /// Accounts the current interval and publishes it to the process clocks.
    pub(crate) fn poll(&mut self, proc_data: &ProcessData) {
        self.account_and_publish(proc_data);
    }

    /// Accounts the interrupted current task from the periodic timer IRQ.
    /// The caller may publish only the bounded worker wake after this returns;
    /// Linux timer/rlimit policy remains in the dedicated task-context worker.
    pub(crate) fn poll_timer_tick(&mut self, proc_data: &ProcessData) {
        self.account_and_publish(proc_data);
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.last_cpu_ns = monotonic_time_nanos() as usize;
        self.state = state;
    }

    /// Pauses CPU-time accounting while this thread is not running.
    pub(crate) fn pause_for_switch(&mut self, proc_data: &ProcessData) {
        self.poll(proc_data);
        self.paused_state = self.state;
        self.set_state(TimerState::None);
    }

    /// Resumes the CPU-time accounting state that was active before switch-out
    /// and adopts generations that became stable while this task slept.
    pub fn resume_after_switch(&mut self, proc_data: &ProcessData) {
        let armed = proc_data.process_itimer_cpu_armed.load(Ordering::Acquire);
        if armed & PROCESS_ITIMER_VIRTUAL_PENDING != 0 {
            let epoch = proc_data
                .process_itimer_virtual_epoch
                .load(Ordering::SeqCst);
            if epoch & 1 == 0 {
                self.virtual_epoch = self.virtual_epoch.max(epoch);
            }
        }
        if armed & PROCESS_ITIMER_PROF_PENDING != 0 {
            let epoch = proc_data.process_itimer_prof_epoch.load(Ordering::SeqCst);
            if epoch & 1 == 0 {
                self.prof_epoch = self.prof_epoch.max(epoch);
            }
        }
        let state = self.paused_state;
        self.paused_state = TimerState::None;
        self.set_state(state);
    }

    pub(crate) fn sync_process_cpu_timer_epoch(&mut self, ty: ITimerType, epoch: u64) {
        debug_assert_eq!(epoch & 1, 0);
        match ty {
            ITimerType::Real => {}
            ITimerType::Virtual => self.virtual_epoch = self.virtual_epoch.max(epoch),
            ITimerType::Prof => self.prof_epoch = self.prof_epoch.max(epoch),
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
            // One scheduling turn owns at most one fixed dispatch batch. A
            // sub-tick periodic timer must not monopolize this worker merely
            // because it is due again by the next clock sample.
            axtask::yield_now();
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

fn alarm_event(clock: AlarmClock) -> &'static Event {
    match clock {
        AlarmClock::Realtime => &REALTIME_ALARM_EVENT,
        AlarmClock::Monotonic => &MONOTONIC_ALARM_EVENT,
    }
}

/// Re-evaluates every realtime wait after a discontinuous wall-clock update.
/// Global alarm objects use the event worker; synchronous clock/futex waits
/// live in per-CPU shards and are retriggered on their owning CPUs.
pub(crate) fn notify_realtime_clock_change() {
    alarm_event(AlarmClock::Realtime).notify(1);
    axruntime::retrigger_timer_events_all();
}

fn timer_runtime(clock: AlarmClock, owner_cpu: usize) -> &'static SpinNoIrq<ClockTimerRuntime> {
    match clock {
        AlarmClock::Realtime => &REALTIME_TIMER_RUNTIMES[owner_cpu],
        AlarmClock::Monotonic => &MONOTONIC_TIMER_RUNTIMES[owner_cpu],
    }
}

fn map_timer_callback_register_error(error: TimerCallbackRegisterError) -> AxError {
    match error {
        TimerCallbackRegisterError::NoMemory => AxError::NoMemory,
        TimerCallbackRegisterError::CapacityExhausted => AxError::ResourceBusy,
        TimerCallbackRegisterError::TokenSpaceExhausted => AxError::OutOfRange,
    }
}

fn map_clock_sleep_admission_error(admission: ClockTimerAdmission, error: AxError) -> AxError {
    if admission == ClockTimerAdmission::General && error == AxError::ResourceBusy {
        // A bounded user-facing wait registry being temporarily full is an
        // admission boundary, not object contention. The Linux adapter maps
        // WouldBlock to EAGAIN so callers may back off explicitly.
        AxError::WouldBlock
    } else {
        error
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
    let _cpu_guard = NoPreempt::new();
    let cpu_id = axhal::percpu::this_cpu_id();
    ensure_clock_timer_runtime_on_cpu(cpu_id)
}

/// Ensures the callback for `cpu_id` while the caller is pinned to that CPU.
fn ensure_clock_timer_runtime_on_cpu(cpu_id: usize) -> AxResult<()> {
    debug_assert_eq!(cpu_id, axhal::percpu::this_cpu_id());
    let owner = &CLOCK_TIMER_CALLBACK_TOKENS[cpu_id];
    if owner.lock().is_some() {
        return Ok(());
    }

    let token = match register_timer_callback(move |_| {
        // Timer callback registries are per CPU; retaining the admitted owner
        // explicitly avoids accidentally draining another CPU's wait shard.
        wake_clock_timers(AlarmClock::Realtime, cpu_id);
        wake_clock_timers(AlarmClock::Monotonic, cpu_id);
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
        let user_capacity_error = map_clock_sleep_admission_error(
            ClockTimerAdmission::General,
            map_timer_callback_register_error(TimerCallbackRegisterError::CapacityExhausted),
        );
        assert_eq!(user_capacity_error, AxError::WouldBlock);
        assert_eq!(LinuxError::from(user_capacity_error), LinuxError::EAGAIN);
        assert_eq!(
            map_clock_sleep_admission_error(ClockTimerAdmission::System, AxError::ResourceBusy,),
            AxError::ResourceBusy
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
    fn per_cpu_clock_shards_isolate_capacity_and_route_refunds() {
        let mut shards = [ClockTimerRuntime::new(), ClockTimerRuntime::new()];
        let mut first_cpu_zero = None;
        for index in 0..CLOCK_TIMER_GENERAL_CAPACITY {
            let key = shards[0]
                .reserve(Duration::ZERO, Duration::from_secs(index as u64 + 1))
                .unwrap()
                .unwrap();
            first_cpu_zero.get_or_insert(key);
        }
        assert_eq!(
            shards[0].reserve(Duration::ZERO, Duration::MAX),
            Err(AxError::ResourceBusy)
        );
        let cpu_zero_system = shards[0]
            .reserve_system(Duration::ZERO, Duration::MAX)
            .unwrap()
            .unwrap();
        let cpu_one_system = shards[1]
            .reserve_system(Duration::ZERO, Duration::MAX)
            .unwrap()
            .unwrap();

        let cpu_one_key = shards[1]
            .reserve(Duration::ZERO, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let cpu_one_token = ClockTimerToken {
            owner_cpu: 1,
            key: cpu_one_key,
        };
        assert_eq!(shards[0].len, CLOCK_TIMER_CAPACITY);
        assert_eq!(shards[1].len, CLOCK_TIMER_SYSTEM_RESERVE + 1);
        assert!(shards[0].is_live(cpu_zero_system));
        assert!(shards[1].is_live(cpu_one_system));

        // A migrated owner routes cancellation by the retained CPU ID. The
        // unrelated full shard remains untouched.
        assert!(
            shards[cpu_one_token.owner_cpu]
                .cancel(cpu_one_token.key)
                .is_none()
        );
        assert_eq!(shards[0].len, CLOCK_TIMER_CAPACITY);
        assert_eq!(shards[1].len, CLOCK_TIMER_SYSTEM_RESERVE);

        let cpu_one_replacement = shards[1]
            .reserve(Duration::ZERO, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_ne!(cpu_one_replacement.generation, cpu_one_token.key.generation);
        assert!(
            shards[cpu_one_token.owner_cpu]
                .cancel(cpu_one_token.key)
                .is_none()
        );
        assert!(shards[1].is_live(cpu_one_replacement));
        assert!(shards[1].cancel(cpu_one_replacement).is_none());

        let cpu_zero_token = ClockTimerToken {
            owner_cpu: 0,
            key: first_cpu_zero.unwrap(),
        };
        assert!(
            shards[cpu_zero_token.owner_cpu]
                .cancel(cpu_zero_token.key)
                .is_none()
        );
        assert!(
            shards[0]
                .reserve(Duration::ZERO, Duration::MAX)
                .unwrap()
                .is_some()
        );
        assert_eq!(shards[0].len, CLOCK_TIMER_CAPACITY);
    }

    #[test]
    fn per_cpu_clock_shard_expiry_never_drains_a_peer() {
        let mut shards = [ClockTimerRuntime::new(), ClockTimerRuntime::new()];
        let cpu_zero = shards[0]
            .reserve(Duration::ZERO, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let cpu_one = shards[1]
            .reserve(Duration::ZERO, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let mut pending = [const { None }; CLOCK_TIMER_WAKE_BATCH];

        assert_eq!(
            shards[1].drain_expired_batch(Duration::from_secs(1), &mut pending),
            1
        );
        assert!(shards[0].is_live(cpu_zero));
        assert!(!shards[1].is_live(cpu_one));
        assert_eq!(shards[0].len, 1);
        assert_eq!(shards[1].len, 0);
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

fn wake_clock_timers(clock: AlarmClock, owner_cpu: usize) {
    debug_assert_eq!(owner_cpu, axhal::percpu::this_cpu_id());
    let now = clock.now();
    let mut woke = false;
    for _ in 0..CLOCK_TIMER_WAKE_BATCHES {
        let mut pending = [const { None }; CLOCK_TIMER_WAKE_BATCH];
        let count = timer_runtime(clock, owner_cpu)
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
    update_clock_timer_deadline(owner_cpu);
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

/// Publishes the next deadline for the current CPU's clock shards.
fn update_clock_timer_deadline(owner_cpu: usize) {
    debug_assert_eq!(owner_cpu, axhal::percpu::this_cpu_id());
    let realtime_deadline = timer_runtime(AlarmClock::Realtime, owner_cpu)
        .lock()
        .next_deadline()
        .map(realtime_deadline_as_monotonic);
    let monotonic_deadline = timer_runtime(AlarmClock::Monotonic, owner_cpu)
        .lock()
        .next_deadline();
    let deadline = match (realtime_deadline, monotonic_deadline) {
        (Some(real), Some(mono)) => Some(real.min(mono)),
        (Some(real), None) => Some(real),
        (None, Some(mono)) => Some(mono),
        (None, None) => None,
    };
    axruntime::set_early_timer_deadline(deadline);
}

/// Prepares an alarm update for a timerfd-owned token. The caller must retain
/// the token for the complete open-file-description lifetime and publish the
/// returned side effects after releasing the timerfd state lock.
pub(crate) fn prepare_pollset_alarm(
    token: &AlarmToken,
    clock: AlarmClock,
    deadline: Duration,
    poll_set: Arc<PollSet>,
) -> AlarmPublication {
    token.prepare_arm(clock, deadline, AlarmAction::WakePollSet(poll_set))
}

fn saturating_duration_mul(duration: Duration, count: u128) -> Duration {
    let nanos = duration
        .as_nanos()
        .saturating_mul(count)
        .min(u64::MAX as u128) as u64;
    Duration::from_nanos(nanos)
}

/// Projects a CLOCK_TAI deadline onto the realtime alarm heap.  Keep the
/// conversion signed: a negative TAI offset is valid input to ADJ_TAI.
fn tai_deadline_as_realtime(deadline: Duration, offset_seconds: i64) -> Duration {
    if offset_seconds >= 0 {
        deadline.saturating_sub(Duration::from_secs(offset_seconds as u64))
    } else {
        deadline.saturating_add(Duration::from_secs(offset_seconds.unsigned_abs()))
    }
}

/// Rebase every live absolute CLOCK_TAI timer after a successful `ADJ_TAI`
/// commit.  This deliberately snapshots process owners before taking any
/// timer lock and publishes each registry change only after releasing that
/// owner lock.  A concurrent `timer_settime` either retains the new timex
/// generation itself or is observed here and replaced with the same
/// projection; the per-arm sequence rejects every stale heap dispatch.
struct TaiTimerRebaseProcess {
    proc_data: Arc<ProcessData>,
    publications: Vec<AlarmPublication>,
}

/// Fully allocated rebase work prepared before the new TAI offset becomes
/// visible.  The caller holds `TAI_TIMER_REBASE_GATE` throughout prepare and
/// apply, which prevents timer_settime from adding an armed TAI timer after a
/// process's capacity was sampled.  Timer creation may still grow a vector,
/// but cannot create an armed entry without that same gate.
pub(crate) struct TaiTimerRebasePlan {
    processes: Vec<TaiTimerRebaseProcess>,
}

pub(crate) fn prepare_tai_absolute_posix_timer_rebase() -> AxResult<TaiTimerRebasePlan> {
    let processes = try_processes()?;
    let mut planned = Vec::new();
    planned
        .try_reserve_exact(processes.len())
        .map_err(|_| AxError::NoMemory)?;
    for proc_data in processes {
        let timers = proc_data.posix_timers.lock();
        let capacity = timers.len();
        // Rebase is a semantic replacement of every armed absolute TAI
        // action, so reserve its successor sequence before TIMEX_STATE is
        // published.  Wrapping would let an ancient queued action collide
        // with a newly rearmed timer after 2^64 updates.
        if timers.iter().flatten().any(|timer| {
            timer.is_published() && timer.tai_deadline.is_some() && timer.sequence == u64::MAX
        }) {
            return Err(AxError::OutOfRange);
        }
        drop(timers);
        let mut publications = Vec::new();
        publications
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        planned.push(TaiTimerRebaseProcess {
            proc_data,
            publications,
        });
    }
    Ok(TaiTimerRebasePlan { processes: planned })
}

impl TaiTimerRebasePlan {
    /// Applies a preallocated TAI rebase after the new timex generation is
    /// published.  This cannot fail or allocate while a timer owner lock is
    /// held; stale heap actions are excluded by the new sequence value.
    pub(crate) fn apply(mut self, generation: u64, offset_seconds: i64) {
        for process in &mut self.processes {
            {
                let mut timers = process.proc_data.posix_timers.lock();
                for (timerid, timer) in timers.iter_mut().enumerate() {
                    let Some(timer) = timer.as_mut().filter(|timer| timer.is_published()) else {
                        continue;
                    };
                    let Some(deadline) =
                        timer.rebase_tai_absolute_deadline(generation, offset_seconds)
                    else {
                        continue;
                    };
                    // Preflight rejected the terminal sequence before the
                    // timex publication, so this successor is infallible and
                    // cannot collide with an earlier queued alarm action.
                    let sequence = timer
                        .sequence
                        .checked_add(1)
                        .expect("TAI rebase preflight admitted a terminal sequence");
                    timer.sequence = sequence;
                    timer.effective_clock = AlarmClock::Realtime;
                    timer.deadline = Some(deadline);
                    process.publications.push(timer.prepare_main_alarm(
                        &process.proc_data,
                        timerid,
                        AlarmClock::Realtime,
                        deadline,
                        sequence,
                    ));
                }
            }
            for publication in core::mem::take(&mut process.publications) {
                publication.publish();
            }
        }
    }
}

fn posix_timer_signal_info(
    signo: Signo,
    timerid: usize,
    overrun: i32,
    value: usize,
    token: u32,
) -> SignalInfo {
    SignalInfo::new_timer(
        signo,
        SignalTimerPayload::new(timerid as i32, overrun, value, token as i32),
    )
}

fn timer_signal_identity(sig: &SignalInfo) -> Option<(usize, u32)> {
    if sig.code() != SI_TIMER {
        return None;
    }

    let timer = sig.timer_payload();
    Some((usize::try_from(timer.tid).ok()?, timer.sys_private as u32))
}

pub(crate) fn acknowledge_posix_timer_signal(proc_data: &ProcessData, sig: &SignalInfo) {
    let Some((timerid, token)) = timer_signal_identity(sig) else {
        return;
    };

    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return;
    };
    if !timer.is_published() {
        return;
    }
    timer.acknowledge_signal_delivery(token);
}

fn fail_posix_timer_signal(
    proc_data: &Arc<ProcessData>,
    timerid: usize,
    token: u32,
    backoff: Duration,
) -> Option<AlarmPublication> {
    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return None;
    };
    if !timer.is_published() {
        return None;
    }
    timer
        .fail_signal_delivery(token)
        .then(|| timer.prepare_retry_alarm(proc_data, timerid, token, backoff))
        .flatten()
}

fn abandon_posix_timer_signal(proc_data: &ProcessData, timerid: usize, token: u32) {
    let mut timers = proc_data.posix_timers.lock();
    let Some(Some(timer)) = timers.get_mut(timerid) else {
        return;
    };
    if !timer.is_published() {
        return;
    }
    timer.abandon_signal_delivery(token);
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
            if let Some(publication) =
                fail_posix_timer_signal(proc_data, timerid, delivery.token, retry_backoff)
            {
                publication.publish();
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
    owner: AlarmSlotKey,
    token: u32,
    backoff: Duration,
) {
    let (notify, delivery) = {
        let mut timers = proc_data.posix_timers.lock();
        let Some(Some(timer)) = timers.get_mut(timerid) else {
            return;
        };
        if !timer.is_published() {
            return;
        }
        if !timer.retry_alarm_matches(owner) {
            return;
        }
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

fn fire_posix_timer(
    proc_data: Arc<ProcessData>,
    timerid: usize,
    owner: AlarmSlotKey,
    sequence: u64,
) {
    let (notify, delivery, next) = {
        let mut timers = proc_data.posix_timers.lock();
        let Some(Some(timer)) = timers.get_mut(timerid) else {
            return;
        };
        if !timer.is_published() {
            return;
        }
        if !timer.main_alarm_matches(owner) || timer.sequence != sequence {
            return;
        }
        let Some(deadline) = timer.deadline else {
            return;
        };

        let domain_deadline = timer.tai_deadline.unwrap_or(deadline);
        let now = timer.timer_now();
        if now < domain_deadline {
            return;
        }

        let expirations = if timer.interval.is_zero() {
            1_u128
        } else {
            let elapsed = now.saturating_sub(domain_deadline).as_nanos();
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
            timer.tai_deadline = None;
            None
        } else {
            let next_domain_deadline = domain_deadline
                .checked_add(saturating_duration_mul(timer.interval, expirations))
                .unwrap_or(Duration::MAX);
            let next_deadline = if timer.tai_deadline.is_some() {
                let (offset_seconds, generation) = crate::syscall::tai_offset_snapshot();
                timer.tai_deadline = Some(next_domain_deadline);
                timer.tai_offset_generation = generation;
                tai_deadline_as_realtime(next_domain_deadline, offset_seconds)
            } else {
                next_domain_deadline
            };
            timer.deadline = Some(next_deadline);
            Some(timer.prepare_main_alarm(
                &proc_data,
                timerid,
                timer.effective_clock,
                next_deadline,
                timer.sequence,
            ))
        };
        (notify, delivery, next)
    };

    if let Some(publication) = next {
        publication.publish();
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
        PosixTimer::try_new(
            PosixTimerClock::Monotonic,
            PosixTimerNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: Some(7),
            },
            None,
            None,
            None,
        )
        .unwrap()
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

        timer.reset_signal_delivery().publish();
        assert!(timer.retry_signal_delivery(first.token).is_none());
        assert!(!timer.signal_pending);
        assert!(!timer.signal_retry_pending);
    }
}

fn queue_deadline(clock: AlarmClock) -> Option<Duration> {
    ALARM_REGISTRY.lock().next_deadline(clock)
}

fn process_due(clock: AlarmClock) -> bool {
    let mut pending = [const { None }; ALARM_DISPATCH_BATCH];
    let count = ALARM_REGISTRY
        .lock()
        .take_due_batch(clock, clock.now(), &mut pending);
    for AlarmDispatch { owner, action } in pending.into_iter().flatten() {
        match action {
            AlarmAction::ProcessITimerReal { proc, sequence } => {
                if let Some(proc_data) = proc.upgrade() {
                    fire_process_itimer_real(proc_data, owner, sequence);
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
                    fire_posix_timer(proc_data, timerid, owner, sequence);
                }
            }
            AlarmAction::PosixTimerRetry {
                proc,
                timerid,
                token,
                backoff,
            } => {
                if let Some(proc_data) = proc.upgrade() {
                    retry_posix_timer_signal(proc_data, timerid, owner, token, backoff);
                }
            }
        }
    }
    count != 0
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
        return Ok(PreparedClockSleep { clock, token: None });
    }

    // Callback ownership, shard selection, reservation, and the first hardware
    // deadline publication must observe one CPU. The resulting token remains
    // remotely pollable/cancellable after this guard is released.
    let _cpu_guard = NoPreempt::new();
    let owner_cpu = axhal::percpu::this_cpu_id();
    if let Err(error) = ensure_clock_timer_runtime_on_cpu(owner_cpu) {
        // Completion wins if the deadline elapsed while callback admission was
        // attempted. Otherwise preserve the exact construction failure.
        if deadline <= clock.now() {
            return Ok(PreparedClockSleep { clock, token: None });
        }
        return Err(map_clock_sleep_admission_error(admission, error));
    }

    let now = clock.now();
    let reservation = {
        let mut runtime = timer_runtime(clock, owner_cpu).lock();
        match admission {
            ClockTimerAdmission::General => runtime.reserve(now, deadline),
            ClockTimerAdmission::System => runtime.reserve_system(now, deadline),
        }
    };
    let key = match reservation {
        Ok(key) => key,
        Err(_) if deadline <= clock.now() => None,
        Err(error) => return Err(map_clock_sleep_admission_error(admission, error)),
    };
    update_clock_timer_deadline(owner_cpu);
    Ok(PreparedClockSleep {
        clock,
        token: key.map(|key| ClockTimerToken { owner_cpu, key }),
    })
}
