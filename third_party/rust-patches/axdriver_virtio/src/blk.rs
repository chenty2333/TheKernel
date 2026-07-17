use alloc::vec::Vec;
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
use axdriver_block::{
    BlockAsyncOp, BlockDriverOps, BlockQueueCaps, BlockQueueRequest, BlockRequestHandle,
    BlockSegment, BlockSegmentDirection, BlockSubmitReport,
};
use axtask::{
    WaitError, WaitQueue,
    future::{BlockOnError, TimerRegistrationError},
};
use spin::Mutex;
use virtio_drivers::{
    Hal,
    device::blk::{
        PendingBlkBatchBuffer, PendingBlkBatchRequest, PendingBlkHandle, VirtIOBlk as InnerDev,
    },
    stats::{
        AsyncBlockWaitPolicy, async_block_enabled, async_block_merge_write_enabled,
        async_block_wait_policy, record_blk_async_interrupt_drain, record_blk_async_irq_first_arm,
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
static REGISTERED_IRQS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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
    notify_irq_first_waiters();
}

#[cfg(feature = "irq")]
fn arm_irq_first_wait(irq: Option<usize>) -> IrqFirstArmState {
    let Some(irq) = irq else {
        return IrqFirstArmState::NoIrq;
    };

    let mut registered = REGISTERED_IRQS.lock();
    if registered.contains(&irq) {
        return IrqFirstArmState::Armed;
    }
    if axhal::irq::register(irq, virtio_blk_irq_wake_handler) {
        registered.push(irq);
        IrqFirstArmState::Armed
    } else {
        IrqFirstArmState::RegisterFailed
    }
}

#[cfg(not(feature = "irq"))]
fn arm_irq_first_wait(irq: Option<usize>) -> IrqFirstArmState {
    if irq.is_some() {
        IrqFirstArmState::FeatureDisabled
    } else {
        IrqFirstArmState::NoIrq
    }
}

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
        })
    }

    /// Enables device-to-driver notifications for completion interrupts.
    pub fn enable_irq(&self) {
        let wait_armed = arm_irq_first_wait(self.irq) == IrqFirstArmState::Armed;
        self.inner.lock().enable_interrupts();
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

    /// Acknowledges a block interrupt, drains completions, and wakes waiters.
    ///
    /// The VirtIO queue lock is dropped before notifying the shared wait queue,
    /// keeping interrupt wakeup order consistent with the hybrid wait path.
    pub fn handle_irq(&self) -> DevResult<usize> {
        let drained = {
            let mut inner = self.inner.lock();
            let _ = inner.ack_interrupt();
            let drained = inner.drain_pending_completions().map_err(as_dev_err)?;
            drained
        };
        if drained > 0 {
            record_blk_async_interrupt_drain();
        }
        Self::notify_completion_waiters(&self.wait_queue, drained);
        notify_irq_first_waiters();
        Ok(drained)
    }

    fn wait_for_pending_done(&self, handle: PendingBlkHandle) -> DevResult
    where
        T: Send,
    {
        let mut polls = 0u64;
        loop {
            let mut inner = self.inner.lock();
            let drained = inner.drain_pending_completions().unwrap_or_else(|error| {
                panic!("lost asynchronous block completion state while reaping: {error}")
            });
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
            let drained = inner.drain_pending_completions().map_err(as_dev_err)?;
            if inner.pending_request_count() == 0 {
                Self::record_wait_hit(polls);
                drop(inner);
                Self::notify_completion_waiters(&self.wait_queue, drained);
                return Ok(());
            }
            drop(inner);
            Self::notify_completion_waiters(&self.wait_queue, drained);
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
            let arm_state = arm_irq_first_wait(self.irq);
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
                let drained = match inner.drain_pending_completions() {
                    Ok(drained) => drained,
                    Err(err) => {
                        wait_error = Some(as_dev_err(err));
                        0
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
                is_ready || wait_error.is_some()
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
                let _ = inner.ack_interrupt();
                let drained = match inner.drain_pending_completions() {
                    Ok(drained) => drained,
                    Err(err) => {
                        wait_error = Some(as_dev_err(err));
                        0
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
                is_ready || wait_error.is_some()
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
        let _ = budget;
        let drained = self
            .inner
            .lock()
            .drain_pending_completions()
            .map_err(as_dev_err)?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
