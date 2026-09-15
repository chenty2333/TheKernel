//! Arc-owned, fixed-capacity provider I/O state machine.
//!
//! `prepare_reserve` allocates nothing after queue construction, `publish`
//! only moves a payload into its reserved slot, and the payload remains owned
//! by the queue execution domain through terminal completion or teardown.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use axfs_ng_vfs::{VfsError, VfsResult};
use axsync::Mutex;
use axtask::WaitQueue;
use core::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Reserved,
    Publishing,
    Published,
    Claimed,
}
struct Slot<T> {
    generation: u64,
    state: SlotState,
    cancel_requested: bool,
    value: Option<T>,
}
struct State<T> {
    slots: Vec<Slot<T>>,
    cursor: usize,
}

/// Provider-defined terminal failure delivery for queue-owned payloads.
/// It is invoked for cancellation, teardown, and abandoned in-flight work.
pub trait ProviderIoTerminalSink<T>: Send + Sync {
    fn terminal_failure(&self, value: T, reason: ProviderIoTerminalReason);
    fn terminal_complete(&self, value: T);
}

/// Why queue ownership ended without a worker completion.  The provider maps
/// this to its public completion contract instead of flattening teardown and
/// explicit cancellation into a synthetic device I/O failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderIoTerminalReason {
    Cancelled,
    Teardown,
    Abandoned,
}

/// Fixed-capacity queue. Every externally-held handle owns an `Arc`, so it
/// can live inside a boxed provider submission hook without borrowed state.
pub struct ProviderIoQueue<T, const SLOTS: usize> {
    state: Mutex<State<T>>,
    terminal: Arc<dyn ProviderIoTerminalSink<T>>,
    admission: AtomicU8,
    wake: WaitQueue,
}

