//! Futex implementation.

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::{Future, poll_fn},
    ops::Deref,
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use axtask::{WeakAxTaskRef, current, future::block_on};
use hashbrown::HashMap;
use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

use crate::{
    mm::{AddrSpace, Backend, SharedPages},
    task::{AlarmClock, AsThread, ProcStateHint, sleep_until_clock},
};

/// Wait queue used by futex.
#[derive(Default)]
pub struct WaitQueue {
    gate: Mutex<()>,
    queue: SpinNoIrq<VecDeque<Arc<SpinNoIrq<WaiterEntry>>>>,
}

struct WaiterEntry {
    bitset: u32,
    awakened: bool,
    cancelled: bool,
    owner: Weak<FutexEntry>,
    task: WeakAxTaskRef,
    waker: Option<Waker>,
}

struct WaitFuture<'a> {
    waiter: &'a Arc<SpinNoIrq<WaiterEntry>>,
}

impl Unpin for WaitFuture<'_> {}

struct WaitRegistration {
    waiter: Option<Arc<SpinNoIrq<WaiterEntry>>>,
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            cancel_waiter(waiter);
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
        let mut waiter = self.waiter.lock();
        if waiter.awakened {
            return Poll::Ready(Ok(true));
        }
        if waiter
            .waker
            .as_ref()
            .is_none_or(|registered| !registered.will_wake(cx.waker()))
        {
            waiter.waker = Some(cx.waker().clone());
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
            let mut waiter = waiter.lock();
            if waiter.awakened {
                return Poll::Ready(Ok(index));
            }
            if waiter
                .waker
                .as_ref()
                .is_none_or(|registered| !registered.will_wake(cx.waker()))
            {
                waiter.waker = Some(cx.waker().clone());
            }
        }

        Poll::Pending
    }
}

fn clear_waiter_proc_state(waiter: &WaiterEntry) {
    if let Some(task) = waiter.task.upgrade()
        && let Some(thread) = task.try_as_thread()
    {
        thread.set_proc_state_hint(ProcStateHint::None);
    }
}

fn cancel_waiter(waiter: Arc<SpinNoIrq<WaiterEntry>>) {
    // Mark the waiter as cancelled first so future requeues can no longer keep
    // moving it between futex queues.
    let mut owner = {
        let mut waiter = waiter.lock();
        if waiter.awakened {
            return;
        }
        waiter.cancelled = true;
        clear_waiter_proc_state(&waiter);
        waiter.owner.clone()
    };

    loop {
        let Some(owner_entry) = owner.upgrade() else {
            return;
        };

        let _gate = owner_entry.wq.gate.lock();
        owner = {
            let waiter_entry = waiter.lock();
            if waiter_entry.awakened {
                return;
            }
            if Weak::ptr_eq(&waiter_entry.owner, &Arc::downgrade(&owner_entry)) {
                owner_entry.wq.remove_waiter_locked(&waiter);
                return;
            }
            waiter_entry.owner.clone()
        };
    }
}

fn awakened_registration_index(registrations: &[WaitRegistration]) -> Option<usize> {
    registrations.iter().position(|registration| {
        registration
            .waiter
            .as_ref()
            .is_some_and(|waiter| waiter.lock().awakened)
    })
}

fn setup_error_or_wake(registrations: &[WaitRegistration], err: AxError) -> AxResult<usize> {
    if let Some(index) = awakened_registration_index(registrations) {
        Ok(index)
    } else {
        Err(err)
    }
}

fn clock_timeout_error(result: Poll<AxResult<()>>) -> Option<AxError> {
    match result {
        Poll::Ready(Ok(())) => Some(AxError::TimedOut),
        Poll::Ready(Err(error)) => Some(error),
        Poll::Pending => None,
    }
}

impl WaitQueue {
    /// Creates a new `WaitQueue`.
    pub fn new() -> Self {
        Self::default()
    }

