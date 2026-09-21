//! Futex implementation.

use alloc::{
    collections::btree_map::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::{Future, poll_fn},
    ops::Deref,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use axtask::{AxTaskRef, SchedClass, SchedState, WeakAxTaskRef, current, future::block_on};
use hashbrown::HashMap;
use kspin::SpinNoIrq;

use crate::{
    mm::{AddrSpace, FutexBackingId, FutexBackingIdentity, FutexWordOffset, SharedFutexKey},
    task::{AlarmClock, AsThread, PreparedClockSleep, ProcStateHint, prepare_clock_sleep},
};

type Waiter = SpinNoIrq<WaiterEntry>;
type WaiterRef = Arc<Waiter>;
type WaiterPtr = NonNull<Waiter>;

/// Key used inside one process-private table or one shared-backing table.
/// Shared backing identity is carried by the table itself and by every entry;
/// only the typed byte offset belongs in its inner map.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum FutexTableKey {
    /// Explicit `FUTEX_PRIVATE_FLAG` key: process table plus virtual address.
    Private(usize),
    /// A non-PRIVATE operation whose mapping resolved to a private/COW VMA.
    ///
    /// Linux keeps this in the same mm-local domain as a private futex, but
    /// marks the key with `FUT_OFF_MMSHARED`.  It must therefore not alias an
    /// explicit PRIVATE operation at the same virtual address.
    PrivateMapping(usize),
    Shared(FutexWordOffset),
}

/// Owner metadata carried by a queued waiter.  The entry/table references are
/// weak because the table slot and the caller's registration own the actual
/// lifetime.  Cancellation upgrades them only after all queue gates are
/// released, allowing an idle slot to be removed without an ABA window.
#[derive(Clone)]
pub(crate) struct WaiterOwner {
    entry: Weak<FutexEntry>,
    table: Weak<FutexTable>,
    key: FutexTableKey,
}

impl WaiterOwner {
    fn without_table(entry: Weak<FutexEntry>, key: FutexTableKey) -> Self {
        Self {
            entry,
            table: Weak::new(),
            key,
        }
    }

    fn cleanup_if_idle(self) {
        let Some(table) = self.table.upgrade() else {
            return;
        };
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        table.try_remove_idle(self.key, &entry);
        // `table`/`entry` are deliberately dropped after try_remove_idle has
        // released its table lock.
        drop(entry);
        drop(table);
    }
}

/// Ordered intrusive waiter storage shared by every futex queue.
///
/// The queue owns one strong `Arc` reference for each linked node, but stores
/// that reference as a raw pointer.  Moving a node between queues therefore
/// only changes links and reference ownership; it never allocates or drops an
/// `Arc` while the queue gate is held.
#[derive(Default)]
struct WaiterQueue {
    head: Option<WaiterPtr>,
    tail: Option<WaiterPtr>,
    len: usize,
}

// Raw links are dereferenced only while the containing `SpinNoIrq` is held,
// except during `Drop`, when exclusive ownership of the queue provides the
// same guarantee. Each linked node retains an owning Arc strong reference.
// The explicit auto-trait impls make that synchronization contract visible to
// the global futex tables without exposing the raw pointers to callers.
unsafe impl Send for WaiterQueue {}
unsafe impl Sync for WaiterQueue {}

/// Arc references detached while an IRQ-safe queue gate is held.
///
/// The list is intrusive, so it has bounded O(1)-storage overhead and can be
/// drained after all gates have been released.  This is also where cancelled
/// waiters go: their final `Arc` drop must not happen in the gate.
#[derive(Default)]
struct DeferredWaiters {
    head: Option<WaiterPtr>,
    tail: Option<WaiterPtr>,
}

impl DeferredWaiters {
    fn push(&mut self, waiter: WaiterRef) {
        let ptr = NonNull::from(waiter.as_ref());
        waiter.lock().next = None;
        let _ = Arc::into_raw(waiter);
        if let Some(tail) = self.tail {
            // SAFETY: `tail` is owned by this list and remains live until
            // `finish` drains it.
            unsafe { tail.as_ref() }.lock().next = Some(ptr);
        } else {
            self.head = Some(ptr);
        }
        self.tail = Some(ptr);
    }

    fn drain(&mut self) {
        while let Some(ptr) = self.head.take() {
            // SAFETY: each pointer came from `Arc::into_raw` in `push`, and
            // this list has exclusive ownership of that strong reference.
            let waiter = unsafe { Arc::from_raw(ptr.as_ptr()) };
            let next = waiter.lock().next.take();
            self.head = next;
            if self.head.is_none() {
                self.tail = None;
            }
            // Do not upgrade the task weak reference while a queue gate is
            // held.  The temporary Arc would otherwise be dropped in an
            // IRQ-disabled section and could be the task's last reference.
            let (task, owner) = {
                let waiter = waiter.lock();
                (waiter.task.clone(), waiter.owner.clone())
            };
            clear_waiter_proc_state(&task);
            drop(waiter);
            owner.cleanup_if_idle();
        }
    }

    fn finish(mut self) {
        self.drain();
    }
}

impl Drop for DeferredWaiters {
    fn drop(&mut self) {
        self.drain();
    }
}

/// Waiters whose wakers must be invoked after queue gates are released.
#[derive(Default)]
struct WakeBatch {
    waiters: DeferredWaiters,
}

impl WakeBatch {
    fn push(&mut self, waiter: WaiterRef) {
        self.waiters.push(waiter);
    }

    fn finish(mut self) {
        self.drain();
    }

    fn drain(&mut self) {
        while let Some(ptr) = self.waiters.head.take() {
            // SAFETY: `ptr` is an owned strong reference detached from a
            // queue and cannot be concurrently reclaimed before this drain.
            let waiter = unsafe { Arc::from_raw(ptr.as_ptr()) };
            let next = waiter.lock().next.take();
            self.waiters.head = next;
            if self.waiters.head.is_none() {
                self.waiters.tail = None;
            }
            let (task, owner) = {
                let waiter = waiter.lock();
                (waiter.task.clone(), waiter.owner.clone())
            };
            clear_waiter_proc_state(&task);
            let waker = waiter.lock().waker.take();
            if let Some(waker) = waker {
                waker.wake();
            }
            drop(waiter);
            owner.cleanup_if_idle();
        }
    }
}

impl Drop for WakeBatch {
    fn drop(&mut self) {
        self.drain();
    }
}

/// Result of a queue condition check.
///
/// `Retry` means that a no-fault user-memory snapshot was unavailable.  The
/// caller must release all queue gates, fault/read in task context, and retry;
/// it must not turn this into an externally visible futex error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitConditionError {
    Retry,
    Fault(AxError),
}

pub(crate) type WaitConditionResult<T> = Result<T, WaitConditionError>;

impl From<AxError> for WaitConditionError {
    fn from(error: AxError) -> Self {
        Self::Fault(error)
    }
}

impl From<WaitConditionError> for AxError {
    fn from(error: WaitConditionError) -> Self {
        match error {
            WaitConditionError::Retry => AxError::WouldBlock,
            WaitConditionError::Fault(error) => error,
        }
    }
}

/// Destination queue, remaining requeue budget, and the owner token that will
/// own moved waiters. The token is borrowed so its weak references are dropped
/// only after the queue gates have been released.
type RequeueTarget<'a> = (&'a mut WaiterQueue, usize, &'a WaiterOwner);

/// Wait queue used by futex.
#[derive(Default)]
pub struct WaitQueue {
    gate: Mutex<()>,
    queue: SpinNoIrq<WaiterQueue>,
}

// ---------------------------------------------------------------------------
// Priority inheritance.
//
// Linux keeps a `struct futex_pi_state` per contended PI futex; it embeds an
// `rt_mutex` whose waiters are a priority-ordered `plist`, and boosts the owner
// through `rt_mutex_setprio()` (`kernel/futex/pi.c`, `kernel/locking/rtmutex.c`).
// This kernel has no rt_mutex, but it does have the two primitives the boost
// needs: a genuine cross-task scheduling-state update (`axtask::set_sched_state`
// with real RT/FIFO priority ordering) and the futex wait queue itself. The
// state below is therefore the honest single-level reduction of `pi_state`:
// it tracks the same owner and applies the same boost, but it does not walk a
// `pi_blocked_on` chain and it does not inherit a SCHED_DEADLINE reservation.
// ---------------------------------------------------------------------------

/// `rt_mutex_waiter::prio`: Linux's kernel-internal priority domain, where a
/// smaller value is a stronger task.
///
/// `include/linux/sched/prio.h`: RT tasks span `0..MAX_RT_PRIO-1` as
/// `MAX_RT_PRIO - 1 - rt_priority` (`normal_prio()`), fair tasks span
/// `MAX_RT_PRIO..MAX_PRIO-1` as `NICE_TO_PRIO(nice)` = `nice + DEFAULT_PRIO`
/// with `DEFAULT_PRIO = MAX_RT_PRIO + NICE_WIDTH / 2`, and deadline tasks sit
/// below every RT task at `MAX_DL_PRIO - 1`.
pub const fn pi_kernel_priority(state: SchedState) -> i16 {
    const MAX_RT_PRIO: i16 = 100;
    /// `DEFAULT_PRIO`; `NICE_TO_PRIO(nice) == nice + DEFAULT_PRIO`.
    const DEFAULT_PRIO: i16 = MAX_RT_PRIO + 20;
    match state.class {
        // Linux schedules deadline entities before every RT waiter and gives
        // `dl_prio()` values below zero (`DEFAULT_PRIO - 1`).
        SchedClass::Deadline => -1,
        SchedClass::Fifo | SchedClass::RoundRobin => {
            // The Linux ABI limits RT priorities to 1..=99 (`sched_setattr`);
            // the mechanism range is wider, so clamp before negating.
            let rt = if state.rt_priority > 99 {
                99
            } else {
                state.rt_priority
            };
            (MAX_RT_PRIO - 1) - rt as i16
        }
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => {
            DEFAULT_PRIO + state.nice as i16
        }
    }
}

/// `rt_mutex_setprio()`'s boost target: `prio = min(p->normal_prio,
/// pi_task->prio)`, restricted to what `set_sched_state` can express.
///
/// A fair owner of a fair waiter only gains weight (its nice is lowered). An
/// RT waiter pulls the owner into the RT class at the waiter's priority, which
/// is exactly what makes `FUTEX_LOCK_PI` bounding for a mixed workload.
///
/// A `SCHED_DEADLINE` waiter is deliberately *not* propagated: Linux inherits
/// a deadline reservation through `pi_se`, and `EevdfTaskParams` carries no
/// reservation, so `set_sched_state` would reject `SchedClass::Deadline` with
/// `SchedulerError::InvalidParameters`. `pi_boost` therefore returns `None`
/// rather than installing a broken deadline state.
pub fn pi_boost(owner: SchedState, waiter: SchedState) -> Option<SchedState> {
    if matches!(waiter.class, SchedClass::Deadline) {
        return None;
    }
    let owner_priority = pi_kernel_priority(owner);
    let waiter_priority = pi_kernel_priority(waiter);
    if waiter_priority >= owner_priority {
        return None;
    }
    let boosted = match waiter.class {
        SchedClass::Fifo | SchedClass::RoundRobin => SchedState {
            // Linux's PI boost uses the RT class without time slicing.
            class: SchedClass::Fifo,
            nice: 0,
            rt_priority: if waiter.rt_priority == 0 {
                1
            } else {
                waiter.rt_priority
            },
        },
        SchedClass::Normal | SchedClass::Batch | SchedClass::Idle => SchedState {
            class: owner.class,
            nice: if waiter.nice < owner.nice {
                waiter.nice
            } else {
                owner.nice
            },
            rt_priority: owner.rt_priority,
        },
        SchedClass::Deadline => return None,
    };
    Some(boosted)
}

/// Result of a queue-gated PI unlock publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiUnlockOutcome {
    /// A PI waiter was promoted and the user word now names it.
    HandedOff(u32),
    /// No PI waiter remained; the caller published the unowned word.
    Cleared,
    /// Nothing was published; the caller must retry or report its error.
    Aborted,
}

/// Result of a queue-gated PI requeue publication.
///
/// `futex_requeue()` (`kernel/futex/requeue.c:495-903`) distinguishes the case
/// where the source queue holds no waiter to promote from the case where the
/// publication raced against userspace, and from a source queue whose waiters
/// are not `FUTEX_WAIT_REQUEUE_PI` waiters: the first ends the syscall with the
/// waiter count it managed to move, only the second loops back to `retry`, and
/// the third is `-EINVAL`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiRequeueOutcome {
    /// The source queue has no live PI waiter, so there is nothing to promote
    /// and nothing to move. `futex_requeue()` skips the chain walk and returns
    /// its `task_count`, which is 0.
    NoWaiters,
    /// The first live waiter of the source queue is not a
    /// `FUTEX_WAIT_REQUEUE_PI` waiter, which `futex_requeue()` refuses with
    /// `-EINVAL` before it touches the target
    /// (`kernel/futex/requeue.c:314-315`).
    Invalid,
    /// The publication lost the race against userspace; retry the operation.
    Retry,
    /// The top waiter was woken and `moved` further waiters were requeued.
    Done {
        /// Waiters woken on the source queue (Linux `task_count`).
        woke: usize,
        /// Waiters moved onto the target queue (Linux `task_count`).
        moved: usize,
    },
}

/// What a successful `pi_requeue()` publication did to the target word.
///
/// These are the two non-error returns of `futex_proxy_trylock_atomic()`;
/// `futex_requeue()` counts the first as an already-satisfied wake and queues
/// the top waiter on the target's `rt_mutex` for the second
/// (`kernel/futex/requeue.c:543-588`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiRequeueTarget {
    /// The target was free and now names the promoted waiter, which is woken
    /// immediately: `futex_proxy_trylock_atomic()` returned 1.
    Promoted,
    /// The target already had an owner, so the publication only set
    /// `FUTEX_WAITERS` over it (`futex_lock_pi_atomic()`,
    /// `kernel/futex/pi.c:660-665`) and the promoted waiter joins the target's
    /// `rt_mutex` queue instead of being woken.
    Contended,
}

/// Outcome of a priority-inheritance wait.
///
/// Linux tells the two interruptions of `FUTEX_WAIT_REQUEUE_PI` apart by the
/// waiter's `futex_q::requeue_state`. A signal that arrives while the waiter is
/// still enqueued on `uaddr` (`Q_REQUEUE_PI_IGNORE`) makes
/// `handle_early_requeue_pi_wakeup()` return `-ERESTARTNOINTR`
/// (`kernel/futex/requeue.c:741-742`); a signal that arrives after
/// `FUTEX_CMP_REQUEUE_PI` moved the waiter onto the target's rt_mutex
/// (`Q_REQUEUE_PI_DONE`) leaves `rt_mutex_wait_proxy_lock()` reporting
/// `-EINTR`, which `futex_wait_requeue_pi()` rewrites to `-EWOULDBLOCK`
/// (`kernel/futex/requeue.c:895-902`). `requeued` carries that distinction out
/// of the wait queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PiWaitOutcome {
    /// `Ok(true)` when the waiter was woken, `Ok(false)` when the registration
    /// condition declined to wait, and `Err` when the wait ended in a fault,
    /// timeout, or signal.
    pub result: WaitConditionResult<bool>,
    /// This waiter was moved onto another queue by `pi_requeue()` before the
    /// wait ended (Linux `Q_REQUEUE_PI_DONE`).
    pub requeued: bool,
}