impl<T, const SLOTS: usize> ProviderIoQueue<T, SLOTS> {
    pub fn try_new(terminal: Arc<dyn ProviderIoTerminalSink<T>>) -> VfsResult<Arc<Self>> {
        if SLOTS == 0 {
            return Err(VfsError::InvalidInput);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(SLOTS)
            .map_err(|_| VfsError::NoMemory)?;
        for _ in 0..SLOTS {
            slots.push(Slot {
                generation: 1,
                state: SlotState::Free,
                cancel_requested: false,
                value: None,
            });
        }
        Arc::try_new(Self {
            state: Mutex::new(State { slots, cursor: 0 }),
            terminal,
            admission: AtomicU8::new(0),
            wake: WaitQueue::new(),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    pub fn prepare_reserve(self: &Arc<Self>) -> VfsResult<PreparedProviderIo<T, SLOTS>> {
        if self.admission.load(Ordering::Acquire) != 0 {
            return Err(VfsError::ResourceBusy);
        }
        let mut state = self.state.lock();
        if self.admission.load(Ordering::Acquire) != 0 {
            return Err(VfsError::ResourceBusy);
        }
        let start = state.cursor;
        for offset in 0..SLOTS {
            let index = (start + offset) % SLOTS;
            let slot = &mut state.slots[index];
            if slot.state == SlotState::Free {
                slot.state = SlotState::Reserved;
                let generation = slot.generation;
                state.cursor = (index + 1) % SLOTS;
                return Ok(PreparedProviderIo {
                    queue: self.clone(),
                    index,
                    generation,
                    armed: true,
                });
            }
        }
        Err(VfsError::WouldBlock)
    }

    fn release(&self, index: usize, generation: u64, expected: SlotState) -> Option<T> {
        let mut state = self.state.lock();
        let slot = state.slots.get_mut(index)?;
        if slot.generation != generation || slot.state != expected {
            return None;
        }
        let value = slot.value.take();
        slot.state = SlotState::Free;
        slot.cancel_requested = false;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        value
    }

    fn claim(self: &Arc<Self>) -> Option<ProviderIoInFlight<T, SLOTS>> {
        if self.admission.load(Ordering::Acquire) != 0 {
            return None;
        }
        let mut state = self.state.lock();
        if self.admission.load(Ordering::Acquire) != 0 {
            return None;
        }
        for index in 0..SLOTS {
            let slot = &mut state.slots[index];
            if slot.state == SlotState::Published {
                slot.state = SlotState::Claimed;
                let value = slot
                    .value
                    .take()
                    .expect("published provider I/O missing value");
                return Some(ProviderIoInFlight {
                    queue: self.clone(),
                    index,
                    generation: slot.generation,
                    value: Some(value),
                });
            }
        }
        None
    }

    /// No new reservations are admitted after this point.  Waking is kept
    /// separate so teardown can first close admission, then atomically make
    /// every persistent worker re-check its predicate.
    /// Starts a reversible unmount quiesce.  Existing claimed I/O may finish,
    /// while new reservations and publications are rejected.
    pub fn begin_quiesce(&self) -> bool {
        self.admission
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn abort_quiesce(&self) {
        let _ = self
            .admission
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
        self.wake_workers();
    }
    pub fn commit_terminal(&self) {
        self.admission.store(2, Ordering::Release);
        self.wake_workers();
    }
    pub fn admission_open(&self) -> bool {
        self.admission.load(Ordering::Acquire) == 0
    }
    pub fn terminal(&self) -> bool {
        self.admission.load(Ordering::Acquire) == 2
    }
    pub fn has_published(&self) -> bool {
        self.state
            .lock()
            .slots
            .iter()
            .any(|slot| slot.state == SlotState::Published)
    }
    pub fn wake_workers(&self) {
        self.wake.notify_all(false);
    }
    pub fn wait_until_published_or_terminal(&self) {
        let _ = self
            .wake
            .wait_until(|| self.terminal() || (self.admission_open() && self.has_published()));
    }
    pub fn wait_until_no_claimed(&self) {
        let _ = self.wake.wait_until(|| self.claimed() == 0);
    }
    /// Closes admission and terminal-fails queued work.  Claimed work remains
    /// owned by its token; callers can wait for `occupied() == 0` before the
    /// filesystem finalizer tears down lower state.
    pub fn close_and_fail_published(&self) {
        self.commit_terminal();
        self.fail_published();
        self.wake_workers();
    }

    /// Delivers terminal failure for every published payload exactly once.
    /// Claimed payloads remain with their in-flight token, which owns the
    /// same terminal sink and fails on abandonment.
    pub fn fail_published(&self) {
        loop {
            let value = {
                let mut state = self.state.lock();
                let Some(slot) = state
                    .slots
                    .iter_mut()
                    .find(|slot| slot.state == SlotState::Published)
                else {
                    break;
                };
                let value = slot.value.take();
                slot.state = SlotState::Free;
                slot.cancel_requested = false;
                slot.generation = slot.generation.wrapping_add(1).max(1);
                value
            };
            if let Some(value) = value {
                self.terminal
                    .terminal_failure(value, ProviderIoTerminalReason::Teardown);
            }
        }
        self.wake_workers();
    }

    pub fn occupied(&self) -> usize {
        self.state
            .lock()
            .slots
            .iter()
            .filter(|slot| slot.state != SlotState::Free)
            .count()
    }
    pub fn claimed(&self) -> usize {
        self.state
            .lock()
            .slots
            .iter()
            .filter(|slot| slot.state == SlotState::Claimed)
            .count()
    }
}

impl<T, const SLOTS: usize> Drop for ProviderIoQueue<T, SLOTS> {
    fn drop(&mut self) {
        // At the final Arc drop no queue handle remains.  Published payloads
        // still reside in slots and must be terminal-failed; a claimed value
        // is instead owned by ProviderIoInFlight, whose Arc necessarily keeps
        // this queue alive until its own Drop/complete path runs.
        loop {
            let value = {
                let mut state = self.state.lock();
                let Some(slot) = state
                    .slots
                    .iter_mut()
                    .find(|slot| slot.state == SlotState::Published)
                else {
                    break;
                };
                let value = slot.value.take();
                slot.state = SlotState::Free;
                slot.generation = slot.generation.wrapping_add(1).max(1);
                value
            };
            if let Some(value) = value {
                self.terminal
                    .terminal_failure(value, ProviderIoTerminalReason::Abandoned);
            }
        }
    }
}

/// Exact pre-publication reservation. It owns the queue Arc and is `'static`
/// when T is, making it suitable for provider-owned VFS submission state.
pub struct PreparedProviderIo<T, const SLOTS: usize> {
    queue: Arc<ProviderIoQueue<T, SLOTS>>,
    index: usize,
    generation: u64,
    armed: bool,
}

impl<T, const SLOTS: usize> PreparedProviderIo<T, SLOTS> {
    /// Transitions the slot under lock before any provider conversion code
    /// runs.  The returned permit owns the exact Publishing generation.
    pub fn begin_publish(
        mut self,
    ) -> Result<ProviderIoPublishPermit<T, SLOTS>, ProviderIoPublishRejected> {
        let mut state = self.queue.state.lock();
        let slot = &mut state.slots[self.index];
        assert_eq!(
            slot.generation, self.generation,
            "provider I/O reservation generation changed"
        );
        assert_eq!(
            slot.state,
            SlotState::Reserved,
            "provider I/O reservation not reserved"
        );
        if self.queue.admission.load(Ordering::Acquire) != 0 {
            slot.state = SlotState::Free;
            slot.cancel_requested = false;
            slot.generation = slot.generation.wrapping_add(1).max(1);
            self.armed = false;
            drop(state);
            return Err(ProviderIoPublishRejected);
        }
        slot.state = SlotState::Publishing;
        slot.cancel_requested = false;
        self.armed = false;
        Ok(ProviderIoPublishPermit {
            queue: self.queue.clone(),
            index: self.index,
            generation: self.generation,
            armed: true,
        })
    }

    pub fn cancel(mut self) {
        let _ = self
            .queue
            .release(self.index, self.generation, SlotState::Reserved);
        self.armed = false;
    }
}

/// A rejected begin-publish has consumed and retired its reservation; it
/// intentionally exposes no prepared handle that another path could reuse.
pub struct ProviderIoPublishRejected;

/// Two-phase publish permit.  Provider code constructs T outside the queue
/// lock; this method only stores it and transitions Publishing → Published.
pub struct ProviderIoPublishPermit<T, const SLOTS: usize> {
    queue: Arc<ProviderIoQueue<T, SLOTS>>,
    index: usize,
    generation: u64,
    armed: bool,
}
impl<T, const SLOTS: usize> ProviderIoPublishPermit<T, SLOTS> {
    pub fn publish(mut self, value: T) -> SubmittedProviderIo<T, SLOTS> {
        let mut state = self.queue.state.lock();
        let slot = &mut state.slots[self.index];
        assert_eq!(
            slot.generation, self.generation,
            "provider I/O publish generation changed"
        );
        assert_eq!(
            slot.state,
            SlotState::Publishing,
            "provider I/O permit not publishing"
        );
        if self.queue.admission.load(Ordering::Acquire) != 0 {
            slot.state = SlotState::Free;
            slot.cancel_requested = false;
            slot.generation = slot.generation.wrapping_add(1).max(1);
            self.armed = false;
            drop(state);
            self.queue
                .terminal
                .terminal_failure(value, ProviderIoTerminalReason::Teardown);
            return SubmittedProviderIo {
                queue: self.queue.clone(),
                index: self.index,
                generation: self.generation,
            };
        }
        slot.value = Some(value);
        slot.state = SlotState::Published;
        self.armed = false;
        self.queue.wake_workers();
        SubmittedProviderIo {
            queue: self.queue.clone(),
            index: self.index,
            generation: self.generation,
        }
    }
}
impl<T, const SLOTS: usize> Drop for ProviderIoPublishPermit<T, SLOTS> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .queue
                .release(self.index, self.generation, SlotState::Publishing);
        }
    }
}
impl<T, const SLOTS: usize> Drop for PreparedProviderIo<T, SLOTS> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .queue
                .release(self.index, self.generation, SlotState::Reserved);
        }
    }
}

