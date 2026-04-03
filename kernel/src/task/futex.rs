//! Futex implementation.

use alloc::{
    collections::{btree_map::BTreeMap, vec_deque::VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::{Future, poll_fn},
    ops::Deref,
    sync::atomic::AtomicBool,
    task::{Context, Poll, Waker},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use axtask::{
    current,
    future::{block_on, interruptible},
};
use hashbrown::HashMap;
use kspin::SpinNoIrq;
use memory_addr::VirtAddr;

use crate::{
    mm::{AddrSpace, Backend, SharedPages},
    task::{AlarmClock, AsThread, sleep_until_clock},
};

/// Wait queue used by futex.
#[derive(Default)]
pub struct WaitQueue {
    queue: SpinNoIrq<VecDeque<Arc<SpinNoIrq<WaiterEntry>>>>,
}

struct WaiterEntry {
    bitset: u32,
    awakened: bool,
    waker: Option<Waker>,
}

struct WaitFuture<'a, F> {
    queue: &'a WaitQueue,
    bitset: u32,
    timeout: Option<(AlarmClock, Duration)>,
    condition: Option<F>,
    waiter: Option<Arc<SpinNoIrq<WaiterEntry>>>,
}

impl<F> Unpin for WaitFuture<'_, F> {}

impl<F> Drop for WaitFuture<'_, F> {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            self.queue.remove_waiter(&waiter);
        }
    }
}

impl<F: FnOnce() -> bool> Future for WaitFuture<'_, F> {
    type Output = AxResult<bool>;

    fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(waiter) = self.waiter.as_ref() {
            let mut waiter = waiter.lock();
            if waiter.awakened {
                return Poll::Ready(Ok(true));
            }
            waiter.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let Some(condition) = self.condition.take() else {
            return Poll::Pending;
        };
        if !condition() {
            return Poll::Ready(Ok(false));
        }
        if self
            .timeout
            .is_some_and(|(clock, deadline)| clock.now() >= deadline)
        {
            return Poll::Ready(Err(AxError::TimedOut));
        }

        let waiter = Arc::new(SpinNoIrq::new(WaiterEntry {
            bitset: self.bitset,
            awakened: false,
            waker: Some(cx.waker().clone()),
        }));
        self.queue.queue.lock().push_back(waiter.clone());
        self.waiter = Some(waiter);
        Poll::Pending
    }
}
impl WaitQueue {
    /// Creates a new `WaitQueue`.
    pub fn new() -> Self {
        Self::default()
    }

    fn remove_waiter(&self, target: &Arc<SpinNoIrq<WaiterEntry>>) {
        self.queue.lock().retain(|waiter| !Arc::ptr_eq(waiter, target));
    }

    /// Waits if the given condition is met.
    ///
    /// Returns `false` if the condition is not met and no actual waiting
    /// occurs.
    pub fn wait_if(
        &self,
        bitset: u32,
        timeout: Option<(AlarmClock, Duration)>,
        condition: impl FnOnce() -> bool,
    ) -> AxResult<bool> {
        let wait = WaitFuture {
            queue: self,
            bitset,
            timeout,
            condition: Some(condition),
            waiter: None,
        };
        let wait = async {
            if let Some((clock, deadline)) = timeout {
                let mut wait = core::pin::pin!(wait);
                let mut sleeper = core::pin::pin!(sleep_until_clock(clock, deadline));
                poll_fn(|cx| {
                    if let Poll::Ready(result) = wait.as_mut().poll(cx) {
                        return Poll::Ready(result);
                    }
                    if sleeper.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(AxError::TimedOut));
                    }
                    Poll::Pending
                })
                .await
            } else {
                wait.await
            }
        };
        block_on(interruptible(wait))?
    }

    /// Wakes up at most `count` tasks whose bitset intersects with the given
    /// bitmask.
    pub fn wake(&self, count: usize, mask: u32) -> usize {
        let mut woke = 0;
        self.queue.lock().retain(|waiter| {
            let mut waiter = waiter.lock();
            if woke >= count || (waiter.bitset & mask) == 0 {
                true
            } else {
                waiter.awakened = true;
                if let Some(waker) = waiter.waker.take() {
                    waker.wake();
                }
                woke += 1;
                false
            }
        });
        woke
    }

    /// Checks if the wait queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Requeue at most `count` tasks to the target wait queue.
    pub fn requeue(&self, mut count: usize, target: &WaitQueue) -> usize {
        let tasks: Vec<_> = {
            let mut wq = self.queue.lock();
            count = count.min(wq.len());
            wq.drain(..count).collect()
        };
        if !tasks.is_empty() {
            let mut wq = target.queue.lock();
            wq.extend(tasks);
        }
        count
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
                    return Self::Shared {
                        offset: address - area.start().as_usize(),
                        region: Ok(Arc::downgrade(backend.pages())),
                    };
                }
                Backend::File(file) => {
                    return Self::Shared {
                        offset: address - area.start().as_usize(),
                        region: Err(file.futex_handle()),
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

    /// Used by robust list, indicates if the owner of this futex is dead.
    pub owner_dead: AtomicBool,
}

impl FutexEntry {
    fn new() -> Self {
        Self {
            wq: WaitQueue::new(),
            owner_dead: AtomicBool::new(false),
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