    fn wake_and_requeue_locked(
        src: &mut VecDeque<Arc<SpinNoIrq<WaiterEntry>>>,
        wake_count: usize,
        mask: u32,
        mut requeue: Option<(
            &mut VecDeque<Arc<SpinNoIrq<WaiterEntry>>>,
            usize,
            Weak<FutexEntry>,
        )>,
        pending_wakers: &mut Vec<Waker>,
    ) -> (usize, usize) {
        let mut woke = 0;
        let mut moved = 0;
        let mut keep = VecDeque::with_capacity(src.len());

        while let Some(waiter) = src.pop_front() {
            enum Action {
                Drop,
                Keep,
                Requeue,
            }

            let action = {
                let mut waiter = waiter.lock();
                if waiter.cancelled {
                    clear_waiter_proc_state(&waiter);
                    Action::Drop
                } else if woke < wake_count && (waiter.bitset & mask) != 0 {
                    waiter.awakened = true;
                    clear_waiter_proc_state(&waiter);
                    if let Some(waker) = waiter.waker.take() {
                        pending_wakers.push(waker);
                    }
                    woke += 1;
                    Action::Drop
                } else if let Some((_, limit, target_owner)) = requeue.as_mut()
                    && moved < *limit
                {
                    waiter.owner = target_owner.clone();
                    moved += 1;
                    Action::Requeue
                } else {
                    Action::Keep
                }
            };

            match action {
                Action::Drop => {}
                Action::Keep => keep.push_back(waiter),
                Action::Requeue => {
                    let (dst, ..) = requeue.as_mut().unwrap();
                    dst.push_back(waiter);
                }
            }
        }

        *src = keep;
        (woke, moved)
    }

    fn remove_waiter_locked(&self, target: &Arc<SpinNoIrq<WaiterEntry>>) {
        self.queue
            .lock()
            .retain(|waiter| !Arc::ptr_eq(waiter, target));
    }

    fn register_waiter_if(
        &self,
        owner: Weak<FutexEntry>,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> AxResult<bool>,
    ) -> AxResult<Option<WaitRegistration>> {
        let _gate = self.gate.lock();
        if !condition()? {
            return Ok(None);
        }
        if timeout.is_some_and(|(clock, deadline)| clock.now() >= deadline) {
            return Err(AxError::TimedOut);
        }

        let waiter = Arc::new(SpinNoIrq::new(WaiterEntry {
            bitset,
            awakened: false,
            cancelled: false,
            owner,
            task: Arc::downgrade(&current()),
            waker: None,
        }));
        self.queue.lock().push_back(waiter.clone());
        if let Some(task) = waiter.lock().task.upgrade()
            && let Some(thread) = task.try_as_thread()
        {
            thread.set_proc_state_hint(ProcStateHint::Interruptible);
        }
        Ok(Some(WaitRegistration {
            waiter: Some(waiter),
        }))
    }

