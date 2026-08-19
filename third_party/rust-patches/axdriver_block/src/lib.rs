//! Common traits and types for block storage device drivers (i.e. disk).

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

#[cfg(feature = "ramdisk")]
pub mod ramdisk;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_static;

#[cfg(feature = "ahci")]
pub mod ahci;

#[doc(no_inline)]
pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};

/// Operation type for an async/batch block request.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockAsyncOp {
    /// Read data from the device into memory.
    Read,
    /// Write data from memory to the device.
    Write,
    /// Flush previously submitted writes.
    Flush,
}

/// Direction of a memory segment in a block request.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockSegmentDirection {
    /// The device writes into this memory segment.
    DeviceToMemory,
    /// The device reads from this memory segment.
    MemoryToDevice,
}

/// A memory segment used by an async/batch block request.
///
/// The caller is responsible for keeping the pointed-to memory valid until the
/// returned request handle has completed. Later phases will make this ownership
/// explicit with queue-owned request guards.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockSegment {
    /// Virtual address of the segment.
    pub addr: usize,
    /// Segment length in bytes.
    pub len: usize,
    /// Segment direction.
    pub direction: BlockSegmentDirection,
}

/// One pinned physical-memory segment for a synchronous direct block request.
///
/// The caller owns the pin and must keep the physical range valid, pinned, and
/// exclusively accessible for the entire synchronous driver call.  Drivers
/// submit this range directly to the device; they do not construct a Rust
/// slice from the physical address or copy the payload through a bounce buffer.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockPhysicalSegment {
    /// Physical address of the first byte in the segment.
    pub paddr: usize,
    /// Segment length in bytes.
    pub len: usize,
}

/// Outcome of a synchronous physical scatter-gather request.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockPhysicalSgOutcome {
    /// The driver submitted and completed the request successfully.
    Completed,
    /// The driver did not publish a request, so the caller may use a fallback.
    NotSubmitted,
    /// The descriptor was published, but the device could not prove that its
    /// DMA owner has retired.  The caller must transfer/retain the pin owner
    /// in quarantine; this is never a fallback or an ordinary I/O error.
    Quarantined,
}

/// Maximum number of physical requests that can be prepared as one atomic
/// publication.  A prepared batch owns all queue state until it is either
/// published or dropped; this bound keeps the rollback path finite.
pub const MAX_PHYSICAL_BATCH_REQUESTS: usize = 32;

/// Maximum number of coalesced physical segments in one request.
pub const MAX_PHYSICAL_COALESCED_SG: usize = 16;

/// A completion status copied from the device response before the request
/// slot is retired.  The raw status is retained because block devices have
/// device-specific status values beyond the common success code.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockCompletionStatus {
    /// The device completed the request successfully.
    Success,
    /// The device completed the request with a protocol status.
    DeviceError(u8),
    /// The queue could not prove that device access had stopped after reset.
    /// The request owner must remain retained and no synthetic I/O error is
    /// reported for this state.
    Quarantined,
}

/// Completion owner selected when a request is published.  A device-global
/// completion owner may drain all used-ring entries, but it must preserve this
/// classification when handing records to ordinary waiters or physical
/// effect routers.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockCompletionOwner {
    /// A normal async request whose handle waiter owns retirement.
    Ordinary,
    /// A legacy request completed through the legacy request/response bridge.
    Legacy,
    /// A pinned physical effect owned by the physical completion router.
    Physical,
}

/// Destination selected before a physical descriptor is published.
///
/// The lower queue has one used-ring consumer.  A route reservation records
/// which higher-level owner is allowed to consume the resulting completion;
/// it is not inferred from whichever waiter happens to wake first.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockPhysicalCompletionRoute {
    /// The device-global asynchronous completion worker owns the record and
    /// will route it to a registered kernel effect.
    Kernel,
    /// One synchronous physical effect owns the exact handle/cookie pair.
    Exact,
}

/// Live/terminal state of a block completion queue.
///
/// The transport generation changes whenever IRQ delivery is cancelled or
/// queue ownership is reset; ordinary IRQ progress has a separate wake
/// generation. Callers must bind a route/admission to the observed transport
/// generation and reject stale callbacks or submissions from an older one.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockCompletionAvailability {
    /// Queue is live and accepts the capabilities exposed by the driver.
    Live { generation: u64 },
    /// Queue access stopped, but the device could not prove owner retirement.
    Quarantined { generation: u64 },
    /// Queue was dismantled and requires transport reinitialization.
    Retired { generation: u64 },
}

