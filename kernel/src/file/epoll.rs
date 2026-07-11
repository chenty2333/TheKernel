// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// Copyright (C) 2025 Azure-stars <Azure_stars@126.com>
// Copyright (C) 2025 Yuekai Jia <equation618@gmail.com>
// See LICENSES for license details.
//
// This file has been modified by KylinSoft on 2025.

use alloc::{
    borrow::Cow,
    collections::vec_deque::VecDeque,
    sync::{Arc, Weak},
    task::Wake,
    vec::Vec,
};
use core::{
    array,
    hash::{Hash, Hasher},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use bitflags::bitflags;
use hashbrown::HashMap;
use kspin::{SpinNoIrq, SpinNoPreempt};
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT, epoll_event};
use spin::Once;

use crate::file::{FileDescription, FileLike, FileLikeKind, Kstat, get_file_description};

const EPOLL_MAX_NESTS: usize = 5;
const EPOLL_MAX_INTERESTS: usize = 16_384;
const EPOLL_MAX_REVERSE_PARENTS: usize = 16_384;
const EPOLL_GRAPH_WALK_LIMIT: usize = 65_536;
const EPOLL_WAITER_SLOTS: usize = 64;
static EPOLL_GRAPH_LOCK: Mutex<()> = Mutex::new(());

struct GraphWalkBudget {
    remaining: usize,
}

impl GraphWalkBudget {
    const fn new() -> Self {
        Self {
            remaining: EPOLL_GRAPH_WALK_LIMIT,
        }
    }

    fn visit(&mut self) -> AxResult<()> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| AxError::from(LinuxError::ELOOP))?;
        Ok(())
    }
}

struct ReadyWaiterSet {
    entries: [Option<Waker>; EPOLL_WAITER_SLOTS],
    cursor: usize,
}

impl Default for ReadyWaiterSet {
    fn default() -> Self {
        Self {
            entries: array::from_fn(|_| None),
            cursor: 0,
        }
    }
}

impl ReadyWaiterSet {
    fn register(&mut self, waker: &Waker, owned: Waker) -> (Option<Waker>, Option<Waker>) {
        if self
            .entries
            .iter()
            .flatten()
            .any(|registered| registered.will_wake(waker))
        {
            return (None, Some(owned));
        }

        let slot = self.cursor;
        self.cursor = (self.cursor + 1) % EPOLL_WAITER_SLOTS;
        (self.entries[slot].replace(owned), None)
    }

    fn take_all(&mut self) -> [Option<Waker>; EPOLL_WAITER_SLOTS] {
        self.cursor = 0;
        array::from_fn(|index| self.entries[index].take())
    }
}

/// Fixed-capacity epoll waiters. Registration and wake never allocate, and
/// duplicate registrations from the same task or parent epoll are coalesced.
struct ReadyWaiters(SpinNoIrq<ReadyWaiterSet>);

impl Default for ReadyWaiters {
    fn default() -> Self {
        Self(SpinNoIrq::new(ReadyWaiterSet::default()))
    }
}

impl ReadyWaiters {
    fn register(&self, waker: &Waker) {
        // RawWaker clone/drop can execute type-specific code. Keep both out of
        // the IRQ-safe slot lock; the lock only compares identities and swaps
        // already-owned values.
        let owned = waker.clone();
        let (retired, duplicate) = self.0.lock().register(waker, owned);
        drop(duplicate);
        if let Some(retired) = retired {
            retired.wake();
        }
    }

    fn wake(&self) -> usize {
        let waiters = self.0.lock().take_all();
        let mut count = 0;
        for waker in waiters.into_iter().flatten() {
            count += 1;
            waker.wake();
        }
        count
    }
}

impl Drop for ReadyWaiters {
    fn drop(&mut self) {
        self.wake();
    }
}

struct ParentEntry {
    parent: Weak<EpollInner>,
    edge_count: usize,
}

#[derive(Default)]
struct ParentRegistry {
    entries: HashMap<usize, ParentEntry>,
}

impl ParentRegistry {
    fn try_admit(&mut self, parent_id: usize) -> AxResult<()> {
        if let Some(entry) = self.entries.get(&parent_id) {
            entry.edge_count.checked_add(1).ok_or(AxError::NoMemory)?;
            return Ok(());
        }
        if self.entries.len() >= EPOLL_MAX_REVERSE_PARENTS {
            return Err(LinuxError::ENOSPC.into());
        }
        self.entries.try_reserve(1).map_err(|_| AxError::NoMemory)
    }

    /// Commit an admission performed while the global graph lock was held.
    /// Concurrent deletion can only reduce counts and never consumes the
    /// reserved vacant capacity.
    fn add_admitted(
        &mut self,
        parent_id: usize,
        parent: &Arc<EpollInner>,
    ) -> AxResult<Option<ParentEntry>> {
        if let Some(entry) = self.entries.get_mut(&parent_id) {
            entry.edge_count = entry.edge_count.checked_add(1).ok_or(AxError::NoMemory)?;
            return Ok(None);
        }
        Ok(self.entries.insert(
            parent_id,
            ParentEntry {
                parent: Arc::downgrade(parent),
                edge_count: 1,
            },
        ))
    }