    /// Waits if the given condition is met.
    ///
    /// Returns `false` if the condition is not met and no actual waiting
    /// occurs.
    pub fn wait_if(
        &self,
        owner: Weak<FutexEntry>,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> AxResult<bool>,
    ) -> AxResult<bool> {
        // Registration may fault while evaluating `condition` and therefore
        // happens before the synchronous block session starts. From this point
        // on, polling and wakeup touch only the waiter's IRQ-safe state.
        let Some(registration) = self.register_waiter_if(owner, bitset, timeout, condition)? else {
            return Ok(false);
        };
        let wait = WaitFuture {
            waiter: registration
                .waiter
                .as_ref()
                .expect("registered futex waiter"),
        };
        let wait = async {
            if let Some((clock, deadline)) = timeout {
                let mut wait = core::pin::pin!(wait);
                let mut sleeper = core::pin::pin!(sleep_until_clock(clock, deadline));
                poll_fn(|cx| {
                    if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                        return Poll::Ready(result);
                    }
                    if let Some(error) = clock_timeout_error(sleeper.as_mut().poll(cx)) {
                        return Poll::Ready(Err(error));
                    }
                    Poll::Pending
                })
                .await
            } else {
                wait.await
            }
        };
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
        .map_err(AxError::from)?
    }

    /// Wakes up at most `count` tasks whose bitset intersects with the given
    /// bitmask.
    pub fn wake(&self, count: usize, mask: u32) -> usize {
        let _gate = self.gate.lock();
        let mut pending_wakers = Vec::new();
        let woke = {
            let mut queue = self.queue.lock();
            Self::wake_and_requeue_locked(&mut queue, count, mask, None, &mut pending_wakers).0
        };
        drop(_gate);
        for waker in pending_wakers {
            waker.wake();
        }
        woke
    }

    /// Checks if the wait queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Requeue at most `count` tasks to the target wait queue.
    pub fn requeue(
        &self,
        mut count: usize,
        target: &WaitQueue,
        target_owner: Weak<FutexEntry>,
    ) -> usize {
        if core::ptr::eq(self, target) {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            queue.retain(|waiter| !waiter.lock().cancelled);
            count = count.min(queue.len());
            return count;
        } else if (self as *const Self as usize) < (target as *const Self as usize) {
            let _self_gate = self.gate.lock();
            let _target_gate = target.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            return Self::wake_and_requeue_locked(
                &mut src,
                0,
                u32::MAX,
                Some((&mut dst, count, target_owner)),
                &mut Vec::new(),
            )
            .1;
        } else {
            let _target_gate = target.gate.lock();
            let _self_gate = self.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            return Self::wake_and_requeue_locked(
                &mut src,
                0,
                u32::MAX,
                Some((&mut dst, count, target_owner)),
                &mut Vec::new(),
            )
            .1;
        }
    }

    /// Wakes up at most `wake_count` tasks and requeues up to
    /// `requeue_count` remaining waiters to the target queue atomically.
    pub fn wake_and_requeue(
        &self,
        wake_count: usize,
        requeue_count: usize,
        target: &WaitQueue,
        target_owner: Weak<FutexEntry>,
        mask: u32,
    ) -> (usize, usize) {
        let mut pending_wakers = Vec::new();
        let result = if core::ptr::eq(self, target) {
            let _gate = self.gate.lock();
            let mut queue = self.queue.lock();
            Self::wake_and_requeue_locked(&mut queue, wake_count, mask, None, &mut pending_wakers)
        } else if (self as *const Self as usize) < (target as *const Self as usize) {
            let _self_gate = self.gate.lock();
            let _target_gate = target.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, target_owner)),
                &mut pending_wakers,
            )
        } else {
            let _target_gate = target.gate.lock();
            let _self_gate = self.gate.lock();
            let mut src = self.queue.lock();
            let mut dst = target.queue.lock();
            Self::wake_and_requeue_locked(
                &mut src,
                wake_count,
                mask,
                Some((&mut dst, requeue_count, target_owner)),
                &mut pending_wakers,
            )
        };

        for waker in pending_wakers {
            waker.wake();
        }
        result
    }
}

/// Waits until any one futex entry is woken.
///
/// The caller must already have validated each futex value while holding the
/// corresponding queue gate. This helper only owns the sleep/wake lifecycle.
pub fn wait_on_any_futex_if(
    waiters: Vec<(FutexHandle, u32)>,
    timeout: Option<(AlarmClock, Duration)>,
    mut condition: impl FnMut(usize) -> AxResult<bool>,
) -> AxResult<usize> {
    let mut _targets = Vec::with_capacity(waiters.len());
    let mut registrations = Vec::with_capacity(waiters.len());
    for (index, (futex, bitset)) in waiters.into_iter().enumerate() {
        let waiter = Arc::new(SpinNoIrq::new(WaiterEntry {
            bitset,
            awakened: false,
            cancelled: false,
            owner: Arc::downgrade(&futex.inner),
            task: Arc::downgrade(&current()),
            waker: None,
        }));
        {
            let _gate = futex.inner.wq.gate.lock();
            let matches = match condition(index) {
                Ok(matches) => matches,
                Err(err) => return setup_error_or_wake(&registrations, err),
            };
            if !matches {
                return setup_error_or_wake(&registrations, AxError::WouldBlock);
            }
            futex.inner.wq.queue.lock().push_back(waiter.clone());
            if let Some(task) = waiter.lock().task.upgrade()
                && let Some(thread) = task.try_as_thread()
            {
                thread.set_proc_state_hint(ProcStateHint::Interruptible);
            }
        }
        registrations.push(WaitRegistration {
            waiter: Some(waiter),
        });
        _targets.push(futex);
    }

    // Keep registrations and their strong futex targets outside the future.
    // Their cancellation path may acquire a sleeping gate and must run only
    // after `block_on` has closed the task's synchronous block session.
    let wait = WaitAnyFuture {
        waiters: &registrations,
    };
    let wait = async {
        if let Some((clock, deadline)) = timeout {
            let mut wait = core::pin::pin!(wait);
            let mut sleeper = core::pin::pin!(sleep_until_clock(clock, deadline));
            poll_fn(|cx| {
                if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                    return Poll::Ready(result);
                }
                if let Some(error) = clock_timeout_error(sleeper.as_mut().poll(cx)) {
                    return Poll::Ready(Err(error));
                }
                Poll::Pending
            })
            .await
        } else {
            wait.await
        }
    };
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
    .map_err(AxError::from)?
}