/// IRQ/task completion notification callback.  The callback must be bounded,
/// allocation-free, and safe to invoke from the device interrupt path.  The
/// context is an opaque owner supplied when the callback is installed.
pub type BlockCompletionNotifier = fn(usize);

/// Reset/generation transition callback for a shared completion broker.
///
/// The callback runs in task context after the lower reset has selected its
/// typed availability state.  It must be bounded and must only cancel or
/// wake higher-level routes; it must not attempt a second lower queue drain.
pub type BlockCompletionTerminalNotifier = fn(usize, BlockCompletionAvailability);

/// One concrete task-side completion.  Unlike the legacy drain count, this
/// value identifies the request, its completion cookie, and the exact device
/// status observed before any slot/mapping retirement.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BlockCompletion {
    /// Opaque generation-safe request handle.
    pub handle: BlockRequestHandle,
    /// Completion owner selected at publication time.
    pub owner: BlockCompletionOwner,
    /// Per-request completion cookie assigned before publication.
    pub cookie: u64,
    /// Device-reported status.
    pub status: BlockCompletionStatus,
    /// Bytes reported by the used descriptor.
    pub bytes: u32,
}

/// Result metadata for a bounded completion drain.  Completed entries are
/// written to the caller-provided output slice in used-ring order.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct BlockCompletionDrain {
    /// Number of valid entries at the start of the output slice.
    pub completed: usize,
    /// Another used-ring entry is ready and task context must schedule a
    /// continuation without waiting for another interrupt.
    pub continuation: bool,
}

/// Result of a bounded device-reset/quiescence proof.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlockResetOutcome {
    /// All device access stopped and DMA owners were released safely.
    Quiesced,
    /// All current device access stopped, but the queue was permanently
    /// retired.  A transport reinitialization is required before any new
    /// submission; this outcome must not be treated as a live queue.
    Retired,
    /// Quiescence could not be proven; request owners remain retained.
    Quarantined,
}

/// A physical request offered to an atomic block batch.
#[derive(Debug)]
pub struct BlockPhysicalRequest<'a> {
    /// First block/sector for this request.
    pub block_id: u64,
    /// Device operation.
    pub op: BlockAsyncOp,
    /// Pinned, coalesced physical segments.  The device never constructs a
    /// Rust slice from these addresses.
    pub segments: &'a [BlockPhysicalSegment],
    /// Driver-filled generation-safe handle after preparation succeeds.
    pub handle: Option<BlockRequestHandle>,
    /// Driver-filled, non-zero completion cookie assigned before publication.
    /// The cookie is carried separately from the opaque raw handle so an
    /// effect owner can bind the expected identity before waiting for
    /// completion.
    pub cookie: Option<u64>,
}

impl BlockPhysicalSgOutcome {
    /// Returns whether the physical request completed successfully.
    pub const fn is_completed(self) -> bool {
        matches!(self, Self::Completed)
    }

    /// Returns whether the driver left the request unpublished.
    pub const fn is_not_submitted(self) -> bool {
        matches!(self, Self::NotSubmitted)
    }

    /// Returns whether publication happened but DMA ownership remains
    /// reset-required/quarantined.
    pub const fn is_quarantined(self) -> bool {
        matches!(self, Self::Quarantined)
    }
}

impl BlockSegment {
    /// Creates a read segment backed by a mutable buffer.
    pub fn from_read_buf(buf: &mut [u8]) -> Self {
        Self {
            addr: buf.as_mut_ptr() as usize,
            len: buf.len(),
            direction: BlockSegmentDirection::DeviceToMemory,
        }
    }

    /// Creates a write segment backed by an immutable buffer.
    pub fn from_write_buf(buf: &[u8]) -> Self {
        Self {
            addr: buf.as_ptr() as usize,
            len: buf.len(),
            direction: BlockSegmentDirection::MemoryToDevice,
        }
    }
}