    fn remove(&mut self, parent_id: usize) -> Option<ParentEntry> {
        let entry = self.entries.get_mut(&parent_id)?;
        if entry.edge_count > 1 {
            entry.edge_count -= 1;
            return None;
        }
        self.entries.remove(&parent_id)
    }
}

struct ReverseParentRegistration {
    child: Weak<EpollInner>,
    parent_id: usize,
    committed: AtomicBool,
}

impl ReverseParentRegistration {
    fn new(child: Weak<EpollInner>, parent_id: usize) -> Self {
        Self {
            child,
            parent_id,
            committed: AtomicBool::new(false),
        }
    }

    fn commit(&self) {
        self.committed.store(true, Ordering::Release);
    }

    fn disarm(&self) -> bool {
        self.committed.swap(false, Ordering::AcqRel)
    }

    fn release(&self) {
        if self.disarm()
            && let Some(child) = self.child.upgrade()
        {
            child.remove_parent(self.parent_id);
        }
    }
}

impl Drop for ReverseParentRegistration {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct EpollEvent {
    pub events: IoEvents,
    pub user_data: u64,
}

bitflags! {
    /// Flags for the entries in the `epoll` instance.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollFlags: u32 {
        const EDGE_TRIGGER = EPOLLET;
        const ONESHOT = EPOLLONESHOT;
    }
}

/// Interest trigger mode
#[derive(Debug, Clone, Copy)]
enum TriggerMode {
    /// Level-triggered: until the condition is cleared
    Level,
    /// Edge-triggered: only notify when the condition changes
    Edge,
    /// One-shot: notify only once
    OneShot { fired: bool },
}

impl TriggerMode {
    fn from_flags(flags: EpollFlags) -> Self {
        if flags.contains(EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false }
        } else if flags.contains(EpollFlags::EDGE_TRIGGER) {
            TriggerMode::Edge
        } else {
            TriggerMode::Level
        }
    }

    // return should notify and new mode
    fn should_notify(&self) -> (bool, Self) {
        match self {
            TriggerMode::Level => {
                // LT: always notify
                (true, *self)
            }
            // if we could wake, we need notify
            TriggerMode::Edge => (true, TriggerMode::Edge),
            TriggerMode::OneShot { fired } => {
                // ONESHOT: 只触发一次
                if *fired {
                    (false, *self)
                } else {
                    (true, TriggerMode::OneShot { fired: true })
                }
            }
        }
    }

    fn is_enabled(&self) -> bool {
        match self {
            TriggerMode::OneShot { fired } => !fired,
            _ => true,
        }
    }
}

enum ConsumeResult {
    // success and should keep in ready list
    EventAndKeep(EpollEvent),
    // success and hould remove ready list
    EventAndRemove(EpollEvent),
    // no event and should remove ready list
    NoEvent,
}

#[derive(Clone)]
struct EntryKey {
    fd: i32,
    file: Arc<FileDescription>,
}
impl EntryKey {
    fn new(fd: i32) -> AxResult<Self> {
        Ok(Self {
            fd,
            file: get_file_description(fd)?,
        })
    }

    #[inline]
    fn get_file(&self) -> &FileDescription {
        self.file.as_ref()
    }
}

impl Hash for EntryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.fd, Arc::as_ptr(&self.file)).hash(state);
    }
}
impl PartialEq for EntryKey {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd && Arc::ptr_eq(&self.file, &other.file)
    }
}

impl Eq for EntryKey {}

struct EpollInterest {
    key: EntryKey,
    event: EpollEvent,
    mode: SpinNoPreempt<TriggerMode>,
    waker: Once<Waker>,
    waker_armed: AtomicBool,
    active: AtomicBool,
    in_ready_queue: AtomicBool,
    rescan_pending: AtomicBool,
    wake_sequence: AtomicU64,
    reverse_parent: Option<ReverseParentRegistration>,
}

impl EpollInterest {
    fn new(
        key: EntryKey,
        event: EpollEvent,
        flags: EpollFlags,
        reverse_parent: Option<ReverseParentRegistration>,
    ) -> Self {
        Self {
            key,
            event,
            mode: SpinNoPreempt::new(TriggerMode::from_flags(flags)),
            waker: Once::new(),
            waker_armed: AtomicBool::new(false),
            active: AtomicBool::new(true),
            in_ready_queue: AtomicBool::new(false),
            rescan_pending: AtomicBool::new(false),
            wake_sequence: AtomicU64::new(0),
            reverse_parent,
        }
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.is_active() && self.mode.lock().is_enabled()
    }

    fn install_waker(&self, waker: Waker) {
        self.waker.call_once(|| waker);
    }

    fn registered_waker(&self) -> Option<&Waker> {
        self.waker.get()
    }