/// Consuming cancellation result. Cancellation itself delivers the queue
/// payload's terminal failure through the queue-owned sink.
pub enum ProviderIoCancelOutcome {
    Cancelled,
    InFlight,
    Terminal,
}

/// Cancellation/control only. Dropping it neither steals nor fails queue data.
pub struct SubmittedProviderIo<T, const SLOTS: usize> {
    queue: Arc<ProviderIoQueue<T, SLOTS>>,
    index: usize,
    generation: u64,
}
impl<T, const SLOTS: usize> SubmittedProviderIo<T, SLOTS> {
    pub fn cancel(self) -> ProviderIoCancelOutcome {
        match self
            .queue
            .release(self.index, self.generation, SlotState::Published)
        {
            Some(value) => {
                self.queue
                    .terminal
                    .terminal_failure(value, ProviderIoTerminalReason::Cancelled);
                self.queue.wake_workers();
                ProviderIoCancelOutcome::Cancelled
            }
            None => {
                let mut state = self.queue.state.lock();
                match state.slots.get_mut(self.index) {
                    Some(slot)
                        if slot.generation == self.generation
                            && slot.state == SlotState::Claimed =>
                    {
                        slot.cancel_requested = true;
                        ProviderIoCancelOutcome::InFlight
                    }
                    _ => ProviderIoCancelOutcome::Terminal,
                }
            }
        }
    }
}