/// Priority-inheritance payload of one queued waiter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PiWaiter {
    /// TID this waiter publishes in the futex word when it becomes the owner.
    pub tid: u32,
    /// `rt_mutex_waiter::prio`, used to pick the top waiter and size the boost.
    pub priority: i16,
    /// Scheduling state handed to the owner through `pi_boost`.
    pub sched: SchedState,
}

impl PiWaiter {
    pub fn new(tid: u32, sched: SchedState) -> Self {
        Self {
            tid,
            priority: pi_kernel_priority(sched),
            sched,
        }
    }
}

/// Linux `struct futex_pi_state`, reduced to the state a single-level boost
/// needs: who owns the futex (`pi_state->owner`), and the scheduling state
/// that must be restored when the last PI waiter goes away.
pub struct PiState {
    inner: SpinNoIrq<PiStateInner>,
}

#[derive(Default)]
struct PiStateInner {
    /// TID currently published in the user word (`pi_state->owner`).
    owner_tid: u32,
    /// The live task named by `owner_tid`, when it is one of our threads.
    owner: Option<WeakAxTaskRef>,
    /// The owner's own scheduling state, captured before the first boost of
    /// this PI state.
    base: Option<SchedState>,
    /// The state this PI state installed, so a deboost can tell whether the
    /// owner replaced it (through `sched_setscheduler`) in the meantime.
    boosted: Option<SchedState>,
}

impl Default for PiState {
    fn default() -> Self {
        Self::new()
    }
}

impl PiState {
    fn new() -> Self {
        Self {
            inner: SpinNoIrq::new(PiStateInner::default()),
        }
    }

    /// TID the user word currently names.
    pub fn owner_tid(&self) -> u32 {
        self.inner.lock().owner_tid
    }

    /// Linux `attach_to_pi_owner()`: bind this PI state to the task named by
    /// the user word.
    ///
    /// Only the TID is recorded here. The task lookup is deferred to
    /// [`Self::resolve_owner`] because it must not run while a futex queue gate
    /// is held.
    pub fn attach(&self, owner_tid: u32) {
        let mut inner = self.inner.lock();
        if inner.owner_tid != owner_tid {
            // A different task owns the futex now, so the boost recorded for
            // the previous owner says nothing about this one.
            inner.base = None;
            inner.boosted = None;
        }
        inner.owner_tid = owner_tid;
        inner.owner = None;
    }

    /// Linux `pi_state_update_owner()`/`put_pi_state()`: the futex has no
    /// owner any more.
    pub fn detach(&self) {
        self.attach(0);
    }

    /// Live owner task, if the TID names one of our threads.
    ///
    /// A miss is not cached: the owner may not have been created yet when a
    /// `FUTEX_LOCK_PI` published `FUTEX_WAITERS` against a stale word, and
    /// Linux's `attach_to_pi_owner()` retries the lookup on the blocking path.
    ///
    /// `owner_tid` is the TID the user word holds, which is rendered in the
    /// caller's PID namespace, so the kernel-wide identity is resolved through
    /// `find_get_task_by_vpid()`'s namespace translation first.
    pub fn resolve_owner(&self) -> Option<AxTaskRef> {
        let (tid, cached) = {
            let inner = self.inner.lock();
            (
                inner.owner_tid,
                inner.owner.as_ref().and_then(WeakAxTaskRef::upgrade),
            )
        };
        if tid == 0 {
            return None;
        }
        if let Some(task) = cached {
            return Some(task);
        }
        let global_tid = current().as_thread().pid_ns().resolve_visible_pid(tid)?;
        let task = crate::task::get_visible_task(global_tid).ok()?;
        self.inner.lock().owner = Some(Arc::downgrade(&task));
        Some(task)
    }

    /// Records the pre-boost scheduling state of the owner once.
    fn remember_base(&self, current: SchedState) {
        let mut inner = self.inner.lock();
        if inner.base.is_none() {
            inner.base = Some(current);
        }
    }

    fn boosted(&self) -> Option<SchedState> {
        self.inner.lock().boosted
    }

    fn set_boosted(&self, state: Option<SchedState>) {
        self.inner.lock().boosted = state;
    }

    /// The state to restore when this PI state stops boosting, if the owner is
    /// still running the boost this PI state installed.
    pub fn deboost_target(&self, current: SchedState) -> Option<SchedState> {
        let mut inner = self.inner.lock();
        let boosted = inner.boosted.take()?;
        if boosted == current {
            inner.base.take()
        } else {
            // The owner's own `sched_setscheduler`, or a stronger boost from a
            // second PI futex, replaced this state. Leaving it alone can only
            // over-prioritise, never invert.
            inner.base = None;
            None
        }
    }
}

/// Applies Linux `rt_mutex_setprio()`'s boost to the owner of a PI futex.
///
/// The owner is raised to the stronger of its own scheduling state and the
/// waiter's, which is the same `min(p->normal_prio, pi_task->prio)` rule and is
/// idempotent, so several waiters converge on the strongest one without a
/// separate priority list. A missing or already-exited owner is not an error:
/// the user word remains authoritative for the protocol.
pub fn pi_boost_owner(pi_state: &PiState, waiter: &PiWaiter) {
    let Some(owner) = pi_state.resolve_owner() else {
        return;
    };
    let current = axtask::sched_state(&owner);
    let Some(target) = pi_boost(current, waiter.sched) else {
        return;
    };
    pi_state.remember_base(current);
    if axtask::set_sched_state(&owner, target).is_ok() {
        pi_state.set_boosted(Some(target));
    }
}

/// Restores the owner's own scheduling state once this PI state stops
/// boosting it (`rt_mutex_adjust_prio_chain()`'s deboost).
pub fn pi_deboost_owner(pi_state: &PiState) {
    let Some(owner) = pi_state.resolve_owner() else {
        pi_state.detach();
        return;
    };
    let current = axtask::sched_state(&owner);
    if let Some(base) = pi_state.deboost_target(current) {
        let _ = axtask::set_sched_state(&owner, base);
    }
}

struct WaiterEntry {
    bitset: u32,
    awakened: bool,
    cancelled: bool,
    owner: WaiterOwner,
    task: WeakAxTaskRef,
    waker: Option<Waker>,
    next: Option<WaiterPtr>,
    /// Priority-inheritance payload. `Some` only for a waiter queued by
    /// `FUTEX_LOCK_PI`/`FUTEX_WAIT_REQUEUE_PI`; it carries the `rt_mutex_waiter`
    /// half of Linux's `struct futex_q` (`kernel/futex/pi.c`).
    pi: Option<PiWaiter>,
    /// Linux `futex_q::requeue_state == Q_REQUEUE_PI_DONE`: a
    /// `FUTEX_CMP_REQUEUE_PI` moved this waiter from the source queue onto the
    /// target's queue, so it now blocks on the target's rt_mutex. Set and read
    /// under this entry's lock, which is the same lock `pi_requeue()` holds
    /// while it moves the node.
    requeued_pi: bool,
}

impl WaiterEntry {
    /// Whether this waiter carries priority-inheritance state, which makes it
    /// ineligible for a non-PI wake or requeue.
    ///
    /// Linux tests `this->pi_state || this->rt_waiter` (`kernel/futex/requeue.c`
    /// and `kernel/futex/waitwake.c`); this kernel's single-level reduction of
    /// `struct futex_pi_state` stores both in [`WaiterEntry::pi`], exactly as
    /// [`WaitQueue::pi_top_locked`] does for the plain wake path.
    fn is_pi(&self) -> bool {
        self.pi.is_some()
    }
}

impl WaiterQueue {
    fn push_back(&mut self, waiter: WaiterRef) {
        let ptr = NonNull::from(waiter.as_ref());
        waiter.lock().next = None;
        // Transfer the caller's queue-owned strong reference into the raw
        // intrusive link. The caller normally passes `Arc::clone`, retaining
        // the wait registration's independent ownership.
        let _ = Arc::into_raw(waiter);
        if let Some(tail) = self.tail {
            // SAFETY: every linked node owns a strong reference and remains
            // live until it is popped or detached.
            unsafe { tail.as_ref() }.lock().next = Some(ptr);
        } else {
            self.head = Some(ptr);
        }
        self.tail = Some(ptr);
        self.len += 1;
    }

    fn pop_front(&mut self) -> Option<WaiterRef> {
        let ptr = self.head?;
        // SAFETY: `ptr` is the queue-owned strong reference at the head.
        let waiter = unsafe { Arc::from_raw(ptr.as_ptr()) };
        let next = waiter.lock().next.take();
        self.head = next;
        if self.head.is_none() {
            self.tail = None;
        }
        self.len = self
            .len
            .checked_sub(1)
            .expect("futex waiter count underflow");
        Some(waiter)
    }

    fn remove(&mut self, target: WaiterPtr, deferred: &mut DeferredWaiters) -> bool {
        let mut previous: Option<WaiterPtr> = None;
        let mut cursor = self.head;
        while let Some(ptr) = cursor {
            // SAFETY: the queue owns a strong reference for every linked
            // pointer, so inspecting its link is valid while this queue is
            // exclusively locked.
            let next = unsafe { ptr.as_ref() }.lock().next;
            if ptr == target {
                if let Some(previous) = previous {
                    // SAFETY: `previous` is still linked and owned by this
                    // queue; only its next link is being rewritten.
                    unsafe { previous.as_ref() }.lock().next = next;
                } else {
                    self.head = next;
                }
                if self.tail == Some(ptr) {
                    self.tail = previous;
                }
                self.len = self
                    .len
                    .checked_sub(1)
                    .expect("futex waiter count underflow");
                // SAFETY: convert the queue-owned strong reference exactly
                // once, then defer its final drop until the gate is released.
                let waiter = unsafe { Arc::from_raw(ptr.as_ptr()) };
                deferred.push(waiter);
                return true;
            }
            previous = Some(ptr);
            cursor = next;
        }
        false
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrows the head waiter without unlinking it.
    ///
    /// The PI rejection of `futex_requeue()` stops the scan *before* the
    /// offending waiter is woken or requeued, so the waiter must be inspected
    /// while it is still in place.
    fn peek_front(&self) -> Option<WaiterRef> {
        let ptr = self.head?;
        // SAFETY: the queue owns a strong reference for every linked node, so
        // one more count cannot outlive the allocation.
        unsafe { Arc::increment_strong_count(ptr.as_ptr()) };
        // SAFETY: the count incremented above is owned by the returned `Arc`.
        Some(unsafe { Arc::from_raw(ptr.as_ptr()) })
    }

    /// Whether the waiter at the head would stop a non-PI wake/requeue scan.
    ///
    /// kernel/futex/requeue.c:606-609:
    ///     if ((requeue_pi && !this->rt_waiter) ||
    ///         (!requeue_pi && this->rt_waiter) ||
    ///         this->pi_state) {
    ///             ret = -EINVAL;
    ///             break;
    ///     }
    /// Only a waiter which matches this queue's key, is still live, and would
    /// still be inside the wake+requeue budget is examined, mirroring the
    /// `futex_match()`/`task_count` guards of the same loop.
    fn front_rejects_pi(&self, mask: u32) -> bool {
        let Some(waiter) = self.peek_front() else {
            return false;
        };
        let waiter = waiter.lock();
        !waiter.cancelled && (waiter.bitset & mask) != 0 && waiter.is_pi()
    }

    /// Whether the first live waiter of this queue is *not* a
    /// `FUTEX_WAIT_REQUEUE_PI` waiter.
    ///
    /// `futex_proxy_trylock_atomic()` promotes `futex_top_waiter()`, Linux's
    /// priority-ordered `plist` head, and refuses the whole operation with
    /// `-EINVAL` when that waiter has no `rt_waiter`
    /// (`kernel/futex/requeue.c:305-316`). This queue keeps FIFO order and
    /// records no priority for plain waiters, so its head stands in for the
    /// plist head; a plain waiter queued behind a PI waiter is caught by the
    /// scan instead (`kernel/futex/requeue.c:605-610`).
    fn front_is_plain(&self) -> bool {
        let mut cursor = self.head;
        let mut seen = 0;
        while let Some(ptr) = cursor {
            if seen >= self.len {
                break;
            }
            seen += 1;
            // SAFETY: the queue owns a strong reference for every linked
            // pointer and the queue lock is held for the whole walk.
            let node = unsafe { ptr.as_ref() };
            let waiter = node.lock();
            cursor = waiter.next;
            if !waiter.cancelled {
                return !waiter.is_pi();
            }
        }
        false
    }
}

impl Drop for WaiterQueue {
    fn drop(&mut self) {
        // A live futex entry should be empty before destruction. Drain raw
        // ownership defensively so an invariant violation cannot leak the
        // waiter arcs; this path never runs with a queue gate held.
        while let Some(waiter) = self.pop_front() {
            drop(waiter);
        }
    }
}

struct WaitFuture<'a> {
    waiter: &'a Arc<SpinNoIrq<WaiterEntry>>,
}

impl Unpin for WaitFuture<'_> {}

struct WaitRegistration {
    waiter: Option<Arc<SpinNoIrq<WaiterEntry>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitTerminalOwnership {
    Woken,
    Cancelled,
}

impl WaitRegistration {
    /// Resolves the waiter's terminal owner before a timeout, interruption, or
    /// setup error escapes to the Linux adapter.
    ///
    /// Wake and cancellation both linearize under `WaiterEntry`'s IRQ-safe
    /// lock. Once cancellation is published, requeue observes it and can no
    /// longer move this waiter, so queue cleanup needs only the captured owner
    /// rather than an unbounded owner-chasing retry loop.
    fn resolve_terminal(&mut self) -> WaitTerminalOwnership {
        self.resolve_terminal_requeued().0
    }

    /// Resolves the waiter and additionally reports whether a
    /// `FUTEX_CMP_REQUEUE_PI` had already moved it onto the target's queue
    /// (Linux `Q_REQUEUE_PI_DONE`).
    fn resolve_terminal_requeued(&mut self) -> (WaitTerminalOwnership, bool) {
        let waiter = self.waiter.take().expect("live futex registration");
        resolve_waiter_terminal(waiter)
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            let _ = resolve_waiter_terminal(waiter);
        }
    }
}

struct WaitAnyFuture<'a> {
    waiters: &'a [WaitRegistration],
}

impl Unpin for WaitAnyFuture<'_> {}

impl Future for WaitFuture<'_> {
    type Output = AxResult<bool>;

    fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Waker cloning may retain executor/task state and is not IRQ-safe.
        // Prepare the candidate before entering the waiter SpinNoIrq lock;
        // the lock only decides whether ownership is published.
        let mut candidate = Some(cx.waker().clone());
        let (awakened, retired_waker) = {
            let mut waiter = self.waiter.lock();
            if waiter.awakened {
                (true, None)
            } else if waiter
                .waker
                .as_ref()
                .is_none_or(|registered| !registered.will_wake(cx.waker()))
            {
                (
                    false,
                    waiter
                        .waker
                        .replace(candidate.take().expect("prepared futex waker")),
                )
            } else {
                (false, None)
            }
        };
        // Both the replaced waker and an unused candidate may carry the last
        // reference to arbitrary executor state. Retire them after leaving the
        // IRQ-disabled waiter lock.
        drop(retired_waker);
        drop(candidate);
        if awakened {
            return Poll::Ready(Ok(true));
        }
        Poll::Pending
    }
}

