use alloc::vec::Vec;
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_block::{
    BlockAsyncOp, BlockDriverOps, BlockPhysicalSegment, BlockPhysicalSgOutcome, BlockQueueCaps,
    BlockQueueRequest, BlockRequestHandle, BlockSegment, BlockSegmentDirection,
    BlockSubmitReport,
};
use axtask::{
    WaitError, WaitQueue,
    future::{BlockOnError, TimerRegistrationError},
};
use spin::Mutex;
use virtio_drivers::{
    Hal,
    device::blk::{
        MAX_PHYSICAL_SG, PENDING_COMPLETION_DRAIN_BUDGET, PendingBlkBatchBuffer,
        PendingBlkBatchRequest, PendingBlkDrainStatus, PendingBlkHandle,
        PhysicalSegment as VirtioPhysicalSegment, VirtIOBlk as InnerDev,
    },
    stats::{
        AsyncBlockWaitPolicy, async_block_enabled, async_block_merge_write_enabled,
        async_block_wait_policy, record_blk_async_irq_first_arm,
        record_blk_async_irq_first_fallback, record_blk_async_irq_first_fallback_cannot_block,
        record_blk_async_irq_first_fallback_feature_disabled,
        record_blk_async_irq_first_fallback_no_irq,
        record_blk_async_irq_first_fallback_register_failed,
        record_blk_async_irq_first_fallback_unarmed, record_blk_async_irq_first_wait,
        record_blk_async_merge_write, record_blk_async_wait_sleep, record_blk_async_wait_spin,
        record_blk_async_wait_spin_hit, record_blk_async_wait_timeout,
        record_blk_async_wait_wakeup, record_blk_async_wait_yield, record_blk_data_fence,
        record_blk_metadata_fence,
    },
    transport::Transport,
};

use crate::as_dev_err;

const ASYNC_WAIT_SPIN_BUDGET: u64 = 64;
const ASYNC_WAIT_TIMEOUT_US: u64 = 100;
const ASYNC_WRITE_SEGMENTS_BASE: usize = 4;
const ASYNC_WRITE_SEGMENTS_INDIRECT_MERGED: usize = 8;

static IRQ_FIRST_WAIT_QUEUE: WaitQueue = WaitQueue::new();
#[cfg(feature = "irq")]
const IRQ_SLOT_EMPTY: usize = usize::MAX;
#[cfg(feature = "irq")]
const IRQ_SLOT_COUNT: usize = 16;
#[cfg(feature = "irq")]
struct IrqEndpoint {
    irq: AtomicUsize,
    ptr: AtomicUsize,
    callback: AtomicUsize,
    active: AtomicBool,
    readers: AtomicUsize,
}

#[cfg(feature = "irq")]
impl IrqEndpoint {
    const fn new() -> Self {
        Self {
            irq: AtomicUsize::new(IRQ_SLOT_EMPTY),
            ptr: AtomicUsize::new(0),
            callback: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "irq")]
static IRQ_ENDPOINTS: [IrqEndpoint; IRQ_SLOT_COUNT] =
    [const { IrqEndpoint::new() }; IRQ_SLOT_COUNT];
#[cfg(feature = "irq")]
static IRQ_REGISTRY_LOCK: Mutex<()> = Mutex::new(());
#[cfg(feature = "irq")]
const REGISTERED_IRQ_CAPACITY: usize = 256;
#[cfg(feature = "irq")]
struct RegisteredIrqs {
    entries: [usize; REGISTERED_IRQ_CAPACITY],
    len: usize,
}

#[cfg(feature = "irq")]
impl RegisteredIrqs {
    const fn new() -> Self {
        Self {
            entries: [0; REGISTERED_IRQ_CAPACITY],
            len: 0,
        }
    }

    fn contains(&self, irq: usize) -> bool {
        self.entries[..self.len].contains(&irq)
    }

    fn insert(&mut self, irq: usize) -> bool {
        if self.len == self.entries.len() {
            return false;
        }
        self.entries[self.len] = irq;
        self.len += 1;
        true
    }
}

#[cfg(feature = "irq")]
static REGISTERED_IRQS: Mutex<RegisteredIrqs> = Mutex::new(RegisteredIrqs::new());

fn reap_all_async_handles<Handle: Copy>(
    handles: &[Handle],
    mut wait_one: impl FnMut(Handle) -> DevResult,
) -> DevResult {
    let mut first_error = None;
    for handle in handles.iter().copied() {
        if let Err(error) = wait_one(handle) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn accepted_pending_handles<'a>(
    pending: &'a [PendingBlkBatchRequest<'_>],
    submitted: usize,
) -> impl Iterator<Item = PendingBlkHandle> + 'a {
    let accepted = pending.get(..submitted).unwrap_or_else(|| {
        panic!(
            "asynchronous block submit overreported {submitted} accepted requests for {} entries",
            pending.len()
        )
    });
    accepted.iter().map(|request| {
        request
            .handle
            .expect("accepted asynchronous block request is missing its handle")
    })
}

fn drain_requires_continuation(
    status: PendingBlkDrainStatus,
    observed_irq_generation: u64,
    current_irq_generation: u64,
) -> bool {
    status.has_continuation() || current_irq_generation != observed_irq_generation
}

fn accepted_request_handles<'a>(
    requests: &'a [BlockQueueRequest<'_>],
    submitted: usize,
) -> impl Iterator<Item = BlockRequestHandle> + 'a {
    let accepted = requests.get(..submitted).unwrap_or_else(|| {
        panic!(
            "block submit overreported {submitted} accepted requests for {} entries",
            requests.len()
        )
    });
    accepted.iter().map(|request| {
        request
            .handle
            .expect("accepted block request is missing its completion handle")
    })
}

fn wait_error_to_dev(error: WaitError) -> DevError {
    match error {
        WaitError::Block(BlockOnError::Busy) => DevError::ResourceBusy,
        WaitError::Block(BlockOnError::CannotBlock) => DevError::Again,
        WaitError::Block(BlockOnError::GenerationExhausted | BlockOnError::StateLost) => {
            DevError::BadState
        }
        WaitError::Interrupted => DevError::Again,
        WaitError::Timer(TimerRegistrationError::CapacityExhausted) => DevError::ResourceBusy,
        WaitError::Timer(
            TimerRegistrationError::TokenSpaceExhausted | TimerRegistrationError::DeadlineOverflow,
        ) => DevError::BadState,
    }
}