#[cfg(test)]
mod tests {
    use core::task::{Context, Poll, Waker};

    use spin::Once;

    use super::*;

    static INIT: Once<()> = Once::new();

    fn init_scheduler() {
        INIT.call_once(|| {
            if let Err(error) = axtask::init_scheduler() {
                assert!(
                    axtask::current_may_uninit().is_some(),
                    "host scheduler initialization failed: {error:?}"
                );
            }
        });
    }

    fn register_test_waiter(entry: &Arc<FutexEntry>) -> WaitRegistration {
        entry
            .wq
            .register_waiter_if(Arc::downgrade(entry), u32::MAX, None, || Ok(true))
            .expect("waiter registration failed")
            .expect("test condition rejected waiter")
    }

    #[test]
    fn timer_registration_failure_is_not_reported_as_timeout() {
        assert_eq!(
            clock_timeout_error(Poll::Ready(Err(AxError::NoMemory))),
            Some(AxError::NoMemory)
        );
        assert_eq!(
            clock_timeout_error(Poll::Ready(Ok(()))),
            Some(AxError::TimedOut)
        );
        assert_eq!(clock_timeout_error(Poll::Pending), None);
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

        assert_eq!(src.wq.requeue(1, &dst.wq, Arc::downgrade(&dst)), 1);
        assert!(src.wq.is_empty());
        assert!(!dst.wq.is_empty());

        drop(registration);
        assert!(dst.wq.is_empty());
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
                owner: Arc::downgrade(&src),
                task: WeakAxTaskRef::new(),
                waker: None,
            })));

        assert_eq!(src.wq.requeue(1, &dst.wq, Arc::downgrade(&dst)), 0);
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
                owner: Arc::downgrade(&src),
                task: WeakAxTaskRef::new(),
                waker: None,
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
                    owner: Arc::downgrade(&src),
                    task: WeakAxTaskRef::new(),
                    waker: None,
                })));
        }

        let (woke, moved) =
            src.wq
                .wake_and_requeue(300, 500, &dst.wq, Arc::downgrade(&dst), u32::MAX);

        assert_eq!(woke, 300);
        assert_eq!(moved, 500);
        assert_eq!(src.wq.wake(usize::MAX, u32::MAX), 200);
        assert!(src.wq.is_empty());
        assert_eq!(dst.wq.wake(usize::MAX, u32::MAX), 500);
        assert!(dst.wq.is_empty());
    }
}

/// A key that uniquely identifies a futex in the system.
pub enum FutexKey {
    /// A futex that is private to the current process.
    Private {
        /// The memory address of the futex.
        address: usize,
    },

    /// A futex in a shared memory region.
    Shared {
        /// The offset of the futex within the shared memory region.
        offset: usize,
        /// The shared memory region.
        region: Result<Weak<SharedPages>, Weak<()>>,
    },
}

