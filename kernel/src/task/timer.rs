//! Time management module.

use alloc::{
    borrow::ToOwned,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::{Future, poll_fn},
    pin::Pin,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::{NANOS_PER_SEC, TimeValue, monotonic_time_nanos};
use axpoll::PollSet;
use axtask::{
    TimerCallbackRegisterError, TimerCallbackToken, cancel_timer_callback, current,
    future::{BlockOnError, block_on},
    register_timer_callback,
};
use event_listener::{Event, listener};
use kernel_guard::NoPreempt;
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use linux_raw_sys::general::{RLIM_INFINITY, RLIMIT_CPU, SI_TIMER};
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use strum::FromRepr;

use super::{
    AsThread, ProcessData, send_queued_signal_to_process_data,
    send_queued_signal_to_visible_thread, send_signal_to_process_data,
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
}

impl PosixTimerClock {
    pub(crate) fn absolute_alarm_clock(self) -> AlarmClock {
        match self {
            Self::Realtime | Self::Tai => AlarmClock::Realtime,
            Self::Monotonic => AlarmClock::Monotonic,
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

#[derive(Debug)]
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
    pub deadline: Option<Duration>,
    pub sequence: u64,
    pub overrun: i32,
    signal_pending: bool,
    signal_retry_pending: bool,
    signal_token: u32,
    main_alarm: AlarmToken,
    retry_alarm: Option<AlarmToken>,
}

impl PosixTimer {
    pub(crate) fn try_new(
        clock: PosixTimerClock,
        notify: PosixTimerNotify,
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
            deadline: None,
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
        debug_assert_eq!(heap.remove_at(heap_index, slots), slot_index);
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

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct ProcessTimerCharge {
    user_ns: usize,
    system_ns: usize,
}

const PROCESS_ITIMER_VIRTUAL_PENDING: u8 = 1 << 0;
const PROCESS_ITIMER_PROF_PENDING: u8 = 1 << 1;
const PROCESS_ITIMER_CPU_ARMED_MASK: u8 =
    PROCESS_ITIMER_VIRTUAL_PENDING | PROCESS_ITIMER_PROF_PENDING;
const PROCESS_ITIMER_WORK_BATCH: usize = 16;
static PROCESS_ITIMER_WORK_HEAD: AtomicPtr<ProcessData> = AtomicPtr::new(ptr::null_mut());

#[derive(Debug)]
struct ProcessITimer {
    interval_ns: usize,
    remaining_ns: usize,
    sequence: u64,
}

impl ProcessITimer {
    const fn new() -> Self {
        Self {
            interval_ns: 0,
            remaining_ns: 0,
            sequence: 0,
        }
    }

    fn charge_cpu(&mut self, delta_ns: usize) -> bool {
        if self.remaining_ns == 0 || delta_ns == 0 {
            return false;
        }
        if delta_ns < self.remaining_ns {
            self.remaining_ns -= delta_ns;
            return false;
        }

        if self.interval_ns == 0 {
            self.remaining_ns = 0;
        } else {
            let overshoot = (delta_ns - self.remaining_ns) % self.interval_ns;
            self.remaining_ns = if overshoot == 0 {
                self.interval_ns
            } else {
                self.interval_ns - overshoot
            };
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

    fn get(&self, ty: ITimerType) -> (TimeValue, TimeValue) {
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
            timer.remaining_ns
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
        let old = self.get(ty);
        let sequence = self.timers[index]
            .sequence
            .checked_add(1)
            .ok_or(ProcessITimerSetAttemptError::Kernel(AxError::OutOfRange))?;

        if ty == ITimerType::Real && remaining_ns != 0 && self.real_alarm.is_none() {
            let Some(alarm) = admitted_alarm.take() else {
                return Err(ProcessITimerSetAttemptError::NeedAlarmToken);
            };
            self.real_alarm = Some(alarm);
        }

        let timer = &mut self.timers[index];
        timer.interval_ns = interval_ns;
        timer.remaining_ns = remaining_ns;
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

        Ok(ProcessITimerSetOutcome { old, publication })
    }

    fn cpu_armed_mask(&self) -> u8 {
        let mut mask = 0;
        if self.timers[ITimerType::Virtual as usize].remaining_ns != 0 {
            mask |= PROCESS_ITIMER_VIRTUAL_PENDING;
        }
        if self.timers[ITimerType::Prof as usize].remaining_ns != 0 {
            mask |= PROCESS_ITIMER_PROF_PENDING;
        }
        mask
    }

    fn charge_cpu(&mut self, charge: ProcessTimerCharge) -> ProcessITimerSignals {
        ProcessITimerSignals {
            virtual_expired: self.timers[ITimerType::Virtual as usize].charge_cpu(charge.user_ns),
            prof_expired: self.timers[ITimerType::Prof as usize]
                .charge_cpu(charge.user_ns.saturating_add(charge.system_ns)),
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
    use super::*;

    fn arm_cpu_timer(
        timers: &mut ProcessITimers,
        ty: ITimerType,
        interval: usize,
        remaining: usize,
    ) {
        let timer = &mut timers.timers[ty as usize];
        timer.interval_ns = interval;
        timer.remaining_ns = remaining;
    }

    #[test]
    fn virtual_consumes_only_user_time_and_prof_consumes_both() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Virtual, 0, 5);
        arm_cpu_timer(&mut timers, ITimerType::Prof, 0, 5);

        let signals = timers.charge_cpu(ProcessTimerCharge {
            user_ns: 0,
            system_ns: 5,
        });
        assert_eq!(
            signals,
            ProcessITimerSignals {
                virtual_expired: false,
                prof_expired: true,
            }
        );
        assert_eq!(timers.timers[ITimerType::Virtual as usize].remaining_ns, 5);
        assert_eq!(timers.timers[ITimerType::Prof as usize].remaining_ns, 0);

        let signals = timers.charge_cpu(ProcessTimerCharge {
            user_ns: 5,
            system_ns: 0,
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

        let signals = timers.charge_cpu(ProcessTimerCharge {
            user_ns: 27,
            system_ns: 0,
        });
        assert!(signals.virtual_expired);
        assert!(!signals.prof_expired);
        assert_eq!(timers.timers[ITimerType::Virtual as usize].remaining_ns, 7);
        assert_eq!(timers.cpu_armed_mask(), PROCESS_ITIMER_VIRTUAL_PENDING);
    }

    #[test]
    fn cpu_timer_one_shot_disarms_after_exact_threshold() {
        let mut timers = ProcessITimers::new();
        arm_cpu_timer(&mut timers, ITimerType::Prof, 0, 8);

        assert!(
            !timers
                .charge_cpu(ProcessTimerCharge {
                    user_ns: 3,
                    system_ns: 4,
                })
                .prof_expired
        );
        assert_eq!(timers.timers[ITimerType::Prof as usize].remaining_ns, 1);
        assert!(
            timers
                .charge_cpu(ProcessTimerCharge {
                    user_ns: 0,
                    system_ns: 1,
                })
                .prof_expired
        );
        assert_eq!(timers.timers[ITimerType::Prof as usize].remaining_ns, 0);
        assert!(
            !timers
                .charge_cpu(ProcessTimerCharge {
                    user_ns: usize::MAX,
                    system_ns: usize::MAX,
                })
                .prof_expired
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
    proc_data.process_itimers.lock().get(ty)
}

pub(crate) fn set_process_itimer(
    proc_data: &Arc<ProcessData>,
    ty: ITimerType,
    interval_ns: usize,
    remaining_ns: usize,
) -> AxResult<(TimeValue, TimeValue)> {
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
                return Ok(outcome.old);
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

pub(crate) fn charge_process_itimers(
    proc_data: &Arc<ProcessData>,
    charge: ProcessTimerCharge,
) -> bool {
    if charge == ProcessTimerCharge::default()
        || proc_data.process_itimer_cpu_armed.load(Ordering::Acquire) == 0
    {
        return false;
    }
    let signals = {
        let mut timers = proc_data.process_itimers.lock();
        let signals = timers.charge_cpu(charge);
        // Publish the fast-path mask while still holding the state owner lock.
        // set_process_itimer() uses the same lock, so a later arm/disarm cannot
        // be overwritten by an older accounting snapshot after lock release.
        proc_data
            .process_itimer_cpu_armed
            .store(timers.cpu_armed_mask(), Ordering::Release);
        signals
    };
    let mut pending = 0;
    if signals.virtual_expired {
        pending |= PROCESS_ITIMER_VIRTUAL_PENDING;
    }
    if signals.prof_expired {
        pending |= PROCESS_ITIMER_PROF_PENDING;
    }
    if pending != 0 {
        proc_data
            .process_itimer_pending
            .fetch_or(pending, Ordering::Release);
        publish_process_itimer_work(proc_data);
        true
    } else {
        false
    }
}

fn publish_process_itimer_work(proc_data: &Arc<ProcessData>) {
    if proc_data
        .process_itimer_work_queued
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    let retained = proc_data.clone();
    let replaced = proc_data.process_itimer_work_owner.lock().replace(retained);
    assert!(
        replaced.is_none(),
        "process timer work owner published twice"
    );
    drop(replaced);

    let node = Arc::as_ptr(proc_data).cast_mut();
    let mut head = PROCESS_ITIMER_WORK_HEAD.load(Ordering::Acquire);
    loop {
        proc_data
            .process_itimer_work_next
            .store(head, Ordering::Relaxed);
        match PROCESS_ITIMER_WORK_HEAD.compare_exchange_weak(
            head,
            node,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

pub(crate) fn has_deferred_process_itimer_work() -> bool {
    !PROCESS_ITIMER_WORK_HEAD.load(Ordering::Acquire).is_null()
}

/// Single-consumer state for the process-timer ingress stack. Detaching and
/// reversing one producer snapshot gives FIFO service: a hot process may
/// republish only into the next ingress snapshot and therefore cannot jump in
/// front of older nodes retained in `backlog`.
pub(crate) struct ProcessITimerWorkConsumer {
    backlog: *mut ProcessData,
}

impl ProcessITimerWorkConsumer {
    pub(crate) const fn new() -> Self {
        Self {
            backlog: ptr::null_mut(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.backlog.is_null() || has_deferred_process_itimer_work()
    }

    fn refill(&mut self) {
        if !self.backlog.is_null() {
            return;
        }

        let mut ingress = PROCESS_ITIMER_WORK_HEAD.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut fifo = ptr::null_mut();
        while !ingress.is_null() {
            // SAFETY: each published node retains a self Arc and remains
            // `queued`. Producers can only prepend to the global ingress and
            // cannot republish a detached node. This dedicated consumer owns
            // every `next` link in the detached snapshot.
            let node = unsafe { &*ingress };
            let next = node.process_itimer_work_next.load(Ordering::Relaxed);
            node.process_itimer_work_next.store(fifo, Ordering::Relaxed);
            fifo = ingress;
            ingress = next;
        }
        self.backlog = fifo;
    }

    fn pop(&mut self) -> Option<Arc<ProcessData>> {
        self.refill();
        let head = self.backlog;
        if head.is_null() {
            return None;
        }

        // SAFETY: `refill` detached this FIFO snapshot from all producers.
        // The queued node's self Arc keeps it alive until the owner is taken.
        let node = unsafe { &*head };
        self.backlog = node.process_itimer_work_next.load(Ordering::Relaxed);
        node.process_itimer_work_next
            .store(ptr::null_mut(), Ordering::Relaxed);
        Some(
            node.process_itimer_work_owner
                .lock()
                .take()
                .expect("queued process timer work lost its self owner"),
        )
    }

    pub(crate) fn drain_batch(&mut self) -> usize {
        let mut drained = 0;
        while drained < PROCESS_ITIMER_WORK_BATCH {
            let Some(proc_data) = self.pop() else {
                break;
            };
            let pending = proc_data.process_itimer_pending.swap(0, Ordering::AcqRel);
            proc_data
                .process_itimer_work_queued
                .store(false, Ordering::Release);
            if proc_data.process_itimer_pending.load(Ordering::Acquire) != 0 {
                publish_process_itimer_work(&proc_data);
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
            debug_assert_eq!(pending & !PROCESS_ITIMER_CPU_ARMED_MASK, 0);
            drained += 1;
        }
        drained
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
    cpu_limit: CpuLimitState,
}

impl TimeManager {
    pub(crate) fn new() -> Self {
        Self {
            utime_ns: 0,
            stime_ns: 0,
            last_cpu_ns: 0,
            state: TimerState::None,
            paused_state: TimerState::None,
            cpu_limit: CpuLimitState::default(),
        }
    }

    /// Returns the current user time and system time as a tuple of `TimeValue`.
    pub fn output(&self) -> (TimeValue, TimeValue) {
        let utime = time_value_from_nanos(self.utime_ns);
        let stime = time_value_from_nanos(self.stime_ns);
        (utime, stime)
    }

    fn account_elapsed(&mut self) -> ProcessTimerCharge {
        let now_ns = monotonic_time_nanos() as usize;
        let cpu_delta = now_ns.saturating_sub(self.last_cpu_ns);
        let charge = match self.state {
            TimerState::User => {
                self.utime_ns += cpu_delta;
                ProcessTimerCharge {
                    user_ns: cpu_delta,
                    system_ns: 0,
                }
            }
            TimerState::Kernel => {
                self.stime_ns += cpu_delta;
                ProcessTimerCharge {
                    user_ns: 0,
                    system_ns: cpu_delta,
                }
            }
            TimerState::None => ProcessTimerCharge::default(),
        };
        self.last_cpu_ns = now_ns;
        charge
    }

    /// Polls the time manager to update the timers and emit signals if
    /// necessary.
    pub(crate) fn poll(&mut self, signals: &mut Vec<Signo>) -> ProcessTimerCharge {
        let charge = self.account_elapsed();
        self.update_rlimit_cpu(signals);
        charge
    }

    /// Accounts the interrupted current task from the periodic timer IRQ.
    /// RLIMIT policy remains at the ordinary user-return/context-switch
    /// boundary; the trap path immediately observes the updated total without
    /// allocating or publishing signals from IRQ context.
    pub(crate) fn poll_timer_tick(&mut self) -> ProcessTimerCharge {
        self.account_elapsed()
    }

    /// Updates the timer state.
    pub fn set_state(&mut self, state: TimerState) {
        self.last_cpu_ns = monotonic_time_nanos() as usize;
        self.state = state;
    }

    /// Pauses CPU-time accounting while this thread is not running.
    pub(crate) fn pause_for_switch(&mut self, signals: &mut Vec<Signo>) -> ProcessTimerCharge {
        let charge = self.poll(signals);
        self.paused_state = self.state;
        self.set_state(TimerState::None);
        charge
    }

    /// Resumes the CPU-time accounting state that was active before switch-out.
    pub fn resume_after_switch(&mut self) {
        let state = self.paused_state;
        self.paused_state = TimerState::None;
        self.set_state(state);
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

        let now = timer.effective_clock.now();
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
            None
        } else {
            let next_deadline = deadline
                .checked_add(saturating_duration_mul(timer.interval, expirations))
                .unwrap_or(Duration::MAX);
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