/// Static and runtime limits for an async/batch block queue.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct BlockQueueCaps {
    /// Maximum in-flight request slots exposed by this queue.
    pub max_requests: usize,
    /// Maximum descriptors available to async admission.
    pub max_descriptors: usize,
    /// Whether indirect descriptors are available.
    pub supports_indirect: bool,
    /// Whether event-index notification suppression is available.
    pub supports_event_idx: bool,
    /// Runtime default queue-depth cap for this architecture/device.
    pub default_depth: usize,
}

/// Opaque handle for a submitted async block request.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct BlockRequestHandle {
    /// Driver-owned request identifier.
    pub raw: u64,
}

/// One request offered to the async/batch block queue.
#[derive(Debug)]
pub struct BlockQueueRequest<'a> {
    /// Requested block operation.
    pub op: BlockAsyncOp,
    /// First block/sector for the request.
    pub block_id: u64,
    /// Data segments for the request.
    pub segments: &'a [BlockSegment],
    /// Driver-filled request handle when submission succeeds.
    pub handle: Option<BlockRequestHandle>,
}

impl<'a> BlockQueueRequest<'a> {
    /// Returns the total bytes covered by all segments.
    pub fn bytes(&self) -> usize {
        self.segments.iter().map(|seg| seg.len).sum()
    }
}

/// Result of a batch submission attempt.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct BlockSubmitReport {
    /// Number of request entries accepted by the queue.
    pub submitted: usize,
    /// Total bytes accepted by the queue.
    pub bytes: usize,
    /// Whether the queue stopped because request slots or descriptors were full.
    pub queue_full: bool,
}

/// Operations that require a block storage device driver to implement.
pub trait BlockDriverOps: BaseDriverOps {
    /// The number of blocks in this storage device.
    ///
    /// The total size of the device is `num_blocks() * block_size()`.
    fn num_blocks(&self) -> u64;
    /// The size of each block in bytes.
    fn block_size(&self) -> usize;

    /// Reads blocked data from the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be read.
    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult;

