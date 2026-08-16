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
use axtask::{WeakAxTaskRef, current, future::block_on};
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

    fn finish(mut self) {
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

    fn finish(self) {
        let mut waiters = self.waiters;
        while let Some(ptr) = waiters.head.take() {
            // SAFETY: `ptr` is an owned strong reference detached from a
            // queue and cannot be concurrently reclaimed before this drain.
            let waiter = unsafe { Arc::from_raw(ptr.as_ptr()) };
            let next = waiter.lock().next.take();
            waiters.head = next;
            if waiters.head.is_none() {
                waiters.tail = None;
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

struct WaiterEntry {
    bitset: u32,
    awakened: bool,
    cancelled: bool,
    owner: WaiterOwner,
    task: WeakAxTaskRef,
    waker: Option<Waker>,
    next: Option<WaiterPtr>,
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

fn resolve_waiter_terminal(waiter: Arc<SpinNoIrq<WaiterEntry>>) -> WaitTerminalOwnership {
    // Mark the waiter as cancelled first so a concurrent wake either already
    // owns completion or observes cancellation and does not count this waiter.
    // Requeue tests `cancelled` while holding the same waiter lock, so the
    // captured owner cannot change after this point.
    let (owner, task) = {
        let mut waiter = waiter.lock();
        if waiter.awakened {
            return WaitTerminalOwnership::Woken;
        }
        waiter.cancelled = true;
        (waiter.owner.clone(), waiter.task.clone())
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
    drop(owner_entry);
    owner.cleanup_if_idle();

    WaitTerminalOwnership::Cancelled
}

fn resolve_single_wait(
    registration: &mut WaitRegistration,
    result: AxResult<bool>,
) -> AxResult<bool> {
    match registration.resolve_terminal() {
        WaitTerminalOwnership::Woken => Ok(true),
        WaitTerminalOwnership::Cancelled => result,
    }
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
        pending_wakers: &mut WakeBatch,
        retired: &mut DeferredWaiters,
    ) -> (usize, usize) {
        let mut woke = 0;
        let mut moved = 0;
        // Kept waiters are appended back to `src`; bound the scan to the
        // entries present when the gate was acquired so a non-woken waiter
        // cannot make this drain loop revisit itself forever.
        let initial_len = src.len;

        for _ in 0..initial_len {
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

        (woke, moved)
    }

    /// Applies the wake/requeue accounting for a source and target which are
    /// the same queue. Linux still counts the waiters covered by
    /// `nr_requeue` even though there is no physical queue move in this case.
    fn wake_and_requeue_same_locked(
        src: &mut WaiterQueue,
        wake_count: usize,
        requeue_count: usize,
        mask: u32,
        pending_wakers: &mut WakeBatch,
        retired: &mut DeferredWaiters,
    ) -> (usize, usize) {
        let mut woke = 0;
        let mut moved = 0;
        // As in the cross-key path, kept waiters are reinserted at the tail.
        // Only process the queue population that existed at the linearization
        // point; otherwise a kept node would be processed indefinitely.
        let initial_len = src.len;

        for _ in 0..initial_len {
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

        (woke, moved)
    }

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
                    return resolve_single_wait(&mut registration, Err(error))
                        .map_err(WaitConditionError::Fault);
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
        resolve_single_wait(&mut registration, result).map_err(WaitConditionError::Fault)
    }

    /// Wakes up at most `count` tasks whose bitset intersects with the given
    /// bitmask.
    pub fn wake(&self, count: usize, mask: u32) -> usize {
        let mut pending_wakers = WakeBatch::default();
        let mut retired = DeferredWaiters::default();
        let woke = {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            Self::wake_and_requeue_locked(
                &mut queue,
                count,
                mask,
                None,
                &mut pending_wakers,
                &mut retired,
            )
            .0
        };
        pending_wakers.finish();
        retired.finish();
        woke
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
                &mut pending_wakers,
                &mut retired,
            )
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
                &mut pending_wakers,
                &mut retired,
            )
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
        let result = if core::ptr::eq(self, target) {
            let _gate = self.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut queue = self.queue.lock();
            Self::wake_and_requeue_same_locked(
                &mut queue,
                wake_count,
                requeue_count,
                mask,
                &mut pending_wakers,
                &mut retired,
            )
        } else if (self as *const Self as usize) < (target as *const Self as usize) {
            let _self_gate = self.gate.lock();
            let _target_gate = target.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, &target_owner)),
                &mut pending_wakers,
                &mut retired,
            )
        } else {
            let _target_gate = target.gate.lock();
            let _self_gate = self.gate.lock();
            if !compare()? {
                return Ok(None);
            }
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, &target_owner)),
                &mut pending_wakers,
                &mut retired,
            )
        };

        pending_wakers.finish();
        retired.finish();
        Ok(Some(result))
    }

    /// Wakes up at most `wake_count` tasks and requeues up to
    /// `requeue_count` remaining waiters to the target queue atomically.
    pub fn wake_and_requeue(
        &self,
        wake_count: usize,
        requeue_count: usize,
        target: &WaitQueue,
        target_owner: WaiterOwner,
        mask: u32,
    ) -> (usize, usize) {
        self.wake_and_requeue_if(
            wake_count,
            requeue_count,
            target,
            target_owner,
            mask,
            || Ok(true),
        )
        .expect("unconditional futex requeue comparison cannot fail")
        .expect("unconditional futex requeue comparison cannot reject")
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
        (&waiters[*left].0.inner.wq as *const WaitQueue) as usize
            == (&waiters[*right].0.inner.wq as *const WaitQueue) as usize
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
            backing_lease,
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
            Ok(true)
        );
        assert!(wake_first.wq.is_empty());

        let error_first = Arc::new(FutexEntry::new());
        let mut registration = register_test_waiter(&error_first);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::TimedOut)),
            Err(AxError::TimedOut)
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
            Ok(true)
        );

        let mut registration = register_test_waiter(&src);
        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 1);
        assert_eq!(
            resolve_single_wait(&mut registration, Err(AxError::TimedOut)),
            Err(AxError::TimedOut)
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
            })));

        assert_eq!(src.wq.requeue(1, &dst.wq, owner(&dst)), 0);
        assert!(src.wq.is_empty());
        assert!(dst.wq.is_empty());
    }

    #[test]
    fn wake_discards_cancelled_waiters() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
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
            })));

        assert_eq!(src.wq.wake(1, u32::MAX), 0);
        assert!(src.wq.is_empty());
    }

    #[test]
    fn wake_and_requeue_keeps_remaining_waiters() {
        init_scheduler();

        let src = Arc::new(FutexEntry::new());
        let dst = Arc::new(FutexEntry::new());

        for _ in 0..1000 {
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
                })));
        }

        let (woke, moved) = src
            .wq
            .wake_and_requeue(300, 500, &dst.wq, owner(&dst), u32::MAX);

        assert_eq!(woke, 300);
        assert_eq!(moved, 500);
        assert_eq!(src.wq.wake(usize::MAX, u32::MAX), 200);
        assert!(src.wq.is_empty());
        assert_eq!(dst.wq.wake(usize::MAX, u32::MAX), 500);
        assert!(dst.wq.is_empty());
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
                .wake_and_requeue(1, 2, &entry.wq, owner(&entry), u32::MAX,),
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