impl Future for WaitAnyFuture<'_> {
    type Output = AxResult<usize>;

    fn poll(self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        for (index, waiter) in self.waiters.iter().enumerate() {
            let Some(waiter) = waiter.waiter.as_ref() else {
                continue;
            };
            // Prepare the clone before taking the IRQ-safe waiter lock. The
            // lock must only compare or publish this already-owned candidate.
            let mut candidate = Some(cx.waker().clone());
            let (awakened, retired_waker) = {
                let mut waiter = waiter.lock();
                if waiter.awakened {
                    (true, None)
                } else if waiter
                    .waker
                    .as_ref()
                    .is_none_or(|registered| !registered.will_wake(cx.waker()))
                {
                    (
                        false,
                        waiter
                            .waker
                            .replace(candidate.take().expect("prepared futex waker")),
                    )
                } else {
                    (false, None)
                }
            };
            // Retire both ownership paths outside SpinNoIrq: the old
            // registration and an unused candidate are equally capable of
            // running arbitrary executor cleanup on drop.
            drop(retired_waker);
            drop(candidate);
            if awakened {
                return Poll::Ready(Ok(index));
            }
        }

        Poll::Pending
    }
}

fn clear_waiter_proc_state(task: &WeakAxTaskRef) {
    if let Some(task) = task.upgrade()
        && let Some(thread) = task.try_as_thread()
    {
        thread.set_proc_state_hint(ProcStateHint::None);
    }
}

fn resolve_waiter_terminal(waiter: Arc<SpinNoIrq<WaiterEntry>>) -> (WaitTerminalOwnership, bool) {
    // Mark the waiter as cancelled first so a concurrent wake either already
    // owns completion or observes cancellation and does not count this waiter.
    // Requeue tests `cancelled` while holding the same waiter lock, so the
    // captured owner cannot change after this point.
    let (owner, task, was_pi, requeued) = {
        let mut waiter = waiter.lock();
        if waiter.awakened {
            return (WaitTerminalOwnership::Woken, waiter.requeued_pi);
        }
        waiter.cancelled = true;
        (
            waiter.owner.clone(),
            waiter.task.clone(),
            waiter.pi.is_some(),
            waiter.requeued_pi,
        )
    };
    // Upgrade/drop the task reference only after the waiter SpinNoIrq lock
    // has been released.  This keeps all possible task destruction out of
    // IRQ-disabled sections as well as out of queue gates.
    clear_waiter_proc_state(&task);

    let owner_entry = owner.entry.upgrade();
    if let Some(owner_entry) = owner_entry.as_ref() {
        let mut deferred = DeferredWaiters::default();
        {
            let _gate = owner_entry.wq.gate.lock();
            // No requeue can change `owner` after `cancelled` was published. A
            // requeue that already held both queue gates will instead discard the
            // cancelled waiter before this gate is acquired, making removal a
            // harmless no-op.
            debug_assert!({
                let waiter_entry = waiter.lock();
                waiter_entry.cancelled
                    && waiter_entry.owner.entry.as_ptr() == Arc::as_ptr(owner_entry)
            });
            let mut queue = owner_entry.wq.queue.lock();
            queue.remove(NonNull::from(waiter.as_ref()), &mut deferred);
        }
        deferred.finish();
    }

    // Do this after the queue gate, the deferred waiter drops, and the local
    // owner entry reference have all gone away.  This is the cancellation path
    // that removes a target entry when its last waiter disappears.
    if was_pi && let Some(owner_entry) = owner_entry.as_ref() {
        // Linux `rt_mutex_cleanup_proxy_lock()`: a PI waiter that leaves
        // without being handed the futex may be the last reason the owner
        // carries a boost, so the chain is re-evaluated on the way out.
        owner_entry.release_pi_state_if_idle();
    }
    drop(owner_entry);
    owner.cleanup_if_idle();

    (WaitTerminalOwnership::Cancelled, requeued)
}

fn resolve_single_wait(
    registration: &mut WaitRegistration,
    result: AxResult<bool>,
) -> (AxResult<bool>, bool) {
    let (ownership, requeued) = registration.resolve_terminal_requeued();
    let result = match ownership {
        WaitTerminalOwnership::Woken => Ok(true),
        WaitTerminalOwnership::Cancelled => result,
    };
    (result, requeued)
}

fn resolve_wait_any(
    registrations: &mut [WaitRegistration],
    result: AxResult<usize>,
) -> AxResult<usize> {
    let proposed = result.as_ref().ok().copied();
    let mut first_woken = None;
    let mut proposed_woken = false;

    for (index, registration) in registrations.iter_mut().enumerate() {
        if registration.resolve_terminal() == WaitTerminalOwnership::Woken {
            first_woken.get_or_insert(index);
            proposed_woken |= proposed == Some(index);
        }
    }

    if proposed_woken {
        Ok(proposed.expect("proposed futex waitv winner"))
    } else if let Some(index) = first_woken {
        Ok(index)
    } else {
        result
    }
}

fn resolve_wait_any_condition(
    registrations: &mut [WaitRegistration],
    result: WaitConditionResult<usize>,
) -> WaitConditionResult<usize> {
    let proposed = result.as_ref().ok().copied();
    let mut first_woken = None;
    let mut proposed_woken = false;

    for (index, registration) in registrations.iter_mut().enumerate() {
        if registration.resolve_terminal() == WaitTerminalOwnership::Woken {
            first_woken.get_or_insert(index);
            proposed_woken |= proposed == Some(index);
        }
    }

    if proposed_woken {
        Ok(proposed.expect("proposed futex waitv winner"))
    } else if let Some(index) = first_woken {
        Ok(index)
    } else {
        result
    }
}

async fn wait_with_prepared_clock_timeout<F, T>(
    wait: F,
    mut sleeper: Option<&mut PreparedClockSleep>,
) -> AxResult<T>
where
    F: Future<Output = AxResult<T>>,
{
    let mut wait = core::pin::pin!(wait);
    poll_fn(|cx| {
        if let Poll::Ready(result) = wait.as_mut().poll(cx) {
            return Poll::Ready(result);
        }
        let Some(sleeper) = sleeper.as_mut() else {
            return Poll::Pending;
        };
        if Pin::new(&mut **sleeper).poll(cx).is_pending() {
            return Poll::Pending;
        }

        // A futex wake that linearized in the same observation window wins
        // over the timeout, matching the completion-first interrupt path.
        match wait.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => Poll::Ready(Err(AxError::TimedOut)),
        }
    })
    .await
}

impl WaitQueue {
    /// Creates a new `WaitQueue`.
    pub fn new() -> Self {
        Self::default()
    }

    fn wake_and_requeue_locked(
        src: &mut WaiterQueue,
        wake_count: usize,
        mask: u32,
        mut requeue: Option<RequeueTarget<'_>>,
        reject_pi: bool,
        pending_wakers: &mut WakeBatch,
        retired: &mut DeferredWaiters,
    ) -> Result<(usize, usize), ()> {
        let mut woke = 0;
        let mut moved = 0;
        // Kept waiters are appended back to `src`; bound the scan to the
        // entries present when the gate was acquired so a non-woken waiter
        // cannot make this drain loop revisit itself forever.
        let initial_len = src.len;
        let requeue_limit = requeue.as_ref().map_or(0, |(_, limit, _)| *limit);

        for _ in 0..initial_len {
            // `futex_requeue()` refuses to wake or requeue a waiter that still
            // carries PI state (kernel/futex/requeue.c:606-609) and `break`s
            // out of the scan, keeping the waiters it already woke or moved:
            // the caller's `out_unlock` path still runs `wake_up_q(&wake_q)`.
            // Its loop stops at the same point this budget does:
            //     if (task_count - nr_wake >= nr_requeue)
            //             break;
            if reject_pi && woke + moved < wake_count + requeue_limit && src.front_rejects_pi(mask)
            {
                return Err(());
            }
            let Some(waiter) = src.pop_front() else {
                break;
            };
            enum Action {
                Drop,
                Keep,
                Requeue,
            }

            let action = {
                let mut waiter = waiter.lock();
                if waiter.cancelled {
                    Action::Drop
                } else if woke < wake_count && (waiter.bitset & mask) != 0 {
                    waiter.awakened = true;
                    woke += 1;
                    Action::Drop
                } else if let Some((_, limit, target_owner)) = requeue.as_mut()
                    && moved < *limit
                {
                    waiter.owner = (*target_owner).clone();
                    moved += 1;
                    Action::Requeue
                } else {
                    Action::Keep
                }
            };

            match action {
                Action::Drop => {
                    let awakened = waiter.lock().awakened;
                    if awakened {
                        pending_wakers.push(waiter);
                    } else {
                        retired.push(waiter);
                    }
                }
                Action::Keep => src.push_back(waiter),
                Action::Requeue => {
                    let (dst, ..) = requeue.as_mut().unwrap();
                    dst.push_back(waiter);
                }
            }
        }

        Ok((woke, moved))
    }

    /// Applies the wake/requeue accounting for a source and target which are
    /// the same queue. Linux still counts the waiters covered by
    /// `nr_requeue` even though there is no physical queue move in this case.
    fn wake_and_requeue_same_locked(
        src: &mut WaiterQueue,
        wake_count: usize,
        requeue_count: usize,
        mask: u32,
        reject_pi: bool,
        pending_wakers: &mut WakeBatch,
        retired: &mut DeferredWaiters,
    ) -> Result<(usize, usize), ()> {
        let mut woke = 0;
        let mut moved = 0;
        // As in the cross-key path, kept waiters are reinserted at the tail.
        // Only process the queue population that existed at the linearization
        // point; otherwise a kept node would be processed indefinitely.
        let initial_len = src.len;

        for _ in 0..initial_len {
            // See `wake_and_requeue_locked`: the PI rejection of
            // `futex_requeue()` (kernel/futex/requeue.c:606-609) applies while
            // the scan still has wake or requeue budget left.
            if reject_pi && woke + moved < wake_count + requeue_count && src.front_rejects_pi(mask)
            {
                return Err(());
            }
            let Some(waiter) = src.pop_front() else {
                break;
            };
            let action = {
                let mut waiter = waiter.lock();
                if waiter.cancelled {
                    0
                } else if woke < wake_count && (waiter.bitset & mask) != 0 {
                    waiter.awakened = true;
                    woke += 1;
                    1
                } else {
                    if moved < requeue_count {
                        moved += 1;
                    }
                    2
                }
            };

            match action {
                0 => retired.push(waiter),
                1 => pending_wakers.push(waiter),
                _ => src.push_back(waiter),
            }
        }

        Ok((woke, moved))
    }

    /// Publishes a non-PI waiter.  The entry carries no `rt_mutex_waiter`
    /// payload: [`Self::register_pi_waiter_if_condition`] is the only way to
    /// publish one, and it does so under this same gate.
    fn register_waiter_if_condition(
        &self,
        owner: WaiterOwner,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> WaitConditionResult<bool>,
    ) -> WaitConditionResult<Option<WaitRegistration>> {
        // Allocate the waiter before taking the IRQ-safe gate.  If the
        // condition rejects publication, the temporary strong reference is
        // dropped only after the gate has been released.
        let waiter = Arc::try_new(SpinNoIrq::new(WaiterEntry {
            bitset,
            awakened: false,
            cancelled: false,
            owner,
            task: Arc::downgrade(&current()),
            waker: None,
            next: None,
            pi: None,
            requeued_pi: false,
        }))
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
        let registration_waiter = waiter.clone();
        let mut waiter = Some(waiter);
        let result = {
            let _gate = self.gate.lock();
            match condition() {
                Err(error) => Err(error),
                Ok(false) => Ok(None),
                Ok(true) if timeout.is_some_and(|(clock, deadline)| clock.now() >= deadline) => {
                    Err(WaitConditionError::Fault(AxError::TimedOut))
                }
                Ok(true) => {
                    self.queue
                        .lock()
                        .push_back(waiter.take().expect("unpublished futex waiter"));
                    // The waiter belongs to the current task.  Inspecting
                    // that task through the per-CPU current-task reference
                    // does not create an Arc, so publication cannot drop a
                    // task reference while the queue gate is held.
                    if let Some(thread) = current().try_as_thread() {
                        thread.set_proc_state_hint(ProcStateHint::Interruptible);
                    }
                    Ok(Some(WaitRegistration {
                        waiter: Some(registration_waiter),
                    }))
                }
            }
        };
        // The queue owns the publication reference; an unaccepted condition
        // retains the preallocated reference until this gate scope ends.
        drop(waiter);
        result
    }

    fn register_waiter_if(
        &self,
        owner: Weak<FutexEntry>,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> AxResult<bool>,
    ) -> AxResult<Option<WaitRegistration>> {
        self.register_waiter_if_condition(
            WaiterOwner::without_table(owner, FutexTableKey::Private(0)),
            bitset,
            timeout,
            || condition().map_err(WaitConditionError::Fault),
        )
        .map_err(AxError::from)
    }