impl FutexKey {
    /// Creates a new `FutexKey`.
    pub fn new(aspace: &AddrSpace, address: usize) -> Self {
        if let Some(area) = aspace.find_area(VirtAddr::from_usize(address)) {
            match area.backend() {
                Backend::Shared(backend) => {
                    if let Some(offset) = backend.backing_offset(address) {
                        return Self::Shared {
                            offset,
                            region: Ok(Arc::downgrade(backend.pages())),
                        };
                    }
                }
                Backend::File(file) => {
                    let (handle, offset) = file.futex_key(address);
                    return Self::Shared {
                        offset,
                        region: Err(handle),
                    };
                }
                _ => {}
            }
        }
        Self::Private { address }
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

    fn as_usize(&self) -> usize {
        match self {
            FutexKey::Private { address } => *address,
            FutexKey::Shared { offset, .. } => *offset,
        }
    }
}

/// The futex entry structure
pub struct FutexEntry {
    /// The wait queue associated with this futex.
    pub wq: WaitQueue,
}

impl FutexEntry {
    fn new() -> Self {
        Self {
            wq: WaitQueue::new(),
        }
    }
}

/// A table mapping memory addresses to futex wait queues.
pub struct FutexTable(Mutex<HashMap<usize, Arc<FutexEntry>>>);

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
        let key = key.as_usize();
        let entry = self.0.lock().get(&key).cloned()?;
        Some(FutexGuard {
            table: self,
            key,
            inner: entry,
        })
    }

    /// Gets the wait queue associated with the given address, or inserts a a
    /// new one if it doesn't exist.
    pub fn get_or_insert(&self, key: &FutexKey) -> FutexGuard<'_> {
        let key = key.as_usize();
        let mut table = self.0.lock();
        let entry = table
            .entry(key)
            .or_insert_with(|| Arc::new(FutexEntry::new()));
        FutexGuard {
            table: self,
            key,
            inner: entry.clone(),
        }
    }

    /// Gets or inserts a futex entry and keeps its table slot alive until the
    /// returned handle is dropped.
    pub fn get_or_insert_owned(self: &Arc<Self>, key: &FutexKey) -> FutexHandle {
        let key = key.as_usize();
        let mut table = self.0.lock();
        let entry = table
            .entry(key)
            .or_insert_with(|| Arc::new(FutexEntry::new()));
        FutexHandle {
            table: self.clone(),
            key,
            inner: entry.clone(),
        }
    }
}

#[doc(hidden)]
pub struct FutexGuard<'a> {
    table: &'a FutexTable,
    key: usize,
    inner: Arc<FutexEntry>,
}

impl Deref for FutexGuard<'_> {
    type Target = Arc<FutexEntry>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// An owned futex table entry handle that can be held across a blocking wait.
pub struct FutexHandle {
    table: Arc<FutexTable>,
    key: usize,
    inner: Arc<FutexEntry>,
}

impl Deref for FutexHandle {
    type Target = Arc<FutexEntry>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for FutexHandle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) <= 2 && self.inner.wq.is_empty() {
            self.table.0.lock().remove(&self.key);
        }
    }
}

impl Drop for FutexGuard<'_> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) <= 2 && self.inner.wq.is_empty() {
            self.table.0.lock().remove(&self.key);
        }
    }
}

struct FutexTables {
    map: BTreeMap<usize, Arc<FutexTable>>,
    operations: usize,
}
impl FutexTables {
    const fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            operations: 0,
        }
    }

    fn get_or_insert(&mut self, key: usize) -> Arc<FutexTable> {
        self.operations += 1;
        if self.operations == 100 {
            self.operations = 0;
            self.map
                .retain(|_, table| Arc::strong_count(table) > 1 || !table.is_empty());
        }
        self.map
            .entry(key)
            .or_insert_with(|| Arc::new(FutexTable::new()))
            .clone()
    }
}

static SHARED_FUTEX_TABLES: Mutex<FutexTables> = Mutex::new(FutexTables::new());

/// Returns the futex table for the given key.
pub fn futex_table_for(key: &FutexKey) -> Arc<FutexTable> {
    match key {
        FutexKey::Private { .. } => current().as_thread().proc_data.futex_table.clone(),
        FutexKey::Shared { region, .. } => {
            let ptr = match region {
                Ok(pages) => Weak::as_ptr(pages) as usize,
                Err(key) => Weak::as_ptr(key) as usize,
            };
            SHARED_FUTEX_TABLES.lock().get_or_insert(ptr)
        }
    }
}
