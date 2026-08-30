//! A blocking mutex implementation.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axtask::{
    can_block_current, current,
    future::{block_on, interruptible},
    yield_now,
};
use event_listener::{Event, listener};

/// A [`lock_api::RawMutex`] implementation.
///
/// When the mutex is locked, the current task will block and be put into the
/// wait queue. When the mutex is unlocked, one task waiting on the queue will
/// be woken up.
pub struct RawMutex {
    event: Event,
    owner_id: AtomicU64,
    /// Number of registered listeners that may block on `event`.
    ///
    /// A waiter initializes and registers with `event` before incrementing
    /// this count. Therefore, a nonzero value proves that `notify()` cannot
    /// take event-listener's lazy-allocation path.
    waiters: AtomicUsize,
    #[cfg(test)]
    notify_calls: AtomicUsize,
    #[cfg(test)]
    handoff_claim_pause: AtomicUsize,
}

// A released lock with an already selected waiter.  Ordinary lockers must
// queue behind it; only the listener woken by unlock may claim this turn.
const HANDOFF_OWNER: u64 = u64::MAX;

impl RawMutex {
    /// Creates a [`RawMutex`].
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            event: Event::new(),
            owner_id: AtomicU64::new(0),
            waiters: AtomicUsize::new(0),
            #[cfg(test)]
            notify_calls: AtomicUsize::new(0),
            #[cfg(test)]
            handoff_claim_pause: AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.waiters.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn notify_count(&self) -> usize {
        self.notify_calls.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn pause_next_handoff_claim(&self) {
        assert_eq!(self.handoff_claim_pause.swap(1, Ordering::SeqCst), 0);
    }

    #[cfg(test)]
    fn wait_for_handoff_claim_pause(&self) {
        while self.handoff_claim_pause.load(Ordering::Acquire) != 2 {
            yield_now();
        }
    }

    #[cfg(test)]
    fn resume_handoff_claim(&self) {
        assert_eq!(self.handoff_claim_pause.swap(3, Ordering::Release), 2);
    }

    #[cfg(test)]
    fn pause_before_handoff_claim(&self) {
        if self
            .handoff_claim_pause
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            while self.handoff_claim_pause.load(Ordering::Acquire) == 2 {
                yield_now();
            }
        }
    }

    /// Acquires this sleeping mutex while allowing a caller-selected terminal
    /// condition to cancel the wait.  Listener registration and the SeqCst
    /// owner recheck are identical to `RawMutex::lock`, so an unlock cannot
    /// be lost between observing contention and blocking.
    fn lock_interruptible(&self, mut cancelled: impl FnMut() -> bool) -> bool {
        let current_id = current().id().as_u64();
        let mut spin = Spin(0);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);