fn physical_submit_error(error: virtio_drivers::Error) -> DevResult<BlockPhysicalSgOutcome> {
    match error {
        // These errors are returned before publish_unpublished, so no device
        // request can retain the caller's physical ranges.
        virtio_drivers::Error::QueueFull
        | virtio_drivers::Error::DmaError
        | virtio_drivers::Error::Unsupported => Ok(BlockPhysicalSgOutcome::NotSubmitted),
        error => Err(as_dev_err(error)),
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum IrqFirstArmState {
    Armed,
    NoIrq,
    RegisterFailed,
    FeatureDisabled,
}

#[cfg(feature = "irq")]
fn virtio_blk_irq_wake_handler() {
    // The platform callback has no IRQ argument.  It therefore dispatches
    // every live VirtIO block endpoint; each transport's acknowledge is the
    // authoritative filter for the line that actually raised the interrupt.
    // Completion draining remains exclusively in task context.
    dispatch_registered_irq(None);
    notify_irq_first_waiters();
}

#[cfg(feature = "irq")]
fn ensure_platform_irq_handler(irq: usize) -> bool {
    let mut registered = REGISTERED_IRQS.lock();
    if registered.contains(irq) {
        return true;
    }
    if !axhal::irq::register(irq, virtio_blk_irq_wake_handler) {
        return false;
    }
    registered.insert(irq)
}

#[cfg(not(feature = "irq"))]
fn arm_irq_first_wait(irq: Option<usize>) -> IrqFirstArmState {
    if irq.is_some() {
        IrqFirstArmState::FeatureDisabled
    } else {
        IrqFirstArmState::NoIrq
    }
}

#[cfg(feature = "irq")]
fn dispatch_registered_irq(irq: Option<usize>) {
    for endpoint in &IRQ_ENDPOINTS {
        if !endpoint.active.load(Ordering::Acquire) {
            continue;
        }
        if let Some(irq) = irq {
            if endpoint.irq.load(Ordering::Acquire) != irq {
                continue;
            }
        }

        // Pin the endpoint before loading its pointer.  Teardown marks the
        // endpoint inactive and waits for this reader count to reach zero
        // before clearing the callback and allowing the object to drop.
        endpoint.readers.fetch_add(1, Ordering::AcqRel);
        if endpoint.active.load(Ordering::Acquire)
            && irq.map_or(true, |irq| endpoint.irq.load(Ordering::Acquire) == irq)
        {
            let ptr = endpoint.ptr.load(Ordering::Acquire);
            let callback = endpoint.callback.load(Ordering::Acquire);
            if ptr != 0 && callback != 0 {
                // SAFETY: registration stores a callback whose concrete
                // generic type matches the pointer in this endpoint.  The
                // reader count keeps that object alive through the call.
                let callback =
                    unsafe { core::mem::transmute::<usize, unsafe fn(*const ())>(callback) };
                unsafe { callback(ptr as *const ()) };
            }
        }
        endpoint.readers.fetch_sub(1, Ordering::Release);
    }
}

#[cfg(feature = "irq")]
unsafe fn irq_callback<H: Hal, T: Transport>(ptr: *const ()) {
    // SAFETY: `ptr` was captured from a live `VirtIoBlkDev<H, T>` by
    // `arm_irq_first_wait`, and the endpoint reader count keeps it alive
    // until this callback returns.
    let dev = unsafe { &*(ptr as *const VirtIoBlkDev<H, T>) };
    let _ = dev.handle_irq();
}

/// Dispatches a platform IRQ to registered VirtIO block devices.
///
/// The top-level IRQ dispatcher may call this hook when it owns a shared
/// IRQ-hook chain.  The direct platform registration above also calls the
/// same endpoint path for kernels without such a chain.
#[cfg(feature = "irq")]
pub fn dispatch_irq(irq: usize) {
    dispatch_registered_irq(Some(irq));
}

#[cfg(not(feature = "irq"))]
#[allow(dead_code)]
pub fn dispatch_irq(_irq: usize) {}

fn notify_irq_first_waiters() {
    if IRQ_FIRST_WAIT_QUEUE.notify_many(usize::MAX, false) > 0 {
        record_blk_async_wait_wakeup();
    }
}

/// The VirtIO block device driver.
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    inner: Mutex<InnerDev<H, T>>,
    wait_queue: WaitQueue,
    irq: Option<usize>,
    irq_enabled: AtomicBool,
    irq_wait_armed: AtomicBool,
    irq_generation: AtomicU64,
    continuation_pending: AtomicBool,
    #[cfg(feature = "irq")]
    irq_slot: AtomicUsize,
}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DevResult<Self> {
        Self::try_new_with_irq(transport, None)
    }

    /// Creates a new driver instance with an optional platform IRQ number.
    pub fn try_new_with_irq(transport: T, irq: Option<usize>) -> DevResult<Self> {
        let inner = InnerDev::new(transport).map_err(as_dev_err)?;
        Ok(Self {
            inner: Mutex::new(inner),
            wait_queue: WaitQueue::new(),
            irq,
            irq_enabled: AtomicBool::new(false),
            irq_wait_armed: AtomicBool::new(false),
            irq_generation: AtomicU64::new(0),
            continuation_pending: AtomicBool::new(false),
            #[cfg(feature = "irq")]
            irq_slot: AtomicUsize::new(IRQ_SLOT_EMPTY),
        })
    }

    #[cfg(feature = "irq")]
    fn arm_irq_first_wait(&self) -> IrqFirstArmState {
        let Some(irq) = self.irq else {
            return IrqFirstArmState::NoIrq;
        };

        let ptr = self as *const Self as *const ();
        let callback = irq_callback::<H, T> as *const () as usize;
        let current = self.irq_slot.load(Ordering::Acquire);
        if current < IRQ_SLOT_COUNT {
            let endpoint = &IRQ_ENDPOINTS[current];
            if endpoint.active.load(Ordering::Acquire)
                && endpoint.irq.load(Ordering::Acquire) == irq
                && endpoint.ptr.load(Ordering::Acquire) == ptr as usize
            {
                return IrqFirstArmState::Armed;
            }
        }

        let _registry = IRQ_REGISTRY_LOCK.lock();
        // The fast path above is only an optimization; re-check under the
        // registry lock before creating a second endpoint for this device.
        for (index, endpoint) in IRQ_ENDPOINTS.iter().enumerate() {
            if endpoint.active.load(Ordering::Acquire)
                && endpoint.irq.load(Ordering::Acquire) == irq
                && endpoint.ptr.load(Ordering::Acquire) == ptr as usize
            {
                self.irq_slot.store(index, Ordering::Release);
                return IrqFirstArmState::Armed;
            }
        }
        let Some((index, endpoint)) = IRQ_ENDPOINTS
            .iter()
            .enumerate()
            .find(|(_, endpoint)| !endpoint.active.load(Ordering::Acquire))
        else {
            return IrqFirstArmState::RegisterFailed;
        };

        if !ensure_platform_irq_handler(irq) {
            return IrqFirstArmState::RegisterFailed;
        }

        endpoint.irq.store(irq, Ordering::Relaxed);
        endpoint.ptr.store(ptr as usize, Ordering::Relaxed);
        endpoint.callback.store(callback, Ordering::Relaxed);
        endpoint.active.store(true, Ordering::Release);
        self.irq_slot.store(index, Ordering::Release);
        IrqFirstArmState::Armed
    }

    #[cfg(not(feature = "irq"))]
    fn arm_irq_first_wait(&self) -> IrqFirstArmState {
        arm_irq_first_wait(self.irq)
    }

    #[cfg(feature = "irq")]
    fn disarm_irq_endpoint(&self) {
        let index = self.irq_slot.swap(IRQ_SLOT_EMPTY, Ordering::AcqRel);
        if index >= IRQ_SLOT_COUNT {
            return;
        }
        let endpoint = &IRQ_ENDPOINTS[index];
        endpoint.active.store(false, Ordering::Release);
        while endpoint.readers.load(Ordering::Acquire) != 0 {
            if axtask::can_block_current() {
                axtask::yield_now();
            } else {
                axtask::resched_if_needed();
            }
        }
        endpoint.ptr.store(0, Ordering::Relaxed);
        endpoint.callback.store(0, Ordering::Relaxed);
        endpoint.irq.store(IRQ_SLOT_EMPTY, Ordering::Release);
    }

    /// Enables device-to-driver notifications for completion interrupts.
    pub fn enable_irq(&self) {
        let wait_armed = self.arm_irq_first_wait() == IrqFirstArmState::Armed;
        // Arm the platform callback before unmasking the queue.  Then perform
        // one task-context acknowledgement to cover an interrupt that was
        // raised before the arm point (or that raced the callback install).
        let pending = {
            let mut inner = self.inner.lock();
            inner.enable_interrupts();
            inner.ack_interrupt()
        };
        if pending {
            self.publish_irq_token();
            notify_irq_first_waiters();
        }
        self.irq_enabled.store(true, Ordering::Release);
        let was_armed = self.irq_wait_armed.swap(wait_armed, Ordering::AcqRel);
        if wait_armed && !was_armed {
            record_blk_async_irq_first_arm();
        }
    }

    /// Disables device-to-driver completion interrupts.
    pub fn disable_irq(&self) {
        self.inner.lock().disable_interrupts();
        self.irq_enabled.store(false, Ordering::Release);
        self.irq_wait_armed.store(false, Ordering::Release);
        #[cfg(feature = "irq")]
        self.disarm_irq_endpoint();
    }

    /// Returns whether completion interrupts are enabled in the wrapper.
    pub fn is_irq_enabled(&self) -> bool {
        self.irq_enabled.load(Ordering::Acquire)
    }

    fn async_write_segments_per_request(&self) -> usize {
        if !async_block_merge_write_enabled() {
            return ASYNC_WRITE_SEGMENTS_BASE;
        }
        if self.inner.lock().supports_indirect_desc() {
            ASYNC_WRITE_SEGMENTS_INDIRECT_MERGED
        } else {
            ASYNC_WRITE_SEGMENTS_BASE
        }
    }

    fn publish_irq_token(&self) {
        self.irq_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("VirtIO block IRQ generation exhausted"));
        // The generation is useful for detecting an IRQ which races a task
        // snapshot, but it is not itself a work queue.  Keep one coalesced
        // ownership token per device so duplicate IRQs cannot create an
        // unbounded continuation backlog.
        self.continuation_pending.store(true, Ordering::Release);
    }

    /// Acknowledges a block interrupt and publishes a task token.
    ///
    /// Completion ownership stays with task-context poll/wait paths.  This
    /// entry point deliberately does not inspect used-ring entries or touch
    /// completion buffers. Every recognized IRQ advances a generation so a
    /// task drain cannot erase an IRQ that raced with its used-ring snapshot;
    /// the downstream queue event and wait queues provide coalescing.
    pub fn handle_irq(&self) -> DevResult<usize> {
        let published = if let Some(mut inner) = self.inner.try_lock() {
            inner.ack_interrupt()
        } else {
            // The IRQ-facing path cannot wait for the task-side queue lock.
            // The task continuation will perform the deferred acknowledge.
            true
        };
        if published {
            self.publish_irq_token();
            notify_irq_first_waiters();
        }
        Ok(usize::from(published))
    }

    fn ack_task_irq(&self, inner: &mut InnerDev<H, T>) {
        if inner.ack_interrupt() {
            self.publish_irq_token();
        }
    }

    fn note_drain_status(
        &self,
        status: PendingBlkDrainStatus,
        observed_irq_generation: u64,
    ) -> (usize, bool) {
        let drained = status.drained();
        let mut continuation = drain_requires_continuation(
            status,
            observed_irq_generation,
            self.irq_generation.load(Ordering::Acquire),
        );
        if continuation {
            self.continuation_pending.store(true, Ordering::Release);
        } else {
            // Clear only the token observed by this pass, then re-check the
            // generation.  An IRQ arriving between the first generation read
            // and the clear must leave a token for the next task pass even if
            // that IRQ did not find a waiter to wake.
            self.continuation_pending
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .ok();
            if self.irq_generation.load(Ordering::Acquire) != observed_irq_generation {
                self.continuation_pending.store(true, Ordering::Release);
                continuation = true;
            }
        }
        (drained, continuation)
    }

    fn continue_pending_drain(&self) -> bool {
        if !self.continuation_pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        if axtask::can_block_current() {
            record_blk_async_wait_yield();
            axtask::yield_now();
        } else {
            // A preemption-disabled task cannot switch voluntarily.  Still
            // run the scheduler boundary so a pending reschedule/deferred
            // action is serviced before the next bounded drain pass.  The
            // normal task path above performs a real yield; this fallback is
            // only for the non-blocking context required by synchronous
            // callers.
            record_blk_async_wait_yield();
            axtask::resched_if_needed();
        }
        true
    }

    fn wait_for_pending_done(&self, handle: PendingBlkHandle) -> DevResult
    where
        T: Send,
    {
        let mut polls = 0u64;
        loop {
            let mut inner = self.inner.lock();
            self.ack_task_irq(&mut inner);
            let observed_irq_generation = self.irq_generation.load(Ordering::Acquire);
            let status = inner
                .drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                .unwrap_or_else(|error| {
                    panic!("lost asynchronous block completion state while reaping: {error}")
                });
            let (drained, continuation) = self.note_drain_status(status, observed_irq_generation);
            if inner.pending_request_done(handle) {
                inner.record_external_queue_wait(polls, handle.notified());
                Self::record_wait_hit(polls);
                let result = inner.complete_pending_request(handle).map_err(as_dev_err);
                drop(inner);
                Self::notify_completion_waiters(&self.wait_queue, drained);
                return result;
            }
            // Lock order: do not spin, yield, or eventually sleep while holding
            // the VirtIO queue lock. Filesystem callers must not hold lwext4
            // locks across async waits; dirty-flush lock policy is handled in
            // the consumer phase.
            drop(inner);
            Self::notify_completion_waiters(&self.wait_queue, drained);
            if continuation {
                self.continue_pending_drain();
                continue;
            }
            match self.wait_backoff(&mut polls, |inner| Ok(inner.pending_request_done(handle))) {
                Ok(()) | Err(DevError::Again | DevError::ResourceBusy) => {}
                Err(error) => {
                    panic!("lost asynchronous block wait state before completion: {error}")
                }
            }
        }
    }

    fn wait_for_all_pending(&self) -> DevResult
    where
        T: Send,
    {
        let mut polls = 0u64;
        loop {
            let mut inner = self.inner.lock();
            self.ack_task_irq(&mut inner);
            let observed_irq_generation = self.irq_generation.load(Ordering::Acquire);
            let status = inner
                .drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                .map_err(as_dev_err)?;
            let (drained, continuation) = self.note_drain_status(status, observed_irq_generation);
            if inner.pending_request_count() == 0 {
                Self::record_wait_hit(polls);
                drop(inner);
                Self::notify_completion_waiters(&self.wait_queue, drained);
                return Ok(());
            }
            drop(inner);
            Self::notify_completion_waiters(&self.wait_queue, drained);
            if continuation {
                self.continue_pending_drain();
                continue;
            }
            self.wait_backoff(&mut polls, |inner| Ok(inner.pending_request_count() == 0))?;
        }
    }

    fn fence_pending_data(&self) -> DevResult
    where
        T: Send,
    {
        record_blk_data_fence();
        self.wait_for_all_pending()
    }

    fn try_flush_async(&mut self) -> DevResult<bool>
    where
        T: Send,
    {
        if !async_block_enabled() || !self.inner.lock().supports_flush() {
            return Ok(false);
        }

        self.fence_pending_data()?;
        let empty_segments: [BlockSegment; 0] = [];
        let mut request = BlockQueueRequest {
            op: BlockAsyncOp::Flush,
            block_id: 0,
            segments: &empty_segments,
            handle: None,
        };

        loop {
            match <Self as BlockDriverOps>::submit_async_batch(
                self,
                core::slice::from_mut(&mut request),
            ) {
                Ok(report) if report.submitted == 1 => {
                    let handle = accepted_request_handles(core::slice::from_ref(&request), 1)
                        .next()
                        .expect("one accepted flush request lost its handle");
                    <Self as BlockDriverOps>::wait_async_all(self, &[handle])?;
                    return Ok(true);
                }
                Ok(report) if report.queue_full => {
                    request.handle = None;
                    spin_loop();
                }
                Ok(_) => return Ok(false),
                Err(DevError::Unsupported) => return Ok(false),
                Err(err) => return Err(err),
            }
        }
    }

    fn wait_backoff<F>(&self, polls: &mut u64, ready: F) -> DevResult
    where
        F: FnMut(&mut InnerDev<H, T>) -> DevResult<bool>,
        T: Send,
    {
        *polls = polls.saturating_add(1);
        if self.continue_pending_drain() {
            return Ok(());
        }
        if *polls <= ASYNC_WAIT_SPIN_BUDGET {
            record_blk_async_wait_spin();
            spin_loop();
            return Ok(());
        }

        if !async_block_enabled() {
            record_blk_async_wait_spin();
            spin_loop();
            return Ok(());
        }

        if async_block_wait_policy() == AsyncBlockWaitPolicy::InterruptFirst {
            let irq_wait_state = self.ensure_irq_first_wait_armed();
            if irq_wait_state == IrqFirstArmState::Armed && axtask::can_block_current() {
                return self.wait_irq_first(ready);
            }
            if *polls == ASYNC_WAIT_SPIN_BUDGET + 1 {
                record_blk_async_irq_first_fallback();
                match irq_wait_state {
                    IrqFirstArmState::Armed => record_blk_async_irq_first_fallback_cannot_block(),
                    IrqFirstArmState::NoIrq => {
                        record_blk_async_irq_first_fallback_unarmed();
                        record_blk_async_irq_first_fallback_no_irq();
                    }
                    IrqFirstArmState::RegisterFailed => {
                        record_blk_async_irq_first_fallback_unarmed();
                        record_blk_async_irq_first_fallback_register_failed();
                    }
                    IrqFirstArmState::FeatureDisabled => {
                        record_blk_async_irq_first_fallback_unarmed();
                        record_blk_async_irq_first_fallback_feature_disabled();
                    }
                }
            }
        }

        self.wait_hybrid(ready)
    }

    fn ensure_irq_first_wait_armed(&self) -> IrqFirstArmState {
        if !self.irq_wait_armed.load(Ordering::Acquire) {
            let arm_state = self.arm_irq_first_wait();
            if arm_state != IrqFirstArmState::Armed {
                return arm_state;
            }
            let was_armed = self.irq_wait_armed.swap(true, Ordering::AcqRel);
            if !was_armed {
                record_blk_async_irq_first_arm();
            }
        }

        if !self.irq_enabled.load(Ordering::Acquire) {
            self.inner.lock().enable_interrupts();
            self.irq_enabled.store(true, Ordering::Release);
        }

        IrqFirstArmState::Armed
    }

    fn wait_hybrid<F>(&self, mut ready: F) -> DevResult
    where
        F: FnMut(&mut InnerDev<H, T>) -> DevResult<bool>,
        T: Send,
    {
        if !axtask::can_block_current() {
            record_blk_async_wait_spin();
            spin_loop();
            return Ok(());
        }
        record_blk_async_wait_yield();
        axtask::yield_now();
        if !axtask::can_block_current() {
            record_blk_async_wait_spin();
            spin_loop();
            return Ok(());
        }
        record_blk_async_wait_sleep();
        let mut wait_error = None;
        let timed_out = self
            .wait_queue
            .wait_timeout_until(Duration::from_micros(ASYNC_WAIT_TIMEOUT_US), || {
                let mut inner = self.inner.lock();
                self.ack_task_irq(&mut inner);
                let observed_irq_generation = self.irq_generation.load(Ordering::Acquire);
                let (drained, continuation) = match inner
                    .drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                {
                    Ok(status) => self.note_drain_status(status, observed_irq_generation),
                    Err(err) => {
                        wait_error = Some(as_dev_err(err));
                        (0, false)
                    }
                };
                let is_ready = if wait_error.is_none() {
                    match ready(&mut inner) {
                        Ok(is_ready) => is_ready,
                        Err(err) => {
                            wait_error = Some(err);
                            true
                        }
                    }
                } else {
                    true
                };
                drop(inner);
                Self::notify_completion_waiters(&self.wait_queue, drained);
                is_ready || continuation || wait_error.is_some()
            })
            .map_err(wait_error_to_dev)?;
        if let Some(err) = wait_error {
            return Err(err);
        }
        if timed_out {
            record_blk_async_wait_timeout();
        }
        Ok(())
    }

    fn wait_irq_first<F>(&self, mut ready: F) -> DevResult
    where
        F: FnMut(&mut InnerDev<H, T>) -> DevResult<bool>,
        T: Send,
    {
        record_blk_async_irq_first_wait();
        record_blk_async_wait_sleep();
        let mut wait_error = None;
        IRQ_FIRST_WAIT_QUEUE
            .wait_until(|| {
                let mut inner = self.inner.lock();
                self.ack_task_irq(&mut inner);
                let observed_irq_generation = self.irq_generation.load(Ordering::Acquire);
                let (drained, continuation) = match inner
                    .drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                {
                    Ok(status) => self.note_drain_status(status, observed_irq_generation),
                    Err(err) => {
                        wait_error = Some(as_dev_err(err));
                        (0, false)
                    }
                };
                let is_ready = if wait_error.is_none() {
                    match ready(&mut inner) {
                        Ok(is_ready) => is_ready,
                        Err(err) => {
                            wait_error = Some(err);
                            true
                        }
                    }
                } else {
                    true
                };
                drop(inner);
                Self::notify_completion_waiters(&self.wait_queue, drained);
                is_ready || continuation || wait_error.is_some()
            })
            .map_err(wait_error_to_dev)?;
        if let Some(err) = wait_error {
            return Err(err);
        }
        Ok(())
    }

    fn notify_completion_waiters(wait_queue: &WaitQueue, drained: usize) {
        if drained == 0 || !async_block_enabled() {
            return;
        }
        if wait_queue.notify_many(usize::MAX, false) > 0 {
            record_blk_async_wait_wakeup();
        }
    }

    fn record_wait_hit(polls: u64) {
        if (1..=ASYNC_WAIT_SPIN_BUDGET).contains(&polls) {
            record_blk_async_wait_spin_hit();
        }
    }

    fn build_pending_batch<'a>(
        requests: &'a mut [BlockQueueRequest<'_>],
        limit: usize,
        block_size: usize,
    ) -> DevResult<Vec<PendingBlkBatchRequest<'a>>> {
        let mut pending = Vec::with_capacity(limit);
        for request in requests.iter_mut().take(limit) {
            request.handle = None;
            let buffer = match request.op {
                BlockAsyncOp::Read => {
                    if request.segments.is_empty() {
                        return Err(DevError::Unsupported);
                    }
                    let mut bufs = Vec::with_capacity(request.segments.len());
                    for segment in request.segments {
                        if segment.direction != BlockSegmentDirection::DeviceToMemory
                            || segment.addr == 0
                            || segment.len == 0
                            || segment.len % block_size != 0
                        {
                            return Err(DevError::InvalidParam);
                        }
                        // SAFETY: `BlockQueueRequest` is the async block API
                        // boundary. The caller must keep this segment valid until
                        // the returned handle completes.
                        bufs.push(unsafe {
                            core::slice::from_raw_parts_mut(segment.addr as *mut u8, segment.len)
                        });
                    }
                    if bufs.len() == 1 {
                        PendingBlkBatchBuffer::Read(bufs.pop().ok_or(DevError::InvalidParam)?)
                    } else {
                        PendingBlkBatchBuffer::ReadVectored(bufs)
                    }
                }
                BlockAsyncOp::Write => {
                    if request.segments.is_empty() {
                        return Err(DevError::Unsupported);
                    }
                    let mut bufs = Vec::with_capacity(request.segments.len());
                    for segment in request.segments {
                        if segment.direction != BlockSegmentDirection::MemoryToDevice
                            || segment.addr == 0
                            || segment.len == 0
                            || segment.len % block_size != 0
                        {
                            return Err(DevError::InvalidParam);
                        }
                        // SAFETY: See the read case above; the device only reads
                        // from this segment.
                        bufs.push(unsafe {
                            core::slice::from_raw_parts(segment.addr as *const u8, segment.len)
                        });
                    }
                    if bufs.len() == 1 {
                        PendingBlkBatchBuffer::Write(bufs.pop().ok_or(DevError::InvalidParam)?)
                    } else {
                        PendingBlkBatchBuffer::WriteVectored(bufs)
                    }
                }
                BlockAsyncOp::Flush => {
                    if !request.segments.is_empty() || request.block_id != 0 {
                        return Err(DevError::InvalidParam);
                    }
                    PendingBlkBatchBuffer::Flush
                }
            };
            pending.push(PendingBlkBatchRequest {
                block_id: request.block_id as usize,
                buffer,
                handle: None,
            });
        }
        Ok(pending)
    }

    fn try_write_block_vectored_async(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult<bool>
    where
        T: Send,
    {
        if !async_block_enabled() {
            return Ok(false);
        }
        let block_size = virtio_drivers::device::blk::SECTOR_SIZE;
        let mut segments = Vec::with_capacity(bufs.len());
        let mut total_blocks = 0u64;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            if buf.len() % block_size != 0 {
                return Ok(false);
            }
            segments.push(BlockSegment::from_write_buf(buf));
            total_blocks = total_blocks
                .checked_add((buf.len() / block_size) as u64)
                .ok_or(DevError::InvalidParam)?;
        }
        if total_blocks == 0 {
            return Ok(false);
        }

        let merge_write_enabled = async_block_merge_write_enabled();
        // Keep each async request within the split-ring descriptor budget. Four
        // data segments cost six descriptors with the request header/response on
        // direct split rings; indirect-capable devices may use eight segments
        // behind the experimental merge switch without changing completion
        // ownership.
        let max_segments_per_request = self.async_write_segments_per_request();

        let mut requests = Vec::with_capacity(segments.len().div_ceil(max_segments_per_request));
        let mut request_block = block_id;
        let mut start = 0usize;
        while start < segments.len() {
            let end = (start + max_segments_per_request).min(segments.len());
            requests.push(BlockQueueRequest {
                op: BlockAsyncOp::Write,
                block_id: request_block,
                segments: &segments[start..end],
                handle: None,
            });
            let request_blocks = segments[start..end]
                .iter()
                .map(|segment| segment.len / block_size)
                .sum::<usize>();
            request_block = request_block
                .checked_add(request_blocks as u64)
                .ok_or(DevError::InvalidParam)?;
            start = end;
        }
        if merge_write_enabled {
            record_blk_async_merge_write(segments.len(), requests.len(), max_segments_per_request);
        }

        let mut next = 0usize;
        let mut handles = Vec::with_capacity(requests.len());
        while next < requests.len() {
            let report = match self.submit_async_batch(&mut requests[next..]) {
                Ok(report) => report,
                Err(DevError::Unsupported) if handles.is_empty() => return Ok(false),
                Err(error) => {
                    if !handles.is_empty() {
                        self.wait_async_all(&handles)?;
                    }
                    return Err(error);
                }
            };
            let accepted = accepted_request_handles(&requests[next..], report.submitted);
            if report.submitted == 0 {
                if handles.is_empty() {
                    return Ok(false);
                }
                self.wait_async_all(&handles)?;
                handles.clear();
                continue;
            }

            for handle in accepted {
                assert!(
                    handles.len() < handles.capacity(),
                    "preallocated asynchronous write handle storage exhausted"
                );
                handles.push(handle);
            }
            next += report.submitted;
            if report.queue_full && next < requests.len() {
                self.wait_async_all(&handles)?;
                handles.clear();
            }
        }

        if !handles.is_empty() {
            self.wait_async_all(&handles)?;
        }
        Ok(true)
    }

    fn try_read_block_vectored_async(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> DevResult<bool>
    where
        T: Send,
    {
        if !async_block_enabled() {
            return Ok(false);
        }
        let block_size = virtio_drivers::device::blk::SECTOR_SIZE;
        let mut segments = Vec::with_capacity(bufs.len());
        let mut total_blocks = 0u64;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            if buf.len() % block_size != 0 {
                return Ok(false);
            }
            segments.push(BlockSegment::from_read_buf(buf));
            total_blocks = total_blocks
                .checked_add((buf.len() / block_size) as u64)
                .ok_or(DevError::InvalidParam)?;
        }

        if total_blocks == 0 {
            return Ok(false);
        }

        let max_segments_per_request = 4usize;

        let mut requests = Vec::with_capacity(segments.len().div_ceil(max_segments_per_request));
        let mut request_block = block_id;
        let mut start = 0usize;
        while start < segments.len() {
            let end = (start + max_segments_per_request).min(segments.len());
            requests.push(BlockQueueRequest {
                op: BlockAsyncOp::Read,
                block_id: request_block,
                segments: &segments[start..end],
                handle: None,
            });
            let request_blocks = segments[start..end]
                .iter()
                .map(|segment| segment.len / block_size)
                .sum::<usize>();
            request_block = request_block
                .checked_add(request_blocks as u64)
                .ok_or(DevError::InvalidParam)?;
            start = end;
        }

        let mut next = 0usize;
        let mut handles = Vec::with_capacity(requests.len());
        while next < requests.len() {
            let report = match self.submit_async_batch(&mut requests[next..]) {
                Ok(report) => report,
                Err(DevError::Unsupported) if handles.is_empty() => return Ok(false),
                Err(error) => {
                    if !handles.is_empty() {
                        self.wait_async_all(&handles)?;
                    }
                    return Err(error);
                }
            };
            let accepted = accepted_request_handles(&requests[next..], report.submitted);
            if report.submitted == 0 {
                if handles.is_empty() {
                    return Ok(false);
                }
                self.wait_async_all(&handles)?;
                handles.clear();
                continue;
            }

            for handle in accepted {
                assert!(
                    handles.len() < handles.capacity(),
                    "preallocated asynchronous read handle storage exhausted"
                );
                handles.push(handle);
            }
            next += report.submitted;
            if report.queue_full && next < requests.len() {
                self.wait_async_all(&handles)?;
                handles.clear();
            }
        }

        if !handles.is_empty() {
            self.wait_async_all(&handles)?;
        }
        Ok(true)
    }
}