    #[inline]
    fn try_arm_waker(&self) -> bool {
        self.waker_armed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    fn disarm_waker(&self) {
        self.waker_armed.store(false, Ordering::Release);
    }

    #[inline]
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    #[inline]
    fn is_in_queue(&self) -> bool {
        self.in_ready_queue.load(Ordering::Acquire)
    }

    #[inline]
    fn try_mark_in_queue(&self) -> bool {
        self.in_ready_queue
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    fn mark_not_in_queue(&self) {
        self.in_ready_queue.store(false, Ordering::Release);
    }

    #[inline]
    fn needs_rescan(&self) -> bool {
        self.rescan_pending.load(Ordering::Acquire)
    }

    #[inline]
    fn mark_for_rescan(&self) {
        self.rescan_pending.store(true, Ordering::Release);
    }

    #[inline]
    fn clear_rescan(&self) {
        self.rescan_pending.store(false, Ordering::Release);
    }

    #[inline]
    fn wake_sequence(&self) -> u64 {
        self.wake_sequence.load(Ordering::Acquire)
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        self.disarm_waker();
        self.mark_not_in_queue();
        self.clear_rescan();
    }

    fn commit_reverse_parent(&self) {
        if let Some(registration) = &self.reverse_parent {
            registration.commit();
        }
    }

    fn transfer_reverse_parent_to(&self, replacement: &Self) {
        if let (Some(current), Some(replacement)) =
            (&self.reverse_parent, &replacement.reverse_parent)
            && current.disarm()
        {
            replacement.commit();
        }
    }

    fn release_reverse_parent(&self) {
        if let Some(registration) = &self.reverse_parent {
            registration.release();
        }
    }

    fn consume(&self, file: &dyn FileLike) -> ConsumeResult {
        if !self.is_active() {
            return ConsumeResult::NoEvent;
        }
        let current_events = file.poll();
        let matched = current_events & self.event.events;

        // not ready
        if matched.is_empty() {
            return ConsumeResult::NoEvent;
        }

        let mut mode = self.mode.lock();
        let (should_notify, new_mode) = mode.should_notify();
        *mode = new_mode;
        trace!(
            "consume fd: {} matches {:?} should notify: {} ",
            self.key.fd, matched, should_notify
        );

        if !should_notify {
            return ConsumeResult::NoEvent;
        }

        // create event
        let event = EpollEvent {
            events: matched,
            user_data: self.event.user_data,
        };

        // shoud still keep in ready?
        match *mode {
            TriggerMode::Level => ConsumeResult::EventAndKeep(event),
            TriggerMode::Edge | TriggerMode::OneShot { .. } => ConsumeResult::EventAndRemove(event),
        }
    }
}

struct InterestWaker {
    epoll: Weak<EpollInner>,
    interest: Weak<EpollInterest>,
}

impl Wake for InterestWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Hold the owning epoll first so its interests map keeps a successfully
        // upgraded interest alive until this callback has finished.
        let Some(epoll) = self.epoll.upgrade() else {
            return;
        };
        let Some(interest) = self.interest.upgrade() else {
            return;
        };
        // axpoll registrations are consumed by wake(). Rearming is owned by
        // the next task-context poll, so repeated LT scans cannot accumulate
        // duplicate copies of this same waker in the source wait set.
        interest.disarm_waker();

        epoll.enqueue_from_waker(&interest);
    }
}

#[derive(Default)]
struct ReadyQueue {
    entries: VecDeque<Weak<EpollInterest>>,
}

impl ReadyQueue {
    /// Keep room for every published interest plus one spare slot. The spare
    /// lets a rescan rotate a popped level-triggered entry behind an overflowed
    /// entry without allocating in a wake path.
    fn try_admit_interest(&mut self, interest_count_after: usize) -> AxResult<()> {
        let required = interest_count_after
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        let additional = required.saturating_sub(self.entries.len());
        self.entries
            .try_reserve(additional)
            .map_err(|_| AxError::NoMemory)
    }

    fn push_back_noalloc(
        &mut self,
        interest: Weak<EpollInterest>,
    ) -> Result<(), Weak<EpollInterest>> {
        if self.entries.len() == self.entries.capacity() {
            return Err(interest);
        }
        self.entries.push_back(interest);
        Ok(())
    }

    fn pop_front(&mut self) -> Option<Weak<EpollInterest>> {
        self.entries.pop_front()
    }