    /// Blocks the current task on a futex queue until the waiter is woken.
    ///
    /// Shared by the plain and the priority-inheritance wait paths; the
    /// registration has already been published under the queue gate. The second
    /// element of the result reports whether the waiter had been moved onto
    /// another queue by `pi_requeue()`.
    fn block_registered(
        &self,
        registration: &mut WaitRegistration,
        timeout: Option<(AlarmClock, Duration)>,
    ) -> (WaitConditionResult<bool>, bool) {
        let wait = WaitFuture {
            waiter: registration
                .waiter
                .as_ref()
                .expect("registered futex waiter"),
        };
        let mut sleeper = match timeout {
            Some((clock, deadline)) => match prepare_clock_sleep(clock, deadline) {
                Ok(sleeper) => Some(sleeper),
                Err(error) => {
                    let (result, requeued) = resolve_single_wait(registration, Err(error));
                    return (result.map_err(WaitConditionError::Fault), requeued);
                }
            },
            None => None,
        };
        let result = {
            let wait = wait_with_prepared_clock_timeout(wait, sleeper.as_mut());
            let curr = current();
            let mut wait = core::pin::pin!(wait);
            block_on(poll_fn(|cx| {
                if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                    return Poll::Ready(result);
                }
                if curr.poll_interrupt(cx).is_ready() {
                    return Poll::Ready(Err(AxError::Interrupted));
                }
                if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                    return Poll::Ready(result);
                }
                Poll::Pending
            }))
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => Err(AxError::from(error)),
        };
        let (result, requeued) = resolve_single_wait(registration, result);
        (result.map_err(WaitConditionError::Fault), requeued)
    }

    /// Waits if the given condition is met.
    ///
    /// Returns `false` if the condition is not met and no actual waiting
    /// occurs.
    ///
    /// The condition callback runs under the queue gate. It must therefore be
    /// a bounded, nonblocking, nonallocating snapshot; a raced user-memory
    /// snapshot must return [`WaitConditionError::Retry`] so the caller can
    /// fault and retry after this method releases the gate.
    pub fn wait_if(
        &self,
        owner: WaiterOwner,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> WaitConditionResult<bool>,
    ) -> WaitConditionResult<bool> {
        // Registration may fault while evaluating `condition` and therefore
        // happens before the synchronous block session starts. From this point
        // on, polling and wakeup touch only the waiter's IRQ-safe state.
        let Some(mut registration) =
            self.register_waiter_if_condition(owner, bitset, timeout, condition)?
        else {
            return Ok(false);
        };
        self.block_registered(&mut registration, timeout).0
    }

    /// [`Self::register_waiter_if_condition`] for a priority-inheritance wait.
    ///
    /// A plain waiter's entry is complete when it is allocated, so
    /// `register_waiter_if_condition` can take its `pi` payload as an argument.
    /// A PI waiter's `rt_mutex_waiter` payload does not exist until the
    /// condition has classified the user word, so it is installed in the queue
    /// entry *under the queue gate*, immediately before publication. That
    /// ordering is what Linux does: `futex_lock_pi_atomic()` fills in
    /// `q.rt_waiter` and `rt_mutex_enqueue()` links it while the futex hash
    /// bucket is locked (`kernel/futex/pi.c:906-940`), so the first
    /// `futex_top_waiter()` after the bucket lock is dropped already sees it.
    /// Publishing the payload after the gate is released instead leaves a
    /// window in which `wake_futex_pi()` (`kernel/futex/pi.c:1090-1100`) finds
    /// `futex_top_waiter() == NULL`, publishes no handoff, and the waiter
    /// sleeps until its timeout or a signal.
    ///
    /// `Ok(Some(pi))` publishes the entry carrying `pi`, `Ok(None)` declines
    /// the wait (the caller has already taken the futex itself), and `Err`
    /// reports a faulted or unavailable snapshot.
    fn register_pi_waiter_if_condition(
        &self,
        owner: WaiterOwner,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> WaitConditionResult<Option<PiWaiter>>,
    ) -> WaitConditionResult<Option<(WaitRegistration, PiWaiter)>> {
        // Allocate the waiter before taking the IRQ-safe gate.  If the
        // condition rejects publication, the temporary strong reference is
        // dropped only after the gate has been released.
        let waiter = Arc::try_new(SpinNoIrq::new(WaiterEntry {
            bitset,
            awakened: false,
            cancelled: false,
            owner,
            task: Arc::downgrade(&current()),
            waker: None,
            next: None,
            pi: None,
            requeued_pi: false,
        }))
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
        let registration_waiter = waiter.clone();
        let mut waiter = Some(waiter);
        let result = {
            let _gate = self.gate.lock();
            match condition() {
                Err(error) => Err(error),
                Ok(None) => Ok(None),
                Ok(Some(_)) if timeout.is_some_and(|(clock, deadline)| clock.now() >= deadline) => {
                    Err(WaitConditionError::Fault(AxError::TimedOut))
                }
                Ok(Some(pi)) => {
                    // The payload must be visible to `pi_top_locked()` before
                    // the entry can be found in the queue.
                    waiter.as_ref().expect("unpublished futex waiter").lock().pi = Some(pi);
                    self.queue
                        .lock()
                        .push_back(waiter.take().expect("unpublished futex waiter"));
                    // The waiter belongs to the current task.  Inspecting
                    // that task through the per-CPU current-task reference
                    // does not create an Arc, so publication cannot drop a
                    // task reference while the queue gate is held.
                    if let Some(thread) = current().try_as_thread() {
                        thread.set_proc_state_hint(ProcStateHint::Interruptible);
                    }
                    Ok(Some((
                        WaitRegistration {
                            waiter: Some(registration_waiter),
                        },
                        pi,
                    )))
                }
            }
        };
        // The queue owns the publication reference; an unaccepted condition
        // retains the preallocated reference until this gate scope ends.
        drop(waiter);
        result
    }

    /// Publishes a priority-inheritance waiter and blocks.
    ///
    /// The condition runs under the queue gate exactly like [`Self::wait_if`],
    /// but returns the waiter's `rt_mutex_waiter` payload instead of a bare
    /// bool: `Ok(Some(pi))` publishes and blocks, `Ok(None)` declines the wait
    /// (the caller has already taken the futex itself), and `Err` reports a
    /// faulted or unavailable snapshot.
    ///
    /// `on_queued` runs after the gate has been released and before the task
    /// sleeps. That is where the PI boost is applied: the owner must already be
    /// reachable through the queue, but neither the scheduler update nor the
    /// owner task lookup may run under the futex gate, because both take locks
    /// a run-queue or registry owner may hold. Returning `Err` abandons the
    /// wait: the registration is cancelled and the error is propagated, which
    /// is how `attach_to_pi_owner()`'s `-ESRCH`/`-EAGAIN` are surfaced without
    /// ever sleeping on a futex whose owner cannot hand it over.
    pub fn wait_pi<C, A>(
        &self,
        owner: WaiterOwner,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: C,
        on_queued: A,
    ) -> PiWaitOutcome
    where
        C: FnOnce() -> WaitConditionResult<Option<PiWaiter>>,
        A: FnOnce(&PiWaiter) -> AxResult<()>,
    {
        let registration = self.register_pi_waiter_if_condition(owner, bitset, timeout, condition);
        let (mut registration, published) = match registration {
            Ok(Some(published)) => published,
            Ok(None) => {
                return PiWaitOutcome {
                    result: Ok(false),
                    requeued: false,
                };
            }
            Err(error) => {
                return PiWaitOutcome {
                    result: Err(error),
                    requeued: false,
                };
            }
        };
        if let Err(error) = on_queued(&published) {
            let _ = registration.resolve_terminal();
            return PiWaitOutcome {
                result: Err(WaitConditionError::Fault(error)),
                requeued: false,
            };
        }
        let (result, requeued) = self.block_registered(&mut registration, timeout);
        PiWaitOutcome { result, requeued }
    }

    /// The highest-priority live PI waiter, mirroring `rt_mutex_top_waiter()`.
    /// The queue is not a `plist`, so the walk is O(n); the selection rule
    /// (smallest `rt_mutex_waiter::prio`, FIFO on ties) is the same.
    pub fn pi_top_tid(&self) -> Option<u32> {
        let queue = self.queue.lock();
        Self::pi_top_locked(&queue).map(|(_, pi)| pi.tid)
    }

    fn pi_top_locked(queue: &WaiterQueue) -> Option<(WaiterPtr, PiWaiter)> {
        let mut cursor = queue.head;
        let mut best: Option<(WaiterPtr, PiWaiter)> = None;
        let mut seen = 0;
        while let Some(ptr) = cursor {
            if seen >= queue.len {
                break;
            }
            seen += 1;
            // SAFETY: the queue owns a strong reference for every linked
            // pointer and the queue lock is held for the whole walk.
            let node = unsafe { ptr.as_ref() };
            let waiter = node.lock();
            cursor = waiter.next;
            if let Some(pi) = waiter.pi
                && !waiter.cancelled
                && best.is_none_or(|(_, current)| pi.priority < current.priority)
            {
                best = Some((ptr, pi));
            }
        }
        best
    }

    /// Runs `body` with both queue gates held in address order.
    ///
    /// Two PI queues are never the same queue: `FUTEX_CMP_REQUEUE_PI` rejects
    /// equal addresses with `EINVAL`. Locking in pointer order keeps two
    /// concurrent requeues between the same pair from deadlocking.
    fn with_two_gates<T>(a: &Self, b: &Self, body: impl FnOnce() -> T) -> T {
        if core::ptr::eq(a, b) {
            let _a = a.gate.lock();
            body()
        } else if (a as *const Self as usize) < (b as *const Self as usize) {
            let _a = a.gate.lock();
            let _b = b.gate.lock();
            body()
        } else {
            let _b = b.gate.lock();
            let _a = a.gate.lock();
            body()
        }
    }

    /// Linux `__futex_unlock_pi()`: publish the next state of the futex word
    /// and, if a PI waiter is queued, hand the futex to the highest-priority
    /// one (`wake_futex_pi()`).
    ///
    /// `publish` runs *inside* the queue gate — that is what makes the word
    /// transition and the dequeue a single linearization point, so no
    /// `FUTEX_LOCK_PI` can slip a waiter in between. It is called with
    /// `Some(tid)` when a waiter must be promoted and `None` when the futex
    /// becomes unowned; returning `false` means the word changed under us
    /// (`wake_futex_pi()`'s retry) and nothing is claimed. The candidate
    /// waiter's entry lock is held across the call, so a concurrent
    /// cancellation — which marks `cancelled` under that same lock — cannot
    /// make the publication name a waiter that has already left.
    pub fn pi_unlock<P>(&self, mut publish: P) -> PiUnlockOutcome
    where
        P: FnMut(Option<u32>) -> bool,
    {
        let mut pending_wakers = WakeBatch::default();
        // No node is retired on this path: the promoted waiter is woken and the
        // rest stay queued.
        let retired = DeferredWaiters::default();
        let mut outcome = PiUnlockOutcome::Aborted;
        {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            loop {
                match Self::pi_top_locked(&queue) {
                    None => {
                        if publish(None) {
                            outcome = PiUnlockOutcome::Cleared;
                        }
                        break;
                    }
                    Some((target, pi)) => {
                        // SAFETY: `target` is linked in `queue`, whose lock is
                        // held, so the queue-owned strong reference is live.
                        let mut entry = unsafe { target.as_ref() }.lock();
                        if entry.cancelled {
                            // The top waiter was cancelled after the selection
                            // walk observed it; pick the next candidate instead
                            // of publishing a word that names a dead waiter.
                            drop(entry);
                            continue;
                        }
                        if !publish(Some(pi.tid)) {
                            break;
                        }
                        entry.awakened = true;
                        drop(entry);
                        let initial_len = queue.len;
                        for _ in 0..initial_len {
                            let Some(waiter) = queue.pop_front() else {
                                break;
                            };
                            if core::ptr::eq(Arc::as_ptr(&waiter), target.as_ptr()) {
                                pending_wakers.push(waiter);
                                outcome = PiUnlockOutcome::HandedOff(pi.tid);
                                break;
                            }
                            queue.push_back(waiter);
                        }
                        break;
                    }
                }
            }
        }
        pending_wakers.finish();
        retired.finish();
        outcome
    }

    /// Linux `futex_requeue()`'s `requeue_pi` path.
    ///
    /// The top PI waiter of `self` is promoted onto `target` — `publish`
    /// installs the new target word while both gates are held and reports
    /// whether the target was free or already owned — and up to `nr_requeue` of
    /// the remaining PI waiters are moved onto `target`'s queue, where they keep
    /// their own `rt_mutex_waiter` payload and block until `target`'s unlock
    /// hands the futex to the strongest of them.
    ///
    /// `nr_wake` and `nr_requeue` are Linux's scan bound
    /// `task_count - nr_wake >= nr_requeue` (`kernel/futex/requeue.c:590-591`).
    /// `task_count` starts at 1 when the promotion took the target
    /// (`requeue.c:546-551`) and at 0 when the waiter was queued on an owner
    /// instead, so a contended target moves `nr_wake + nr_requeue` waiters and a
    /// free one moves the remaining `nr_wake + nr_requeue - 1`.
    ///
    /// The outcomes mirror the ways `futex_requeue()` can leave its `retry`
    /// loop: `NoWaiters` when `futex_proxy_trylock_atomic()` found nothing to
    /// promote, `Invalid` when the queue's waiters are not
    /// `FUTEX_WAIT_REQUEUE_PI` waiters, `Retry` after a lost publication race,
    /// and `Done` for the completed move.
    pub fn pi_requeue<P>(
        &self,
        target: &WaitQueue,
        target_owner: WaiterOwner,
        nr_wake: usize,
        nr_requeue: usize,
        mut publish: P,
    ) -> PiRequeueOutcome
    where
        P: FnMut(u32) -> Option<PiRequeueTarget>,
    {
        if core::ptr::eq(self, target) {
            // Unreachable: `futex_requeue()` rejects `key1 == key2` with
            // `-EINVAL` before it ever reaches the retry loop, so a caller
            // that gets here has already reported an error and this arm only
            // preserves "nothing was published, the caller must not report
            // success".
            return PiRequeueOutcome::Retry;
        }
        let mut pending_wakers = WakeBatch::default();
        // Waiters are either woken or moved; none is retired here.
        let retired = DeferredWaiters::default();
        let result = Self::with_two_gates(self, target, || {
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            // `futex_proxy_trylock_atomic()` inspects `futex_top_waiter()`
            // first and refuses the operation when that waiter is not a
            // `FUTEX_WAIT_REQUEUE_PI` waiter
            // (`kernel/futex/requeue.c:305-316`), so a queue whose head is a
            // plain waiter never reaches the promotion below.
            if src.front_is_plain() {
                return PiRequeueOutcome::Invalid;
            }
            // Claim the top waiter under its entry lock so a cancellation —
            // which marks `cancelled` under that same lock — cannot interleave
            // between the selection and the publication; a cancelled candidate
            // is skipped and the next-strongest waiter is selected instead.
            let (top, mode) = loop {
                let Some((top, pi)) = Self::pi_top_locked(&src) else {
                    return PiRequeueOutcome::NoWaiters;
                };
                // SAFETY: `top` is linked in `src`, whose lock is held, so the
                // queue-owned strong reference is live.
                let mut entry = unsafe { top.as_ref() }.lock();
                if entry.cancelled {
                    drop(entry);
                    continue;
                }
                let Some(mode) = publish(pi.tid) else {
                    return PiRequeueOutcome::Retry;
                };
                if mode == PiRequeueTarget::Promoted {
                    // `requeue_pi_wake_futex()`: the promoted waiter took the
                    // target and has to be woken.
                    entry.awakened = true;
                } else {
                    // `rt_mutex_start_proxy_lock()`: the promoted waiter joins
                    // the target's rt_mutex queue immediately
                    // (`kernel/futex/requeue.c:543-588`), so the strongest
                    // waiter is never stranded in the source queue when the
                    // requeue budget runs out.
                    entry.owner = target_owner.clone();
                    entry.requeued_pi = true;
                }
                drop(entry);
                break (top, mode);
            };
            let contended = mode == PiRequeueTarget::Contended;
            let mut woke = 0;
            let mut moved = 0;
            // Move the claimed top waiter out of the source queue, into the
            // wake batch when it took the target and onto the target queue
            // when it queued on the target's owner.
            let initial_len = src.len;
            for _ in 0..initial_len {
                let Some(waiter) = src.pop_front() else {
                    break;
                };
                if core::ptr::eq(Arc::as_ptr(&waiter), top.as_ptr()) {
                    if contended {
                        moved += 1;
                        dst.push_back(waiter);
                    } else {
                        woke = 1;
                        pending_wakers.push(waiter);
                    }
                    break;
                }
                src.push_back(waiter);
            }
            // `futex_requeue_pi_complete()`/`requeue_futex()`: the waiters that
            // follow the top one move onto the target's rt_mutex queue.
            let budget = if contended {
                nr_wake.saturating_add(nr_requeue)
            } else {
                nr_wake.saturating_add(nr_requeue).saturating_sub(1)
            };
            let initial_len = src.len;
            for _ in 0..initial_len {
                let Some(waiter) = src.pop_front() else {
                    break;
                };
                let mut entry = waiter.lock();
                if entry.cancelled {
                    drop(entry);
                    src.push_back(waiter);
                    continue;
                }
                if moved >= budget {
                    // `task_count - nr_wake >= nr_requeue` stops the scan and
                    // leaves the rest of the queue in place.
                    drop(entry);
                    src.push_back(waiter);
                    break;
                }
                if !entry.pi.is_some() {
                    // `(requeue_pi && !this->rt_waiter) || ... -> -EINVAL`
                    // (`kernel/futex/requeue.c:605-610`).
                    drop(entry);
                    src.push_back(waiter);
                    return PiRequeueOutcome::Invalid;
                }
                entry.owner = target_owner.clone();
                // Linux `futex_requeue_pi_complete(this, 0)`: the waiter is
                // queued on the target's rt_mutex, which
                // `futex_wait_requeue_pi()` reports as `Q_REQUEUE_PI_DONE`.
                entry.requeued_pi = true;
                moved += 1;
                drop(entry);
                dst.push_back(waiter);
            }
            PiRequeueOutcome::Done { woke, moved }
        });
        pending_wakers.finish();
        retired.finish();
        result
    }

    /// Boosts `pi_state`'s owner by every PI waiter currently queued on this
    /// queue, as requeueing a waiter onto an owned target does.
    ///
    /// Linux's `rt_mutex_start_proxy_lock()` -> `task_blocks_on_rt_mutex()` ->
    /// `rt_mutex_adjust_prio_chain()` raises the target owner to the strongest
    /// waiter when `FUTEX_CMP_REQUEUE_PI` queues it
    /// (`kernel/locking/rtmutex.c:1236-1270`, called from
    /// `kernel/futex/requeue.c:658-661`). The boost is applied after the queue
    /// gates are released because it resolves and reschedules tasks.
    pub fn pi_boost_queued_waiters(&self, pi_state: &PiState) {
        let mut queued: Vec<WaiterRef> = Vec::new();
        {
            let _gate = self.gate.lock();
            let queue = self.queue.lock();
            let mut cursor = queue.head;
            let mut seen = 0;
            while let Some(ptr) = cursor {
                if seen >= queue.len {
                    break;
                }
                seen += 1;
                // SAFETY: the queue owns a strong reference for every linked
                // pointer and the queue lock is held for the whole walk.
                let node = unsafe { ptr.as_ref() };
                let waiter = node.lock();
                cursor = waiter.next;
                let boost = waiter.pi.is_some() && !waiter.cancelled;
                drop(waiter);
                if boost {
                    // SAFETY: one more count cannot outlive the allocation
                    // while the queue lock is held and the node is linked.
                    unsafe { Arc::increment_strong_count(ptr.as_ptr()) };
                    // SAFETY: the count incremented above is owned by this
                    // reference, which is dropped after the gates are released.
                    queued.push(unsafe { Arc::from_raw(ptr.as_ptr()) });
                }
            }
        }
        for waiter in &queued {
            if let Some(pi) = waiter.lock().pi {
                pi_boost_owner(pi_state, &pi);
            }
        }
    }

    /// Wakes up at most `count` tasks whose bitset intersects with the given
    /// bitmask.
    pub fn wake(&self, count: usize, mask: u32) -> usize {
        self.wake_inner(count, mask, false)
            .expect("plain futex wake")
    }

    /// `futex_wake()`'s waiter loop.
    ///
    /// Linux refuses to run a *plain* wake over a queue that holds
    /// priority-inheritance waiters (`if (this->pi_state || this->rt_waiter) {
    /// ret = -EINVAL; break; }`), because such a waiter is waiting for the
    /// owner to publish the futex through `wake_futex_pi()` and cannot be
    /// completed by a bare wakeup. `reject_pi` selects that behaviour; the
    /// PI paths themselves pass `false`.
    pub fn wake_inner(&self, count: usize, mask: u32, reject_pi: bool) -> Result<usize, ()> {
        let mut pending_wakers = WakeBatch::default();
        let mut retired = DeferredWaiters::default();
        let woke = {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            // The scan rejects only when the next waiter it would wake carries
            // PI state: the waiters already woken stay woken, like the
            // `break` in `futex_wake()`'s `hb_waiters_pending` walk which
            // still runs `wake_up_q(&wake_q)` on the way to `-EINVAL`.
            Self::wake_and_requeue_locked(
                &mut queue,
                count,
                mask,
                None,
                reject_pi,
                &mut pending_wakers,
                &mut retired,
            )
        };
        pending_wakers.finish();
        retired.finish();
        woke.map(|(woke, _)| woke)
    }

    /// Runs a nofault atomic operation under both queue gates, always waking
    /// the first queue and conditionally waking the second. Same-key calls
    /// consume successive waiters from one queue, never waking a waiter twice.
    pub fn wake_op<F>(
        &self,
        count: usize,
        target: &WaitQueue,
        target_count: usize,
        operation: F,
    ) -> WaitConditionResult<usize>
    where
        F: FnOnce() -> WaitConditionResult<bool>,
    {
        let mut pending = WakeBatch::default();
        let mut retired = DeferredWaiters::default();
        let woke = {
            let (first, second) =
                if (self as *const Self as usize) <= (target as *const Self as usize) {
                    (self, target)
                } else {
                    (target, self)
                };
            let _first_gate = first.gate.lock();
            let _second_gate = if core::ptr::eq(self, target) {
                None
            } else {
                Some(second.gate.lock())
            };
            let wake_target = operation()?;
            // `futex_wake_op()` (kernel/futex/waitwake.c:330-347) applies the
            // same PI rejection to both chains, and `goto out_unlock` keeps the
            // wakeups the first chain already recorded while skipping the
            // second chain entirely.
            let mut rejected = false;
            let first = Self::wake_and_requeue_locked(
                &mut self.queue.lock(),
                count,
                u32::MAX,
                None,
                true,
                &mut pending,
                &mut retired,
            );
            let woke = match first {
                Ok((woke, _)) => woke,
                Err(()) => {
                    rejected = true;
                    0
                }
            };
            let woke = woke
                + if wake_target && !rejected {
                    match Self::wake_and_requeue_locked(
                        &mut target.queue.lock(),
                        target_count,
                        u32::MAX,
                        None,
                        true,
                        &mut pending,
                        &mut retired,
                    ) {
                        Ok((woke, _)) => woke,
                        Err(()) => {
                            rejected = true;
                            0
                        }
                    }
                } else {
                    0
                };
            (woke, rejected)
        };
        pending.finish();
        retired.finish();
        let (woke, rejected) = woke;
        if rejected {
            return Err(WaitConditionError::Fault(AxError::InvalidInput));
        }
        Ok(woke)
    }

    /// Checks if the wait queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Requeue at most `count` tasks to the target wait queue.
    pub fn requeue(&self, count: usize, target: &WaitQueue, target_owner: WaiterOwner) -> usize {
        let mut pending_wakers = WakeBatch::default();
        let mut retired = DeferredWaiters::default();
        let moved = if core::ptr::eq(self, target) {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            let mut moved = 0;
            let initial_len = queue.len;
            for _ in 0..initial_len {
                let Some(waiter) = queue.pop_front() else {
                    break;
                };
                if waiter.lock().cancelled {
                    retired.push(waiter);
                } else if moved < count {
                    moved += 1;
                    queue.push_back(waiter);
                } else {
                    queue.push_back(waiter);
                }
            }
            moved.min(count)
        } else if (self as *const Self as usize) < (target as *const Self as usize) {
            let _self_gate = self.gate.lock();
            let _target_gate = target.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                0,
                u32::MAX,
                Some((&mut dst, count, &target_owner)),
                false,
                &mut pending_wakers,
                &mut retired,
            )
            .expect("a wake-free requeue cannot reject PI waiters")
            .1
        } else {
            let _target_gate = target.gate.lock();
            let _self_gate = self.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                0,
                u32::MAX,
                Some((&mut dst, count, &target_owner)),
                false,
                &mut pending_wakers,
                &mut retired,
            )
            .expect("a wake-free requeue cannot reject PI waiters")
            .1
        };
        pending_wakers.finish();
        retired.finish();
        moved
    }

    /// Wakes and requeues waiters after atomically checking a user value.
    ///
    /// The comparison callback runs while both source and target queue gates
    /// are held (or the one shared gate for a same-key operation). A false
    /// comparison returns `Ok(None)` and leaves both queues unchanged. User
    /// memory validation belongs outside this method; the callback itself is
    /// the value comparison linearization point.
    pub fn wake_and_requeue_if<F>(
        &self,
        wake_count: usize,
        requeue_count: usize,
        target: &WaitQueue,
        target_owner: WaiterOwner,
        mask: u32,
        mut compare: F,
    ) -> WaitConditionResult<Option<(usize, usize)>>
    where
        F: FnMut() -> WaitConditionResult<bool>,
    {
        let mut pending_wakers = WakeBatch::default();
        let mut retired = DeferredWaiters::default();
        // The PI rejection stops the scan without rolling back what the scan
        // already did; Linux keeps those wakeups (`goto out_unlock` still runs
        // `wake_up_q(&wake_q)`) and the requeues `requeue_futex()` performed.
        // The deferred batches must therefore be released on this path too,
        // which is why the failure is recorded instead of propagated with `?`.
        let mut rejected = false;
        let result = if core::ptr::eq(self, target) {
            let _gate = self.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut queue = self.queue.lock();
            match Self::wake_and_requeue_same_locked(
                &mut queue,
                wake_count,
                requeue_count,
                mask,
                true,
                &mut pending_wakers,
                &mut retired,
            ) {
                Ok(result) => result,
                Err(()) => {
                    rejected = true;
                    (0, 0)
                }
            }
        } else if (self as *const Self as usize) < (target as *const Self as usize) {
            let _self_gate = self.gate.lock();
            let _target_gate = target.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            match Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, &target_owner)),
                true,
                &mut pending_wakers,
                &mut retired,
            ) {
                Ok(result) => result,
                Err(()) => {
                    rejected = true;
                    (0, 0)
                }
            }
        } else {
            let _target_gate = target.gate.lock();
            let _self_gate = self.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            match Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, &target_owner)),
                true,
                &mut pending_wakers,
                &mut retired,
            ) {
                Ok(result) => result,
                Err(()) => {
                    rejected = true;
                    (0, 0)
                }
            }
        };

        pending_wakers.finish();
        retired.finish();
        if rejected {
            return Err(WaitConditionError::Fault(AxError::InvalidInput));
        }
        Ok(Some(result))
    }

    /// Wakes up at most `wake_count` tasks and requeues up to
    /// `requeue_count` remaining waiters to the target queue atomically.
    ///
    /// `FUTEX_REQUEUE` reaches the same `futex_requeue()` loop as the
    /// comparison form, so the PI rejection applies here as well: a scan that
    /// meets a waiter still carrying PI state stops with `-EINVAL` after
    /// keeping the wakeups and requeues it already performed
    /// (kernel/futex/requeue.c:586-610).
    pub fn wake_and_requeue(
        &self,
        wake_count: usize,
        requeue_count: usize,
        target: &WaitQueue,
        target_owner: WaiterOwner,
        mask: u32,
    ) -> WaitConditionResult<(usize, usize)> {
        let scan = self.wake_and_requeue_if(
            wake_count,
            requeue_count,
            target,
            target_owner,
            mask,
            || Ok(true),
        )?;
        // The comparison always succeeds, so a refusal is impossible and the
        // PI rejection above is the only failure this call can report.
        Ok(scan.expect("an unconditional futex requeue comparison cannot refuse"))
    }
}