        loop {
            if cancelled() {
                return false;
            }
            if owner_id == current_id {
                panic!("task attempted to recursively acquire an interruptible mutex");
            }
            if owner_id == 0 {
                match self.owner_id.compare_exchange_weak(
                    owner_id,
                    current_id,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(observed) => owner_id = observed,
                }
                continue;
            }
            if !can_block_current() {
                panic!("non-blockable context attempted an interruptible mutex wait");
            }
            if spin.spin() {
                owner_id = self.owner_id.load(Ordering::Relaxed);
                continue;
            }

            listener!(self.event => listener);
            let interest = WaiterInterest::new(&self.waiters);
            owner_id = self.owner_id.load(Ordering::SeqCst);
            if owner_id == 0 {
                continue;
            }

            match block_on(interruptible(listener)) {
                Ok(Ok(())) => {
                    // A notification selects this listener.  Claim the
                    // handoff while its waiter interest is still live, so a
                    // different cancelling listener cannot mistake an empty
                    // counter for permission to reopen this selected turn.
                    #[cfg(test)]
                    self.pause_before_handoff_claim();
                    match self.owner_id.compare_exchange(
                        HANDOFF_OWNER,
                        current_id,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            drop(interest);
                            if cancelled() {
                                self.owner_id.store(HANDOFF_OWNER, Ordering::Release);
                                self.pass_handoff(false);
                                return false;
                            }
                            return true;
                        }
                        Err(observed) => owner_id = observed,
                    }
                }
                Ok(Err(_)) if cancelled() => {
                    // Drop interest while the listener remains registered,
                    // then claim-or-pass any unlock handoff which sampled it.
                    drop(interest);
                    self.cancel_waiter_handoff(current_id);
                    return false;
                }
                // Ordinary task interrupts do not cancel this lock. They are
                // consumed at this wait boundary and the caller's predicate
                // selects only its terminal signal (SIGKILL for mmap).
                Ok(Err(_)) => {}
                Err(error) => panic!("interruptible sleeping mutex wait failed: {error}"),
            }
            owner_id = self.owner_id.load(Ordering::Acquire);
        }
    }

    /// Passes a selected turn. `caller_interest_live` is explicit because a
    /// cancel path may have removed its own registration before handing off;
    /// inferring that from the aggregate waiter count loses the final other
    /// waiter in the two-waiter cancellation interleave.
    fn pass_handoff(&self, caller_interest_live: bool) {
        debug_assert_eq!(self.owner_id.load(Ordering::Acquire), HANDOFF_OWNER);
        // The caller may or may not still be represented in `waiters`; the
        // explicit parameter above is the only reliable distinction.  If no
        // waiter remains, opening the lock is safe: a concurrently
        // registering waiter rechecks owner after listener publication and
        // therefore cannot sleep past it.
        let waiters = self.waiters.load(Ordering::SeqCst);
        let remaining = if caller_interest_live {
            waiters.saturating_sub(1)
        } else {
            waiters
        };
        if remaining == 0 {
            let _ = self.owner_id.compare_exchange(
                HANDOFF_OWNER,
                0,
                Ordering::SeqCst,
                Ordering::Relaxed,
            );
        } else {
            self.event.notify(1);
        }
    }

    /// Finishes cancellation after waiter-interest withdrawal.  A concurrent
    /// unlock may already have selected this listener by publishing the
    /// handoff sentinel; claim and forward that turn so it cannot strand.
    fn cancel_waiter_handoff(&self, current_id: u64) {
        loop {
            if self.owner_id.load(Ordering::SeqCst) != HANDOFF_OWNER {
                return;
            }
            if self
                .owner_id
                .compare_exchange(
                    HANDOFF_OWNER,
                    current_id,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                self.owner_id.store(HANDOFF_OWNER, Ordering::Release);
                self.pass_handoff(false);
                return;
            }
        }
    }
}

impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

/// Advertises that a fully registered event listener may go to sleep.
///
/// Both this counter and the slow-path owner recheck are sequentially
/// consistent with `unlock`. That rules out the lost-wakeup execution where
/// the waiter observes the old owner while the unlocker observes zero waiters.
struct WaiterInterest<'a>(&'a AtomicUsize);

impl<'a> WaiterInterest<'a> {
    fn new(waiters: &'a AtomicUsize) -> Self {
        waiters
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_add(1)
            })
            .expect("mutex waiter count overflow");
        Self(waiters)
    }
}

impl Drop for WaiterInterest<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::SeqCst);
        debug_assert_ne!(previous, 0, "mutex waiter count underflow");
    }
}

struct Spin(u32);

impl Spin {
    #[inline]
    fn spin(&mut self) -> bool {
        if self.0 >= 10 {
            return false;
        }
        self.0 += 1;
        if self.0 <= 3 {
            for _ in 0..(1 << self.0) {
                core::hint::spin_loop();
            }
        } else {
            yield_now();
        }
        true
    }
}

unsafe impl lock_api::RawMutex for RawMutex {
    type GuardMarker = lock_api::GuardSend;

    /// Initial value for an unlocked mutex.
    ///
    /// A “non-constant” const item is a legacy way to supply an initialized
    /// value to downstream static items. Can hopefully be replaced with
    /// `const fn new() -> Self` at some point.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = RawMutex::new();

    #[inline(always)]
    fn lock(&self) {
        let current_id = current().id().as_u64();
        let mut spin = Spin(0);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);

