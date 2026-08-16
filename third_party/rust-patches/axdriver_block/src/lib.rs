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

    /// Drains at most `budget` completed async block requests without
    /// blocking. A zero budget is a no-op and returns `Ok(0)`.
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