/// Waits until any one futex entry is woken.
///
/// The condition callback runs once while every distinct queue gate is held. It
/// must be a bounded, nonblocking, nonallocating snapshot; a raced user-memory
/// snapshot must return [`WaitConditionError::Retry`]. All entries are checked
/// before any waiter is published, giving waitv one atomic snapshot/publication
/// point. This helper owns the sleep/wake lifecycle and releases all queue
/// gates before returning `Retry`.
fn wait_on_any_futex_inner(
    waiters: Vec<(FutexHandle, u32)>,
    timeout: Option<(AlarmClock, Duration)>,
    condition: impl FnOnce() -> WaitConditionResult<bool>,
) -> WaitConditionResult<usize> {
    // All storage used by setup is reserved before any queue gate is taken.
    // In particular, publication below must be infallible once the gates are
    // held: a no-memory result must never leave a partially published waitv.
    let mut gate_order = Vec::new();
    gate_order
        .try_reserve_exact(waiters.len())
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
    for index in 0..waiters.len() {
        gate_order.push(index);
    }
    gate_order
        .sort_unstable_by_key(|index| (&waiters[*index].0.inner.wq as *const WaitQueue) as usize);
    gate_order.dedup_by(|left, right| {
        core::ptr::eq(&waiters[*left].0.inner.wq, &waiters[*right].0.inner.wq)
    });

    let mut waiters_refs = Vec::new();
    waiters_refs
        .try_reserve_exact(waiters.len())
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
    let mut registrations = Vec::new();
    registrations
        .try_reserve_exact(waiters.len())
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
    for (futex, bitset) in &waiters {
        let waiter = Arc::try_new(SpinNoIrq::new(WaiterEntry {
            bitset: *bitset,
            awakened: false,
            cancelled: false,
            owner: futex.waiter_owner(),
            task: Arc::downgrade(&current()),
            waker: None,
            next: None,
            pi: None,
            requeued_pi: false,
        }))
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
        waiters_refs.push(waiter);
    }
    // Keep the queue-owned strong references in a separate, preallocated
    // vector.  Taking one out of this vector during publication transfers its
    // ownership without cloning or dropping an Arc while a queue gate is held.
    let mut queue_waiters = Vec::new();
    queue_waiters
        .try_reserve_exact(waiters_refs.len())
        .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
    for waiter in &waiters_refs {
        queue_waiters.push(Some(waiter.clone()));
    }

    // Waitv has one atomic snapshot/publication point.  Every distinct queue
    // gate is acquired in address order before any callback is evaluated,
    // preventing a wake or a late mismatch from observing partial setup.
    let setup = {
        let mut gates = Vec::new();
        gates
            .try_reserve_exact(gate_order.len())
            .map_err(|_| WaitConditionError::Fault(AxError::NoMemory))?;
        for index in &gate_order {
            gates.push(waiters[*index].0.inner.wq.gate.lock());
        }

        let result = (|| {
            if !condition()? {
                return Err(WaitConditionError::Fault(AxError::WouldBlock));
            }
            if timeout.is_some_and(|(clock, deadline)| clock.now() >= deadline) {
                return Err(WaitConditionError::Fault(AxError::TimedOut));
            }
            for (index, (futex, _)) in waiters.iter().enumerate() {
                let waiter = queue_waiters[index]
                    .take()
                    .expect("waitv queue reference already transferred");
                futex.inner.wq.queue.lock().push_back(waiter);
                // The waiter is always owned by this current task.  This
                // view does not clone an Arc, so no task reference can be
                // dropped while the queue gates are held.
                if let Some(thread) = current().try_as_thread() {
                    thread.set_proc_state_hint(ProcStateHint::Interruptible);
                }
            }
            Ok(())
        })();

        // Remove guards without dropping Vec storage while any other gate is
        // still held.  The empty vector's allocation is dropped after the
        // final gate has been released.
        while let Some(gate) = gates.pop() {
            drop(gate);
        }
        result
    };
    if let Err(error) = setup {
        // Conditions are checked before publication, so no registration needs
        // queue cleanup on mismatch, nofault Retry, or setup failure.
        drop(waiters_refs);
        drop(queue_waiters);
        return Err(error);
    }

    // Transfer the preallocated waiter references only after every queue gate
    // has been released. Their queue-owned clones were published atomically.
    for waiter in waiters_refs {
        registrations.push(WaitRegistration {
            waiter: Some(waiter),
        });
    }

    // Keep registrations and their strong futex targets outside the future.
    // Their cancellation path may acquire a sleeping gate and must run only
    // after `block_on` has closed the task's synchronous block session.
    let wait = WaitAnyFuture {
        waiters: &registrations,
    };
    let mut sleeper = match timeout {
        Some((clock, deadline)) => match prepare_clock_sleep(clock, deadline) {
            Ok(sleeper) => Some(sleeper),
            Err(error) => {
                return resolve_wait_any_condition(
                    &mut registrations,
                    Err(WaitConditionError::Fault(error)),
                );
            }
        },
        None => None,
    };
    let result = {
        let wait = wait_with_prepared_clock_timeout(wait, sleeper.as_mut());
        let curr = current();
        let mut wait = core::pin::pin!(wait);
        block_on(poll_fn(|cx| {
            if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if curr.poll_interrupt(cx).is_ready() {
                return Poll::Ready(Err(AxError::Interrupted));
            }
            if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            Poll::Pending
        }))
    };
    let result: WaitConditionResult<usize> = match result {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(WaitConditionError::Fault(error)),
        Err(error) => Err(WaitConditionError::Fault(AxError::from(error))),
    };
    let result = resolve_wait_any_condition(&mut registrations, result);
    drop(waiters);
    result
}