    fn remove(&mut self, interest: &Arc<EpollInterest>) -> Option<Weak<EpollInterest>> {
        let interest = Arc::as_ptr(interest);
        let position = self
            .entries
            .iter()
            .position(|queued| queued.as_ptr() == interest)?;
        self.entries.remove(position)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

struct EpollInner {
    /// Reverse graph edges are weak so an acyclic forward eventpoll graph does
    /// not acquire a second ownership direction. Counts cover duplicate fds
    /// that refer to the same child epoll.
    // A stale source callback may hold the final interest Arc after DEL and
    // release its reverse token from IRQ context.
    parents: SpinNoIrq<ParentRegistry>,
    interests: SpinNoPreempt<HashMap<EntryKey, Arc<EpollInterest>>>,
    /// Interest wakers may run from IRQ context, so every lock they touch must
    /// disable local interrupts rather than only disabling preemption.
    ready_queue: SpinNoIrq<ReadyQueue>,
    /// A coalesced hint that one or more interests could not enter the ready
    /// queue. Per-interest bits retain the exact work; this flag is only the
    /// allocation-free readiness/wakeup summary.
    rescan_needed: AtomicBool,
    poll_ready: ReadyWaiters,
}

impl Default for EpollInner {
    fn default() -> Self {
        Self {
            parents: SpinNoIrq::new(ParentRegistry::default()),
            interests: SpinNoPreempt::new(HashMap::new()),
            ready_queue: SpinNoIrq::new(ReadyQueue::default()),
            rescan_needed: AtomicBool::new(false),
            poll_ready: ReadyWaiters::default(),
        }
    }
}

impl EpollInner {
    fn remove_parent(&self, parent_id: usize) {
        let retired = self.parents.lock().remove(parent_id);
        drop(retired);
    }

    fn parent_inners(&self) -> AxResult<Vec<Arc<EpollInner>>> {
        let mut parents = Vec::new();
        loop {
            let required = self.parents.lock().entries.len();
            if parents.capacity() < required {
                parents
                    .try_reserve(required)
                    .map_err(|_| AxError::NoMemory)?;
            }

            let registry = self.parents.lock();
            if registry.entries.len() > parents.capacity() {
                drop(registry);
                continue;
            }
            for entry in registry.entries.values() {
                if let Some(parent) = entry.parent.upgrade() {
                    parents.push(parent);
                }
            }
            return Ok(parents);
        }
    }

    fn max_parent_depth(
        &self,
        stack: &mut Vec<usize>,
        budget: &mut GraphWalkBudget,
    ) -> AxResult<usize> {
        budget.visit()?;
        let id = self as *const Self as usize;
        if stack.contains(&id) || stack.len() > EPOLL_MAX_NESTS {
            return Ok(EPOLL_MAX_NESTS.saturating_add(1));
        }

        stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        stack.push(id);
        let result = (|| {
            let mut max_depth = 0usize;
            for parent in self.parent_inners()? {
                let depth = parent.max_parent_depth(stack, budget)?.saturating_add(1);
                max_depth = max_depth.max(depth);
            }
            Ok(max_depth)
        })();
        stack.pop();
        result
    }

    fn enqueue_ready(&self, interest: &Arc<EpollInterest>, record_wake: bool) {
        let mut queued = false;
        let mut requested_rescan = false;
        {
            let mut queue = self.ready_queue.lock();
            if record_wake {
                interest.wake_sequence.fetch_add(1, Ordering::AcqRel);
            }
            // A file waker may run in interrupt context. Do not take the
            // trigger-mode spin lock here; a harmless stale one-shot wake is
            // filtered by consume() in task context.
            if !interest.is_active() || interest.is_in_queue() {
                return;
            }

            if interest.try_mark_in_queue() {
                match queue.push_back_noalloc(Arc::downgrade(interest)) {
                    Ok(()) => {
                        interest.clear_rescan();
                        queued = true;
                    }
                    Err(_) => {
                        interest.mark_not_in_queue();
                        interest.mark_for_rescan();
                        requested_rescan = !self.rescan_needed.swap(true, Ordering::AcqRel);
                    }
                }
            }
        }

        if queued {
            trace!(
                "Epoll: fd={} added to ready queue, events={:?} wake up poller",
                interest.key.fd, interest.event.events
            );
            self.poll_ready.wake();
        } else if requested_rescan {
            trace!(
                "Epoll: fd={} requested allocation-free ready rescan",
                interest.key.fd
            );
            self.poll_ready.wake();
        }
    }

    fn enqueue_from_waker(&self, interest: &Arc<EpollInterest>) {
        self.enqueue_ready(interest, true);
    }

    /// Move as many overflowed interests as possible into already admitted
    /// queue storage. This scans bounded, published state once and never polls
    /// a child while either epoll spin lock is held.
    fn refill_from_rescan(&self) {
        if !self.rescan_needed.load(Ordering::Acquire) {
            return;
        }

        let interests = self.interests.lock();
        let mut queue = self.ready_queue.lock();
        for interest in interests.values() {
            if !interest.needs_rescan() {
                continue;
            }
            if !interest.is_enabled() || interest.is_in_queue() {
                interest.clear_rescan();
                continue;
            }
            if interest.try_mark_in_queue() {
                match queue.push_back_noalloc(Arc::downgrade(interest)) {
                    Ok(()) => interest.clear_rescan(),
                    Err(_) => {
                        interest.mark_not_in_queue();
                        break;
                    }
                }
            }
        }
        let pending = interests.values().any(|interest| interest.needs_rescan());
        self.rescan_needed.store(pending, Ordering::Release);
    }

    /// The admitted-capacity invariant makes this a last-resort path. Keeping
    /// it explicit means a future invariant regression still produces bounded
    /// rescan work instead of a lost edge, allocator panic, or busy loop.
    fn take_rescan_direct(&self) -> Option<(Arc<EpollInterest>, u64)> {
        if !self.rescan_needed.load(Ordering::Acquire) {
            return None;
        }

        let interests = self.interests.lock();
        let _queue = self.ready_queue.lock();
        let mut selected = None;
        for interest in interests.values() {
            if !interest.needs_rescan() {
                continue;
            }
            if !interest.is_enabled() || interest.is_in_queue() {
                interest.clear_rescan();
                continue;
            }
            if interest.try_mark_in_queue() {
                interest.clear_rescan();
                selected = Some((Arc::clone(interest), interest.wake_sequence()));
                break;
            }
        }
        let pending = interests.values().any(|interest| interest.needs_rescan());
        self.rescan_needed.store(pending, Ordering::Release);
        selected
    }

    /// Requeue an LT interest without growing the queue. If the spare-capacity
    /// invariant is unexpectedly unavailable, rotate this interest through
    /// the coalesced rescan path and let the current wait return promptly.
    fn requeue_consumed(&self, interest: &Arc<EpollInterest>) -> bool {
        let mut requested_rescan = false;
        let requeued = {
            let mut queue = self.ready_queue.lock();
            if !interest.is_enabled() {
                interest.mark_not_in_queue();
                false
            } else {
                match queue.push_back_noalloc(Arc::downgrade(interest)) {
                    Ok(()) => {
                        interest.clear_rescan();
                        true
                    }
                    Err(_) => {
                        interest.mark_not_in_queue();
                        interest.mark_for_rescan();
                        requested_rescan = !self.rescan_needed.swap(true, Ordering::AcqRel);
                        false
                    }
                }
            }
        };
        if requested_rescan {
            self.poll_ready.wake();
        }
        requeued
    }

    fn release_consumed(&self, interest: &Arc<EpollInterest>, observed_wakes: u64) {
        interest.mark_not_in_queue();
        interest.clear_rescan();
        if interest.is_enabled() && interest.wake_sequence() != observed_wakes {
            // A callback raced while the popped token still owned the
            // in-ready bit. Re-publish that coalesced wake after releasing the
            // token; a callback racing after this point can publish itself.
            self.enqueue_ready(interest, false);
        }
    }

    fn refresh_rescan_hint_locked(&self, interests: &HashMap<EntryKey, Arc<EpollInterest>>) {
        self.rescan_needed.store(
            interests.values().any(|interest| interest.needs_rescan()),
            Ordering::Release,
        );
    }
}

pub struct Epoll {
    inner: Arc<EpollInner>,
}

impl Epoll {
    pub fn new() -> AxResult<Self> {
        Ok(Self {
            inner: Arc::try_new(EpollInner::default()).map_err(|_| AxError::NoMemory)?,
        })
    }

    #[inline]
    fn id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    fn child_descriptions(&self) -> AxResult<Vec<Arc<FileDescription>>> {
        let mut children = Vec::new();
        loop {
            let required = self.inner.interests.lock().len();
            if children.capacity() < required {
                children
                    .try_reserve(required)
                    .map_err(|_| AxError::NoMemory)?;
            }

            let interests = self.inner.interests.lock();
            if interests.len() > children.capacity() {
                drop(interests);
                continue;
            }
            for key in interests.keys() {
                children.push(Arc::clone(&key.file));
            }
            return Ok(children);
        }
    }

    fn reaches_epoll_id(
        &self,
        target_id: usize,
        stack: &mut Vec<usize>,
        budget: &mut GraphWalkBudget,
    ) -> AxResult<bool> {
        budget.visit()?;
        let id = self.id();
        if id == target_id {
            return Ok(true);
        }
        if stack.contains(&id) {
            return Ok(false);
        }

        stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        stack.push(id);
        let result = (|| {
            for child in self.child_descriptions()? {
                if let Some(epoll) = child.inner.downcast_ref::<Epoll>()
                    && epoll.reaches_epoll_id(target_id, stack, budget)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        stack.pop();
        result
    }

    fn max_nested_depth(
        &self,
        stack: &mut Vec<usize>,
        budget: &mut GraphWalkBudget,
    ) -> AxResult<usize> {
        budget.visit()?;
        let id = self.id();
        if stack.contains(&id) || stack.len() > EPOLL_MAX_NESTS {
            return Ok(EPOLL_MAX_NESTS.saturating_add(1));
        }

        stack.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        stack.push(id);
        let result = (|| {
            let mut max_depth = 0usize;
            for child in self.child_descriptions()? {
                let depth = if let Some(epoll) = child.inner.downcast_ref::<Epoll>() {
                    epoll.max_nested_depth(stack, budget)?.saturating_add(1)
                } else {
                    1
                };
                max_depth = max_depth.max(depth);
            }
            Ok(max_depth)
        })();
        stack.pop();
        result
    }

    fn validate_combined_depth(parent_depth: usize, child_depth: usize) -> AxResult<()> {
        if parent_depth
            .checked_add(child_depth)
            .is_none_or(|depth| depth > EPOLL_MAX_NESTS)
        {
            return Err(LinuxError::ELOOP.into());
        }
        Ok(())
    }

    fn validate_add_target(&self, key: &EntryKey) -> AxResult<()> {
        match FileLikeKind::from_file_like(key.get_file()) {
            FileLikeKind::Regular | FileLikeKind::Directory => {
                return Err(LinuxError::EPERM.into());
            }
            FileLikeKind::Fifo | FileLikeKind::Socket | FileLikeKind::Other => {}
        }

        let mut parent_stack = Vec::new();
        let parent_depth = self
            .inner
            .max_parent_depth(&mut parent_stack, &mut GraphWalkBudget::new())?;

        let Some(epoll) = key.get_file().inner.downcast_ref::<Epoll>() else {
            return Self::validate_combined_depth(parent_depth, 1);
        };

        if epoll.id() == self.id() {
            return Err(AxError::InvalidInput);
        }
        let mut stack = Vec::new();
        let child_depth = epoll
            .max_nested_depth(&mut stack, &mut GraphWalkBudget::new())?
            .saturating_add(1);
        Self::validate_combined_depth(parent_depth, child_depth)?;

        if epoll.reaches_epoll_id(self.id(), &mut stack, &mut GraphWalkBudget::new())? {
            return Err(LinuxError::ELOOP.into());
        }

        Ok(())
    }

    fn try_new_interest(
        &self,
        key: EntryKey,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> AxResult<Arc<EpollInterest>> {
        let reverse_parent =
            key.get_file().inner.downcast_ref::<Epoll>().map(|child| {
                ReverseParentRegistration::new(Arc::downgrade(&child.inner), self.id())
            });
        let interest = Arc::try_new(EpollInterest::new(key, event, flags, reverse_parent))
            .map_err(|_| AxError::NoMemory)?;
        let waker = Waker::from(
            Arc::try_new(InterestWaker {
                epoll: Arc::downgrade(&self.inner),
                interest: Arc::downgrade(&interest),
            })
            .map_err(|_| AxError::NoMemory)?,
        );
        interest.install_waker(waker);
        Ok(interest)
    }

    // only register waker, not add to ready queue
    fn register_waker_only(&self, interest: &Arc<EpollInterest>) {
        if !interest.is_enabled() || !interest.try_arm_waker() {
            return;
        }

        let Some(waker) = interest.registered_waker() else {
            interest.disarm_waker();
            return;
        };
        if !interest.is_enabled() {
            interest.disarm_waker();
            return;
        }

        let mut context = Context::from_waker(waker);
        interest
            .key
            .get_file()
            .register(&mut context, interest.event.events);
    }

    // for add/modify
    fn check_and_register_waker(&self, interest: &Arc<EpollInterest>) {
        if !interest.is_enabled() {
            return;
        }

        let file = interest.key.get_file();
        let current = file.poll() & interest.event.events;

        if !current.is_empty() {
            self.inner.enqueue_ready(interest, false);
        } else {
            self.register_waker_only(interest);

            let current = file.poll() & interest.event.events;
            if !current.is_empty() {
                self.inner.enqueue_ready(interest, false);
            }
        }
    }

    pub fn add(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> AxResult<()> {
        let key = EntryKey::new(fd)?;
        let interest = self.try_new_interest(key.clone(), event, flags)?;
        let child_inner = key
            .get_file()
            .inner
            .downcast_ref::<Epoll>()
            .map(|child| Arc::clone(&child.inner));
        // Linux serializes graph validation with edge publication. Without a
        // global transaction, concurrent A<-B and B<-A additions can both pass
        // their snapshots and create an uncollectable FileDescription cycle.
        let _graph = EPOLL_GRAPH_LOCK.lock();
        self.validate_add_target(&key)?;

        // Phase 1: make every allocation fallible before either graph
        // direction becomes visible. Deletes and modifies cannot consume the
        // reserved forward/queue capacity while graph additions are globally
        // serialized.
        let interest_count_after = {
            let mut interests = self.inner.interests.lock();
            if interests.contains_key(&key) {
                return Err(AxError::AlreadyExists);
            }
            if interests.len() >= EPOLL_MAX_INTERESTS {
                return Err(LinuxError::ENOSPC.into());
            }
            let count = interests.len().checked_add(1).ok_or(AxError::NoMemory)?;
            interests.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            count
        };
        self.inner
            .ready_queue
            .lock()
            .try_admit_interest(interest_count_after)?;
        if let Some(child) = &child_inner {
            child.parents.lock().try_admit(self.id())?;
        }

        // Phase 2: publish reverse ownership first and then the strong forward
        // edge. There are no fallible operations after reverse publication.
        let mut interests = self.inner.interests.lock();
        if interests.contains_key(&key) {
            return Err(AxError::AlreadyExists);
        }
        let retired_parent = if let Some(child) = &child_inner {
            let mut parents = child.parents.lock();
            let retired = parents.add_admitted(self.id(), &self.inner)?;
            interest.commit_reverse_parent();
            retired
        } else {
            None
        };
        let replaced = interests.insert(key, Arc::clone(&interest));
        drop(interests);
        drop(replaced);
        drop(retired_parent);
        drop(_graph);
        trace!("Epoll add fd: {} interest {:?} ", fd, interest.event.events);
        self.check_and_register_waker(&interest);
        Ok(())
    }

    pub fn modify(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> AxResult<()> {
        let key = EntryKey::new(fd)?;
        let interest = self.try_new_interest(key.clone(), event, flags)?;

        let mut interests = self.inner.interests.lock();
        let old = {
            let slot = interests.get_mut(&key).ok_or(AxError::NotFound)?;
            let old = Arc::clone(slot);
            old.transfer_reverse_parent_to(&interest);
            old.deactivate();
            *slot = Arc::clone(&interest);
            old
        };
        let mut ready_queue = self.inner.ready_queue.lock();
        let stale_ready = ready_queue.remove(&old);
        self.inner.refresh_rescan_hint_locked(&interests);
        drop(ready_queue);
        drop(interests);
        drop(stale_ready);
        trace!(
            "Epoll: modify fd={}, events={:?}",
            fd, interest.event.events
        );
        // reset waker
        self.check_and_register_waker(&interest);
        Ok(())
    }

    pub fn delete(&self, fd: i32) -> AxResult<()> {
        let key = EntryKey::new(fd)?;
        let mut interests = self.inner.interests.lock();
        let interest = interests.remove(&key).ok_or(AxError::NotFound)?;
        interest.deactivate();
        let mut ready_queue = self.inner.ready_queue.lock();
        let stale_ready = ready_queue.remove(&interest);
        self.inner.refresh_rescan_hint_locked(&interests);
        drop(ready_queue);
        drop(interests);
        drop(stale_ready);
        interest.release_reverse_parent();
        trace!("Epoll: delete fd={fd}");
        Ok(())
    }

    pub fn poll_events(&self, out: &mut [epoll_event]) -> AxResult<usize> {
        trace!("Epoll: poll_events called, out.len()={}", out.len());
        let mut count = 0;
        self.inner.refill_from_rescan();
        // Scan a snapshot of the ready-list token count. LT entries rotated to
        // the tail and callbacks arriving during this transfer belong to the
        // next wait, matching Linux's detached transfer-list behavior instead
        // of returning the same still-ready fd `maxevents` times.
        let mut scan_budget = self.inner.ready_queue.lock().len();
        if scan_budget == 0 && self.inner.rescan_needed.load(Ordering::Acquire) {
            scan_budget = 1;
        }
        loop {
            if count >= out.len() || scan_budget == 0 {
                break;
            }
            scan_budget -= 1;

            let (retired_weak, popped, had_queue_entry) = {
                let mut queue = self.inner.ready_queue.lock();
                let weak = queue.pop_front();
                let had_queue_entry = weak.is_some();
                let popped = weak.as_ref().and_then(Weak::upgrade).map(|interest| {
                    let observed_wakes = interest.wake_sequence();
                    (interest, observed_wakes)
                });
                (weak, popped, had_queue_entry)
            };
            drop(retired_weak);

            let (interest, observed_wakes) = if let Some(popped) = popped {
                popped
            } else if had_queue_entry {
                continue;
            } else {
                let Some(popped) = self.inner.take_rescan_direct() else {
                    break;
                };
                popped
            };

            trace!(
                "Epoll: consuming ready interest for fd={}, events={:?}",
                interest.key.fd, interest.event.events
            );

            // Source wait registrations are consumed by wake(). Install the
            // next callback before polling; the poll that follows closes the
            // usual register-vs-readiness race without allocating a new waker.
            self.register_waker_only(&interest);
            match interest.consume(interest.key.get_file()) {
                ConsumeResult::EventAndKeep(event) => {
                    out[count] = epoll_event {
                        events: event.events.bits(),
                        data: event.user_data,
                    };
                    count += 1;
                    if !self.inner.requeue_consumed(&interest) {
                        // The direct fallback may have no queue capacity at
                        // all. Return this event now; its LT readiness remains
                        // coalesced for the next bounded rescan.
                        break;
                    }
                }
                ConsumeResult::EventAndRemove(event) => {
                    out[count] = epoll_event {
                        events: event.events.bits(),
                        data: event.user_data,
                    };
                    count += 1;
                    self.inner.release_consumed(&interest, observed_wakes);
                }
                ConsumeResult::NoEvent => {
                    self.inner.release_consumed(&interest, observed_wakes);
                }
            }
        }

        if count == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(count)
        }
    }
}

impl FileLike for Epoll {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(super::anon_inode_stat())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[eventpoll]".into()
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        // epoll_wait's timeout controls waiting. The OFD nevertheless retains
        // O_NONBLOCK just as Linux's generic struct file does.
        Ok(())
    }
}

impl Pollable for Epoll {
    fn poll(&self) -> IoEvents {
        if self.inner.ready_queue.lock().is_empty()
            && !self.inner.rescan_needed.load(Ordering::Acquire)
        {
            IoEvents::empty()
        } else {
            IoEvents::IN
        }
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.inner.poll_ready.register(context.waker());
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        sync::{Arc, Weak},
        task::Wake,
        vec::Vec,
    };
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use super::{
        EPOLL_MAX_NESTS, EPOLL_WAITER_SLOTS, Epoll, EpollInner, EpollInterest, GraphWalkBudget,
        ReadyQueue, ReadyWaiters,
    };

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn add_parent(child: &Arc<EpollInner>, parent: &Arc<EpollInner>) {
        let parent_id = Arc::as_ptr(parent) as usize;
        let retired = {
            let mut parents = child.parents.lock();
            parents.try_admit(parent_id).unwrap();
            parents.add_admitted(parent_id, parent).unwrap()
        };
        drop(retired);
    }

    fn parent_depth(inner: &EpollInner) -> usize {
        inner
            .max_parent_depth(&mut Vec::new(), &mut GraphWalkBudget::new())
            .unwrap()
    }

    #[test]
    fn ready_admission_reserves_all_interests_and_a_spare() {
        let mut queue = ReadyQueue::default();
        for interest_count in 1..=64 {
            queue.try_admit_interest(interest_count).unwrap();
            assert!(queue.entries.capacity() > interest_count);
        }
    }

    #[test]
    fn ready_push_reports_full_without_growing_or_panicking() {
        let mut queue = ReadyQueue::default();
        queue.try_admit_interest(1).unwrap();
        let capacity = queue.entries.capacity();
        while queue.entries.len() < capacity {
            assert!(
                queue
                    .push_back_noalloc(Weak::<EpollInterest>::new())
                    .is_ok()
            );
        }

        assert!(
            queue
                .push_back_noalloc(Weak::<EpollInterest>::new())
                .is_err()
        );
        assert_eq!(queue.entries.capacity(), capacity);
        assert_eq!(queue.entries.len(), capacity);
    }

    #[test]
    fn ready_waiters_coalesce_duplicate_task_registration() {
        let waiters = ReadyWaiters::default();
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));

        waiters.register(&waker);
        waiters.register(&waker);

        assert_eq!(waiters.wake(), 1);
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ready_waiter_overflow_wakes_evicted_slot_without_allocating() {
        let waiters = ReadyWaiters::default();
        let mut counters = Vec::new();
        for _ in 0..=EPOLL_WAITER_SLOTS {
            let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
            waiters.register(&Waker::from(Arc::clone(&counter)));
            counters.push(counter);
        }

        assert_eq!(counters[0].0.load(Ordering::Relaxed), 1);
        assert_eq!(waiters.wake(), EPOLL_WAITER_SLOTS);
        assert!(
            counters
                .iter()
                .all(|counter| counter.0.load(Ordering::Relaxed) == 1)
        );
    }

    #[test]
    fn evicted_waiter_can_re_register_before_the_real_event() {
        let waiters = ReadyWaiters::default();
        let mut counters = Vec::new();
        let mut wakers = Vec::new();
        for _ in 0..=EPOLL_WAITER_SLOTS {
            let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&counter));
            waiters.register(&waker);
            counters.push(counter);
            wakers.push(waker);
        }

        assert_eq!(counters[0].0.load(Ordering::Relaxed), 1);
        waiters.register(&wakers[0]);
        assert_eq!(counters[1].0.load(Ordering::Relaxed), 1);
        waiters.wake();
        assert_eq!(counters[0].0.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn reverse_parents_reject_a_late_leaf_beyond_the_depth_limit() {
        let mut chain = Vec::new();
        for _ in 0..=EPOLL_MAX_NESTS {
            chain.push(Arc::new(EpollInner::default()));
        }
        for index in 0..EPOLL_MAX_NESTS {
            add_parent(&chain[index], &chain[index + 1]);
        }

        assert_eq!(parent_depth(&chain[0]), EPOLL_MAX_NESTS);
        assert!(Epoll::validate_combined_depth(EPOLL_MAX_NESTS - 1, 1).is_ok());
        assert!(Epoll::validate_combined_depth(parent_depth(&chain[0]), 1).is_err());
    }

    #[test]
    fn reverse_parent_refcounts_duplicate_epoll_edges() {
        let child = Arc::new(EpollInner::default());
        let parent = Arc::new(EpollInner::default());
        let parent_id = Arc::as_ptr(&parent) as usize;
        add_parent(&child, &parent);
        add_parent(&child, &parent);

        child.remove_parent(parent_id);
        assert_eq!(parent_depth(&child), 1);
        child.remove_parent(parent_id);
        assert_eq!(parent_depth(&child), 0);
    }
}