        loop {
            if owner_id == current_id {
                let task = current();
                match task.id_name() {
                    Ok(name) => panic!("{name} tried to acquire mutex it already owns."),
                    Err(error) => panic!(
                        "Task({current_id}) (name unavailable: {error}) tried to acquire mutex it \
                         already owns."
                    ),
                }
            }

            if owner_id == 0 {
                match self.owner_id.compare_exchange_weak(
                    owner_id,
                    current_id,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(x) => owner_id = x,
                }
                continue;
            }

            if !can_block_current() {
                let task = current();
                match task.id_name() {
                    Ok(name) => panic!(
                        "{name} attempted to wait on a sleeping mutex from a non-blockable context"
                    ),
                    Err(error) => panic!(
                        "Task({current_id}) (name unavailable: {error}) attempted to wait on a \
                         sleeping mutex from a non-blockable context"
                    ),
                }
            }

            if spin.spin() {
                owner_id = self.owner_id.load(Ordering::Relaxed);
                continue;
            }

            // Registration initializes the event before waiter interest can
            // become visible to an unlocker.
            listener!(self.event => listener);
            let _interest = WaiterInterest::new(&self.waiters);

            // This SeqCst recheck pairs with the SeqCst owner release and
            // waiter-count load in unlock. If unlock misses our interest, this
            // load must observe the unlocked owner and we cannot go to sleep.
            owner_id = self.owner_id.load(Ordering::SeqCst);
            if owner_id == 0 {
                continue;
            }

            block_on(listener).unwrap_or_else(|error| {
                let task = current();
                match task.id_name() {
                    Ok(name) => panic!("sleeping mutex wait failed for {name}: {error}"),
                    Err(name_error) => panic!(
                        "sleeping mutex wait failed for Task({}) (name unavailable: \
                         {name_error}): {error}",
                        task.id().as_u64()
                    ),
                }
            });
            // Keep the registered interest until this notified listener has
            // claimed the handoff.  An interruptible peer cancelling in this
            // interval must still see a waiter and cannot reopen our turn.
            match self.owner_id.compare_exchange(
                HANDOFF_OWNER,
                current_id,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => owner_id = observed,
            }
        }
    }

    #[inline(always)]
    fn try_lock(&self) -> bool {
        let current_id = current().id().as_u64();
        // The reason for using a strong compare_exchange is explained here:
        // https://github.com/Amanieu/parking_lot/pull/207#issuecomment-575869107
        self.owner_id
            .compare_exchange(0, current_id, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        let task = current();
        let current_id = task.id().as_u64();
        let next_owner = if self.waiters.load(Ordering::SeqCst) != 0 {
            HANDOFF_OWNER
        } else {
            0
        };
        if self
            .owner_id
            .compare_exchange(current_id, next_owner, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            match task.id_name() {
                Ok(name) => panic!("{name} tried to release mutex it doesn't own"),
                Err(error) => panic!(
                    "Task({current_id}) (name unavailable: {error}) tried to release mutex it \
                     doesn't own"
                ),
            }
        }

        // A waiter publishes this count only after Event initialization and
        // listener registration. The SeqCst ordering with the slow-path owner
        // recheck prevents both sides from missing each other.
        if next_owner == HANDOFF_OWNER {
            #[cfg(test)]
            self.notify_calls.fetch_add(1, Ordering::Relaxed);
            self.event.notify(1);
        }
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.owner_id.load(Ordering::Relaxed) != 0
    }
}

/// An alias of [`lock_api::Mutex`].
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
/// An alias of [`lock_api::MutexGuard`].
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