/// Queue-owned in-flight payload. Providers retain this Arc-owned token until
/// their device/RPC terminal path runs; `complete` is the only normal release.
pub struct ProviderIoInFlight<T, const SLOTS: usize> {
    queue: Arc<ProviderIoQueue<T, SLOTS>>,
    index: usize,
    generation: u64,
    value: Option<T>,
}
impl<T, const SLOTS: usize> ProviderIoInFlight<T, SLOTS> {
    pub fn with_value<R>(&mut self, use_value: impl FnOnce(&mut T) -> R) -> R {
        use_value(
            self.value
                .as_mut()
                .expect("in-flight provider I/O missing value"),
        )
    }
    pub fn cancel_requested(&self) -> bool {
        let state = self.queue.state.lock();
        matches!(state.slots.get(self.index), Some(slot) if slot.generation == self.generation && slot.state == SlotState::Claimed && slot.cancel_requested)
    }
    pub fn complete(mut self) {
        let value = self
            .value
            .take()
            .expect("in-flight provider I/O missing value");
        let _ = self
            .queue
            .release(self.index, self.generation, SlotState::Claimed);
        self.queue.wake_workers();
        // Release the queue claim before invoking an external completion
        // callback.  A callback may synchronously unmount this filesystem;
        // teardown must observe the lower operation as terminal rather than
        // waiting for this worker to return from the callback.
        self.queue.terminal.terminal_complete(value);
    }
}
impl<T, const SLOTS: usize> Drop for ProviderIoInFlight<T, SLOTS> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            let _ = self
                .queue
                .release(self.index, self.generation, SlotState::Claimed);
            self.queue.wake_workers();
            // Match `complete`: the queue is terminal before an arbitrary
            // provider callback can re-enter unmount or cancellation paths.
            // `value` remains uniquely owned on this stack, while `queue`
            // retains the terminal sink for the duration of delivery.
            self.queue
                .terminal
                .terminal_failure(value, ProviderIoTerminalReason::Abandoned);
        }
    }
}

/// Persistent worker handle owning an Arc queue; no borrowed lifetime leaks
/// into provider task state.
pub struct ProviderIoWorker<T, const SLOTS: usize> {
    queue: Arc<ProviderIoQueue<T, SLOTS>>,
}
impl<T, const SLOTS: usize> ProviderIoWorker<T, SLOTS> {
    pub fn new(queue: Arc<ProviderIoQueue<T, SLOTS>>) -> Self {
        Self { queue }
    }
    pub fn run_once(&self, consume: impl FnOnce(ProviderIoInFlight<T, SLOTS>)) -> bool {
        let Some(in_flight) = self.queue.claim() else {
            return false;
        };
        consume(in_flight);
        true
    }
}

/// Persistent worker state for a provider task.  It deliberately retains only
/// a weak queue reference, so an idle task cannot keep a filesystem mounted.
pub struct ProviderIoWeakWorker<T, const SLOTS: usize> {
    queue: Weak<ProviderIoQueue<T, SLOTS>>,
}
impl<T, const SLOTS: usize> ProviderIoWeakWorker<T, SLOTS> {
    pub fn new(queue: &Arc<ProviderIoQueue<T, SLOTS>>) -> Self {
        Self {
            queue: Arc::downgrade(queue),
        }
    }
    pub fn run(self, mut consume: impl FnMut(ProviderIoInFlight<T, SLOTS>)) {
        loop {
            let Some(queue) = self.queue.upgrade() else {
                break;
            };
            queue.wait_until_published_or_terminal();
            if queue.terminal() {
                break;
            }
            while let Some(in_flight) = queue.claim() {
                consume(in_flight);
            }
        }
    }
}