    /// Reads blocked data into a scatter list, starting from the given block.
    ///
    /// Each non-empty segment must contain a whole number of blocks. Drivers can
    /// override this to submit one hardware request with multiple descriptors.
    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        let block_size = self.block_size();
        let mut cur_block = block_id;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            if block_size == 0 || buf.len() % block_size != 0 {
                return Err(DevError::InvalidParam);
            }
            self.read_block(cur_block, buf)?;
            cur_block = cur_block
                .checked_add((buf.len() / block_size) as u64)
                .ok_or(DevError::InvalidParam)?;
        }
        Ok(())
    }

    /// Writes blocked data to the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be written.
    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult;

    /// Writes blocked data from a scatter list, starting from the given block.
    ///
    /// Each non-empty segment must contain a whole number of blocks. Drivers can
    /// override this to submit one hardware request with multiple descriptors.
    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        let block_size = self.block_size();
        let mut cur_block = block_id;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            if block_size == 0 || buf.len() % block_size != 0 {
                return Err(DevError::InvalidParam);
            }
            self.write_block(cur_block, buf)?;
            cur_block = cur_block
                .checked_add((buf.len() / block_size) as u64)
                .ok_or(DevError::InvalidParam)?;
        }
        Ok(())
    }

    /// Reads blocks directly into caller-owned pinned physical segments.
    ///
    /// The default does not publish a request. A driver may implement this only
    /// when it can validate the physical SG request and keep every DMA mapping
    /// alive until the synchronous request has been consumed.
    ///
    /// # Safety
    ///
    /// The caller must keep every segment pinned and valid for the entire
    /// synchronous call and must give access in the direction implied by this
    /// method (the device writes the segments for a read). Concurrent CPU and
    /// device accesses race on contents; this API never creates Rust references
    /// from the physical addresses.
    unsafe fn read_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        let _ = (block_id, segments);
        Ok(BlockPhysicalSgOutcome::NotSubmitted)
    }

    /// Writes blocks directly from caller-owned pinned physical segments.
    ///
    /// The default does not publish a request. Virtual [`BlockSegment`]
    /// requests remain the fallback API for drivers without this capability.
    ///
    /// # Safety
    ///
    /// The caller must keep every segment pinned and valid for the entire
    /// synchronous call and must give access in the direction implied by this
    /// method (the device reads the segments for a write). Concurrent CPU and
    /// device accesses race on contents; this API never creates Rust references
    /// from the physical addresses.
    unsafe fn write_block_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[BlockPhysicalSegment],
    ) -> DevResult<BlockPhysicalSgOutcome> {
        let _ = (block_id, segments);
        Ok(BlockPhysicalSgOutcome::NotSubmitted)
    }

    /// Flushes the device to write all pending data to the storage.
    fn flush(&mut self) -> DevResult;

    /// Returns async/batch queue capabilities when this driver supports them.
    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        None
    }

    /// Submits a batch of async block requests.
    ///
    /// On success, `submitted` must not exceed `requests.len()`, and every
    /// request in `requests[..submitted]` must contain a unique handle. On
    /// error, no request may remain able to access caller-owned buffers.
    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        let _ = requests;
        Err(DevError::Unsupported)
    }

    /// Submits a bounded ordinary batch without waiting for completion.
    /// Shared synchronous adapters use this split-phase hook to release an
    /// outer device mutex before the exact handle wait. Drivers may keep their
    /// optional asynchronous-I/O policy gate on [`Self::submit_async_batch`];
    /// this hook only admits the finite publication phase and never performs a
    /// blocking wait.
    fn submit_sync_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        self.submit_async_batch(requests)
    }

    /// Prepares and publishes a bounded batch of pinned physical requests.
    ///
    /// Implementations must finish slot allocation, descriptor construction,
    /// request header/status initialization, and all DMA mappings before the
    /// first descriptor becomes visible to the device.  A failure before
    /// publication leaves every request unpublished and eligible for caller
    /// fallback.  Once this method reports an accepted request, completion or
    /// a typed quarantine is the only valid terminal path; the driver must not
    /// silently retry it through a virtual-buffer fallback.
    ///
    /// # Safety
    ///
    /// Every physical range must remain pinned and valid, and must not be
    /// accessed by the caller until its concrete [`BlockCompletion`] has been
    /// returned by [`Self::wait_any_physical_completion`] or
    /// [`Self::drain_async_completions`].
    unsafe fn submit_physical_batch(
        &mut self,
        requests: &mut [BlockPhysicalRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        let _ = requests;
        Err(DevError::Unsupported)
    }

    /// Drains at most `output.len()` task-side completions.  The output slice
    /// is populated in used-ring order; no count-only completion API is needed
    /// by new callers.  A continuation indicates that another bounded drain
    /// must be scheduled immediately.
    fn drain_async_completions(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        let _ = output;
        Err(DevError::Unsupported)
    }

    /// Waits in task context until at least one concrete physical completion
    /// is available, then drains a bounded prefix into `output`.
    ///
    /// The caller-provided slice is the completion credit for this pass.  A
    /// non-empty slice must produce at least one physical completion before
    /// returning successfully; `continuation` asks the caller to schedule a
    /// further bounded pass without waiting for another interrupt.  Ordinary
    /// async completions are owned by their own drain path and must not be
    /// consumed here.  An empty slice is a no-op on a driver which implements
    /// this capability.
    ///
    /// The method is intended for a task-context completion owner.  Interrupt
    /// handlers only acknowledge the device and publish a wake/generation
    /// token; they never call this method or allocate its output state.
    fn wait_any_physical_completion(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> DevResult<BlockCompletionDrain> {
        let _ = output;
        Err(DevError::Unsupported)
    }

    /// Installs a bounded completion notification callback.  Drivers that do
    /// not have an IRQ/task wake source may leave this unsupported; shared
    /// wrappers retain a timed check fallback, while VirtIO installs the
    /// callback before enabling queue notifications.
    fn install_completion_notifier(
        &mut self,
        notifier: Option<BlockCompletionNotifier>,
        context: usize,
    ) -> DevResult {
        let _ = (notifier, context);
        Err(DevError::Unsupported)
    }

    /// Resets the device and releases DMA owners only after quiescence is
    /// proven.  A quarantined result is typed and must not be converted into a
    /// fabricated I/O completion.
    fn reset_device(&mut self) -> DevResult<BlockResetOutcome> {
        Err(DevError::Unsupported)
    }

    /// Drains and retires at most `budget` completed ordinary async block
    /// requests without blocking. A zero budget is a no-op and returns
    /// `Ok(0)`. This is the terminal owner for the count-only API: handles
    /// belonging to records consumed here must not be used afterwards. Use
    /// [`Self::drain_async_completions`] when the caller needs a concrete
    /// completion/status owner instead.
    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        let _ = budget;
        Err(DevError::Unsupported)
    }

    /// Waits for all listed async block requests to complete and releases their
    /// access to the submitted buffers.
    ///
    /// Implementations must reap every listed request before returning, even
    /// when one completed request reports a completion-status error. Transient
    /// wait errors must be retried. An implementation that loses the ability to
    /// prove that a request is quiescent must fail closed instead of returning
    /// while the device may still access caller-owned memory.
    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        let _ = handles;
        Err(DevError::Unsupported)
    }

    /// Enables completion interrupts for drivers that support IRQ-driven drain.
    fn enable_irq(&mut self) -> DevResult {
        Err(DevError::Unsupported)
    }

    /// Disables completion interrupts for drivers that support IRQ-driven drain.
    fn disable_irq(&mut self) -> DevResult {
        Err(DevError::Unsupported)
    }

    /// Returns whether the driver wrapper currently treats completion IRQs as enabled.
    fn is_irq_enabled(&self) -> bool {
        false
    }

    /// Handles a completion IRQ by acknowledging the device and publishing a
    /// coalesced task-context completion token.
    fn handle_irq(&mut self) -> DevResult<usize> {
        Err(DevError::Unsupported)
    }

    /// Fences previously submitted async writes without forcing a device-cache flush.
    fn fence_async(&mut self) -> DevResult {
        Err(DevError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    impl BaseDriverOps for Stub {
        fn device_name(&self) -> &str {
            "stub"
        }

        fn device_type(&self) -> DeviceType {
            DeviceType::Block
        }
    }

    impl BlockDriverOps for Stub {
        fn num_blocks(&self) -> u64 {
            1
        }

        fn block_size(&self) -> usize {
            512
        }

        fn read_block(&mut self, _block_id: u64, _buf: &mut [u8]) -> DevResult {
            Ok(())
        }

        fn write_block(&mut self, _block_id: u64, _buf: &[u8]) -> DevResult {
            Ok(())
        }

        fn flush(&mut self) -> DevResult {
            Ok(())
        }
    }

    #[test]
    fn physical_sg_defaults_to_not_submitted() {
        let mut stub = Stub;
        let segment = BlockPhysicalSegment {
            paddr: 0x2000,
            len: 512,
        };
        assert!(matches!(
            unsafe { stub.read_block_physical_sg(0, &[segment]) },
            Ok(BlockPhysicalSgOutcome::NotSubmitted)
        ));
        assert!(matches!(
            unsafe { stub.write_block_physical_sg(0, &[segment]) },
            Ok(BlockPhysicalSgOutcome::NotSubmitted)
        ));
    }

    #[test]
    fn completion_status_keeps_device_error_distinct_from_quarantine() {
        assert_ne!(
            BlockCompletionStatus::DeviceError(1),
            BlockCompletionStatus::Quarantined
        );
        assert_eq!(
            BlockCompletionStatus::Success,
            BlockCompletionStatus::Success
        );
        assert!(BlockPhysicalSgOutcome::Quarantined.is_quarantined());
        assert!(!BlockPhysicalSgOutcome::Quarantined.is_not_submitted());
        assert_ne!(BlockResetOutcome::Retired, BlockResetOutcome::Quiesced);
    }

    #[test]
    fn physical_completion_wait_defaults_to_unsupported() {
        let mut stub = Stub;
        let mut output = [BlockCompletion {
            handle: BlockRequestHandle { raw: 0 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0,
            status: BlockCompletionStatus::Success,
            bytes: 0,
        }];
        assert!(matches!(
            stub.wait_any_physical_completion(&mut output),
            Err(DevError::Unsupported)
        ));
    }

    #[test]
    fn physical_completion_wait_zero_output_does_not_hide_unsupported() {
        let mut stub = Stub;
        let mut output = [];
        assert!(matches!(
            stub.wait_any_physical_completion(&mut output),
            Err(DevError::Unsupported)
        ));
    }
}