impl<H: Hal, T: Transport + Send> BaseDriverOps for VirtIoBlkDev<H, T> {
    fn device_name(&self) -> &str {
        "virtio-blk"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn irq_num(&self) -> Option<usize> {
        self.irq
    }
}

impl<H: Hal, T: Transport + Send> BlockDriverOps for VirtIoBlkDev<H, T> {
    #[inline]
    fn num_blocks(&self) -> u64 {
        self.inner.lock().capacity()
    }

    #[inline]
    fn block_size(&self) -> usize {
        virtio_drivers::device::blk::SECTOR_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        let handle = loop {
            let mut inner = self.inner.lock();
            match unsafe { inner.submit_read_blocks_pending(block_id as _, buf) } {
                Ok(handle) => break handle,
                Err(virtio_drivers::Error::QueueFull) => {
                    drop(inner);
                    spin_loop();
                }
                Err(err) => return Err(as_dev_err(err)),
            }
        };
        self.wait_for_pending_done(handle)
    }

    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        if self.try_read_block_vectored_async(block_id, bufs)? {
            return Ok(());
        }
        self.wait_for_all_pending()?;
        let mut inner = self.inner.lock();
        inner
            .read_blocks_vectored(block_id as _, bufs)
            .map_err(as_dev_err)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        let handle = loop {
            let mut inner = self.inner.lock();
            match unsafe { inner.submit_write_blocks_pending(block_id as _, buf) } {
                Ok(handle) => break handle,
                Err(virtio_drivers::Error::QueueFull) => {
                    drop(inner);
                    spin_loop();
                }
                Err(err) => return Err(as_dev_err(err)),
            }
        };
        self.wait_for_pending_done(handle)
    }

    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        if self.try_write_block_vectored_async(block_id, bufs)? {
            return Ok(());
        }
        self.wait_for_all_pending()?;
        let mut inner = self.inner.lock();
        inner
            .write_blocks_vectored(block_id as _, bufs)
            .map_err(as_dev_err)
    }

    /// # Safety
    ///
    /// The caller must keep all physical segments pinned and valid until this
    /// synchronous operation returns and must obey the device-write direction
    /// of a read. Concurrent CPU/device access races on contents.
    unsafe fn read_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        if segments.len() > MAX_PHYSICAL_SG {
            return Err(DevError::InvalidParam);
        }
        let mut physical = [VirtioPhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        for (index, segment) in segments.iter().copied().enumerate() {
            physical[index] = VirtioPhysicalSegment {
                paddr: segment.paddr,
                len: segment.len,
            };
        }
        let handle = {
            let mut inner = self.inner.lock();
            // SAFETY: The method's caller owns the pin and direction contract;
            // the inner driver retains each mapping until the returned pending
            // handle is completed.
            let result = unsafe {
                inner.submit_read_blocks_physical_pending(block_id, &physical[..segments.len()])
            };
            match result {
                Ok(handle) => handle,
                Err(error) => return physical_submit_error(error),
            }
        };
        self.wait_for_pending_done(handle)
            .map(|()| BlockPhysicalSgOutcome::Completed)
    }

    /// # Safety
    ///
    /// The caller must keep all physical segments pinned and valid until this
    /// synchronous operation returns and must obey the device-read direction
    /// of a write. Concurrent CPU/device access races on contents.
    unsafe fn write_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        if segments.len() > MAX_PHYSICAL_SG {
            return Err(DevError::InvalidParam);
        }
        let mut physical = [VirtioPhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        for (index, segment) in segments.iter().copied().enumerate() {
            physical[index] = VirtioPhysicalSegment {
                paddr: segment.paddr,
                len: segment.len,
            };
        }
        let handle = {
            let mut inner = self.inner.lock();
            // SAFETY: The method's caller owns the pin and direction contract;
            // the inner driver retains each mapping until the returned pending
            // handle is completed.
            let result = unsafe {
                inner.submit_write_blocks_physical_pending(block_id, &physical[..segments.len()])
            };
            match result {
                Ok(handle) => handle,
                Err(error) => return physical_submit_error(error),
            }
        };
        self.wait_for_pending_done(handle)
            .map(|()| BlockPhysicalSgOutcome::Completed)
    }

    fn flush(&mut self) -> DevResult {
        record_blk_metadata_fence();
        if self.try_flush_async()? {
            return Ok(());
        }
        self.fence_pending_data()?;
        let mut inner = self.inner.lock();
        inner.flush().map_err(as_dev_err)
    }

    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        let inner = self.inner.lock();
        Some(BlockQueueCaps {
            max_requests: inner.virt_queue_size() as usize,
            max_descriptors: inner.virt_queue_size() as usize,
            supports_indirect: inner.supports_indirect_desc(),
            supports_event_idx: inner.supports_event_idx(),
            default_depth: inner.async_default_depth(),
        })
    }

    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        if !async_block_enabled() {
            return Err(DevError::Unsupported);
        }
        if requests.is_empty() {
            return Ok(BlockSubmitReport::default());
        }

        let (depth_available, block_size) = {
            let inner = self.inner.lock();
            let default_depth = inner.async_default_depth();
            let depth = match async_block_wait_policy() {
                AsyncBlockWaitPolicy::Sync => 1,
                AsyncBlockWaitPolicy::Hybrid | AsyncBlockWaitPolicy::InterruptFirst => {
                    default_depth
                }
            };
            (
                depth.saturating_sub(inner.async_pending_request_count()),
                virtio_drivers::device::blk::SECTOR_SIZE,
            )
        };
        if depth_available == 0 {
            return Ok(BlockSubmitReport {
                queue_full: true,
                ..BlockSubmitReport::default()
            });
        }

        let limit = requests.len().min(depth_available);
        let mut accepted_handles = Vec::with_capacity(limit);
        let mut pending = Self::build_pending_batch(requests, limit, block_size)?;
        let report = {
            let mut inner = self.inner.lock();
            // SAFETY: The `BlockQueueRequest` contract requires segment
            // lifetime to cover completion; the driver stores only raw segment
            // identities in owned request slots.
            unsafe { inner.submit_pending_batch(pending.as_mut_slice()) }.map_err(as_dev_err)?
        };
        let submitted = report.submitted;
        for handle in accepted_pending_handles(&pending, submitted) {
            assert!(
                accepted_handles.len() < accepted_handles.capacity(),
                "preallocated accepted-handle storage exhausted"
            );
            accepted_handles.push(handle.into_raw());
        }
        drop(pending);

        for (request, raw) in requests.iter_mut().zip(accepted_handles) {
            request.handle = Some(BlockRequestHandle { raw });
        }

        Ok(BlockSubmitReport {
            submitted,
            bytes: report.bytes,
            queue_full: report.queue_full,
        })
    }

    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        if budget == 0 {
            return Ok(0);
        }
        let (drained, _) = {
            let mut inner = self.inner.lock();
            self.ack_task_irq(&mut inner);
            let observed_irq_generation = self.irq_generation.load(Ordering::Acquire);
            let status = inner
                .drain_pending_completions_bounded(budget)
                .map_err(as_dev_err)?;
            self.note_drain_status(status, observed_irq_generation)
        };
        Self::notify_completion_waiters(&self.wait_queue, drained);
        Ok(drained)
    }

    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        reap_all_async_handles(handles, |handle| {
            self.wait_for_pending_done(PendingBlkHandle::from_raw(handle.raw))
        })
    }

    fn enable_irq(&mut self) -> DevResult {
        VirtIoBlkDev::enable_irq(self);
        Ok(())
    }

    fn disable_irq(&mut self) -> DevResult {
        VirtIoBlkDev::disable_irq(self);
        Ok(())
    }

    fn is_irq_enabled(&self) -> bool {
        VirtIoBlkDev::is_irq_enabled(self)
    }

    fn handle_irq(&mut self) -> DevResult<usize> {
        VirtIoBlkDev::handle_irq(self)
    }

    fn fence_async(&mut self) -> DevResult {
        self.fence_pending_data()
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoBlkDev<H, T> {
    fn drop(&mut self) {
        // Stop device notifications and retire the IRQ endpoint before the
        // inner VirtIO object tears down its queue.  A late IRQ can therefore
        // only observe an inactive slot, never a pointer to this object.
        self.irq_enabled.store(false, Ordering::Release);
        self.irq_wait_armed.store(false, Ordering::Release);
        #[cfg(feature = "irq")]
        self.disarm_irq_endpoint();
        self.inner.lock().disable_interrupts();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_generation_race_preserves_task_continuation() {
        let complete = PendingBlkDrainStatus::Complete { drained: 1 };
        assert!(!drain_requires_continuation(complete, 7, 7));
        assert!(drain_requires_continuation(complete, 7, 8));

        let backlog = PendingBlkDrainStatus::Continuation { drained: 4 };
        assert!(drain_requires_continuation(backlog, 8, 8));
    }

    #[test]
    fn reap_all_keeps_waiting_after_completed_request_errors() {
        let mut attempts = [0usize; 4];
        let result = reap_all_async_handles(&[0usize, 1, 2, 3], |handle| {
            attempts[handle] += 1;
            match handle {
                0 => Err(DevError::Io),
                1 => Err(DevError::Again),
                2 => Err(DevError::Unsupported),
                _ => Ok(()),
            }
        });

        assert!(matches!(result, Err(DevError::Io)));
        assert_eq!(attempts, [1, 1, 1, 1]);
    }

    #[test]
    fn physical_submit_maps_only_unpublished_resource_errors_to_fallback() {
        for error in [
            virtio_drivers::Error::QueueFull,
            virtio_drivers::Error::DmaError,
            virtio_drivers::Error::Unsupported,
        ] {
            assert!(matches!(
                physical_submit_error(error),
                Ok(BlockPhysicalSgOutcome::NotSubmitted)
            ));
        }

        assert!(matches!(
            physical_submit_error(virtio_drivers::Error::NotReady),
            Err(DevError::Again)
        ));
        assert!(matches!(
            physical_submit_error(virtio_drivers::Error::InvalidParam),
            Err(DevError::InvalidParam)
        ));
        assert!(matches!(
            physical_submit_error(virtio_drivers::Error::IoError),
            Err(DevError::Io)
        ));
    }

    fn pending_request(handle: Option<u64>) -> PendingBlkBatchRequest<'static> {
        PendingBlkBatchRequest {
            block_id: 0,
            buffer: PendingBlkBatchBuffer::Flush,
            handle: handle.map(PendingBlkHandle::from_raw),
        }
    }

    #[test]
    fn accepted_pending_handles_preserve_the_reported_prefix() {
        let pending = [
            pending_request(Some(11)),
            pending_request(Some(12)),
            pending_request(None),
        ];

        let handles = accepted_pending_handles(&pending, 2);
        assert_eq!(
            handles
                .into_iter()
                .map(PendingBlkHandle::into_raw)
                .collect::<Vec<_>>(),
            [11, 12]
        );
    }

    #[test]
    #[should_panic(expected = "missing its handle")]
    fn accepted_pending_handles_fail_closed_on_a_missing_handle() {
        let pending = [pending_request(None)];
        let _ = accepted_pending_handles(&pending, 1).next();
    }

    #[test]
    #[should_panic(expected = "overreported")]
    fn accepted_pending_handles_fail_closed_on_an_overreported_count() {
        let pending = [pending_request(Some(11))];
        let _ = accepted_pending_handles(&pending, 2);
    }

    static NO_SEGMENTS: [BlockSegment; 0] = [];

    fn request(handle: Option<u64>) -> BlockQueueRequest<'static> {
        BlockQueueRequest {
            op: BlockAsyncOp::Read,
            block_id: 0,
            segments: &NO_SEGMENTS,
            handle: handle.map(|raw| BlockRequestHandle { raw }),
        }
    }

    #[test]
    fn accepted_request_handles_preserve_the_reported_prefix() {
        let requests = [request(Some(31)), request(Some(32)), request(None)];
        let handles = accepted_request_handles(&requests, 2);
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.raw)
                .collect::<Vec<_>>(),
            [31, 32]
        );
    }

    #[test]
    #[should_panic(expected = "missing its completion handle")]
    fn accepted_request_handles_fail_closed_on_a_missing_handle() {
        let requests = [request(None)];
        let _ = accepted_request_handles(&requests, 1).next();
    }

    #[test]
    #[should_panic(expected = "overreported")]
    fn accepted_request_handles_fail_closed_on_an_overreported_count() {
        let requests = [request(Some(31))];
        let _ = accepted_request_handles(&requests, 2);
    }
}
