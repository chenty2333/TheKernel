//! A blocking mutex implementation.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axtask::{can_block_current, current, future::block_on, yield_now};
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
}

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
            owner_id = self.owner_id.load(Ordering::Acquire);
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
        let owner_id = self.owner_id.swap(0, Ordering::SeqCst);
        let task = current();
        let current_id = task.id().as_u64();
        if owner_id != current_id {
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
        if self.waiters.load(Ordering::SeqCst) != 0 {
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

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::{Arc, Once},
        vec::Vec,
    };

    use axtask as thread;

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

    fn init_scheduler() {
        INIT.call_once(|| thread::init_scheduler().unwrap());
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
        init_scheduler();
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
        init_scheduler();
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
    fn lots_and_lots() {
        init_scheduler();

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