/// Waits until any one futex entry is woken after checking each value while
/// all distinct queue gates are held.  The callback is invoked exactly once;
/// callers use it to hold one address-space guard for the complete snapshot.
pub fn wait_on_any_futex_if_atomic(
    waiters: Vec<(FutexHandle, u32)>,
    timeout: Option<(AlarmClock, Duration)>,
    condition: impl FnOnce() -> WaitConditionResult<bool>,
) -> WaitConditionResult<usize> {
    wait_on_any_futex_inner(waiters, timeout, condition)
}

/// Compatibility wrapper for callers which express their condition one
/// futex at a time.  The wrapper still evaluates all entries at one queue-gate
/// snapshot and publishes no waiter until every callback has accepted.
pub fn wait_on_any_futex_if(
    waiters: Vec<(FutexHandle, u32)>,
    timeout: Option<(AlarmClock, Duration)>,
    mut condition: impl FnMut(usize) -> WaitConditionResult<bool>,
) -> WaitConditionResult<usize> {
    let waiter_count = waiters.len();
    wait_on_any_futex_inner(waiters, timeout, || {
        for index in 0..waiter_count {
            if !condition(index)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

/// A key that uniquely identifies a futex in the system.
pub enum FutexKey {
    /// An explicit `FUTEX_PRIVATE_FLAG` futex.
    Private {
        /// The memory address of the futex.
        address: usize,
    },

    /// A non-PRIVATE futex operation on a private/COW mapping.
    PrivateMapping {
        /// The memory address of the futex.
        address: usize,
    },

    /// A futex in a shared memory region.
    Shared(SharedFutexKey),
}

impl FutexKey {
    /// Creates a new `FutexKey`.
    pub fn new(aspace: &AddrSpace, address: usize) -> Self {
        if let Some(key) = aspace.futex_shared_key_at(address) {
            return Self::Shared(key);
        }
        Self::PrivateMapping { address }
    }

    /// Shortcut to create a `FutexKey` for the current task's address space.
    pub fn new_current(address: usize) -> Self {
        let aspace_handle = current().as_thread().proc_data.aspace();
        Self::new(&aspace_handle.lock(), address)
    }

    /// Creates a `FutexKey` for a private futex, skipping the aspace lock and
    /// VMA walk that `new_current` performs. Only valid when the caller has
    /// already determined the futex is process-private (e.g. via
    /// `FUTEX_PRIVATE_FLAG`).
    pub fn new_private(address: usize) -> Self {
        Self::Private { address }
    }

    fn table_key(&self) -> FutexTableKey {
        match self {
            FutexKey::Private { address } => FutexTableKey::Private(*address),
            FutexKey::PrivateMapping { address } => FutexTableKey::PrivateMapping(*address),
            FutexKey::Shared(key) => FutexTableKey::Shared(key.offset()),
        }
    }

    pub fn shared_key(&self) -> Option<&SharedFutexKey> {
        match self {
            Self::Shared(key) => Some(key),
            Self::Private { .. } | Self::PrivateMapping { .. } => None,
        }
    }

    fn backing(&self) -> Option<&FutexBackingIdentity> {
        self.shared_key().map(SharedFutexKey::backing)
    }
}

/// The futex entry structure
pub struct FutexEntry {
    /// The wait queue associated with this futex.
    pub wq: WaitQueue,
    /// Linux `struct futex_pi_state`. Created on the first PI operation that
    /// attaches an owner, and kept for the lifetime of the entry so that a
    /// later `FUTEX_UNLOCK_PI` can still find the boost it installed.
    pi: SpinNoIrq<Option<Arc<PiState>>>,
    /// Strong lease for the exact shared backing identity.  This field is
    /// absent only for process-private futexes.
    backing_lease: Option<FutexBackingIdentity>,
}

impl FutexEntry {
    fn new() -> Self {
        Self::with_backing(None)
    }

    fn with_backing(backing_lease: Option<FutexBackingIdentity>) -> Self {
        Self {
            wq: WaitQueue::new(),
            pi: SpinNoIrq::new(None),
            backing_lease,
        }
    }

    /// Returns this futex's PI state, creating it on first use.
    pub fn pi_state(&self) -> Arc<PiState> {
        let mut slot = self.pi.lock();
        slot.get_or_insert_with(|| Arc::new(PiState::new())).clone()
    }

    /// Returns this futex's PI state only if a PI operation ever created one.
    pub fn existing_pi_state(&self) -> Option<Arc<PiState>> {
        self.pi.lock().clone()
    }

    /// True once a PI operation has made this futex a priority-inheritance
    /// futex. Linux refuses to attach a plain waiter to a PI futex and to
    /// requeue a PI waiter through the non-PI requeue paths.
    pub fn is_pi(&self) -> bool {
        self.pi.lock().is_some()
    }

    /// Drops the PI state once the futex has no owner and no PI waiter left,
    /// releasing the boost the owner may still be carrying.
    pub fn release_pi_state_if_idle(&self) {
        // The idle check and the slot take are one linearization point under
        // the wait-queue gate: `FUTEX_LOCK_PI` publishes its waiter under the
        // same gate, so a lock that queues a waiter either is observed here
        // (the futex is not idle) or finds the slot empty afterwards and
        // creates a fresh PI state to boost through. The deboost itself
        // resolves the owner task, which takes locks a run-queue or registry
        // owner may hold, so it runs after the gate is released.
        let state = {
            let _gate = self.wq.gate.lock();
            let queue = self.wq.queue.lock();
            let mut slot = self.pi.lock();
            let Some(state) = slot.as_ref() else {
                return;
            };
            if state.owner_tid() != 0 || WaitQueue::pi_top_locked(&queue).is_some() {
                return;
            }
            slot.take()
        };
        if let Some(state) = state {
            pi_deboost_owner(&state);
            state.detach();
        }
    }
}

/// A table mapping memory addresses to futex wait queues.
pub struct FutexTable(Mutex<HashMap<FutexTableKey, Arc<FutexEntry>>>);

impl FutexTable {
    /// Creates a new `FutexTable`.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    /// Checks if the futex table is empty.
    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }

    /// Gets the wait queue associated with the given address.
    pub fn get(&self, key: &FutexKey) -> Option<FutexGuard<'_>> {
        let table_key = key.table_key();
        let entry = self.0.lock().get(&table_key).cloned()?;
        Some(FutexGuard {
            table: self,
            key: table_key,
            inner: entry,
        })
    }

    /// Gets the wait queue associated with the given address, or inserts a a
    /// new one if it doesn't exist.
    pub fn get_or_insert(&self, key: &FutexKey) -> FutexGuard<'_> {
        let table_key = key.table_key();
        let mut table = self.0.lock();
        let entry = table
            .entry(table_key)
            .or_insert_with(|| Arc::new(FutexEntry::with_backing(key.backing().cloned())));
        FutexGuard {
            table: self,
            key: table_key,
            inner: entry.clone(),
        }
    }

    /// Gets or inserts a futex entry and keeps its table slot alive until the
    /// returned handle is dropped.
    pub fn get_or_insert_owned(self: &Arc<Self>, key: &FutexKey) -> FutexHandle {
        let table_key = key.table_key();
        let mut table = self.0.lock();
        let entry = table
            .entry(table_key)
            .or_insert_with(|| Arc::new(FutexEntry::with_backing(key.backing().cloned())));
        FutexHandle {
            table: self.clone(),
            key: table_key,
            inner: entry.clone(),
        }
    }

    /// Opportunistically removes an idle entry without splitting one futex
    /// identity into two independently wakeable queues.
    ///
    /// Both the map identity and the strong-reference count must be observed
    /// while the table is locked. Otherwise a concurrent lookup can clone the
    /// mapped entry after an out-of-lock liveness check but before removal,
    /// leaving that lookup attached to an entry which future wakers can no
    /// longer find through the table.
    fn try_remove_idle(&self, key: FutexTableKey, entry: &Arc<FutexEntry>) {
        let removed = {
            let mut table = self.0.lock();
            let Some(mapped) = table.get(&key) else {
                return;
            };
            if Arc::ptr_eq(mapped, entry) && Arc::strong_count(entry) == 2 && entry.wq.is_empty() {
                table.remove(&key)
            } else {
                None
            }
        };
        // Never run the removed entry/backing destructor while the table lock
        // is held.  In particular a file identity may release a cache handle.
        drop(removed);
    }
}

#[doc(hidden)]
pub struct FutexGuard<'a> {
    table: &'a FutexTable,
    key: FutexTableKey,
    inner: Arc<FutexEntry>,
}

impl Deref for FutexGuard<'_> {
    type Target = Arc<FutexEntry>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl FutexGuard<'_> {
    pub(crate) fn waiter_owner(&self) -> WaiterOwner {
        WaiterOwner::without_table(Arc::downgrade(&self.inner), self.key)
    }
}

/// An owned futex table entry handle that can be held across a blocking wait.
pub struct FutexHandle {
    table: Arc<FutexTable>,
    key: FutexTableKey,
    inner: Arc<FutexEntry>,
}

impl Deref for FutexHandle {
    type Target = Arc<FutexEntry>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl FutexHandle {
    pub(crate) fn waiter_owner(&self) -> WaiterOwner {
        WaiterOwner {
            entry: Arc::downgrade(&self.inner),
            table: Arc::downgrade(&self.table),
            key: self.key,
        }
    }
}

impl Drop for FutexHandle {
    fn drop(&mut self) {
        self.table.try_remove_idle(self.key, &self.inner);
    }
}

impl Drop for FutexGuard<'_> {
    fn drop(&mut self) {
        self.table.try_remove_idle(self.key, &self.inner);
    }
}

struct FutexTables {
    map: BTreeMap<FutexBackingId, Arc<FutexTable>>,
    operations: usize,
}
impl FutexTables {
    const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            operations: 0,
        }
    }

    fn get_or_insert(&mut self, key: &FutexBackingIdentity) -> Arc<FutexTable> {
        let table_key = key.id();
        self.operations += 1;
        if self.operations == 100 {
            self.operations = 0;
            self.map
                .retain(|_, table| Arc::strong_count(table) > 1 || !table.is_empty());
        }
        self.map
            .entry(table_key)
            .or_insert_with(|| Arc::new(FutexTable::new()))
            .clone()
    }
}

static SHARED_FUTEX_TABLES: Mutex<FutexTables> = Mutex::new(FutexTables::new());