/// Acquires a sleeping mutex with caller-defined cancellation while preserving
/// the raw mutex's event-listener wakeup protocol.
pub fn lock_interruptible<T>(
    mutex: &Mutex<T>,
    cancelled: impl FnMut() -> bool,
) -> Option<MutexGuard<'_, T>> {
    // SAFETY: a successful raw acquisition gives this current task sole lock
    // ownership, which is exactly the precondition for creating its guard.
    if unsafe { mutex.raw() }.lock_interruptible(cancelled) {
        Some(unsafe { mutex.make_guard_unchecked() })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard, Once},
        vec::Vec,
    };

    use axtask as thread;

    use super::{HANDOFF_OWNER, lock_interruptible};
    use crate::Mutex;

    struct TrackingAllocator;

    std::thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe impl GlobalAlloc for TrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS
                .try_with(|tracking| tracking.get())
                .unwrap_or(false)
            {
                ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if TRACK_ALLOCATIONS
                .try_with(|tracking| tracking.get())
                .unwrap_or(false)
            {
                ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if TRACK_ALLOCATIONS
                .try_with(|tracking| tracking.get())
                .unwrap_or(false)
            {
                ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL_ALLOCATOR: TrackingAllocator = TrackingAllocator;

    static INIT: Once = Once::new();
    static TEST_SERIAL: StdMutex<()> = StdMutex::new(());

    fn init_scheduler() -> StdMutexGuard<'static, ()> {
        let serial = TEST_SERIAL.lock().unwrap();
        INIT.call_once(|| thread::init_scheduler().unwrap());
        serial
    }

    fn allocation_count(f: impl FnOnce()) -> usize {
        TRACK_ALLOCATIONS.with(|tracking| {
            assert!(!tracking.replace(true));
        });
        let before = ALLOCATION_COUNT.load(Ordering::Relaxed);
        f();
        let after = ALLOCATION_COUNT.load(Ordering::Relaxed);
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        after - before
    }

    fn may_interrupt() {
        // simulate interrupts
        if fastrand::u8(0..3) == 0 {
            thread::yield_now();
        }
    }

    #[test]
    fn uncontended_unlock_does_not_notify_or_allocate() {
        let _test_serial = init_scheduler();
        let mutex = Mutex::new(());

        // Warm current-task lookup before observing allocator traffic.
        let _ = thread::current().id();
        let allocations = allocation_count(|| drop(mutex.lock()));
        let raw = unsafe { mutex.raw() };

        assert_eq!(allocations, 0);
        assert_eq!(raw.waiter_count(), 0);
        assert_eq!(raw.notify_count(), 0);
    }

    #[test]
    fn registered_waiter_is_not_lost() {
        let _test_serial = init_scheduler();
        let mutex = Arc::new(Mutex::new(()));
        let acquired = Arc::new(AtomicBool::new(false));
        let guard = mutex.lock();

        let waiter_mutex = Arc::clone(&mutex);
        let waiter_acquired = Arc::clone(&acquired);
        let waiter = thread::spawn(move || {
            let _guard = waiter_mutex.lock();
            waiter_acquired.store(true, Ordering::Release);
        })
        .unwrap();

        while unsafe { mutex.raw() }.waiter_count() == 0 {
            thread::yield_now();
        }
        drop(guard);
        waiter.join().unwrap();

        assert!(acquired.load(Ordering::Acquire));
        assert_eq!(unsafe { mutex.raw() }.waiter_count(), 0);
        assert!(unsafe { mutex.raw() }.notify_count() >= 1);
    }

    #[test]
    fn interruptible_wait_ignores_nonterminal_interrupt_and_wakes_on_unlock() {
        let _test_serial = init_scheduler();
        let mutex = Arc::new(Mutex::new(()));
        let acquired = Arc::new(AtomicBool::new(false));
        let guard = mutex.lock();
        let waiter_mutex = Arc::clone(&mutex);
        let waiter_acquired = Arc::clone(&acquired);
        let waiter = thread::spawn(move || {
            let _guard = lock_interruptible(&waiter_mutex, || false).unwrap();
            waiter_acquired.store(true, Ordering::Release);
        })
        .unwrap();

        while unsafe { mutex.raw() }.waiter_count() == 0 {
            thread::yield_now();
        }
        waiter.interrupt();
        thread::yield_now();
        assert!(!acquired.load(Ordering::Acquire));
        drop(guard);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::Acquire));
    }

    #[test]
    fn interruptible_wait_cancels_terminal_signal_without_lost_wakeup() {
        let _test_serial = init_scheduler();
        let mutex = Arc::new(Mutex::new(()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let waiter_cancelled_result = Arc::new(AtomicBool::new(false));
        let guard = mutex.lock();
        let waiter_mutex = Arc::clone(&mutex);
        let waiter_cancelled = Arc::clone(&cancelled);
        let waiter_result = Arc::clone(&waiter_cancelled_result);
        let waiter = thread::spawn(move || {
            waiter_result.store(
                lock_interruptible(&waiter_mutex, || waiter_cancelled.load(Ordering::Acquire))
                    .is_none(),
                Ordering::Release,
            );
        })
        .unwrap();

        while unsafe { mutex.raw() }.waiter_count() == 0 {
            thread::yield_now();
        }
        cancelled.store(true, Ordering::Release);
        waiter.interrupt();
        assert_eq!(waiter.join().unwrap(), 0);
        assert!(waiter_cancelled_result.load(Ordering::Acquire));
        // The waiter never acquired the held mutex.
        drop(guard);
    }

    #[test]
    fn queued_waiter_receives_handoff_before_a_barging_locker() {
        let _test_serial = init_scheduler();
        let mutex = Arc::new(Mutex::new(()));
        let order = Arc::new(AtomicUsize::new(0));
        let guard = mutex.lock();
        let queued_mutex = Arc::clone(&mutex);
        let queued_order = Arc::clone(&order);
        let queued = thread::spawn(move || {
            let _guard = queued_mutex.lock();
            assert_eq!(queued_order.fetch_add(1, Ordering::AcqRel), 0);
        })
        .unwrap();
        while unsafe { mutex.raw() }.waiter_count() == 0 {
            thread::yield_now();
        }
        let barger_mutex = Arc::clone(&mutex);
        let barger_order = Arc::clone(&order);
        let barger = thread::spawn(move || {
            let _guard = barger_mutex.lock();
            assert_eq!(barger_order.fetch_add(1, Ordering::AcqRel), 1);
        })
        .unwrap();
        drop(guard);
        queued.join().unwrap();
        barger.join().unwrap();
    }

    #[test]
    fn cancelled_other_waiter_cannot_reopen_selected_handoff() {
        let _test_serial = init_scheduler();
        let mutex = Arc::new(Mutex::new(()));
        let order = Arc::new(AtomicUsize::new(0));
        let guard = mutex.lock();

        let selected_mutex = Arc::clone(&mutex);
        let selected_order = Arc::clone(&order);
        let selected = thread::spawn(move || {
            let _guard = lock_interruptible(&selected_mutex, || false).unwrap();
            assert_eq!(selected_order.fetch_add(1, Ordering::AcqRel), 0);
        })
        .unwrap();
        while unsafe { mutex.raw() }.waiter_count() != 1 {
            thread::yield_now();
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let cancelling_mutex = Arc::clone(&mutex);
        let cancelling_signal = Arc::clone(&cancelled);
        let cancelling_observed = Arc::clone(&cancellation_observed);
        let cancelling = thread::spawn(move || {
            cancelling_observed.store(
                lock_interruptible(&cancelling_mutex, || {
                    cancelling_signal.load(Ordering::Acquire)
                })
                .is_none(),
                Ordering::Release,
            );
        })
        .unwrap();
        while unsafe { mutex.raw() }.waiter_count() != 2 {
            thread::yield_now();
        }

        let raw = unsafe { mutex.raw() };
        raw.pause_next_handoff_claim();
        drop(guard);
        raw.wait_for_handoff_claim_pause();

        // The first listener has been selected by unlock but deliberately has
        // not claimed it yet.  Cancel the other real listener in this window.
        cancelled.store(true, Ordering::Release);
        cancelling.interrupt();
        assert_eq!(cancelling.join().unwrap(), 0);
        assert!(cancellation_observed.load(Ordering::Acquire));
        assert_eq!(raw.owner_id.load(Ordering::Acquire), HANDOFF_OWNER);

        let barger_mutex = Arc::clone(&mutex);
        let barger_order = Arc::clone(&order);
        let barger = thread::spawn(move || {
            let _guard = barger_mutex.lock();
            assert_eq!(barger_order.fetch_add(1, Ordering::AcqRel), 1);
        })
        .unwrap();
        while raw.waiter_count() != 2 {
            thread::yield_now();
        }
        assert_eq!(raw.owner_id.load(Ordering::Acquire), HANDOFF_OWNER);

        raw.resume_handoff_claim();
        selected.join().unwrap();
        barger.join().unwrap();
        assert_eq!(order.load(Ordering::Acquire), 2);
    }

    #[test]
    fn lots_and_lots() {
        let _test_serial = init_scheduler();

        const NUM_TASKS: u32 = 10;
        const NUM_ITERS: u32 = 10_000;
        let mutex = Arc::new(Mutex::new(0_u32));
        let mut tasks = Vec::new();

        fn spawn_inc(mutex: &Arc<Mutex<u32>>, tasks: &mut Vec<thread::AxTaskRef>, delta: u32) {
            let mutex = Arc::clone(mutex);
            tasks.push(
                thread::spawn(move || {
                    for _ in 0..NUM_ITERS {
                        let mut val = mutex.lock();
                        *val += delta;
                        may_interrupt();
                        drop(val);
                        may_interrupt();
                    }
                })
                .unwrap(),
            );
        }

        for _ in 0..NUM_TASKS {
            spawn_inc(&mutex, &mut tasks, 1);
            spawn_inc(&mutex, &mut tasks, 2);
        }
        for task in tasks {
            task.join().unwrap();
        }

        assert_eq!(*mutex.lock(), NUM_ITERS * NUM_TASKS * 3);
    }
}