/// Returns the futex table for the given key.
pub fn futex_table_for(key: &FutexKey) -> Arc<FutexTable> {
    match key {
        FutexKey::Private { .. } | FutexKey::PrivateMapping { .. } => {
            current().as_thread().proc_data.futex_table.clone()
        }
        FutexKey::Shared(shared) => SHARED_FUTEX_TABLES.lock().get_or_insert(shared.backing()),
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::task::{Context, Poll, Waker};

    use axhal::paging::PageSize;

    use super::*;
    use crate::mm::SharedPages;

    fn init_scheduler() {
        crate::test_support::ensure_scheduler();
    }

    fn register_test_waiter(entry: &Arc<FutexEntry>) -> WaitRegistration {
        entry
            .wq
            .register_waiter_if(Arc::downgrade(entry), u32::MAX, None, || Ok(true))
            .expect("waiter registration failed")
            .expect("test condition rejected waiter")
    }

    fn owner(entry: &Arc<FutexEntry>) -> WaiterOwner {
        WaiterOwner::without_table(Arc::downgrade(entry), FutexTableKey::Private(0))
    }

    #[test]
    fn dropping_requeued_waiter_cleans_target_queue() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let registration = register_test_waiter(&src);
        {
            let mut wait = core::pin::pin!(WaitFuture {
                waiter: registration.waiter.as_ref().unwrap(),
            });
            assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        }
        assert!(!src.wq.is_empty());

        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 1);
        assert!(src.wq.is_empty());
        assert!(!dst.wq.is_empty());

        drop(registration);
        assert!(dst.wq.is_empty());
    }

    #[test]
    fn requeue_cancellation_removes_target_table_entry_after_target_handle_drop() {
        init_scheduler();

        let table = Arc::new(FutexTable::new());
        let source_key = FutexKey::new_private(0x7100);
        let target_key = FutexKey::new_private(0x7200);
        let source = table.get_or_insert_owned(&source_key);
        let target = table.get_or_insert_owned(&target_key);
        let registration = source
            .wq
            .register_waiter_if_condition(source.waiter_owner(), u32::MAX, None, || Ok(true))
            .unwrap()
            .expect("waiter registration failed");

        assert_eq!(source.wq.requeue(1, &target.wq, target.waiter_owner()), 1);
        // The target handle may disappear while its queue is still occupied;
        // the requeued waiter's owned-table token must perform the final idle
        // removal when cancellation detaches that last node.
        drop(target);
        assert!(table.get(&target_key).is_some());

        drop(registration);
        assert!(table.get(&target_key).is_none());
        drop(source);
        assert!(table.get(&source_key).is_none());
    }

    #[test]
    fn registered_waiter_poll_never_reenters_sleeping_gate() {
        init_scheduler();

        let entry = Arc::new(FutexEntry::new());
        let registration = register_test_waiter(&entry);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        {
            let _gate = entry.wq.gate.lock();
            let mut wait = core::pin::pin!(WaitFuture {
                waiter: registration.waiter.as_ref().unwrap(),
            });
            assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        }
        drop(registration);
        assert!(entry.wq.is_empty());
    }

    #[test]
    fn wake_before_first_poll_is_observed() {
        init_scheduler();

        let entry = Arc::new(FutexEntry::new());
        let registration = register_test_waiter(&entry);
        assert_eq!(entry.wq.wake(1, u32::MAX), 1);

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut wait = core::pin::pin!(WaitFuture {
            waiter: registration.waiter.as_ref().unwrap(),
        });
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(true)));
        drop(wait);
        drop(registration);
        assert!(entry.wq.is_empty());
    }

    #[test]
    fn single_wait_terminal_owner_is_wake_or_error_never_both() {
        init_scheduler();

        let wake_first = Arc::new(FutexEntry::new());
        let mut registration = register_test_waiter(&wake_first);
        assert_eq!(wake_first.wq.wake(1, u32::MAX), 1);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::Interrupted)),
            (Ok(true), false)
        );
        assert!(wake_first.wq.is_empty());

        let error_first = Arc::new(FutexEntry::new());
        let mut registration = register_test_waiter(&error_first);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::TimedOut)),
            (Err(AxError::TimedOut), false)
        );
        assert_eq!(error_first.wq.wake(1, u32::MAX), 0);
        assert!(error_first.wq.is_empty());
    }

    #[test]
    fn requeued_wait_terminal_owner_is_wake_or_error_never_both() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let mut registration = register_test_waiter(&src);
        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 1);
        assert_eq!(dst.wq.wake(1, u32::MAX), 1);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::Interrupted)),
            (Ok(true), false)
        );

        let mut registration = register_test_waiter(&src);
        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 1);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::TimedOut)),
            (Err(AxError::TimedOut), false)
        );
        assert_eq!(dst.wq.wake(1, u32::MAX), 0);
        assert!(src.wq.is_empty());
        assert!(dst.wq.is_empty());
    }

    #[test]
    fn waitv_terminal_owner_is_wake_index_or_error_never_both() {
        init_scheduler();

        let first = Arc::new(FutexEntry::new());
        let second = Arc::new(FutexEntry::new());
        let mut registrations = [register_test_waiter(&first), register_test_waiter(&second)];
        assert_eq!(second.wq.wake(1, u32::MAX), 1);
        assert_eq!(
            resolve_wait_any(&mut registrations, Err(AxError::Interrupted)),
            Ok(1)
        );
        assert_eq!(first.wq.wake(1, u32::MAX), 0);

        let mut registrations = [register_test_waiter(&first), register_test_waiter(&second)];
        assert_eq!(
            resolve_wait_any(&mut registrations, Err(AxError::TimedOut)),
            Err(AxError::TimedOut)
        );
        assert_eq!(first.wq.wake(1, u32::MAX), 0);
        assert_eq!(second.wq.wake(1, u32::MAX), 0);
        assert!(first.wq.is_empty());
        assert!(second.wq.is_empty());
    }

    #[test]
    fn waitv_checks_all_entries_before_publication() {
        init_scheduler();

        let table = Arc::new(FutexTable::new());
        let first = table.get_or_insert_owned(&FutexKey::new_private(0x1000));
        let second = table.get_or_insert_owned(&FutexKey::new_private(0x2000));
        let first_entry = first.inner.clone();
        let second_entry = second.inner.clone();
        let mut callback_count = 0;
        let result =
            wait_on_any_futex_if(vec![(first, u32::MAX), (second, u32::MAX)], None, |index| {
                callback_count += 1;
                assert!(first_entry.wq.gate.try_lock().is_none());
                assert!(second_entry.wq.gate.try_lock().is_none());
                if index == 0 {
                    Ok(true)
                } else {
                    Err(WaitConditionError::Fault(AxError::BadAddress))
                }
            });
        assert_eq!(result, Err(WaitConditionError::Fault(AxError::BadAddress)));
        assert_eq!(callback_count, 2);
        assert_eq!(first_entry.wq.wake(1, u32::MAX), 0);
        assert!(first_entry.wq.is_empty());

        let table = Arc::new(FutexTable::new());
        let first = table.get_or_insert_owned(&FutexKey::new_private(0x3000));
        let second = table.get_or_insert_owned(&FutexKey::new_private(0x4000));
        let first_entry = first.inner.clone();
        let result =
            wait_on_any_futex_if(vec![(first, u32::MAX), (second, u32::MAX)], None, |index| {
                if index == 0 {
                    Ok(true)
                } else {
                    Err(WaitConditionError::Fault(AxError::BadAddress))
                }
            });
        assert_eq!(result, Err(WaitConditionError::Fault(AxError::BadAddress)));
        assert_eq!(first_entry.wq.wake(1, u32::MAX), 0);
        assert!(first_entry.wq.is_empty());
    }

    #[test]
    fn waitv_deduplicates_same_queue_gate() {
        init_scheduler();

        let table = Arc::new(FutexTable::new());
        let first = table.get_or_insert_owned(&FutexKey::new_private(0x4500));
        let second = table.get_or_insert_owned(&FutexKey::new_private(0x4500));
        let entry = first.inner.clone();
        let mut callback_count = 0;
        let result =
            wait_on_any_futex_if(vec![(first, u32::MAX), (second, u32::MAX)], None, |index| {
                callback_count += 1;
                assert!(entry.wq.gate.try_lock().is_none());
                if index == 0 {
                    Ok(true)
                } else {
                    Err(WaitConditionError::Fault(AxError::BadAddress))
                }
            });

        assert_eq!(result, Err(WaitConditionError::Fault(AxError::BadAddress)));
        assert_eq!(callback_count, 2);
        assert_eq!(entry.wq.wake(1, u32::MAX), 0);
        assert!(entry.wq.is_empty());
    }

    #[test]
    fn waitv_atomic_snapshot_retries_without_partial_publication() {
        init_scheduler();

        let table = Arc::new(FutexTable::new());
        let first = table.get_or_insert_owned(&FutexKey::new_private(0x4700));
        let second = table.get_or_insert_owned(&FutexKey::new_private(0x4800));
        let first_entry = first.inner.clone();
        let second_entry = second.inner.clone();
        let mut calls = 0;
        let result =
            wait_on_any_futex_if_atomic(vec![(first, u32::MAX), (second, u32::MAX)], None, || {
                calls += 1;
                assert!(first_entry.wq.gate.try_lock().is_none());
                assert!(second_entry.wq.gate.try_lock().is_none());
                Err(WaitConditionError::Retry)
            });

        assert_eq!(result, Err(WaitConditionError::Retry));
        assert_eq!(calls, 1);
        assert!(first_entry.wq.is_empty());
        assert!(second_entry.wq.is_empty());
    }

    #[test]
    fn expired_waitv_never_publishes_any_entry() {
        init_scheduler();

        let table = Arc::new(FutexTable::new());
        let first = table.get_or_insert_owned(&FutexKey::new_private(0x5000));
        let second = table.get_or_insert_owned(&FutexKey::new_private(0x6000));
        let first_entry = first.inner.clone();
        let second_entry = second.inner.clone();
        let result = wait_on_any_futex_if(
            vec![(first, u32::MAX), (second, u32::MAX)],
            Some((AlarmClock::Monotonic, Duration::ZERO)),
            |index| {
                assert!(first_entry.wq.gate.try_lock().is_none());
                assert!(second_entry.wq.gate.try_lock().is_none());
                assert!(index < 2);
                Ok(true)
            },
        );

        assert_eq!(result, Err(WaitConditionError::Fault(AxError::TimedOut)));
        assert_eq!(first_entry.wq.wake(1, u32::MAX), 0);
        assert_eq!(second_entry.wq.wake(1, u32::MAX), 0);
        assert!(first_entry.wq.is_empty());
        assert!(second_entry.wq.is_empty());
    }

    #[test]
    fn rejected_or_expired_registration_never_enqueues() {
        init_scheduler();

        let entry = Arc::new(FutexEntry::new());
        assert!(
            entry
                .wq
                .register_waiter_if(Arc::downgrade(&entry), u32::MAX, None, || Ok(false))
                .unwrap()
                .is_none()
        );
        assert!(entry.wq.is_empty());

        assert!(matches!(
            entry
                .wq
                .register_waiter_if(Arc::downgrade(&entry), u32::MAX, None, || Err(
                    AxError::BadAddress
                ),),
            Err(AxError::BadAddress)
        ));
        assert!(entry.wq.is_empty());

        assert!(matches!(
            entry.wq.register_waiter_if(
                Arc::downgrade(&entry),
                u32::MAX,
                Some((AlarmClock::Monotonic, Duration::ZERO)),
                || Ok(true),
            ),
            Err(AxError::TimedOut)
        ));
        assert!(entry.wq.is_empty());
    }

    #[test]
    fn requeue_skips_cancelled_waiters() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        #[allow(clippy::arc_with_non_send_sync)]
        src.wq
            .queue
            .lock()
            .push_back(Arc::new(SpinNoIrq::new(WaiterEntry {
                bitset: u32::MAX,
                awakened: false,
                cancelled: true,
                owner: owner(&src),
                task: WeakAxTaskRef::new(),
                waker: None,
                next: None,
                pi: None,
                requeued_pi: false,
            })));

        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 0);
        assert!(src.wq.is_empty());
        assert!(dst.wq.is_empty());
    }

    #[test]
    fn wake_discards_cancelled_waiters() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        #[allow(clippy::arc_with_non_send_sync)]
        src.wq
            .queue
            .lock()
            .push_back(Arc::new(SpinNoIrq::new(WaiterEntry {
                bitset: u32::MAX,
                awakened: false,
                cancelled: true,
                owner: owner(&src),
                task: WeakAxTaskRef::new(),
                waker: None,
                next: None,
                pi: None,
                requeued_pi: false,
            })));

        assert_eq!(src.wq.wake(1, u32::MAX), 0);
        assert!(src.wq.is_empty());
    }

    #[test]
    fn cancellation_unlinks_the_raw_queue_owner_exactly_once() {
        init_scheduler();

        let entry = Arc::new(FutexEntry::new());
        #[allow(clippy::arc_with_non_send_sync)]
        let waiter = Arc::new(SpinNoIrq::new(WaiterEntry {
            bitset: u32::MAX,
            awakened: false,
            cancelled: false,
            owner: owner(&entry),
            task: WeakAxTaskRef::new(),
            waker: None,
            next: None,
            pi: None,
            requeued_pi: false,
        }));

        // Keep this typed owner while the queue transfers its clone into a
        // raw Arc.  The count makes the raw ownership observable without
        // relying on a destructor after the test has lost the pointer.
        entry.wq.queue.lock().push_back(waiter.clone());
        assert_eq!(Arc::strong_count(&waiter), 2);
        assert!(!entry.wq.is_empty());

        assert_eq!(
            resolve_waiter_terminal(waiter.clone()),
            (WaitTerminalOwnership::Cancelled, false)
        );

        // Cancellation is the sole unlinker: DeferredWaiters reconstructs
        // and drops the queue count once after releasing the queue gate.
        assert!(entry.wq.is_empty());
        assert_eq!(Arc::strong_count(&waiter), 1);
        assert!(waiter.lock().next.is_none());
    }

    #[test]
    fn wake_and_requeue_keeps_remaining_waiters() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());

        for _ in 0..1000 {
            #[allow(clippy::arc_with_non_send_sync)]
            src.wq
                .queue
                .lock()
                .push_back(Arc::new(SpinNoIrq::new(WaiterEntry {
                    bitset: u32::MAX,
                    awakened: false,
                    cancelled: false,
                    owner: owner(&src),
                    task: WeakAxTaskRef::new(),
                    waker: None,
                    next: None,
                    pi: None,
                    requeued_pi: false,
                })));
        }

        let (woke, moved) = src
            .wq
            .wake_and_requeue(300, 500, &dst.wq, owner(&dst), u32::MAX)
            .expect("plain waiters are never rejected");

        assert_eq!(woke, 300);
        assert_eq!(moved, 500);
        assert_eq!(src.wq.wake(usize::MAX, u32::MAX), 200);
        assert!(src.wq.is_empty());
        assert_eq!(dst.wq.wake(usize::MAX, u32::MAX), 500);
        assert!(dst.wq.is_empty());
    }

    #[test]
    fn wake_op_updates_under_both_gates_and_wakes_conditionally() {
        init_scheduler();
        for compare in [false, true] {
            let src = Arc::new(FutexEntry::new());
            let dst = Arc::new(FutexEntry::new());
            let first = register_test_waiter(&src);
            let second = register_test_waiter(&dst);
            let result = src.wq.wake_op(1, &dst.wq, 1, || {
                assert!(src.wq.gate.try_lock().is_none());
                assert!(dst.wq.gate.try_lock().is_none());
                Ok(compare)
            });
            assert_eq!(result, Ok(if compare { 2 } else { 1 }));
            assert!(src.wq.is_empty());
            assert_eq!(dst.wq.is_empty(), compare);
            drop((first, second));
        }
    }

    #[test]
    fn wake_op_retry_leaves_both_queues_unchanged() {
        init_scheduler();
        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let first = register_test_waiter(&src);
        let second = register_test_waiter(&dst);
        assert_eq!(
            src.wq
                .wake_op(1, &dst.wq, 1, || Err(WaitConditionError::Retry)),
            Err(WaitConditionError::Retry)
        );
        assert!(!src.wq.is_empty());
        assert!(!dst.wq.is_empty());
        assert!(src.wq.gate.try_lock().is_some());
        assert!(dst.wq.gate.try_lock().is_some());
        drop((first, second));
    }

    #[test]
    fn wake_op_same_queue_wakes_successive_waiters_once() {
        init_scheduler();
        let entry = Arc::new(FutexEntry::new());
        let first = register_test_waiter(&entry);
        let second = register_test_waiter(&entry);
        let third = register_test_waiter(&entry);
        let mut calls = 0;
        assert_eq!(
            entry.wq.wake_op(1, &entry.wq, 1, || {
                calls += 1;
                Ok(true)
            }),
            Ok(2)
        );
        assert_eq!(calls, 1);
        assert_eq!(entry.wq.wake(usize::MAX, u32::MAX), 1);
        drop((first, second, third));
    }

    #[test]
    fn requeue_compare_runs_under_both_queue_gates() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let registration = register_test_waiter(&src);
        let mut calls = 0;
        let result = src
            .wq
            .wake_and_requeue_if(1, 1, &dst.wq, owner(&dst), u32::MAX, || {
                calls += 1;
                assert!(src.wq.gate.try_lock().is_none());
                assert!(dst.wq.gate.try_lock().is_none());
                Ok(false)
            });

        assert_eq!(result, Ok(None));
        assert_eq!(calls, 1);
        assert!(!src.wq.is_empty());
        assert!(dst.wq.is_empty());
        drop(registration);
    }

    #[test]
    fn requeue_compare_retry_releases_gates_without_modifying_queues() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let registration = register_test_waiter(&src);
        let result = src
            .wq
            .wake_and_requeue_if(1, 1, &dst.wq, owner(&dst), u32::MAX, || {
                Err(WaitConditionError::Retry)
            });

        assert_eq!(result, Err(WaitConditionError::Retry));
        assert!(src.wq.gate.try_lock().is_some());
        assert!(dst.wq.gate.try_lock().is_some());
        assert!(!src.wq.is_empty());
        assert!(dst.wq.is_empty());
        drop(registration);
    }

    #[test]
    fn same_key_requeue_counts_waiters_without_only_waking_them() {
        init_scheduler();

        let entry = Arc::new(FutexEntry::new());
        let mut registrations = Vec::new();
        for _ in 0..3 {
            registrations.push(register_test_waiter(&entry));
        }

        assert_eq!(
            entry
                .wq
                .wake_and_requeue(1, 2, &entry.wq, owner(&entry), u32::MAX,)
                .expect("plain waiters are never rejected"),
            (1, 2)
        );
        assert_eq!(entry.wq.wake(usize::MAX, u32::MAX), 2);
        assert!(entry.wq.is_empty());
        drop(registrations);
    }

    #[test]
    fn explicit_private_key_does_not_alias_nonprivate_private_mapping_key() {
        let address = 0x1000;
        let explicit_private = FutexKey::new_private(address);
        let mapped_private = FutexKey::PrivateMapping { address };

        assert_ne!(explicit_private.table_key(), mapped_private.table_key());

        let table = FutexTable::new();
        let explicit_entry = table.get_or_insert(&explicit_private);
        let mapped_entry = table.get_or_insert(&mapped_private);
        assert!(!Arc::ptr_eq(&explicit_entry.inner, &mapped_entry.inner));
    }

    #[test]
    fn idle_cleanup_rechecks_a_lookup_acquired_after_the_old_precheck_window() {
        let table = FutexTable::new();
        let key = FutexKey::new_private(0x1000);
        let retiring = table.get_or_insert(&key);

        // The old Drop path could observe exactly these two references outside
        // the table lock and decide that removal was safe.
        assert_eq!(Arc::strong_count(&retiring.inner), 2);

        // Model a lookup which wins the table lock after that observation but
        // before the old unconditional remove. Cleanup must recheck this live
        // reference while holding the same lock used by lookup.
        let concurrent = table.get(&key).expect("mapped futex entry");
        assert_eq!(Arc::strong_count(&retiring.inner), 3);
        table.try_remove_idle(key.table_key(), &retiring.inner);
        assert!(Arc::ptr_eq(
            table.0.lock().get(&key.table_key()).unwrap(),
            &concurrent.inner,
        ));

        drop(concurrent);
        drop(retiring);
        assert!(table.is_empty());
    }

    #[test]
    fn shared_table_id_does_not_pin_backing_after_last_entry_drop() {
        // A zero-length allocation avoids touching a user page while still
        // giving the identity a real SharedPages owner. Its final destructor
        // needs a running kernel task in host tests, so intentionally leak the
        // one test-owner reference after checking that all futex leases ended.
        let pages = Arc::new(SharedPages::new(0, PageSize::Size4K).unwrap());
        let identity = FutexBackingIdentity::Shared(pages.clone());
        let key = FutexKey::Shared(SharedFutexKey::new(
            identity.clone(),
            FutexWordOffset::new(0),
        ));
        let mut tables = FutexTables::new();
        let table = tables.get_or_insert(&identity);
        let entry = table.get_or_insert(&key);
        assert!(Arc::strong_count(&pages) > 1);

        drop(entry);
        drop(key);
        drop(identity);
        assert_eq!(Arc::strong_count(&pages), 1);

        drop(table);
        drop(tables);
        core::mem::forget(pages);
    }
}

#[cfg(test)]
mod pi_tests {
    use alloc::vec;

    use super::*;
    use crate::test_support::ensure_scheduler;

    fn add_pi_waiter(entry: &Arc<FutexEntry>, tid: u32, sched: SchedState) -> WaitRegistration {
        // The registration API is only reachable through the wait path, so the
        // queue is populated the same way `FUTEX_LOCK_PI` does it: publish a
        // waiter whose condition supplies the `rt_mutex_waiter` payload.
        ensure_scheduler();
        // `FUTEX_LOCK_PI` creates the entry's `futex_pi_state` before it ever
        // queues a waiter; `is_pi()` observes exactly that.
        let _pi_state = entry.pi_state();
        let (registration, published) = entry
            .wq
            .register_pi_waiter_if_condition(
                WaiterOwner::without_table(Arc::downgrade(entry), FutexTableKey::Private(0)),
                u32::MAX,
                None,
                || Ok(Some(PiWaiter::new(tid, sched))),
            )
            .expect("PI waiter registration failed")
            .expect("PI waiter condition rejected");
        // The payload the top-waiter search reads is the one publication
        // stored, not a copy attached afterwards.
        assert_eq!(
            entry.wq.pi_top_tid(),
            Some(published.tid),
            "a published PI waiter must be visible to rt_mutex_top_waiter()"
        );
        registration
    }

    fn fifo(rt_priority: u8) -> SchedState {
        SchedState {
            class: SchedClass::Fifo,
            nice: 0,
            rt_priority,
        }
    }

    fn add_plain_waiter(entry: &Arc<FutexEntry>) -> WaitRegistration {
        ensure_scheduler();
        entry
            .wq
            .register_waiter_if(Arc::downgrade(entry), u32::MAX, None, || Ok(true))
            .expect("waiter registration failed")
            .expect("test condition rejected waiter")
    }

    fn target_owner(entry: &Arc<FutexEntry>) -> WaiterOwner {
        WaiterOwner::without_table(Arc::downgrade(entry), FutexTableKey::Private(0))
    }

    #[test]
    fn requeue_rejects_a_pi_waiter_before_any_effect() {
        // `futex_requeue()` scans the source hash bucket and refuses a waiter
        // that still carries PI state (kernel/futex/requeue.c:606-609):
        //     if ((requeue_pi && !this->rt_waiter) ||
        //         (!requeue_pi && this->rt_waiter) ||
        //         this->pi_state) {
        //             ret = -EINVAL;
        //             break;
        //     }
        // The rejection precedes the unlink, so a plain requeue must neither
        // wake nor move the PI waiter: the source queue keeps it and the
        // target queue stays empty.
        ensure_scheduler();
        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let pi = add_pi_waiter(&src, 0x51, fifo(1));
        assert_eq!(src.wq.queue.lock().len, 1);
        assert_eq!(
            src.wq
                .wake_and_requeue_if(1, 1, &dst.wq, target_owner(&dst), u32::MAX, || Ok(true),),
            Err(WaitConditionError::Fault(AxError::InvalidInput))
        );
        assert_eq!(src.wq.queue.lock().len, 1, "the PI waiter must stay queued");
        assert!(dst.wq.is_empty(), "a rejected scan must not requeue anyone");
        // The waiter is still the PI waiter, not a demoted plain one.
        assert_eq!(src.wq.wake_inner(usize::MAX, u32::MAX, true), Err(()));
        drop(pi);
    }

    #[test]
    fn requeue_keeps_the_wakeups_that_precede_a_pi_rejection() {
        // The `break` above stops the scan without rolling it back: the
        // caller's `out_unlock` path still runs `wake_up_q(&wake_q)`, so a
        // waiter that was already woken stays woken while the PI waiter that
        // stopped the scan stays queued.
        ensure_scheduler();
        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let first = add_plain_waiter(&src);
        let pi = add_pi_waiter(&src, 0x52, fifo(1));
        assert_eq!(src.wq.queue.lock().len, 2);
        assert_eq!(
            src.wq
                .wake_and_requeue_if(1, 1, &dst.wq, target_owner(&dst), u32::MAX, || Ok(true),),
            Err(WaitConditionError::Fault(AxError::InvalidInput))
        );
        assert_eq!(
            src.wq.queue.lock().len,
            1,
            "the leading non-PI waiter must have been woken"
        );
        assert!(dst.wq.is_empty());
        assert_eq!(src.wq.wake_inner(usize::MAX, u32::MAX, true), Err(()));
        drop((first, pi));
    }

    #[test]
    fn wake_keeps_the_wakeups_that_precede_a_pi_rejection() {
        // `FUTEX_WAKE` scans the same way: the PI waiter stops the scan with
        // `-EINVAL`, but a plain waiter ahead of it stays woken.
        ensure_scheduler();
        let src = Arc::new(FutexEntry::new());
        let first = add_plain_waiter(&src);
        let pi = add_pi_waiter(&src, 0x55, fifo(1));
        assert_eq!(src.wq.queue.lock().len, 2);
        assert_eq!(src.wq.wake_inner(usize::MAX, u32::MAX, true), Err(()));
        assert_eq!(
            src.wq.queue.lock().len,
            1,
            "the leading non-PI waiter must have been woken"
        );
        drop((first, pi));
    }

    #[test]
    fn wake_op_rejects_a_pi_waiter_on_either_queue() {
        // `futex_wake_op()` applies the same rejection to both chains
        // (kernel/futex/waitwake.c:330-333 and :344-347):
        //     if ((!requeue_pi && this->rt_waiter) || this->pi_state) {
        //             ret = -EINVAL;
        //             goto out_unlock;
        //     }
        // and `out_unlock` keeps the wakeups the first chain recorded.
        ensure_scheduler();
        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let first = add_plain_waiter(&src);
        let pi = add_pi_waiter(&dst, 0x53, fifo(1));
        assert_eq!(
            src.wq.wake_op(1, &dst.wq, 1, || Ok(true)),
            Err(WaitConditionError::Fault(AxError::InvalidInput))
        );
        assert_eq!(
            src.wq.queue.lock().len,
            0,
            "the first chain woke its waiter"
        );
        assert_eq!(dst.wq.queue.lock().len, 1, "the PI waiter must stay queued");
        drop((first, pi));
    }

    #[test]
    fn requeue_without_a_comparison_rejects_a_pi_waiter() {
        // `FUTEX_REQUEUE` and `FUTEX_CMP_REQUEUE` share one `futex_requeue()`
        // loop (kernel/futex/requeue.c:586-610), so the PI rejection is not
        // limited to the comparison form: the scanned waiter stops the loop
        // with `-EINVAL` while the wakers that preceded it stay accounted.
        ensure_scheduler();
        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());
        let first = add_plain_waiter(&src);
        let pi = add_pi_waiter(&src, 0x54, fifo(1));
        assert_eq!(
            src.wq
                .wake_and_requeue(1, 1, &dst.wq, target_owner(&dst), u32::MAX),
            Err(WaitConditionError::Fault(AxError::InvalidInput))
        );
        assert_eq!(
            src.wq.queue.lock().len,
            1,
            "the leading non-PI waiter must have been woken"
        );
        assert!(dst.wq.is_empty(), "a rejected scan must not requeue anyone");
        assert_eq!(src.wq.wake_inner(usize::MAX, u32::MAX, true), Err(()));
        drop((first, pi));
    }

    fn fair(nice: i8) -> SchedState {
        SchedState {
            class: SchedClass::Normal,
            nice,
            rt_priority: 0,
        }
    }

    #[test]
    fn rt_priorities_map_to_linux_kernel_priority_domain() {
        // Linux `rt_mutex_waiter::prio`: 0 is the strongest RT priority and
        // fair tasks sit above every RT task.
        assert!(pi_kernel_priority(fifo(99)) < pi_kernel_priority(fifo(1)));
        assert!(pi_kernel_priority(fifo(1)) < pi_kernel_priority(fair(-20)));
        assert!(pi_kernel_priority(fair(-20)) < pi_kernel_priority(fair(19)));
    }

    #[test]
    fn boost_raises_a_fair_owner_to_the_rt_waiter() {
        let boosted = pi_boost(fair(0), fifo(10)).expect("RT waiter must boost a fair owner");
        assert_eq!(boosted.class, SchedClass::Fifo);
        assert_eq!(boosted.rt_priority, 10);
        // PI uses the RT class without time slicing, so a RoundRobin waiter
        // still boosts to FIFO.
        let boosted = pi_boost(
            fair(0),
            SchedState {
                class: SchedClass::RoundRobin,
                nice: 0,
                rt_priority: 5,
            },
        )
        .unwrap();
        assert_eq!(boosted.class, SchedClass::Fifo);
    }

    #[test]
    fn boost_is_monotone_and_idempotent() {
        // A larger `rt_priority` is a stronger RT task, so the stronger waiter
        // raises the owner and the weaker one leaves it alone.
        assert_eq!(pi_boost(fifo(10), fifo(20)), Some(fifo(20)));
        assert_eq!(pi_boost(fifo(20), fifo(10)), None);
        // No fair waiter can ever be stronger than an RT owner.
        assert_eq!(pi_boost(fifo(1), fair(-20)), None);
        assert_eq!(pi_boost(fifo(1), fifo(255)), Some(fifo(255)));
        // A fair waiter only moves the owner's nice downwards.
        assert_eq!(pi_boost(fair(5), fair(0)), Some(fair(0)));
        assert_eq!(pi_boost(fair(5), fair(9)), None);
        assert_eq!(
            pi_boost(fair(5), fifo(1)).map(|s| s.class),
            Some(SchedClass::Fifo)
        );
    }

    #[test]
    fn deadline_waiters_do_not_produce_a_broken_boost() {
        // `EevdfTaskParams` carries no deadline reservation, so a deadline
        // boost would be rejected by `set_sched_state`; the reduction reports
        // the gap instead of installing it.
        let deadline = SchedState {
            class: SchedClass::Deadline,
            nice: 0,
            rt_priority: 0,
        };
        assert_eq!(pi_boost(fair(0), deadline), None);
        assert_eq!(pi_boost(fifo(1), deadline), None);
    }

    #[test]
    fn top_pi_waiter_is_the_strongest_regardless_of_queue_order() {
        let entry = Arc::new(FutexEntry::new());
        // Larger `rt_priority` is stronger, and every RT task beats every
        // fair task, so TID 13 (FIFO 50) is the top waiter no matter that it
        // was queued last.
        let low = add_pi_waiter(&entry, 11, fair(10));
        let mid = add_pi_waiter(&entry, 12, fifo(2));
        let high = add_pi_waiter(&entry, 13, fifo(50));
        assert_eq!(entry.wq.pi_top_tid(), Some(13));
        assert!(entry.is_pi());
        drop(high);
        assert_eq!(entry.wq.pi_top_tid(), Some(12));
        drop(mid);
        assert_eq!(entry.wq.pi_top_tid(), Some(11));
        drop(low);
        assert_eq!(entry.wq.pi_top_tid(), None);
    }

    #[test]
    fn pi_unlock_publishes_the_top_waiter_before_waking_it() {
        let entry = Arc::new(FutexEntry::new());
        let _low = add_pi_waiter(&entry, 21, fair(0));
        let _high = add_pi_waiter(&entry, 22, fifo(3));
        let mut published = Vec::new();
        let outcome = entry.wq.pi_unlock(|next| {
            published.push(next);
            // Refuse the handoff: nothing may be claimed or woken.
            false
        });
        assert_eq!(outcome, PiUnlockOutcome::Aborted);
        assert_eq!(published, vec![Some(22)]);
        // Both waiters are still queued because the publication was refused.
        assert_eq!(entry.wq.pi_top_tid(), Some(22));
    }

    #[test]
    fn pi_unlock_without_waiters_publishes_the_unowned_word() {
        let entry = Arc::new(FutexEntry::new());
        let mut published = Vec::new();
        let outcome = entry.wq.pi_unlock(|next| {
            published.push(next);
            true
        });
        assert_eq!(outcome, PiUnlockOutcome::Cleared);
        assert_eq!(published, vec![None]);
    }

    #[test]
    fn plain_wake_refuses_a_queue_holding_pi_waiters() {
        let entry = Arc::new(FutexEntry::new());
        let _registration = add_pi_waiter(&entry, 31, fifo(1));
        // Linux `futex_wake()`: `if (this->pi_state || this->rt_waiter)
        // return -EINVAL;`.
        assert_eq!(entry.wq.wake_inner(1, u32::MAX, true), Err(()));
        // The PI paths themselves must still be able to wake.
        assert_eq!(entry.wq.wake_inner(0, u32::MAX, false), Ok(0));
    }

    /// The regression test for a dropped hand-off.
    ///
    /// `wake_futex_pi()` reads the top waiter out of the queue that
    /// `futex_lock_pi_atomic()` published (`kernel/futex/pi.c:1090-1100`), so a
    /// `FUTEX_UNLOCK_PI` which finds `futex_top_waiter() == NULL` publishes no
    /// hand-off and the waiter sleeps on. A payload attached to the queue node
    /// anywhere but by the publication itself is invisible to that search.
    #[test]
    fn published_pi_waiter_is_the_one_unlock_hands_the_futex_to() {
        let entry = Arc::new(FutexEntry::new());
        let _registration = add_pi_waiter(&entry, 42, fifo(1));
        assert_eq!(entry.wq.pi_top_tid(), Some(42));

        let state = entry.pi_state();
        state.attach(7);
        let mut published = vec![];
        assert_eq!(
            entry.wq.pi_unlock(|top| {
                published.push(top);
                true
            }),
            PiUnlockOutcome::HandedOff(42)
        );
        assert_eq!(published, vec![Some(42)]);
    }
}
