//! Driver for VirtIO block devices.

#[cfg(feature = "alloc")]
use alloc::{boxed::Box, vec::Vec};
use core::{hint::spin_loop, mem::ManuallyDrop};

use bitflags::bitflags;
use log::info;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{
    hal::{BufferDirection, DmaMapping, Hal},
    queue::{PhysicalBuffer, VirtQueue},
    stats::{
        record_blk_async_adaptive_completion, record_blk_async_admission_stall,
        record_blk_async_completion, record_blk_async_flush_completion,
        record_blk_async_flush_request, record_blk_async_queue_full,
        record_blk_async_resource_leaks, record_blk_async_submit_batch, record_blk_flush,
        record_blk_flush_unsupported, record_blk_pending_depth, record_blk_pending_drain,
        record_blk_pending_queue_full, record_blk_read, record_blk_write, record_queue_sync_wait,
    },
    transport::{DeviceStatus, Transport},
    volatile::{volread, Volatile},
    Error, Result,
};

const QUEUE: u16 = 0;
const QUEUE_SIZE: u16 = 128;
/// Fixed number of used-ring entries consumed by one task-context drain.
///
/// The interrupt path never calls the drain routine.  Keeping the credit in
/// this crate makes it impossible for a caller to accidentally turn a
/// completion notification into an unbounded queue walk.
pub const PENDING_COMPLETION_DRAIN_BUDGET: usize = 4;
/// Maximum number of pinned physical payload segments in one direct request.
pub const MAX_PHYSICAL_SG: usize = 16;
/// Maximum requests admitted by one prepared physical batch.
pub const MAX_PHYSICAL_BATCH_REQUESTS: usize = 32;
const RESET_POLL_BUDGET: usize = 1024;

/// Result of a device reset attempt.  `Quarantined` means the driver retained
/// all pending request owners and DMA mappings because quiescence was not
/// proven; callers must not turn it into a synthetic I/O status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetOutcome {
    /// The transport acknowledged reset and all outstanding device access was
    /// proven quiescent.
    Quiesced,
    /// The transport acknowledged reset and all outstanding device access was
    /// proven quiescent, but queue ownership was retired.  A complete
    /// transport reinitialization is required before submission can resume.
    Retired,
    /// The transport did not acknowledge reset within the bounded proof
    /// window; pending owners remain retained by the device object.
    Quarantined,
}

/// Generation-safe identity of the block device's sole virtqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockQueueHandle {
    /// Device/queue lifetime generation.
    pub generation: u64,
    /// VirtIO queue number.
    pub queue: u16,
}
const SUPPORTED_FEATURES: BlkFeature = BlkFeature::RO
    .union(BlkFeature::FLUSH)
    .union(BlkFeature::RING_INDIRECT_DESC)
    .union(BlkFeature::RING_EVENT_IDX);

// Raw handles reserve their low bit for the notification hint.  The remaining
// bits carry an opaque request cookie; once this space is exhausted the queue
// must fail closed instead of wrapping back into an earlier request identity.
const MAX_REQUEST_COOKIE: u64 = u64::MAX >> 1;

/// The completion owner selected at publication time.
///
/// Ordinary requests are retained for their exact waiter (or legacy caller),
/// while physical-effect requests are consumed only by the physical drain.
/// Keeping this class in the pending entry prevents a count-only consumer from
/// stealing a completion that still owns a caller-visible handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockCompletionOwner {
    /// A normal async/legacy request completed by its exact handle owner.
    Ordinary,
    /// A legacy `*_nb` request whose exact `complete_*` call still owns the
    /// terminal slot after a count-only drain reaps its used entry.
    Legacy,
    /// A physical DMA effect completed by the physical completion consumer.
    Physical,
}

/// Driver for a VirtIO block device.
///
/// This is a simple virtual block device, e.g. disk.
///
/// Read and write requests (and other exotic requests) are placed in the queue and serviced
/// (probably out of order) by the device except where noted.
///
/// # Example
///
/// ```
/// # use thekernel_virtio_drivers::{Error, Hal};
/// # use thekernel_virtio_drivers::transport::Transport;
/// use thekernel_virtio_drivers::device::blk::{SECTOR_SIZE, VirtIOBlk};
///
/// # fn example<HalImpl: Hal, T: Transport>(transport: T) -> Result<(), Error> {
/// let mut disk = VirtIOBlk::<HalImpl, _>::new(transport)?;
///
/// println!(
///     "VirtIO block device: {} kB",
///     disk.capacity() * SECTOR_SIZE as u64 / 2
/// );
///
/// // Read sector 0 and then copy it to sector 1.
/// let mut buf = [0; SECTOR_SIZE];
/// disk.read_blocks(0, &mut buf)?;
/// disk.write_blocks(1, &buf)?;
/// # Ok(())
/// # }
/// ```
pub struct VirtIOBlk<H: Hal, T: Transport> {
    // These resources are manually dropped so a failed reset can retain the
    // complete DMA owner without relying on panic/unwind behavior. The
    // quarantine branch of Drop intentionally leaves these values in place.
    transport: ManuallyDrop<T>,
    queue: ManuallyDrop<VirtQueue<H, { QUEUE_SIZE as usize }>>,
    #[cfg(feature = "alloc")]
    pending: ManuallyDrop<Box<[Option<PendingBlkRequest>]>>,
    #[cfg(not(feature = "alloc"))]
    pending: ManuallyDrop<[Option<PendingBlkRequest>; QUEUE_SIZE as usize]>,
    /// Submission-time notification bits retained with each slot so a raw
    /// completion handle is byte-for-byte identical to the handle returned
    /// to the caller (including its low notification hint bit).
    notified_slots: [bool; QUEUE_SIZE as usize],
    token_slots: [Option<usize>; QUEUE_SIZE as usize],
    pending_count: usize,
    async_pending_count: usize,
    physical_pending_count: usize,
    capacity: u64,
    negotiated_features: BlkFeature,
    /// Monotonic generation for this device/queue lifetime.  Handles carry
    /// this generation and are rejected after reset or teardown.
    generation: u64,
    /// Monotonic request identity. It is deliberately not reset with the
    /// transport generation, so a released slot/token pair can never ABA.
    next_cookie: u64,
    quarantined: bool,
    retired: bool,
}

/// One caller-owned physical payload segment for a synchronous direct request.
///
/// The caller must keep this physical range pinned and valid until the
/// corresponding read/write method returns. A read is device-to-driver and a
/// write is driver-to-device; concurrent CPU/device access races on contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalSegment {
    /// Physical address of the first byte.
    pub paddr: usize,
    /// Segment length in bytes.
    pub len: usize,
}

enum PendingBlkBuffer {
    Read {
        buf: *mut u8,
        len: usize,
    },
    Write {
        buf: *const u8,
        len: usize,
    },
    #[cfg(feature = "alloc")]
    ReadVectored {
        bufs: Vec<PendingReadSegment>,
    },
    #[cfg(feature = "alloc")]
    WriteVectored {
        bufs: Vec<PendingWriteSegment>,
    },
    PhysicalRead {
        mappings: [Option<DmaMapping>; MAX_PHYSICAL_SG],
        buffers: [PhysicalBuffer; MAX_PHYSICAL_SG],
        count: usize,
    },
    PhysicalWrite {
        mappings: [Option<DmaMapping>; MAX_PHYSICAL_SG],
        buffers: [PhysicalBuffer; MAX_PHYSICAL_SG],
        count: usize,
    },
    Flush,
}

#[cfg(feature = "alloc")]
struct PendingReadSegment {
    buf: *mut u8,
    len: usize,
}

#[cfg(feature = "alloc")]
struct PendingWriteSegment {
    buf: *const u8,
    len: usize,
}

struct PendingBlkRequest {
    req: BlkReq,
    resp: BlkResp,
    buffer: PendingBlkBuffer,
    token: Option<u16>,
    bytes: usize,
    done: bool,
    async_accounted: bool,
    completion_cookie: u64,
    completion_owner: BlockCompletionOwner,
    /// Optional caller-owned legacy response destination. It is written only
    /// after the matching used element has been reaped.
    legacy_resp: Option<*mut BlkResp>,
    /// Optional caller-owned legacy request identity used to reject a stale
    /// `complete_*` call for a reused token.
    legacy_req: Option<*const BlkReq>,
    /// Data bytes reported by the device used length after protocol
    /// validation. This is populated exactly once when `done` becomes true.
    completion_bytes: u32,
    /// A physical completion may be reaped by its exact handle waiter, but
    /// ordinary/count-only paths must not retire it.  This bit records that
    /// explicit owner claim before `complete_pending_request` is allowed to
    /// release the slot.
    completion_claimed: bool,
}

/// Handle for a submitted pending block request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingBlkHandle {
    generation: u64,
    cookie: u64,
    slot: u16,
    token: u16,
    notified: bool,
}

impl PendingBlkHandle {
    /// Returns whether submitting this request notified the device.
    pub fn notified(self) -> bool {
        self.notified
    }

    /// Encodes this handle for cross-crate async block APIs.
    pub fn into_raw(self) -> u64 {
        // All handles produced by the driver have a non-zero cookie bounded
        // by MAX_REQUEST_COOKIE. Raw transport APIs intentionally carry only
        // this opaque identity plus the notification hint; slot/token are an
        // implementation detail and must not be used for ABA validation.
        debug_assert!(self.cookie != 0 && self.cookie <= MAX_REQUEST_COOKIE);
        (self.cookie << 1) | u64::from(self.notified)
    }

    /// Decodes a handle previously returned by [`Self::into_raw`].
    pub fn from_raw(raw: u64) -> Self {
        Self {
            generation: 0,
            cookie: raw >> 1,
            slot: 0,
            token: 0,
            notified: (raw & 1) != 0,
        }
    }

    /// Returns the generation captured when this request was submitted.
    pub fn generation(self) -> u64 {
        self.generation
    }

    /// Returns the completion cookie assigned to this request.
    pub fn completion_cookie(self) -> u64 {
        self.cookie
    }

    /// Returns the queue generation captured by this request.
    pub fn queue_handle(self) -> BlockQueueHandle {
        BlockQueueHandle {
            generation: self.generation,
            queue: QUEUE,
        }
    }
}

/// Data buffer for one pending batch request.
pub enum PendingBlkBatchBuffer<'a> {
    /// Device writes into the buffer.
    Read(&'a mut [u8]),
    /// Device reads from the buffer.
    Write(&'a [u8]),
    /// Device writes into a scatter list.
    #[cfg(feature = "alloc")]
    ReadVectored(Vec<&'a mut [u8]>),
    /// Device reads from a scatter list.
    #[cfg(feature = "alloc")]
    WriteVectored(Vec<&'a [u8]>),
    /// Flush previously completed writes to stable backend storage.
    Flush,
}

/// Physical payload offered to the atomic device-side batch preparer.
pub enum PendingBlkPhysicalBatchBuffer<'a> {
    /// Device writes into the pinned ranges.
    Read(&'a [PhysicalSegment]),
    /// Device reads from the pinned ranges.
    Write(&'a [PhysicalSegment]),
}

/// One pinned physical request in a [`PreparedBlockBatch`].
pub struct PendingBlkPhysicalBatchRequest<'a> {
    /// First 512-byte sector for this request.
    pub block_id: u64,
    /// Pinned physical scatter-gather payload.
    pub buffer: PendingBlkPhysicalBatchBuffer<'a>,
    /// Filled only when the prepared batch is consumed by `publish`.
    pub handle: Option<PendingBlkHandle>,
}

/// An all-or-nothing prepared physical block batch.
///
/// Preparation owns every queue slot, descriptor chain, request header/status
/// object, and DMA mapping.  Dropping this value before [`Self::publish`] never
/// exposes a descriptor to the device and returns all ownership to the caller.
pub struct PreparedBlockBatch<'dev, 'req, H: Hal, T: Transport> {
    device: &'dev mut VirtIOBlk<H, T>,
    requests: *mut PendingBlkPhysicalBatchRequest<'req>,
    request_count: usize,
    prepared_count: usize,
    total_bytes: usize,
    heads: [u16; MAX_PHYSICAL_BATCH_REQUESTS],
    slots: [u16; MAX_PHYSICAL_BATCH_REQUESTS],
    tokens: [u16; MAX_PHYSICAL_BATCH_REQUESTS],
    published: bool,
    _request_lifetime: core::marker::PhantomData<&'req mut [PendingBlkPhysicalBatchRequest<'req>]>,
}

impl PendingBlkBatchBuffer<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Read(buf) => buf.len(),
            Self::Write(buf) => buf.len(),
            #[cfg(feature = "alloc")]
            Self::ReadVectored(bufs) => bufs.iter().map(|buf| buf.len()).sum(),
            #[cfg(feature = "alloc")]
            Self::WriteVectored(bufs) => bufs.iter().map(|buf| buf.len()).sum(),
            Self::Flush => 0,
        }
    }

    fn segment_count(&self) -> usize {
        match self {
            Self::Read(buf) => usize::from(!buf.is_empty()),
            Self::Write(buf) => usize::from(!buf.is_empty()),
            #[cfg(feature = "alloc")]
            Self::ReadVectored(bufs) => bufs.iter().filter(|buf| !buf.is_empty()).count(),
            #[cfg(feature = "alloc")]
            Self::WriteVectored(bufs) => bufs.iter().filter(|buf| !buf.is_empty()).count(),
            Self::Flush => 0,
        }
    }
}

/// One request offered to the pending block batch submitter.
pub struct PendingBlkBatchRequest<'a> {
    /// First 512-byte sector for this request.
    pub block_id: usize,
    /// Data buffer for this request.
    pub buffer: PendingBlkBatchBuffer<'a>,
    /// Driver-filled handle when this request is accepted.
    pub handle: Option<PendingBlkHandle>,
}

/// Report returned by pending batch submission.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PendingBlkBatchReport {
    /// Number of accepted requests.
    pub submitted: usize,
    /// Bytes covered by accepted requests.
    pub bytes: usize,
    /// Whether admission stopped because the queue or request pool was full.
    pub queue_full: bool,
    /// Whether the batch submit notified the device.
    pub notified: bool,
}

/// Result of one bounded pending-completion drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingBlkDrainStatus {
    /// The used ring was empty after consuming `drained` entries.
    Complete {
        /// Number of requests completed by this drain.
        drained: usize,
    },
    /// The fixed credit was exhausted and at least one used-ring entry remains.
    Continuation {
        /// Number of requests completed by this drain.
        drained: usize,
    },
}

/// A concrete completion returned by the bounded task-side drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockCompletion {
    /// Generation-safe pending handle.
    pub handle: PendingBlkHandle,
    /// Completion cookie initialized before publication.
    pub cookie: u64,
    /// Completion class selected when the request was published.
    pub owner: BlockCompletionOwner,
    /// Raw VirtIO block status.
    pub status: RespStatus,
    /// Bytes reported by the used element.
    pub bytes: u32,
}

/// Result metadata for a bounded completion drain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockCompletionDrain {
    /// Number of entries written to the caller's output slice.
    pub completed: usize,
    /// Whether another used-ring entry remains ready.
    pub continuation: bool,
}

impl PendingBlkDrainStatus {
    /// Returns the number of completions consumed by this drain.
    pub const fn drained(self) -> usize {
        match self {
            Self::Complete { drained } | Self::Continuation { drained } => drained,
        }
    }

    /// Returns whether the caller must schedule another task-context drain.
    pub const fn has_continuation(self) -> bool {
        matches!(self, Self::Continuation { .. })
    }
}

// SAFETY: Pending requests carry raw identities for caller/user buffers that
// have already been shared with the device. Request header/status ownership is
// now internal to the queue. Installation, completion, and removal are
// serialized by the owning block device's outer mutex.
unsafe impl Send for PendingBlkRequest {}

// SAFETY: Shared references to pending entries do not expose mutation.
// Completion mutates the internal response and caller buffer only through the
// serialized drain path described above.
unsafe impl Sync for PendingBlkRequest {}

impl PendingBlkRequest {
    fn read(block_id: usize, buf: &mut [u8]) -> Self {
        Self {
            req: BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: block_id as u64,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::Read {
                buf: buf.as_mut_ptr(),
                len: buf.len(),
            },
            token: None,
            bytes: buf.len(),
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    fn write(block_id: usize, buf: &[u8]) -> Self {
        Self {
            req: BlkReq {
                type_: ReqType::Out,
                reserved: 0,
                sector: block_id as u64,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::Write {
                buf: buf.as_ptr(),
                len: buf.len(),
            },
            token: None,
            bytes: buf.len(),
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    fn flush() -> Self {
        Self {
            req: BlkReq {
                type_: ReqType::Flush,
                reserved: 0,
                sector: 0,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::Flush,
            token: None,
            bytes: 0,
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    fn physical_read(
        block_id: u64,
        mappings: [Option<DmaMapping>; MAX_PHYSICAL_SG],
        buffers: [PhysicalBuffer; MAX_PHYSICAL_SG],
        count: usize,
        bytes: usize,
    ) -> Self {
        Self {
            req: BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: block_id,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::PhysicalRead {
                mappings,
                buffers,
                count,
            },
            token: None,
            bytes,
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    fn physical_write(
        block_id: u64,
        mappings: [Option<DmaMapping>; MAX_PHYSICAL_SG],
        buffers: [PhysicalBuffer; MAX_PHYSICAL_SG],
        count: usize,
        bytes: usize,
    ) -> Self {
        Self {
            req: BlkReq {
                type_: ReqType::Out,
                reserved: 0,
                sector: block_id,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::PhysicalWrite {
                mappings,
                buffers,
                count,
            },
            token: None,
            bytes,
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    #[cfg(feature = "alloc")]
    fn read_vectored(block_id: usize, bufs: &mut [&mut [u8]]) -> Self {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        Self {
            req: BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: block_id as u64,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::ReadVectored {
                bufs: bufs
                    .iter_mut()
                    .map(|buf| PendingReadSegment {
                        buf: buf.as_mut_ptr(),
                        len: buf.len(),
                    })
                    .collect(),
            },
            token: None,
            bytes,
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    #[cfg(feature = "alloc")]
    fn write_vectored(block_id: usize, bufs: &[&[u8]]) -> Self {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        Self {
            req: BlkReq {
                type_: ReqType::Out,
                reserved: 0,
                sector: block_id as u64,
            },
            resp: BlkResp::default(),
            buffer: PendingBlkBuffer::WriteVectored {
                bufs: bufs
                    .iter()
                    .map(|buf| PendingWriteSegment {
                        buf: buf.as_ptr(),
                        len: buf.len(),
                    })
                    .collect(),
            },
            token: None,
            bytes,
            done: false,
            async_accounted: false,
            completion_cookie: 0,
            completion_owner: BlockCompletionOwner::Ordinary,
            legacy_resp: None,
            legacy_req: None,
            completion_bytes: 0,
            completion_claimed: false,
        }
    }

    fn mark_async_accounted(&mut self) {
        self.async_accounted = true;
    }

    fn mark_physical_owner(&mut self) {
        self.completion_owner = BlockCompletionOwner::Physical;
    }

    fn legacy_read(
        block_id: usize,
        req: *const BlkReq,
        buf: &mut [u8],
        resp: *mut BlkResp,
    ) -> Self {
        let mut pending = Self::read(block_id, buf);
        // The queue owns the stable request header/status in `pending`; the
        // caller-owned legacy objects are only an identity/output bridge.
        pending.legacy_req = Some(req);
        pending.legacy_resp = Some(resp);
        pending.completion_owner = BlockCompletionOwner::Legacy;
        pending
    }

    fn legacy_write(block_id: usize, req: *const BlkReq, buf: &[u8], resp: *mut BlkResp) -> Self {
        let mut pending = Self::write(block_id, buf);
        pending.legacy_req = Some(req);
        pending.legacy_resp = Some(resp);
        pending.completion_owner = BlockCompletionOwner::Legacy;
        pending
    }

    fn legacy_matches(
        &self,
        req: *const BlkReq,
        buf: *const u8,
        len: usize,
        resp: *mut BlkResp,
        read: bool,
    ) -> bool {
        if self.legacy_req != Some(req)
            || self.legacy_resp != Some(resp)
            || self.completion_cookie == 0
        {
            return false;
        }
        // The caller-owned request object carries the full monotonic cookie
        // after submission.  The queue uses the driver's private request
        // header, so this marker never reaches the device.  Comparing the
        // full cookie closes the u16 token ABA hole even when the caller
        // reuses the same request/buffer variables for a later operation.
        let cookie_matches =
            unsafe { req.as_ref() }.is_some_and(|request| request.sector == self.completion_cookie);
        let buffer_matches = match (&self.buffer, read) {
            (
                PendingBlkBuffer::Read {
                    buf: stored,
                    len: stored_len,
                },
                true,
            ) => (*stored as *const u8) == buf && *stored_len == len,
            (
                PendingBlkBuffer::Write {
                    buf: stored,
                    len: stored_len,
                },
                false,
            ) => *stored == buf && *stored_len == len,
            _ => false,
        };
        cookie_matches && buffer_matches
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn is_flush(&self) -> bool {
        self.req.type_ == ReqType::Flush
    }

    fn is_read(&self) -> bool {
        match &self.buffer {
            PendingBlkBuffer::Read { .. } | PendingBlkBuffer::PhysicalRead { .. } => true,
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { .. } => true,
            _ => false,
        }
    }

    /// Convert a VirtIO used length into payload bytes. A malformed device
    /// length is a protocol violation: the used entry has already been
    /// reaped, but the request owner must remain retained under typed
    /// quarantine rather than being released as an ordinary I/O error.
    fn validate_completion_bytes(&mut self, used_len: u32) -> Result<u32> {
        let used_len = usize::try_from(used_len).unwrap_or(usize::MAX);
        let valid = if self.is_read() {
            // The block read chain includes the complete device-written
            // payload followed by its one-byte status. Any status-only,
            // short, or oversized used length is malformed, even when the
            // status reports an I/O error.
            used_len == self.bytes.checked_add(1).unwrap_or(usize::MAX)
        } else {
            // Writes and flushes have no device-writable payload beyond
            // status, regardless of the returned status code.
            used_len == 1
        };
        if !valid {
            self.resp.status = RespStatus::IO_ERR;
            return Err(Error::Quarantined);
        }
        if self.is_read() {
            Ok((used_len - 1) as u32)
        } else {
            Ok(0)
        }
    }

    fn data_segments(&self) -> usize {
        match &self.buffer {
            PendingBlkBuffer::Read { .. } | PendingBlkBuffer::Write { .. } => 1,
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { bufs } => bufs.len(),
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::WriteVectored { bufs } => bufs.len(),
            PendingBlkBuffer::PhysicalRead { count, .. }
            | PendingBlkBuffer::PhysicalWrite { count, .. } => *count,
            PendingBlkBuffer::Flush => 0,
        }
    }

    fn descriptor_cost(&self, indirect: bool) -> usize {
        let full_chain = self.data_segments() + 2;
        // Physical chains use the same indirect-table path as virtual
        // chains.  Keeping them out of this accounting makes the reset
        // ownership proof think that an otherwise tracked request is an
        // untracked descriptor.
        if indirect && full_chain > 1 {
            1
        } else {
            full_chain
        }
    }

    /// Releases physical mappings after a pre-publish submission failure.
    ///
    /// This is only used before the descriptor is visible to the device. Once
    /// published, mappings are released by `complete` only after the used
    /// entry has also passed protocol validation.
    fn unmap_physical<H: Hal>(self) {
        let (mappings, count, direction) = match self.buffer {
            PendingBlkBuffer::PhysicalRead {
                mappings, count, ..
            } => (mappings, count, BufferDirection::DeviceToDriver),
            PendingBlkBuffer::PhysicalWrite {
                mappings, count, ..
            } => (mappings, count, BufferDirection::DriverToDevice),
            _ => return,
        };
        for mapping in mappings[..count].iter().rev().flatten().copied() {
            // SAFETY: This method is called only before publication.
            unsafe { H::unmap_physical(mapping, direction) };
        }
    }

    /// Releases mappings after a used entry has been consumed and transport
    /// quiescence has been proven.  A malformed used length deliberately keeps
    /// these slots populated while the device is quarantined; dropping or
    /// unmapping them before reset would turn a protocol violation into an
    /// ordinary completion and could let the device retain an untracked DMA
    /// owner.
    fn release_physical_mappings<H: Hal>(&mut self) {
        let (mappings, count, direction) = match &mut self.buffer {
            PendingBlkBuffer::PhysicalRead {
                mappings, count, ..
            } => (mappings, *count, BufferDirection::DeviceToDriver),
            PendingBlkBuffer::PhysicalWrite {
                mappings, count, ..
            } => (mappings, *count, BufferDirection::DriverToDevice),
            _ => return,
        };
        for mapping in mappings[..count].iter_mut().rev() {
            let Some(mapping_value) = mapping.take() else {
                continue;
            };
            // SAFETY: the caller invokes this only after a valid used entry or
            // a reset proof has stopped device access to the mapping.
            unsafe { H::unmap_physical(mapping_value, direction) };
        }
    }

    /// Rolls back a descriptor chain that was installed but not published.
    /// This is used when request identity allocation fails after descriptor
    /// preparation; no caller-visible fallback is allowed to observe a live
    /// queue owner in that case.
    unsafe fn discard_unpublished<H: Hal, const SIZE: usize>(
        self,
        queue: &mut VirtQueue<H, SIZE>,
        token: u16,
    ) {
        let mut this = self;
        match &this.buffer {
            PendingBlkBuffer::Read { buf, len } => {
                let data = unsafe { core::slice::from_raw_parts_mut(*buf, *len) };
                // SAFETY: this chain has not been published and these are its
                // exact virtual buffers.
                unsafe {
                    queue.discard_unpublished(
                        token,
                        &[this.req.as_bytes()],
                        &mut [data, this.resp.as_bytes_mut()],
                    )
                };
            }
            PendingBlkBuffer::Write { buf, len } => {
                let data = unsafe { core::slice::from_raw_parts(*buf, *len) };
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_unpublished(
                        token,
                        &[this.req.as_bytes(), data],
                        &mut [this.resp.as_bytes_mut()],
                    )
                };
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { bufs } => {
                let mut outputs = Vec::with_capacity(bufs.len() + 1);
                for segment in bufs {
                    outputs
                        .push(unsafe { core::slice::from_raw_parts_mut(segment.buf, segment.len) });
                }
                outputs.push(this.resp.as_bytes_mut());
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_unpublished(token, &[this.req.as_bytes()], outputs.as_mut_slice())
                };
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::WriteVectored { bufs } => {
                let mut inputs = Vec::with_capacity(bufs.len() + 1);
                inputs.push(this.req.as_bytes());
                for segment in bufs {
                    inputs.push(unsafe { core::slice::from_raw_parts(segment.buf, segment.len) });
                }
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_unpublished(
                        token,
                        inputs.as_slice(),
                        &mut [this.resp.as_bytes_mut()],
                    )
                };
            }
            PendingBlkBuffer::Flush => {
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_unpublished(
                        token,
                        &[this.req.as_bytes()],
                        &mut [this.resp.as_bytes_mut()],
                    )
                };
            }
            PendingBlkBuffer::PhysicalRead { .. } | PendingBlkBuffer::PhysicalWrite { .. } => {
                // SAFETY: this is the physical-chain rollback path.
                unsafe { this.discard_unpublished_physical(queue, token) };
                return;
            }
        }
    }

    unsafe fn discard_unpublished_physical<H: Hal, const SIZE: usize>(
        self,
        queue: &mut VirtQueue<H, SIZE>,
        token: u16,
    ) {
        let mut this = self;
        match &this.buffer {
            PendingBlkBuffer::PhysicalRead { buffers, count, .. } => {
                // SAFETY: the descriptor chain was installed by this request
                // and has not been published to the device.
                unsafe {
                    queue.discard_unpublished_physical(
                        token,
                        &[this.req.as_bytes()],
                        &[],
                        &buffers[..*count],
                        &mut [this.resp.as_bytes_mut()],
                    );
                }
            }
            PendingBlkBuffer::PhysicalWrite { buffers, count, .. } => {
                // SAFETY: the descriptor chain was installed by this request
                // and has not been published to the device.
                unsafe {
                    queue.discard_unpublished_physical(
                        token,
                        &[this.req.as_bytes()],
                        &buffers[..*count],
                        &[],
                        &mut [this.resp.as_bytes_mut()],
                    );
                }
            }
            _ => unreachable!("prepared physical batch contains virtual request"),
        }
        this.unmap_physical::<H>();
    }

    unsafe fn recycle_after_quiescence<H: Hal, const SIZE: usize>(
        self,
        queue: &mut VirtQueue<H, SIZE>,
        token: u16,
    ) {
        let mut this = self;
        match &this.buffer {
            PendingBlkBuffer::Read { buf, len } => {
                let data = unsafe { core::slice::from_raw_parts_mut(*buf, *len) };
                // SAFETY: the transport reset proved that the device no
                // longer accesses this chain; the buffers match publication.
                unsafe {
                    queue.discard_quiesced(
                        token,
                        &[this.req.as_bytes()],
                        &mut [data, this.resp.as_bytes_mut()],
                    );
                }
            }
            PendingBlkBuffer::Write { buf, len } => {
                let data = unsafe { core::slice::from_raw_parts(*buf, *len) };
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_quiesced(
                        token,
                        &[this.req.as_bytes(), data],
                        &mut [this.resp.as_bytes_mut()],
                    );
                }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { bufs } => {
                let mut outputs = Vec::with_capacity(bufs.len() + 1);
                for segment in bufs {
                    outputs
                        .push(unsafe { core::slice::from_raw_parts_mut(segment.buf, segment.len) });
                }
                outputs.push(this.resp.as_bytes_mut());
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_quiesced(token, &[this.req.as_bytes()], outputs.as_mut_slice());
                }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::WriteVectored { bufs } => {
                let mut inputs = Vec::with_capacity(bufs.len() + 1);
                inputs.push(this.req.as_bytes());
                for segment in bufs {
                    inputs.push(unsafe { core::slice::from_raw_parts(segment.buf, segment.len) });
                }
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_quiesced(
                        token,
                        inputs.as_slice(),
                        &mut [this.resp.as_bytes_mut()],
                    );
                }
            }
            PendingBlkBuffer::Flush => {
                // SAFETY: see the read branch above.
                unsafe {
                    queue.discard_quiesced(
                        token,
                        &[this.req.as_bytes()],
                        &mut [this.resp.as_bytes_mut()],
                    );
                }
            }
            PendingBlkBuffer::PhysicalRead { .. } | PendingBlkBuffer::PhysicalWrite { .. } => {
                // SAFETY: the transport reset proved quiescence, so the
                // published physical chain can use the same descriptor
                // recycling path as an unpublished chain.
                unsafe { this.discard_unpublished_physical(queue, token) };
            }
        }
    }

    unsafe fn complete<H: Hal, const SIZE: usize>(
        &mut self,
        queue: &mut VirtQueue<H, SIZE>,
        token: u16,
    ) -> Result<u32> {
        if self.token != Some(token) {
            return Err(Error::WrongToken);
        }
        let used_len = match &self.buffer {
            PendingBlkBuffer::Read { buf, len } => {
                // SAFETY: The caller that submitted the pending request promised
                // this buffer remains valid until the returned handle completes.
                let buf = unsafe { core::slice::from_raw_parts_mut(*buf, *len) };
                // SAFETY: These are exactly the buffers passed to `add_unpublished`
                // for this token.
                unsafe {
                    queue.pop_used(
                        token,
                        &[self.req.as_bytes()],
                        &mut [buf, self.resp.as_bytes_mut()],
                    )
                }
            }
            PendingBlkBuffer::Write { buf, len } => {
                // SAFETY: The caller that submitted the pending request promised
                // this buffer remains valid until the returned handle completes.
                let buf = unsafe { core::slice::from_raw_parts(*buf, *len) };
                // SAFETY: These are exactly the buffers passed to `add_unpublished`
                // for this token.
                unsafe {
                    queue.pop_used(
                        token,
                        &[self.req.as_bytes(), buf],
                        &mut [self.resp.as_bytes_mut()],
                    )
                }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { bufs } => {
                let mut outputs = Vec::with_capacity(bufs.len() + 1);
                for segment in bufs {
                    // SAFETY: The caller that submitted the pending request promised
                    // these buffers remain valid until the returned handle completes.
                    outputs
                        .push(unsafe { core::slice::from_raw_parts_mut(segment.buf, segment.len) });
                }
                outputs.push(self.resp.as_bytes_mut());
                // SAFETY: These are exactly the buffers passed to
                // `add_unpublished` for this token.
                unsafe { queue.pop_used(token, &[self.req.as_bytes()], outputs.as_mut_slice()) }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::WriteVectored { bufs } => {
                let mut inputs = Vec::with_capacity(bufs.len() + 1);
                inputs.push(self.req.as_bytes());
                for segment in bufs {
                    // SAFETY: The caller that submitted the pending request promised
                    // these buffers remain valid until the returned handle completes.
                    inputs.push(unsafe { core::slice::from_raw_parts(segment.buf, segment.len) });
                }
                // SAFETY: These are exactly the buffers passed to
                // `add_unpublished` for this token.
                unsafe { queue.pop_used(token, inputs.as_slice(), &mut [self.resp.as_bytes_mut()]) }
            }
            PendingBlkBuffer::Flush => {
                // SAFETY: These are exactly the buffers passed to
                // `add_unpublished` for this token.
                unsafe {
                    queue.pop_used(
                        token,
                        &[self.req.as_bytes()],
                        &mut [self.resp.as_bytes_mut()],
                    )
                }
            }
            PendingBlkBuffer::PhysicalRead { buffers, count, .. } => unsafe {
                queue.pop_used_physical(
                    token,
                    &[self.req.as_bytes()],
                    &[],
                    &buffers[..*count],
                    &mut [self.resp.as_bytes_mut()],
                )
            },
            PendingBlkBuffer::PhysicalWrite { buffers, count, .. } => unsafe {
                queue.pop_used_physical(
                    token,
                    &[self.req.as_bytes()],
                    &buffers[..*count],
                    &[],
                    &mut [self.resp.as_bytes_mut()],
                )
            },
        }?;

        let completion_bytes = self.validate_completion_bytes(used_len)?;
        // Do not mark the request done or release a physical mapping until the
        // used length has passed protocol validation.  A malformed entry has
        // already been consumed, but `done` must remain false so reset can
        // recycle the retained pending owner after transport quiescence.
        self.release_physical_mappings::<H>();
        self.completion_bytes = completion_bytes;
        self.done = true;
        if let Some(resp) = self.legacy_resp {
            // SAFETY: a legacy caller promises this response remains valid
            // until it calls the matching complete_* method. The used entry
            // has already retired the device's descriptor access.
            unsafe {
                *resp = BlkResp {
                    status: self.resp.status(),
                };
            }
        }
        Ok(self.completion_bytes)
    }
}

impl<H: Hal, T: Transport> VirtIOBlk<H, T> {
    fn validate_sync_used_len(
        _status: RespStatus,
        payload_len: Option<usize>,
        used_len: u32,
    ) -> Result<()> {
        let used_len = usize::try_from(used_len).unwrap_or(usize::MAX);
        let valid = match payload_len {
            Some(payload_len) => {
                used_len == payload_len.checked_add(1).ok_or(Error::InvalidParam)?
            }
            None => used_len == 1,
        };
        if valid {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Create a new VirtIO-Blk driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        // Read configuration space.
        let config = transport.config_space::<BlkConfig>()?;
        info!("config: {:?}", config);
        // Safe because config is a valid pointer to the device configuration space.
        let capacity = unsafe {
            volread!(config, capacity_low) as u64 | (volread!(config, capacity_high) as u64) << 32
        };
        info!("found a block device of size {}KB", capacity / 2);

        let queue = VirtQueue::new(
            &mut transport,
            QUEUE,
            negotiated_features.contains(BlkFeature::RING_INDIRECT_DESC),
            negotiated_features.contains(BlkFeature::RING_EVENT_IDX),
        )?;
        transport.finish_init();

        #[cfg(feature = "alloc")]
        let pending = {
            let pending_len = QUEUE_SIZE as usize;
            let mut pending = Vec::new();
            pending
                .try_reserve_exact(pending_len)
                .map_err(|_| Error::DmaError)?;
            pending.resize_with(pending_len, || None);
            pending.into_boxed_slice()
        };
        #[cfg(not(feature = "alloc"))]
        let pending = core::array::from_fn(|_| None);

        Ok(VirtIOBlk {
            transport: ManuallyDrop::new(transport),
            queue: ManuallyDrop::new(queue),
            pending: ManuallyDrop::new(pending),
            notified_slots: [false; QUEUE_SIZE as usize],
            token_slots: [None; QUEUE_SIZE as usize],
            pending_count: 0,
            async_pending_count: 0,
            physical_pending_count: 0,
            capacity,
            negotiated_features,
            generation: 1,
            next_cookie: 1,
            quarantined: false,
            retired: false,
        })
    }

    fn alloc_pending_slot(&self) -> Result<usize> {
        self.pending
            .iter()
            .position(Option::is_none)
            .ok_or(Error::QueueFull)
    }

    fn ensure_live(&self) -> Result {
        if self.quarantined || self.retired {
            Err(Error::Quarantined)
        } else {
            Ok(())
        }
    }

    fn pending_handle(&self, slot: usize, notified: bool) -> Result<PendingBlkHandle> {
        let request = self.pending[slot].as_ref().ok_or(Error::WrongToken)?;
        let token = request.token.ok_or(Error::WrongToken)?;
        Ok(PendingBlkHandle {
            generation: self.generation,
            cookie: request.completion_cookie,
            slot: slot as u16,
            token,
            notified: notified || self.notified_slots[slot],
        })
    }

    fn find_pending_cookie(&self, cookie: u64) -> Option<usize> {
        if cookie == 0 {
            return None;
        }
        self.pending.iter().enumerate().find_map(|(slot, entry)| {
            entry
                .as_ref()
                .and_then(|entry| (entry.completion_cookie == cookie).then_some(slot))
        })
    }

    fn pending_slot_for_token(&self, token: u16) -> Result<usize> {
        self.token_slots
            .get(usize::from(token))
            .copied()
            .flatten()
            .ok_or(Error::WrongToken)
    }

    fn retire_pending_slot(&mut self, slot: usize) -> Result<RespStatus> {
        let token = match self
            .pending
            .get(slot)
            .and_then(Option::as_ref)
            .and_then(|entry| entry.token)
        {
            Some(token) => token,
            None => {
                // Never take/drop a pending owner until its token binding is
                // proven. A malformed owner must remain retained for the
                // typed quarantine/reset path rather than becoming an EIO
                // after its DMA mappings have been released by Drop.
                if self.pending.get(slot).and_then(Option::as_ref).is_some() {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                return Err(Error::WrongToken);
            }
        };
        let Some(token_slot) = self.token_slots.get(usize::from(token)) else {
            self.quarantined = true;
            return Err(Error::Quarantined);
        };
        if *token_slot != Some(slot) {
            self.quarantined = true;
            return Err(Error::Quarantined);
        }
        let entry = self.pending[slot].take().ok_or(Error::WrongToken)?;
        self.token_slots[usize::from(token)] = None;
        self.notified_slots[slot] = false;
        if entry.async_accounted {
            self.async_pending_count = self
                .async_pending_count
                .checked_sub(1)
                .expect("async pending-count underflow");
        }
        if entry.completion_owner == BlockCompletionOwner::Physical {
            self.physical_pending_count = self
                .physical_pending_count
                .checked_sub(1)
                .expect("physical pending-count underflow");
        }
        Ok(entry.resp.status())
    }

    /// Retires at most `budget` done requests for a bounded count-only pass.
    /// Exact waiters retain their done entries until they call
    /// `complete_pending_request`; this method is the explicit owner for the
    /// count-only terminal path and therefore does not need a FIFO.
    pub fn retire_completion_records_bounded(&mut self, budget: usize) -> Option<RespStatus> {
        let mut first_error = None;
        let mut retired = 0usize;
        for slot in 0..self.pending.len() {
            if retired >= budget {
                break;
            }
            if self.quarantined {
                break;
            }
            let retire = self.pending[slot].as_ref().is_some_and(|entry| {
                entry.done && entry.completion_owner == BlockCompletionOwner::Ordinary
            });
            if retire {
                match self.retire_pending_slot(slot) {
                    Ok(status) if status != RespStatus::OK => {
                        first_error.get_or_insert(status);
                    }
                    Ok(_) => {}
                    Err(Error::Quarantined) => break,
                    Err(_) => {}
                }
                retired = retired.saturating_add(1);
            }
        }
        first_error
    }

    /// Retires every done request for callers that deliberately use a
    /// count-only fence.  Task polling uses the bounded variant above so an
    /// arbitrarily large caller budget cannot turn one invocation into an
    /// unbounded pending-slot walk.
    pub fn retire_completion_records(&mut self) -> Option<RespStatus> {
        self.retire_completion_records_bounded(self.pending.len())
    }

    fn assign_pending_token(&mut self, slot: usize, token: u16) -> Result<u64> {
        let token_idx = usize::from(token);
        let token_slot = self
            .token_slots
            .get_mut(token_idx)
            .ok_or(Error::WrongToken)?;
        if token_slot.is_some() {
            return Err(Error::WrongToken);
        }
        let cookie = self.next_cookie;
        if cookie == 0 || cookie > MAX_REQUEST_COOKIE {
            self.retired = true;
            return Err(Error::Quarantined);
        }
        self.next_cookie = self
            .next_cookie
            .checked_add(1)
            .unwrap_or(MAX_REQUEST_COOKIE.saturating_add(1));
        *token_slot = Some(slot);
        let request = self.pending[slot].as_mut().ok_or(Error::WrongToken)?;
        request.token = Some(token);
        request.completion_cookie = cookie;
        Ok(cookie)
    }

    fn pending_descriptor_cost(&self, data_segments: usize) -> usize {
        let full_chain = data_segments + 2;
        if self
            .negotiated_features
            .contains(BlkFeature::RING_INDIRECT_DESC)
            && full_chain > 1
        {
            1
        } else {
            full_chain
        }
    }

    fn pending_desc_in_use(&self) -> usize {
        if self
            .negotiated_features
            .contains(BlkFeature::RING_INDIRECT_DESC)
        {
            self.pending_count
        } else {
            QUEUE_SIZE as usize - self.queue.available_desc()
        }
    }

    fn add_pending_slot_unpublished(&mut self, slot: usize) -> Result<u16> {
        self.ensure_live()?;
        let request = self.pending[slot].as_mut().ok_or(Error::WrongToken)?;
        match &request.buffer {
            PendingBlkBuffer::Read { buf, len } => {
                // SAFETY: The caller promised the read buffer stays alive until the
                // pending handle completes.
                let data = unsafe { core::slice::from_raw_parts_mut(*buf, *len) };
                // SAFETY: The request header and response are owned by the pending
                // slot and stay stable until the handle is reaped.
                unsafe {
                    self.queue.add_unpublished(
                        &[request.req.as_bytes()],
                        &mut [data, request.resp.as_bytes_mut()],
                    )
                }
            }
            PendingBlkBuffer::Write { buf, len } => {
                // SAFETY: The caller promised the write buffer stays alive until the
                // pending handle completes.
                let data = unsafe { core::slice::from_raw_parts(*buf, *len) };
                // SAFETY: The request header and response are owned by the pending
                // slot and stay stable until the handle is reaped.
                unsafe {
                    self.queue.add_unpublished(
                        &[request.req.as_bytes(), data],
                        &mut [request.resp.as_bytes_mut()],
                    )
                }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::ReadVectored { bufs } => {
                let mut outputs = Vec::with_capacity(bufs.len() + 1);
                for segment in bufs {
                    // SAFETY: The caller promised all read buffers stay alive
                    // until the pending handle completes.
                    outputs
                        .push(unsafe { core::slice::from_raw_parts_mut(segment.buf, segment.len) });
                }
                outputs.push(request.resp.as_bytes_mut());
                // SAFETY: The request header and response are owned by the
                // pending slot and stay stable until the handle is reaped.
                unsafe {
                    self.queue
                        .add_unpublished(&[request.req.as_bytes()], outputs.as_mut_slice())
                }
            }
            #[cfg(feature = "alloc")]
            PendingBlkBuffer::WriteVectored { bufs } => {
                let mut inputs = Vec::with_capacity(bufs.len() + 1);
                inputs.push(request.req.as_bytes());
                for segment in bufs {
                    // SAFETY: The caller promised all write buffers stay alive
                    // until the pending handle completes.
                    inputs.push(unsafe { core::slice::from_raw_parts(segment.buf, segment.len) });
                }
                // SAFETY: The request header and response are owned by the
                // pending slot and stay stable until the handle is reaped.
                unsafe {
                    self.queue
                        .add_unpublished(inputs.as_slice(), &mut [request.resp.as_bytes_mut()])
                }
            }
            PendingBlkBuffer::PhysicalRead { buffers, count, .. } => {
                // SAFETY: The request owns the header/status storage and the
                // HAL mappings remain active until the matching completion.
                unsafe {
                    self.queue.add_unpublished_physical(
                        &[request.req.as_bytes()],
                        &[],
                        &buffers[..*count],
                        &mut [request.resp.as_bytes_mut()],
                    )
                }
            }
            PendingBlkBuffer::PhysicalWrite { buffers, count, .. } => {
                // SAFETY: The request owns the header/status storage and the
                // HAL mappings remain active until the matching completion.
                unsafe {
                    self.queue.add_unpublished_physical(
                        &[request.req.as_bytes()],
                        &buffers[..*count],
                        &[],
                        &mut [request.resp.as_bytes_mut()],
                    )
                }
            }
            PendingBlkBuffer::Flush => {
                // SAFETY: The request header and response are owned by the
                // pending slot and stay stable until the handle is reaped.
                unsafe {
                    self.queue.add_unpublished(
                        &[request.req.as_bytes()],
                        &mut [request.resp.as_bytes_mut()],
                    )
                }
            }
        }
    }

    fn rollback_unpublished_slot(&mut self, slot: usize, token: u16) -> Result {
        let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
        // SAFETY: callers invoke this only before publish_unpublished.  The
        // request owns the exact buffers/mappings used to install `token`.
        unsafe { request.discard_unpublished(&mut self.queue, token) };
        Ok(())
    }

    fn rollback_assigned_unpublished_slot(&mut self, slot: usize, token: u16) -> Result {
        if self.token_slots.get(usize::from(token)).copied() != Some(Some(slot)) {
            self.quarantined = true;
            return Err(Error::Quarantined);
        }
        self.rollback_unpublished_slot(slot, token)?;
        self.token_slots[usize::from(token)] = None;
        Ok(())
    }

    fn rollback_unpublished_batch(
        &mut self,
        requests: &mut [PendingBlkBatchRequest<'_>],
        slots: &[u16],
        tokens: &[u16],
        count: usize,
    ) -> Result {
        for index in (0..count).rev() {
            let slot = usize::from(slots[index]);
            let token = tokens[index];
            if self.token_slots.get(usize::from(token)).copied() != Some(Some(slot)) {
                self.quarantined = true;
                return Err(Error::Quarantined);
            }
            let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
            // SAFETY: batch publication has not started. The request owns the
            // exact buffers used by this unpublished descriptor chain.
            unsafe { request.discard_unpublished(&mut self.queue, token) };
            self.token_slots[usize::from(token)] = None;
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("batch rollback pending-count underflow");
            self.async_pending_count = self
                .async_pending_count
                .checked_sub(1)
                .expect("batch rollback async-count underflow");
            requests[index].handle = None;
        }
        Ok(())
    }

    /// Waits for a pending request handle and reaps its response.
    pub fn wait_pending_request(&mut self, handle: PendingBlkHandle) -> Result {
        self.ensure_live()?;
        let mut polls = 0u64;
        loop {
            // A handle waiter is an exact completion consumer. This path must
            // also work for physical requests; the count-only drain is
            // deliberately barred from claiming their used-ring entries.
            self.drain_pending_completions_for_handle(handle, PENDING_COMPLETION_DRAIN_BUDGET)?;
            if self.pending_request_done(handle) {
                self.record_external_queue_wait(polls, handle.notified());
                return self.complete_pending_request(handle);
            }
            polls = polls.saturating_add(1);
            spin_loop();
        }
    }

    fn configured_async_depth_cap(&self) -> usize {
        let configured = crate::stats::async_block_depth() as usize;
        if configured == 0 {
            usize::from(QUEUE_SIZE / 2)
        } else {
            configured
        }
        .clamp(1, QUEUE_SIZE as usize)
    }

    fn default_async_depth(&self) -> usize {
        crate::stats::async_block_effective_depth(self.configured_async_depth_cap())
            .clamp(1, QUEUE_SIZE as usize)
    }

    /// Gets the capacity of the block device, in 512 byte ([`SECTOR_SIZE`]) sectors.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Returns the default async depth cap for this architecture/device.
    pub fn async_default_depth(&self) -> usize {
        self.default_async_depth().clamp(1, QUEUE_SIZE as usize)
    }

    /// Returns the number of async-accounted requests currently in flight.
    pub fn async_pending_request_count(&self) -> usize {
        self.async_pending_count
    }

    /// Returns whether indirect descriptors were negotiated.
    pub fn supports_indirect_desc(&self) -> bool {
        self.negotiated_features
            .contains(BlkFeature::RING_INDIRECT_DESC)
    }

    /// Returns whether event-index notification suppression was negotiated.
    pub fn supports_event_idx(&self) -> bool {
        self.negotiated_features
            .contains(BlkFeature::RING_EVENT_IDX)
    }

    /// Returns whether cache flush requests were negotiated.
    pub fn supports_flush(&self) -> bool {
        self.negotiated_features.contains(BlkFeature::FLUSH)
    }

    /// Returns the current descriptor budget visible to async admission.
    pub fn async_descriptor_budget(&self) -> usize {
        self.queue.available_desc()
    }

    /// Returns true if the block device is read-only, or false if it allows writes.
    pub fn readonly(&self) -> bool {
        self.negotiated_features.contains(BlkFeature::RO)
    }

    /// Acknowledges a pending interrupt, if any.
    ///
    /// Returns true if there was an interrupt to acknowledge.
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }

    /// Enables interrupts from the device.
    pub fn enable_interrupts(&mut self) {
        self.queue.set_dev_notify(true);
    }

    /// Disables interrupts from the device.
    pub fn disable_interrupts(&mut self) {
        self.queue.set_dev_notify(false);
    }

    /// Sends the given request to the device and waits for a response, with no extra data.
    fn request(&mut self, request: BlkReq) -> Result {
        self.ensure_live()?;
        let mut resp = BlkResp::default();
        let used_len = self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [resp.as_bytes_mut()],
            &mut *self.transport,
        )?;
        Self::validate_sync_used_len(resp.status(), None, used_len)?;
        resp.status.into()
    }

    /// Sends the given request to the device and waits for a response, including the given data.
    fn request_read(&mut self, request: BlkReq, data: &mut [u8]) -> Result {
        self.ensure_live()?;
        let mut resp = BlkResp::default();
        let used_len = self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [data, resp.as_bytes_mut()],
            &mut *self.transport,
        )?;
        Self::validate_sync_used_len(resp.status(), Some(data.len()), used_len)?;
        resp.status.into()
    }

    #[cfg(feature = "alloc")]
    fn request_read_vectored(&mut self, request: BlkReq, data: &mut [&mut [u8]]) -> Result {
        self.ensure_live()?;
        let mut resp = BlkResp::default();
        let mut outputs = Vec::with_capacity(data.len() + 1);
        outputs.extend(data.iter_mut().map(|buf| &mut **buf));
        outputs.push(resp.as_bytes_mut());
        let used_len = self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            outputs.as_mut_slice(),
            &mut *self.transport,
        )?;
        let payload_len = data.iter().try_fold(0usize, |total, buf| {
            total.checked_add(buf.len()).ok_or(Error::InvalidParam)
        })?;
        Self::validate_sync_used_len(resp.status(), Some(payload_len), used_len)?;
        resp.status.into()
    }

    /// Sends the given request and data to the device and waits for a response.
    fn request_write(&mut self, request: BlkReq, data: &[u8]) -> Result {
        self.ensure_live()?;
        let mut resp = BlkResp::default();
        let used_len = self.queue.add_notify_wait_pop(
            &[request.as_bytes(), data],
            &mut [resp.as_bytes_mut()],
            &mut *self.transport,
        )?;
        Self::validate_sync_used_len(resp.status(), None, used_len)?;
        resp.status.into()
    }

    #[cfg(feature = "alloc")]
    fn request_write_vectored(&mut self, request: BlkReq, data: &[&[u8]]) -> Result {
        self.ensure_live()?;
        let mut resp = BlkResp::default();
        let mut inputs = Vec::with_capacity(data.len() + 1);
        inputs.push(request.as_bytes());
        inputs.extend(data.iter().copied());
        let used_len = self.queue.add_notify_wait_pop(
            inputs.as_slice(),
            &mut [resp.as_bytes_mut()],
            &mut *self.transport,
        )?;
        Self::validate_sync_used_len(resp.status(), None, used_len)?;
        resp.status.into()
    }

    /// Requests the device to flush any pending writes to storage.
    ///
    /// If `VIRTIO_BLK_F_FLUSH` was not offered, the VirtIO block contract
    /// treats the device as write-through (unless configurable writeback was
    /// negotiated), so no explicit flush command is required.
    pub fn flush(&mut self) -> Result {
        if self.negotiated_features.contains(BlkFeature::FLUSH) {
            record_blk_flush();
            self.request(BlkReq {
                type_: ReqType::Flush,
                ..Default::default()
            })
        } else {
            record_blk_flush_unsupported();
            Ok(())
        }
    }

    /// Gets the device ID.
    ///
    /// The ID is written as ASCII into the given buffer, which must be 20 bytes long, and the used
    /// length returned.
    pub fn device_id(&mut self, id: &mut [u8; 20]) -> Result<usize> {
        self.request_read(
            BlkReq {
                type_: ReqType::GetId,
                ..Default::default()
            },
            id,
        )?;

        let length = id.iter().position(|&x| x == 0).unwrap_or(20);
        Ok(length)
    }

    /// Reads one or more blocks into the given buffer.
    ///
    /// The buffer length must be a non-zero multiple of [`SECTOR_SIZE`].
    ///
    /// Blocks until the read completes or there is an error.
    pub fn read_blocks(&mut self, block_id: usize, buf: &mut [u8]) -> Result {
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        loop {
            match unsafe { self.submit_read_blocks_pending(block_id, buf) } {
                Ok(handle) => return self.wait_pending_request(handle),
                Err(Error::QueueFull) => {
                    self.drain_pending_completions()?;
                    spin_loop();
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Submits a read request and publishes it after recording pending metadata.
    ///
    /// The caller may drop the device lock after this returns, but must keep
    /// `buf` alive and unaccessed until the returned handle completes.
    /// Another waiter may call [`drain_pending_completions`](Self::drain_pending_completions)
    /// and complete this request.
    ///
    /// # Safety
    ///
    /// The caller must not access `buf` until the returned handle is complete.
    pub unsafe fn submit_read_blocks_pending(
        &mut self,
        block_id: usize,
        buf: &mut [u8],
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.drain_pending_completions()?;
        let slot = match self.alloc_pending_slot() {
            Ok(slot) => slot,
            Err(Error::QueueFull) => {
                record_blk_pending_queue_full();
                return Err(Error::QueueFull);
            }
            Err(err) => return Err(err),
        };
        self.pending[slot] = Some(PendingBlkRequest::read(block_id, buf));

        let add_result = self.add_pending_slot_unpublished(slot);
        let token = match add_result {
            Ok(token) => token,
            Err(Error::QueueFull) => {
                self.pending[slot] = None;
                record_blk_pending_queue_full();
                return Err(Error::QueueFull);
            }
            Err(err) => {
                self.pending[slot] = None;
                return Err(err);
            }
        };
        if let Err(error) = self.assign_pending_token(slot, token) {
            self.rollback_unpublished_slot(slot, token)?;
            return Err(error);
        }
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_read(buf.len(), 0);

        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        handle.notified = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        handle.token = token;
        Ok(handle)
    }

    /// Reads one or more blocks into a scatter list.
    ///
    /// The total data length and every non-empty segment length must be a multiple of
    /// [`SECTOR_SIZE`]. Blocks until the read completes or there is an error.
    #[cfg(feature = "alloc")]
    pub fn read_blocks_vectored(&mut self, block_id: usize, bufs: &mut [&mut [u8]]) -> Result {
        let total = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if total == 0 || total % SECTOR_SIZE != 0 {
            return Err(Error::InvalidParam);
        }
        if bufs
            .iter()
            .any(|buf| !buf.is_empty() && buf.len() % SECTOR_SIZE != 0)
        {
            return Err(Error::InvalidParam);
        }
        let segments = bufs.iter().filter(|buf| !buf.is_empty()).count();
        record_blk_read(total, segments);
        self.request_read_vectored(
            BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: block_id as u64,
            },
            bufs,
        )
    }

    fn validate_physical_segments(
        &self,
        block_id: u64,
        segments: &[PhysicalSegment],
        coalesced: &mut [PhysicalSegment; MAX_PHYSICAL_SG],
    ) -> Result<(usize, usize)> {
        if segments.is_empty() {
            return Err(Error::InvalidParam);
        }
        let mut total = 0usize;
        let mut count = 0usize;
        for segment in segments {
            if segment.paddr == 0
                || segment.paddr % SECTOR_SIZE != 0
                || segment.len == 0
                || segment.len % SECTOR_SIZE != 0
                || segment.paddr.checked_add(segment.len).is_none()
            {
                return Err(Error::InvalidParam);
            }
            total = total.checked_add(segment.len).ok_or(Error::InvalidParam)?;
            if count != 0
                && coalesced[count - 1]
                    .paddr
                    .checked_add(coalesced[count - 1].len)
                    == Some(segment.paddr)
            {
                coalesced[count - 1].len = coalesced[count - 1]
                    .len
                    .checked_add(segment.len)
                    .ok_or(Error::InvalidParam)?;
            } else {
                if count == MAX_PHYSICAL_SG {
                    return Err(Error::InvalidParam);
                }
                coalesced[count] = *segment;
                count = count.checked_add(1).expect("physical SG count overflow");
            }
        }
        let sectors = u64::try_from(total / SECTOR_SIZE).map_err(|_| Error::InvalidParam)?;
        let end = block_id.checked_add(sectors).ok_or(Error::InvalidParam)?;
        if end > self.capacity {
            return Err(Error::InvalidParam);
        }
        Ok((total, count))
    }

    fn map_physical_segments(
        &self,
        segments: &[PhysicalSegment],
        direction: BufferDirection,
    ) -> Result<(
        [Option<DmaMapping>; MAX_PHYSICAL_SG],
        [PhysicalBuffer; MAX_PHYSICAL_SG],
    )> {
        let mut mappings = [None; MAX_PHYSICAL_SG];
        let mut buffers = [PhysicalBuffer { addr: 0, len: 0 }; MAX_PHYSICAL_SG];
        for (index, segment) in segments.iter().enumerate() {
            let mapping = match unsafe { H::map_physical(segment.paddr, segment.len, direction) } {
                Ok(mapping) => mapping,
                Err(error) => {
                    Self::unmap_physical_mappings(&mappings, index, direction);
                    return Err(error);
                }
            };
            if mapping.source != segment.paddr || mapping.len != segment.len || mapping.device == 0
            {
                unsafe { H::unmap_physical(mapping, direction) };
                Self::unmap_physical_mappings(&mappings, index, direction);
                return Err(Error::DmaError);
            }
            buffers[index] = PhysicalBuffer {
                addr: mapping.device,
                len: mapping.len,
            };
            mappings[index] = Some(mapping);
        }
        Ok((mappings, buffers))
    }

    fn unmap_physical_mappings(
        mappings: &[Option<DmaMapping>; MAX_PHYSICAL_SG],
        count: usize,
        direction: BufferDirection,
    ) {
        for mapping in mappings[..count].iter().rev().flatten().copied() {
            unsafe { H::unmap_physical(mapping, direction) };
        }
    }

    fn install_prepared_physical_request(
        &mut self,
        slot: usize,
        request: &PendingBlkPhysicalBatchRequest<'_>,
        coalesced: &[PhysicalSegment],
        mappings: [Option<DmaMapping>; MAX_PHYSICAL_SG],
        buffers: [PhysicalBuffer; MAX_PHYSICAL_SG],
        bytes: usize,
    ) -> Result<u16> {
        let pending = match request.buffer {
            PendingBlkPhysicalBatchBuffer::Read(_) => PendingBlkRequest::physical_read(
                request.block_id,
                mappings,
                buffers,
                coalesced.len(),
                bytes,
            ),
            PendingBlkPhysicalBatchBuffer::Write(_) => PendingBlkRequest::physical_write(
                request.block_id,
                mappings,
                buffers,
                coalesced.len(),
                bytes,
            ),
        };
        self.pending[slot] = Some(pending);
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_physical_owner();
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_async_accounted();
        let token = match self.add_pending_slot_unpublished(slot) {
            Ok(token) => token,
            Err(error) => {
                let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
                request.unmap_physical::<H>();
                return Err(error);
            }
        };
        if let Err(error) = self.assign_pending_token(slot, token) {
            let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
            // SAFETY: publication has not happened, so the queue chain can be
            // discarded before releasing its physical mappings.
            unsafe { request.discard_unpublished_physical(&mut self.queue, token) };
            return Err(error);
        }
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        self.async_pending_count = self
            .async_pending_count
            .checked_add(1)
            .expect("async pending-count overflow");
        self.physical_pending_count = self
            .physical_pending_count
            .checked_add(1)
            .expect("physical pending-count overflow");
        Ok(token)
    }

    /// Prepares an all-or-nothing physical batch.  No descriptor is published
    /// until the returned [`PreparedBlockBatch`] is consumed by `publish`.
    /// Any validation, queue, slot, or DMA mapping failure leaves the device
    /// queue and every caller range untouched.
    pub unsafe fn prepare_physical_batch<'dev, 'req>(
        &'dev mut self,
        requests: &'req mut [PendingBlkPhysicalBatchRequest<'req>],
    ) -> Result<PreparedBlockBatch<'dev, 'req, H, T>> {
        // SAFETY: this preserves the historical standalone entry point. Its
        // caller owns the lower used-ring drain and may reap ordinary
        // completions before preparing a physical batch.
        unsafe { self.prepare_physical_batch_inner(requests, true) }
    }

    /// Prepares a physical batch without touching the used ring.
    ///
    /// A shared block wrapper uses this entry point after installing its
    /// device-global completion broker. The broker is then the sole task
    /// context allowed to consume used entries; a synchronous physical
    /// submitter may reserve/publish descriptors under the wrapper route lock
    /// but must not perform a hidden ordinary drain here. Admission remains
    /// all-or-nothing and no descriptor is visible until the returned batch is
    /// published.
    ///
    /// # Safety
    ///
    /// The caller must serialize this operation with the same device owner
    /// that drains the used ring and must keep every request range pinned until
    /// the returned batch is completed or reset proves quiescence.
    pub unsafe fn prepare_physical_batch_no_drain<'dev, 'req>(
        &'dev mut self,
        requests: &'req mut [PendingBlkPhysicalBatchRequest<'req>],
    ) -> Result<PreparedBlockBatch<'dev, 'req, H, T>> {
        // SAFETY: the caller supplies the single completion-owner guarantee
        // documented above; preparation itself is identical to the regular
        // all-or-nothing path except for the used-ring drain.
        unsafe { self.prepare_physical_batch_inner(requests, false) }
    }

    unsafe fn prepare_physical_batch_inner<'dev, 'req>(
        &'dev mut self,
        requests: &'req mut [PendingBlkPhysicalBatchRequest<'req>],
        drain_used_ring: bool,
    ) -> Result<PreparedBlockBatch<'dev, 'req, H, T>> {
        self.ensure_live()?;
        if requests.is_empty() || requests.len() > MAX_PHYSICAL_BATCH_REQUESTS {
            return Err(Error::InvalidParam);
        }
        for request in requests.iter_mut() {
            request.handle = None;
        }
        self.reject_untracked_queue_users()?;
        if drain_used_ring {
            self.drain_pending_completions()?;
        }

        let mut coalesced =
            [[PhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG]; MAX_PHYSICAL_BATCH_REQUESTS];
        let mut counts = [0usize; MAX_PHYSICAL_BATCH_REQUESTS];
        let mut bytes = [0usize; MAX_PHYSICAL_BATCH_REQUESTS];
        let mut descriptors = 0usize;
        let mut total_bytes = 0usize;
        for (index, request) in requests.iter().enumerate() {
            let (total, count) = self.validate_physical_segments(
                request.block_id,
                match request.buffer {
                    PendingBlkPhysicalBatchBuffer::Read(segments)
                    | PendingBlkPhysicalBatchBuffer::Write(segments) => segments,
                },
                &mut coalesced[index],
            )?;
            counts[index] = count;
            bytes[index] = total;
            total_bytes = total_bytes.checked_add(total).ok_or(Error::InvalidParam)?;
            descriptors = descriptors
                .checked_add(
                    if self
                        .negotiated_features
                        .contains(BlkFeature::RING_INDIRECT_DESC)
                    {
                        1
                    } else {
                        count + 2
                    },
                )
                .ok_or(Error::QueueFull)?;
        }
        if descriptors > self.queue.available_desc()
            || requests.len() > self.pending.iter().filter(|entry| entry.is_none()).count()
        {
            return Err(Error::QueueFull);
        }

        let mut prepared = PreparedBlockBatch {
            device: self,
            requests: requests.as_mut_ptr(),
            request_count: requests.len(),
            prepared_count: 0,
            total_bytes,
            heads: [0; MAX_PHYSICAL_BATCH_REQUESTS],
            slots: [0; MAX_PHYSICAL_BATCH_REQUESTS],
            tokens: [0; MAX_PHYSICAL_BATCH_REQUESTS],
            published: false,
            _request_lifetime: core::marker::PhantomData,
        };

        for index in 0..prepared.request_count {
            let request = unsafe { &*prepared.requests.add(index) };
            let direction = match request.buffer {
                PendingBlkPhysicalBatchBuffer::Read(_) => BufferDirection::DeviceToDriver,
                PendingBlkPhysicalBatchBuffer::Write(_) => BufferDirection::DriverToDevice,
            };
            let (mappings, buffers) = match prepared
                .device
                .map_physical_segments(&coalesced[index][..counts[index]], direction)
            {
                Ok(value) => value,
                Err(error) => return Err(error),
            };
            let slot = match prepared.device.alloc_pending_slot() {
                Ok(slot) => slot,
                Err(error) => {
                    Self::unmap_physical_mappings(&mappings, counts[index], direction);
                    return Err(error);
                }
            };
            let token = match prepared.device.install_prepared_physical_request(
                slot,
                request,
                &coalesced[index][..counts[index]],
                mappings,
                buffers,
                bytes[index],
            ) {
                Ok(token) => token,
                Err(error) => return Err(error),
            };
            prepared.slots[index] = slot as u16;
            prepared.tokens[index] = token;
            prepared.heads[index] = token;
            prepared.prepared_count = prepared
                .prepared_count
                .checked_add(1)
                .expect("prepared batch count overflow");
        }

        Ok(prepared)
    }
}

impl<'dev, 'req, H: Hal, T: Transport> PreparedBlockBatch<'dev, 'req, H, T> {
    /// Publishes every prepared descriptor.  Preparation has already checked
    /// all fallible conditions, so publication consumes the token and cannot
    /// return an admission error.
    pub fn publish(self) -> PendingBlkBatchReport {
        self.publish_with_handles().0
    }

    /// Publishes and returns the concrete handles without borrowing the
    /// caller's prepared request vector after publication.  This is useful to
    /// adapters that must copy handles across a shared-block trait boundary.
    pub fn publish_with_handles(
        mut self,
    ) -> (
        PendingBlkBatchReport,
        [PendingBlkHandle; MAX_PHYSICAL_BATCH_REQUESTS],
        usize,
    ) {
        let mut report = PendingBlkBatchReport::default();
        let mut handles = [PendingBlkHandle {
            generation: 0,
            cookie: 0,
            slot: 0,
            token: 0,
            notified: false,
        }; MAX_PHYSICAL_BATCH_REQUESTS];
        report.submitted = self.request_count;
        report.bytes = self.total_bytes;
        for index in 0..self.request_count {
            let notified_handle = self
                .device
                .pending_handle(usize::from(self.slots[index]), false)
                .expect("prepared request lost pending handle");
            handles[index] = notified_handle;
            // SAFETY: the request slice remains exclusively borrowed for the
            // lifetime of this prepared batch.
            unsafe {
                (*self.requests.add(index)).handle = Some(notified_handle);
            }
        }
        self.published = true;
        for index in 0..self.request_count {
            self.device.queue.publish_unpublished(self.heads[index]);
        }
        report.notified = self.device.queue.should_notify();
        for index in 0..self.request_count {
            self.device.notified_slots[usize::from(self.slots[index])] = report.notified;
        }
        if report.notified {
            self.device.transport.notify(QUEUE);
            for index in 0..self.request_count {
                handles[index].notified = true;
                // SAFETY: see the handle write above.
                unsafe {
                    (*self.requests.add(index))
                        .handle
                        .as_mut()
                        .expect("prepared publish lost request handle")
                        .notified = true;
                }
            }
        }
        (report, handles, self.request_count)
    }
}

impl<'dev, 'req, H: Hal, T: Transport> Drop for PreparedBlockBatch<'dev, 'req, H, T> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        for index in (0..self.prepared_count).rev() {
            let slot = usize::from(self.slots[index]);
            let token = self.tokens[index];
            if let Some(request) = self.device.pending[slot].take() {
                // SAFETY: no descriptor in this prepared batch was published;
                // the queue owns exactly the chain represented by `token`.
                unsafe {
                    request.discard_unpublished_physical(&mut self.device.queue, token);
                }
                self.device.pending_count = self
                    .device
                    .pending_count
                    .checked_sub(1)
                    .expect("prepared batch pending-count underflow");
                self.device.async_pending_count = self
                    .device
                    .async_pending_count
                    .checked_sub(1)
                    .expect("prepared batch async-count underflow");
                self.device.physical_pending_count = self
                    .device
                    .physical_pending_count
                    .checked_sub(1)
                    .expect("prepared batch physical-count underflow");
            }
            self.device.token_slots[usize::from(token)] = None;
            // SAFETY: see the handle write in `publish`; preparation itself
            // leaves request handles empty, but clear a caller-visible stale
            // value defensively if one was supplied.
            unsafe {
                (*self.requests.add(index)).handle = None;
            }
        }
    }
}

impl<H: Hal, T: Transport> VirtIOBlk<H, T> {
    fn tracked_pending_descriptor_count(&self) -> usize {
        self.pending
            .iter()
            .filter_map(Option::as_ref)
            .filter(|entry| !entry.done)
            .map(|entry| {
                entry.descriptor_cost(
                    self.negotiated_features
                        .contains(BlkFeature::RING_INDIRECT_DESC),
                )
            })
            .sum()
    }

    fn reject_untracked_queue_users(&self) -> Result {
        // The public *_nb APIs have no pending metadata, so their tokens
        // cannot safely be reaped by the bounded pending drain. Refuse to
        // publish a physical request while such a chain is outstanding; all
        // requests tracked by this driver remain eligible and are drained in
        // used-ring order.
        if self.queue.outstanding_descriptor_count() != self.tracked_pending_descriptor_count() {
            return Err(Error::QueueFull);
        }
        Ok(())
    }

    /// Submits a read into pinned physical SG payload buffers.
    ///
    /// The returned handle is completed through the normal pending/token-slot
    /// path. The payload is mapped by the HAL using physical addresses; no
    /// Rust slice is constructed from those addresses.
    ///
    /// # Safety
    ///
    /// The caller must keep every segment pinned and valid until the returned
    /// handle is completed and must provide ranges in the device-write
    /// direction of a read. Concurrent CPU/device access races on contents.
    pub unsafe fn submit_read_blocks_physical_pending(
        &mut self,
        block_id: u64,
        segments: &[PhysicalSegment],
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        self.reject_untracked_queue_users()?;
        self.drain_pending_completions()?;
        let mut coalesced = [PhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        let (bytes, coalesced_count) =
            self.validate_physical_segments(block_id, segments, &mut coalesced)?;
        let (mappings, buffers) = self.map_physical_segments(
            &coalesced[..coalesced_count],
            BufferDirection::DeviceToDriver,
        )?;
        let slot = match self.alloc_pending_slot() {
            Ok(slot) => slot,
            Err(error) => {
                Self::unmap_physical_mappings(
                    &mappings,
                    coalesced_count,
                    BufferDirection::DeviceToDriver,
                );
                return Err(error);
            }
        };
        self.pending[slot] = Some(PendingBlkRequest::physical_read(
            block_id,
            mappings,
            buffers,
            coalesced_count,
            bytes,
        ));
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_physical_owner();
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_async_accounted();
        let token = match self.add_pending_slot_unpublished(slot) {
            Ok(token) => token,
            Err(error) => {
                let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
                request.unmap_physical::<H>();
                return Err(error);
            }
        };
        if let Err(error) = self.assign_pending_token(slot, token) {
            let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
            // SAFETY: publication has not happened, so the queue chain can
            // be discarded before releasing its physical mappings.
            unsafe { request.discard_unpublished_physical(&mut self.queue, token) };
            return Err(error);
        }
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        self.physical_pending_count = self
            .physical_pending_count
            .checked_add(1)
            .expect("physical pending-count overflow");
        self.async_pending_count = self
            .async_pending_count
            .checked_add(1)
            .expect("async pending-count overflow");
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_read(bytes, coalesced_count);
        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        handle.notified = notified;
        handle.token = token;
        Ok(handle)
    }

    /// Submits a write from pinned physical SG payload buffers.
    ///
    /// The returned handle is completed through the normal pending/token-slot
    /// path. The payload is mapped by the HAL using physical addresses; no
    /// Rust slice is constructed from those addresses.
    ///
    /// # Safety
    ///
    /// The caller must keep every segment pinned and valid until the returned
    /// handle is completed and must provide ranges in the device-read direction
    /// of a write. Concurrent CPU/device access races on contents.
    pub unsafe fn submit_write_blocks_physical_pending(
        &mut self,
        block_id: u64,
        segments: &[PhysicalSegment],
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        self.reject_untracked_queue_users()?;
        self.drain_pending_completions()?;
        let mut coalesced = [PhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        let (bytes, coalesced_count) =
            self.validate_physical_segments(block_id, segments, &mut coalesced)?;
        let (mappings, buffers) = self.map_physical_segments(
            &coalesced[..coalesced_count],
            BufferDirection::DriverToDevice,
        )?;
        let slot = match self.alloc_pending_slot() {
            Ok(slot) => slot,
            Err(error) => {
                Self::unmap_physical_mappings(
                    &mappings,
                    coalesced_count,
                    BufferDirection::DriverToDevice,
                );
                return Err(error);
            }
        };
        self.pending[slot] = Some(PendingBlkRequest::physical_write(
            block_id,
            mappings,
            buffers,
            coalesced_count,
            bytes,
        ));
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_physical_owner();
        self.pending[slot]
            .as_mut()
            .ok_or(Error::WrongToken)?
            .mark_async_accounted();
        let token = match self.add_pending_slot_unpublished(slot) {
            Ok(token) => token,
            Err(error) => {
                let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
                request.unmap_physical::<H>();
                return Err(error);
            }
        };
        if let Err(error) = self.assign_pending_token(slot, token) {
            let request = self.pending[slot].take().ok_or(Error::WrongToken)?;
            // SAFETY: publication has not happened, so the queue chain can
            // be discarded before releasing its physical mappings.
            unsafe { request.discard_unpublished_physical(&mut self.queue, token) };
            return Err(error);
        }
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        self.physical_pending_count = self
            .physical_pending_count
            .checked_add(1)
            .expect("physical pending-count overflow");
        self.async_pending_count = self
            .async_pending_count
            .checked_add(1)
            .expect("async pending-count overflow");
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_write(bytes, coalesced_count);
        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        handle.notified = notified;
        handle.token = token;
        Ok(handle)
    }

    /// Submits a request to read one or more blocks, but returns immediately without waiting for
    /// the read to complete.
    ///
    /// # Arguments
    ///
    /// * `block_id` - The identifier of the first block to read.
    /// * `req` - A buffer which the driver can use for the request to send to the device. The
    ///   contents don't matter as `read_blocks_nb` will initialise it, but like the other buffers
    ///   it needs to be valid (and not otherwise used) until the corresponding
    ///   `complete_read_blocks` call. Its length must be a non-zero multiple of [`SECTOR_SIZE`].
    /// * `buf` - The buffer in memory into which the block should be read.
    /// * `resp` - A mutable reference to a variable provided by the caller
    ///   to contain the status of the request. The caller can safely
    ///   read the variable only after the request is complete.
    ///
    /// # Usage
    ///
    /// It will submit the request to the VirtIO block device and return a
    /// generation-safe handle identifying its completion. If there are not
    /// enough descriptors to allocate, then it returns [`Error::QueueFull`].
    ///
    /// The caller can then call [`Self::pending_request_done`] with the returned
    /// handle to check whether the device has finished handling the request.
    /// Once it has, the caller must call `complete_read_blocks` with the same
    /// buffers before reading the response.
    ///
    /// ```
    /// # use thekernel_virtio_drivers::{Error, Hal};
    /// # use thekernel_virtio_drivers::device::blk::VirtIOBlk;
    /// # use thekernel_virtio_drivers::transport::Transport;
    /// use thekernel_virtio_drivers::device::blk::{BlkReq, BlkResp, RespStatus};
    ///
    /// # fn example<H: Hal, T: Transport>(blk: &mut VirtIOBlk<H, T>) -> Result<(), Error> {
    /// let mut request = BlkReq::default();
    /// let mut buffer = [0; 512];
    /// let mut response = BlkResp::default();
    /// let handle = unsafe { blk.read_blocks_nb(42, &mut request, &mut buffer, &mut response) }?;
    ///
    /// // Wait for an interrupt to tell us that the request completed...
    /// assert!(!blk.pending_request_done(handle));
    ///
    /// unsafe {
    ///     blk.complete_read_blocks(handle, &request, &mut buffer, &mut response)?;
    /// }
    /// if response.status() == RespStatus::OK {
    ///     println!("Successfully read block.");
    /// } else {
    ///     println!("Error {:?} reading block.", response.status());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Safety
    ///
    /// `req`, `buf` and `resp` are still borrowed by the underlying VirtIO block device even after
    /// this method returns. Thus, it is the caller's responsibility to guarantee that they are not
    /// accessed before the request is completed in order to avoid data races.
    pub unsafe fn read_blocks_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buf: &mut [u8],
        resp: &mut BlkResp,
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.drain_pending_completions()?;
        let slot = self.alloc_pending_slot()?;
        record_blk_read(buf.len(), 0);
        *req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        self.pending[slot] = Some(PendingBlkRequest::legacy_read(
            block_id,
            req as *const BlkReq,
            buf,
            resp as *mut BlkResp,
        ));
        let token = match self.add_pending_slot_unpublished(slot) {
            Ok(token) => token,
            Err(error) => {
                self.pending[slot] = None;
                return Err(error);
            }
        };
        let cookie = match self.assign_pending_token(slot, token) {
            Ok(cookie) => cookie,
            Err(error) => {
                self.rollback_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        // Keep the full-width identity in the caller-owned request object as
        // a second binding check.  It is not the request header published to
        // the device; the returned handle is the authoritative completion
        // identity and rejects a stale token after variables are reused.
        req.sector = cookie;
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        self.queue.publish_unpublished(token);
        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        record_blk_pending_depth(self.pending_count);
        handle.notified = notified;
        handle.token = token;
        Ok(handle)
    }

    /// Completes a read operation which was started by `read_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers must be passed in again as were passed to `read_blocks_nb` when it returned
    /// the token.
    pub unsafe fn complete_read_blocks(
        &mut self,
        handle: PendingBlkHandle,
        req: &BlkReq,
        buf: &mut [u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        self.ensure_live()?;
        if handle.generation != 0 && handle.generation != self.generation {
            return Err(Error::WrongToken);
        }
        let slot = self
            .find_pending_cookie(handle.completion_cookie())
            .ok_or(Error::WrongToken)?;
        if handle.generation != 0
            && (handle.slot != slot as u16
                || self.pending[slot].as_ref().and_then(|entry| entry.token) != Some(handle.token))
        {
            return Err(Error::WrongToken);
        }
        let done = self.pending[slot].as_ref().is_some_and(|entry| {
            entry.completion_owner == BlockCompletionOwner::Legacy
                && entry.legacy_matches(
                    req as *const BlkReq,
                    buf.as_ptr(),
                    buf.len(),
                    resp as *mut BlkResp,
                    true,
                )
                && entry.done
        });
        let matches = self.pending[slot].as_ref().is_some_and(|entry| {
            entry.completion_owner == BlockCompletionOwner::Legacy
                && entry.legacy_matches(
                    req as *const BlkReq,
                    buf.as_ptr(),
                    buf.len(),
                    resp as *mut BlkResp,
                    true,
                )
        });
        if !matches {
            return Err(Error::WrongToken);
        }
        if !done {
            let entry = self.pending[slot].as_mut().ok_or(Error::WrongToken)?;
            // SAFETY: the caller supplied the exact legacy buffers and the
            // used entry must still be the FIFO head for this token.
            let token = entry.token.ok_or(Error::WrongToken)?;
            let complete = unsafe { entry.complete(&mut self.queue, token) };
            if matches!(complete, Err(Error::WrongToken | Error::Quarantined)) {
                self.quarantined = true;
                return Err(Error::Quarantined);
            }
            complete?;
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("completion pending-count underflow");
        }
        let status = self.pending[slot]
            .as_ref()
            .ok_or(Error::WrongToken)?
            .resp
            .status();
        let _ = self.retire_pending_slot(slot)?;
        status.into()
    }

    /// Writes the contents of the given buffer to a block or blocks.
    ///
    /// The buffer length must be a non-zero multiple of [`SECTOR_SIZE`].
    ///
    /// Blocks until the write is complete or there is an error.
    pub fn write_blocks(&mut self, block_id: usize, buf: &[u8]) -> Result {
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        loop {
            match unsafe { self.submit_write_blocks_pending(block_id, buf) } {
                Ok(handle) => return self.wait_pending_request(handle),
                Err(Error::QueueFull) => {
                    self.drain_pending_completions()?;
                    spin_loop();
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Submits a write request and publishes it after recording pending metadata.
    ///
    /// See [`submit_read_blocks_pending`](Self::submit_read_blocks_pending) for
    /// the completion and lifetime contract.
    ///
    /// # Safety
    ///
    /// The caller must not access `buf` until the returned handle is complete.
    pub unsafe fn submit_write_blocks_pending(
        &mut self,
        block_id: usize,
        buf: &[u8],
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.drain_pending_completions()?;
        let slot = match self.alloc_pending_slot() {
            Ok(slot) => slot,
            Err(Error::QueueFull) => {
                record_blk_pending_queue_full();
                return Err(Error::QueueFull);
            }
            Err(err) => return Err(err),
        };
        self.pending[slot] = Some(PendingBlkRequest::write(block_id, buf));

        let add_result = self.add_pending_slot_unpublished(slot);
        let token = match add_result {
            Ok(token) => token,
            Err(Error::QueueFull) => {
                self.pending[slot] = None;
                record_blk_pending_queue_full();
                return Err(Error::QueueFull);
            }
            Err(err) => {
                self.pending[slot] = None;
                return Err(err);
            }
        };
        if let Err(error) = self.assign_pending_token(slot, token) {
            self.rollback_unpublished_slot(slot, token)?;
            return Err(error);
        }
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_write(buf.len(), 0);

        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        handle.notified = notified;
        handle.token = token;
        Ok(handle)
    }

    /// Submits as many pending read/write requests as descriptor and slot
    /// budgets allow, publishes accepted descriptors together, and notifies the
    /// device at most once.
    ///
    /// This is the first descriptor-aware async block queue entry point. It
    /// intentionally accepts only one contiguous data segment per request;
    /// scatter-gather callers should use the synchronous vectored path until the
    /// later SG consumer phase extends owned request guards.
    ///
    /// # Safety
    ///
    /// The caller must keep all accepted request buffers valid and unaccessed
    /// until their returned handles complete.
    pub unsafe fn submit_pending_batch(
        &mut self,
        requests: &mut [PendingBlkBatchRequest<'_>],
    ) -> Result<PendingBlkBatchReport> {
        // SAFETY: this is the historical standalone entry point. Its caller
        // owns the lower used-ring consumer and may reap ordinary entries
        // before admitting another batch.
        unsafe { self.submit_pending_batch_inner(requests, true) }
    }

    /// Submits an ordinary batch without touching the used ring.
    ///
    /// A shared block wrapper uses this entry point after installing its
    /// device-global completion broker. The broker is then the sole task
    /// context allowed to consume used entries; ordinary submission may
    /// reserve/publish descriptors but must not perform a hidden drain that
    /// bypasses the shared completion mailbox.
    ///
    /// # Safety
    ///
    /// The caller must serialize this operation with the same completion
    /// owner that drains the used ring and must keep every accepted request
    /// buffer valid until its concrete completion is retired.
    pub unsafe fn submit_pending_batch_no_drain(
        &mut self,
        requests: &mut [PendingBlkBatchRequest<'_>],
    ) -> Result<PendingBlkBatchReport> {
        // SAFETY: the caller supplies the single completion-owner guarantee
        // documented above; admission/publication is otherwise identical to
        // the historical batch path.
        unsafe { self.submit_pending_batch_inner(requests, false) }
    }

    unsafe fn submit_pending_batch_inner(
        &mut self,
        requests: &mut [PendingBlkBatchRequest<'_>],
        drain_used_ring: bool,
    ) -> Result<PendingBlkBatchReport> {
        self.ensure_live()?;
        let mut report = PendingBlkBatchReport::default();
        if requests.is_empty() {
            return Ok(report);
        }
        for request in requests.iter_mut() {
            request.handle = None;
            let len = request.buffer.len();
            let segments = request.buffer.segment_count();
            if matches!(&request.buffer, PendingBlkBatchBuffer::Flush) {
                if request.block_id != 0 {
                    return Err(Error::InvalidParam);
                }
                if !self.negotiated_features.contains(BlkFeature::FLUSH) {
                    record_blk_flush_unsupported();
                    return Err(Error::Unsupported);
                }
                continue;
            }
            if len == 0 || segments == 0 || len % SECTOR_SIZE != 0 {
                return Err(Error::InvalidParam);
            }
            #[cfg(feature = "alloc")]
            if matches!(
                &request.buffer,
                PendingBlkBatchBuffer::ReadVectored(bufs)
                    if bufs.iter().any(|buf| buf.is_empty())
            ) || matches!(
                &request.buffer,
                PendingBlkBatchBuffer::WriteVectored(bufs)
                    if bufs.iter().any(|buf| buf.is_empty())
            ) {
                return Err(Error::InvalidParam);
            }
        }

        if drain_used_ring {
            self.drain_pending_completions()?;
        }

        let depth_cap = self.async_default_depth();
        let depth_available = depth_cap.saturating_sub(self.async_pending_count);
        if depth_available == 0 {
            record_blk_async_admission_stall();
            record_blk_async_queue_full();
            report.queue_full = true;
            return Ok(report);
        }

        let desc_budget = self.queue.available_desc();
        let mut accepted_heads = [0u16; QUEUE_SIZE as usize];
        let mut accepted_slots = [0u16; QUEUE_SIZE as usize];

        for request in requests.iter_mut() {
            if report.submitted >= depth_available {
                report.queue_full = true;
                record_blk_async_admission_stall();
                break;
            }

            let segments = request.buffer.segment_count();
            let descriptor_cost = self.pending_descriptor_cost(segments);
            if self.queue.available_desc() < descriptor_cost {
                report.queue_full = true;
                record_blk_async_admission_stall();
                record_blk_async_queue_full();
                break;
            }

            let slot = match self.alloc_pending_slot() {
                Ok(slot) => slot,
                Err(Error::QueueFull) => {
                    report.queue_full = true;
                    record_blk_async_admission_stall();
                    record_blk_async_queue_full();
                    break;
                }
                Err(err) => return Err(err),
            };

            let bytes = request.buffer.len();
            let pending = match &mut request.buffer {
                PendingBlkBatchBuffer::Read(buf) => PendingBlkRequest {
                    req: BlkReq {
                        type_: ReqType::In,
                        reserved: 0,
                        sector: request.block_id as u64,
                    },
                    resp: BlkResp::default(),
                    buffer: PendingBlkBuffer::Read {
                        buf: buf.as_mut_ptr(),
                        len: buf.len(),
                    },
                    token: None,
                    bytes,
                    done: false,
                    async_accounted: false,
                    completion_cookie: 0,
                    completion_owner: BlockCompletionOwner::Ordinary,
                    legacy_resp: None,
                    legacy_req: None,
                    completion_bytes: 0,
                    completion_claimed: false,
                },
                PendingBlkBatchBuffer::Write(buf) => PendingBlkRequest {
                    req: BlkReq {
                        type_: ReqType::Out,
                        reserved: 0,
                        sector: request.block_id as u64,
                    },
                    resp: BlkResp::default(),
                    buffer: PendingBlkBuffer::Write {
                        buf: buf.as_ptr(),
                        len: buf.len(),
                    },
                    token: None,
                    bytes,
                    done: false,
                    async_accounted: false,
                    completion_cookie: 0,
                    completion_owner: BlockCompletionOwner::Ordinary,
                    legacy_resp: None,
                    legacy_req: None,
                    completion_bytes: 0,
                    completion_claimed: false,
                },
                #[cfg(feature = "alloc")]
                PendingBlkBatchBuffer::ReadVectored(bufs) => {
                    PendingBlkRequest::read_vectored(request.block_id, bufs.as_mut_slice())
                }
                #[cfg(feature = "alloc")]
                PendingBlkBatchBuffer::WriteVectored(bufs) => {
                    PendingBlkRequest::write_vectored(request.block_id, bufs.as_slice())
                }
                PendingBlkBatchBuffer::Flush => PendingBlkRequest::flush(),
            };
            self.pending[slot] = Some(pending);
            let Some(entry) = self.pending[slot].as_mut() else {
                self.rollback_unpublished_batch(
                    requests,
                    &accepted_slots,
                    &accepted_heads,
                    report.submitted,
                )?;
                return Err(Error::WrongToken);
            };
            entry.mark_async_accounted();

            let token = match self.add_pending_slot_unpublished(slot) {
                Ok(token) => token,
                Err(Error::QueueFull) => {
                    self.pending[slot] = None;
                    report.queue_full = true;
                    record_blk_async_admission_stall();
                    record_blk_async_queue_full();
                    break;
                }
                Err(err) => {
                    self.pending[slot] = None;
                    self.rollback_unpublished_batch(
                        requests,
                        &accepted_slots,
                        &accepted_heads,
                        report.submitted,
                    )?;
                    return Err(err);
                }
            };

            if let Err(error) = self.assign_pending_token(slot, token) {
                self.rollback_unpublished_slot(slot, token)?;
                self.rollback_unpublished_batch(
                    requests,
                    &accepted_slots,
                    &accepted_heads,
                    report.submitted,
                )?;
                return Err(error);
            }
            let handle = match self.pending_handle(slot, false) {
                Ok(handle) => handle,
                Err(error) => {
                    self.rollback_assigned_unpublished_slot(slot, token)?;
                    self.rollback_unpublished_batch(
                        requests,
                        &accepted_slots,
                        &accepted_heads,
                        report.submitted,
                    )?;
                    return Err(error);
                }
            };
            self.pending_count = self
                .pending_count
                .checked_add(1)
                .expect("pending-count overflow");
            self.async_pending_count = self
                .async_pending_count
                .checked_add(1)
                .expect("async pending-count overflow");
            record_blk_pending_depth(self.pending_count);
            accepted_heads[report.submitted] = token;
            accepted_slots[report.submitted] = slot as u16;

            match &request.buffer {
                PendingBlkBatchBuffer::Read(_) => record_blk_read(bytes, 0),
                PendingBlkBatchBuffer::Write(_) => record_blk_write(bytes, 0),
                #[cfg(feature = "alloc")]
                PendingBlkBatchBuffer::ReadVectored(_) => record_blk_read(bytes, segments),
                #[cfg(feature = "alloc")]
                PendingBlkBatchBuffer::WriteVectored(_) => record_blk_write(bytes, segments),
                PendingBlkBatchBuffer::Flush => {
                    record_blk_flush();
                    record_blk_async_flush_request();
                }
            }
            request.handle = Some(handle);
            report.submitted = report
                .submitted
                .checked_add(1)
                .expect("batch submitted-count overflow");
            report.bytes = report
                .bytes
                .checked_add(bytes)
                .expect("batch byte-count overflow");
        }

        for head in accepted_heads.iter().copied().take(report.submitted) {
            self.queue.publish_unpublished(head);
        }

        if report.submitted != 0 {
            report.notified = self.queue.should_notify();
            if report.notified {
                self.transport.notify(QUEUE);
            }
            for slot in accepted_slots.iter().copied().take(report.submitted) {
                self.notified_slots[usize::from(slot)] = report.notified;
            }
            if report.notified {
                for request in requests.iter_mut().take(report.submitted) {
                    if let Some(handle) = request.handle.as_mut() {
                        handle.notified = true;
                    }
                }
            }
            record_blk_async_submit_batch(
                report.submitted,
                report.bytes,
                report.queue_full || report.submitted < requests.len(),
                self.async_pending_count,
                self.pending_desc_in_use(),
                desc_budget,
                report.notified,
            );
        }

        Ok(report)
    }

    /// Writes one or more blocks from a scatter list.
    ///
    /// The total data length and every non-empty segment length must be a multiple of
    /// [`SECTOR_SIZE`]. Blocks until the write completes or there is an error.
    #[cfg(feature = "alloc")]
    pub fn write_blocks_vectored(&mut self, block_id: usize, bufs: &[&[u8]]) -> Result {
        let total = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if total == 0 || total % SECTOR_SIZE != 0 {
            return Err(Error::InvalidParam);
        }
        if bufs
            .iter()
            .any(|buf| !buf.is_empty() && buf.len() % SECTOR_SIZE != 0)
        {
            return Err(Error::InvalidParam);
        }
        let segments = bufs.iter().filter(|buf| !buf.is_empty()).count();
        record_blk_write(total, segments);
        self.request_write_vectored(
            BlkReq {
                type_: ReqType::Out,
                sector: block_id as u64,
                ..Default::default()
            },
            bufs,
        )
    }

    /// Submits a request to write one or more blocks, but returns immediately without waiting for
    /// the write to complete.
    ///
    /// # Arguments
    ///
    /// * `block_id` - The identifier of the first block to write.
    /// * `req` - A buffer which the driver can use for the request to send to the device. The
    ///   contents don't matter as `read_blocks_nb` will initialise it, but like the other buffers
    ///   it needs to be valid (and not otherwise used) until the corresponding
    ///   `complete_write_blocks` call.
    /// * `buf` - The buffer in memory containing the data to write to the blocks. Its length must
    ///   be a non-zero multiple of [`SECTOR_SIZE`].
    /// * `resp` - A mutable reference to a variable provided by the caller
    ///   to contain the status of the request. The caller can safely
    ///   read the variable only after the request is complete.
    ///
    /// # Usage
    ///
    /// See [VirtIOBlk::read_blocks_nb].
    ///
    /// # Safety
    ///
    /// See  [VirtIOBlk::read_blocks_nb].
    pub unsafe fn write_blocks_nb(
        &mut self,
        block_id: usize,
        req: &mut BlkReq,
        buf: &[u8],
        resp: &mut BlkResp,
    ) -> Result<PendingBlkHandle> {
        self.ensure_live()?;
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        self.drain_pending_completions()?;
        let slot = self.alloc_pending_slot()?;
        record_blk_write(buf.len(), 0);
        *req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        self.pending[slot] = Some(PendingBlkRequest::legacy_write(
            block_id,
            req as *const BlkReq,
            buf,
            resp as *mut BlkResp,
        ));
        let token = match self.add_pending_slot_unpublished(slot) {
            Ok(token) => token,
            Err(error) => {
                self.pending[slot] = None;
                return Err(error);
            }
        };
        let cookie = match self.assign_pending_token(slot, token) {
            Ok(cookie) => cookie,
            Err(error) => {
                self.rollback_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        req.sector = cookie;
        let mut handle = match self.pending_handle(slot, false) {
            Ok(handle) => handle,
            Err(error) => {
                self.rollback_assigned_unpublished_slot(slot, token)?;
                return Err(error);
            }
        };
        self.pending_count = self
            .pending_count
            .checked_add(1)
            .expect("pending-count overflow");
        self.queue.publish_unpublished(token);
        let notified = self.queue.should_notify();
        self.notified_slots[slot] = notified;
        if notified {
            self.transport.notify(QUEUE);
        }
        record_blk_pending_depth(self.pending_count);
        handle.notified = notified;
        handle.token = token;
        Ok(handle)
    }

    /// Completes a write operation which was started by `write_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers must be passed in again as were passed to `write_blocks_nb` when it
    /// returned the token.
    pub unsafe fn complete_write_blocks(
        &mut self,
        handle: PendingBlkHandle,
        req: &BlkReq,
        buf: &[u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        self.ensure_live()?;
        if handle.generation != 0 && handle.generation != self.generation {
            return Err(Error::WrongToken);
        }
        let slot = self
            .find_pending_cookie(handle.completion_cookie())
            .ok_or(Error::WrongToken)?;
        if handle.generation != 0
            && (handle.slot != slot as u16
                || self.pending[slot].as_ref().and_then(|entry| entry.token) != Some(handle.token))
        {
            return Err(Error::WrongToken);
        }
        let matches = self.pending[slot].as_ref().is_some_and(|entry| {
            entry.completion_owner == BlockCompletionOwner::Ordinary
                && entry.legacy_matches(
                    req as *const BlkReq,
                    buf.as_ptr(),
                    buf.len(),
                    resp as *mut BlkResp,
                    false,
                )
        });
        if !matches {
            return Err(Error::WrongToken);
        }
        if !self.pending[slot].as_ref().is_some_and(|entry| entry.done) {
            let entry = self.pending[slot].as_mut().ok_or(Error::WrongToken)?;
            // SAFETY: see complete_read_blocks.
            let token = entry.token.ok_or(Error::WrongToken)?;
            let complete = unsafe { entry.complete(&mut self.queue, token) };
            if matches!(complete, Err(Error::WrongToken | Error::Quarantined)) {
                self.quarantined = true;
                return Err(Error::Quarantined);
            }
            complete?;
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("completion pending-count underflow");
        }
        let status = self.pending[slot]
            .as_ref()
            .ok_or(Error::WrongToken)?
            .resp
            .status();
        let _ = self.retire_pending_slot(slot)?;
        status.into()
    }

    /// Fetches the token of the next completed request from the used ring and returns it, without
    /// removing it from the used ring. If there are no pending completed requests returns `None`.
    pub fn peek_used(&mut self) -> Option<u16> {
        if self.quarantined || self.retired {
            return None;
        }
        self.queue.peek_used()
    }

    fn reap_used_completions(
        &mut self,
        budget: usize,
        allow_physical: bool,
    ) -> Result<PendingBlkDrainStatus> {
        self.ensure_live()?;
        let budget = budget.min(PENDING_COMPLETION_DRAIN_BUDGET);
        let mut drained = 0usize;
        let mut async_drained = 0usize;
        let mut async_drained_bytes = 0usize;
        while drained < budget {
            let Some(token) = self.queue.peek_used() else {
                break;
            };
            let slot = match self.pending_slot_for_token(token) {
                Ok(slot) => slot,
                Err(Error::WrongToken) => {
                    // A used-ring token with no live owner is a malformed or
                    // duplicate completion.  Retain every pending owner and
                    // force the typed reset/quarantine path; never convert
                    // this protocol violation into an ordinary I/O error.
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                Err(error) => return Err(error),
            };
            let (bytes, async_accounted, owner, is_flush) = {
                let entry = match self.pending[slot].as_mut() {
                    Some(entry) => entry,
                    None => {
                        self.quarantined = true;
                        return Err(Error::Quarantined);
                    }
                };
                let owner = entry.completion_owner;
                if owner == BlockCompletionOwner::Physical && !allow_physical {
                    // A count-only consumer may retire ordinary entries ahead
                    // of a physical effect, but it must stop at the physical
                    // owner rather than stealing its used element.
                    break;
                }
                // SAFETY: the used entry proves that device access has
                // stopped for this fully-installed pending request.
                let complete = unsafe { entry.complete(&mut self.queue, token) };
                if matches!(complete, Err(Error::WrongToken | Error::Quarantined)) {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                let _ = complete?;
                (
                    entry.completion_bytes,
                    entry.async_accounted,
                    owner,
                    entry.is_flush(),
                )
            };
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("completion pending-count underflow");
            if async_accounted {
                async_drained = async_drained
                    .checked_add(1)
                    .expect("async drain count overflow");
                async_drained_bytes = async_drained_bytes
                    .checked_add(usize::try_from(bytes).unwrap_or(usize::MAX))
                    .expect("async drain byte-count overflow");
                if is_flush {
                    record_blk_async_flush_completion();
                }
            }
            // Physical requests remain in their slot until the physical
            // effect consumer takes the concrete completion. Ordinary
            // requests likewise retain their done state for an exact waiter;
            // neither path needs an unbounded/intermediate completion FIFO.
            debug_assert!(matches!(
                owner,
                BlockCompletionOwner::Ordinary
                    | BlockCompletionOwner::Legacy
                    | BlockCompletionOwner::Physical
            ));
            drained = drained.checked_add(1).expect("drain count overflow");
        }
        record_blk_pending_drain(drained);
        record_blk_async_completion(async_drained, async_drained_bytes, self.async_pending_count);
        record_blk_async_adaptive_completion(async_drained, self.configured_async_depth_cap());
        Ok(if self.queue.peek_used().is_some() {
            PendingBlkDrainStatus::Continuation { drained }
        } else {
            PendingBlkDrainStatus::Complete { drained }
        })
    }

    /// Drains at most `budget` completed pending block requests.
    ///
    /// The budget is capped at [`PENDING_COMPLETION_DRAIN_BUDGET`] so every
    /// caller gets the same bounded task-context work unit.  A continuation is
    /// reported when another used-ring entry is still available; callers must
    /// invoke this method again without waiting for another interrupt.
    /// A zero budget performs no work and reports whether a continuation is
    /// already available. Ordinary records remain in their pending slots after
    /// this count-only reap; callers that do not have an exact handle owner
    /// must call [`Self::retire_completion_records`] to release those slots.
    /// This keeps the count-only path bounded without an intermediate FIFO.
    pub fn drain_pending_completions_bounded(
        &mut self,
        budget: usize,
    ) -> Result<PendingBlkDrainStatus> {
        self.reap_used_completions(budget, false)
    }

    /// Drains one fixed task-context completion credit.
    pub fn drain_pending_completions(&mut self) -> Result<usize> {
        Ok(self
            .drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)?
            .drained())
    }

    /// Drains used-ring entries for a handle-specific waiter. Unlike the
    /// count-only API this is allowed to claim physical completions, because
    /// the waiter will consume the matching owned record immediately.
    pub fn drain_pending_completions_for_handle(
        &mut self,
        handle: PendingBlkHandle,
        budget: usize,
    ) -> Result<PendingBlkDrainStatus> {
        if handle.generation != 0 && handle.generation != self.generation {
            return Err(Error::WrongToken);
        }
        self.ensure_live()?;
        let target_slot = self
            .find_pending_cookie(handle.completion_cookie())
            .ok_or(Error::WrongToken)?;
        let target_owner = self.pending[target_slot]
            .as_ref()
            .ok_or(Error::WrongToken)?
            .completion_owner;
        if self.pending[target_slot]
            .as_ref()
            .is_some_and(|entry| entry.done)
        {
            return Ok(if self.queue.peek_used().is_some() {
                PendingBlkDrainStatus::Continuation { drained: 0 }
            } else {
                PendingBlkDrainStatus::Complete { drained: 0 }
            });
        }

        let budget = budget.min(PENDING_COMPLETION_DRAIN_BUDGET);
        let mut drained = 0usize;
        let mut async_drained = 0usize;
        let mut async_drained_bytes = 0usize;
        while drained < budget {
            let Some(token) = self.queue.peek_used() else {
                break;
            };
            let slot = match self.pending_slot_for_token(token) {
                Ok(slot) => slot,
                Err(Error::WrongToken) => {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                Err(error) => return Err(error),
            };
            let owner = match self.pending[slot].as_ref() {
                Some(entry) => entry.completion_owner,
                None => {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
            };
            if target_owner == BlockCompletionOwner::Physical && slot != target_slot {
                // A physical exact waiter has no authority over either an
                // ordinary request or another physical effect.  Leave the
                // used-ring head to its owner; the any-physical completion
                // worker will demultiplex all physical entries in order.
                break;
            }
            if slot != target_slot && owner == BlockCompletionOwner::Physical {
                // A different physical effect owns the FIFO head.  An exact
                // waiter must leave it for the physical drain instead of
                // claiming a completion merely because it is in front of its
                // own handle.
                break;
            }
            let (bytes, async_accounted, is_flush) = {
                let entry = match self.pending[slot].as_mut() {
                    Some(entry) => entry,
                    None => {
                        self.quarantined = true;
                        return Err(Error::Quarantined);
                    }
                };
                // SAFETY: the used entry at the FIFO head identifies this
                // fully-installed request and proves device access stopped.
                let complete = unsafe { entry.complete(&mut self.queue, token) };
                if matches!(complete, Err(Error::WrongToken | Error::Quarantined)) {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                let _ = complete?;
                if slot == target_slot && owner == BlockCompletionOwner::Physical {
                    entry.completion_claimed = true;
                }
                (
                    entry.completion_bytes,
                    entry.async_accounted,
                    entry.is_flush(),
                )
            };
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("completion pending-count underflow");
            if async_accounted {
                async_drained = async_drained
                    .checked_add(1)
                    .expect("async drain count overflow");
                async_drained_bytes = async_drained_bytes
                    .checked_add(usize::try_from(bytes).unwrap_or(usize::MAX))
                    .expect("async drain byte-count overflow");
                if is_flush {
                    record_blk_async_flush_completion();
                }
            }
            drained = drained.checked_add(1).expect("drain count overflow");
            if slot == target_slot {
                break;
            }
        }
        record_blk_pending_drain(drained);
        record_blk_async_completion(async_drained, async_drained_bytes, self.async_pending_count);
        record_blk_async_adaptive_completion(async_drained, self.configured_async_depth_cap());
        Ok(if self.queue.peek_used().is_some() {
            PendingBlkDrainStatus::Continuation { drained }
        } else {
            PendingBlkDrainStatus::Complete { drained }
        })
    }

    /// Drains concrete physical completions into a caller-owned bounded output
    /// slice. Ordinary entries at the used-ring head remain untouched for the
    /// device-global ordinary owner.
    pub fn drain_pending_completions_into(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> Result<BlockCompletionDrain> {
        self.drain_pending_completions_into_mode(output, true)
    }

    /// Drains concrete completions for the single device-global completion
    /// owner. Unlike the physical-only drain, this path may consume either
    /// owner class at the FIFO head and preserves the owner in each record.
    /// Every consumed record is retired exactly once by this owner.
    pub fn drain_pending_completions_all_into(
        &mut self,
        output: &mut [BlockCompletion],
    ) -> Result<BlockCompletionDrain> {
        self.drain_pending_completions_into_mode(output, false)
    }

    fn drain_pending_completions_into_mode(
        &mut self,
        output: &mut [BlockCompletion],
        physical_only: bool,
    ) -> Result<BlockCompletionDrain> {
        self.ensure_live()?;
        let budget = output.len().min(MAX_PHYSICAL_BATCH_REQUESTS);
        let mut completed = 0usize;
        let mut drained = 0usize;
        while completed < budget && drained < PENDING_COMPLETION_DRAIN_BUDGET {
            let Some(token) = self.queue.peek_used() else {
                break;
            };
            let slot = match self.pending_slot_for_token(token) {
                Ok(slot) => slot,
                Err(Error::WrongToken) => {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                Err(error) => return Err(error),
            };
            let owner = match self.pending[slot].as_ref() {
                Some(entry) => entry.completion_owner,
                None => {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
            };
            if physical_only && owner != BlockCompletionOwner::Physical {
                // The physical completion consumer has no authority over an
                // ordinary request.  Leave the FIFO head untouched so the
                // ordinary exact/count owner can reap it first.
                break;
            }
            let handle = match self.pending_handle(slot, false) {
                Ok(handle) => handle,
                Err(Error::WrongToken) => {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                Err(error) => return Err(error),
            };
            let (cookie, status, bytes, is_flush, async_accounted) = {
                let entry = match self.pending[slot].as_mut() {
                    Some(entry) => entry,
                    None => {
                        self.quarantined = true;
                        return Err(Error::Quarantined);
                    }
                };
                let complete = unsafe { entry.complete(&mut self.queue, token) };
                if matches!(complete, Err(Error::WrongToken | Error::Quarantined)) {
                    self.quarantined = true;
                    return Err(Error::Quarantined);
                }
                let _ = complete?;
                (
                    entry.completion_cookie,
                    entry.resp.status(),
                    entry.completion_bytes,
                    entry.is_flush(),
                    entry.async_accounted,
                )
            };
            self.pending_count = self
                .pending_count
                .checked_sub(1)
                .expect("completion pending-count underflow");
            if async_accounted {
                record_blk_async_completion(
                    1,
                    usize::try_from(bytes).unwrap_or(usize::MAX),
                    self.async_pending_count,
                );
                if is_flush {
                    record_blk_async_flush_completion();
                }
            }
            output[completed] = BlockCompletion {
                handle,
                cookie,
                owner,
                status,
                bytes,
            };
            completed = completed.checked_add(1).expect("completion count overflow");
            let _ = self.retire_pending_slot(slot)?;
            drained = drained.checked_add(1).expect("drain count overflow");
        }
        Ok(BlockCompletionDrain {
            completed,
            // Report any used-ring work so the task scheduler can keep the
            // device's bounded continuation alive.  The physical owner still
            // consumes only a physical FIFO head; an ordinary head remains
            // untouched for its owner and the wait path uses
            // `physical_completion_ready` to avoid retrying it in a loop.
            continuation: self.queue.peek_used().is_some(),
        })
    }

    /// Returns whether the next used-ring entry belongs to the physical
    /// completion owner.  This deliberately examines only the FIFO head: a
    /// physical consumer cannot skip an ordinary entry without claiming work
    /// owned by another completion path.
    pub fn physical_completion_ready(&mut self) -> bool {
        if self.ensure_live().is_err() {
            return false;
        }
        let Some(token) = self.queue.peek_used() else {
            return false;
        };
        self.pending_slot_for_token(token)
            .ok()
            .and_then(|slot| self.pending[slot].as_ref())
            .is_some_and(|entry| entry.completion_owner == BlockCompletionOwner::Physical)
    }

    /// Returns the completion owner for a live handle without claiming its
    /// used-ring entry.
    pub fn pending_request_owner(&self, handle: PendingBlkHandle) -> Option<BlockCompletionOwner> {
        if handle.generation != 0 && handle.generation != self.generation {
            return None;
        }
        self.find_pending_cookie(handle.completion_cookie())
            .and_then(|slot| self.pending[slot].as_ref())
            .map(|entry| entry.completion_owner)
    }

    /// Returns whether the used-ring head is the exact physical handle.  A
    /// handle-specific physical waiter must not advance past another effect,
    /// even when that other effect is also physical-owned.
    pub fn physical_completion_ready_for_handle(&mut self, handle: PendingBlkHandle) -> bool {
        if self.ensure_live().is_err() {
            return false;
        }
        let Some(token) = self.queue.peek_used() else {
            return false;
        };
        let Some(slot) = self.pending_slot_for_token(token).ok() else {
            return false;
        };
        self.pending[slot].as_ref().is_some_and(|entry| {
            entry.completion_owner == BlockCompletionOwner::Physical
                && entry.completion_cookie == handle.completion_cookie()
        })
    }

    /// Returns whether a pending block request has completed.
    pub fn pending_request_done(&self, handle: PendingBlkHandle) -> bool {
        (handle.generation == 0 || handle.generation == self.generation)
            && self
                .find_pending_cookie(handle.completion_cookie())
                .and_then(|slot| self.pending[slot].as_ref())
                .is_some_and(|entry| entry.done)
    }

    /// Reaps a completed pending request and returns its device status.
    pub fn complete_pending_request(&mut self, handle: PendingBlkHandle) -> Result {
        if handle.generation != 0 && handle.generation != self.generation {
            return Err(Error::WrongToken);
        }
        self.ensure_live()?;
        let slot = self
            .find_pending_cookie(handle.completion_cookie())
            .ok_or(Error::WrongToken)?;
        if !self.pending[slot].as_ref().is_some_and(|entry| entry.done) {
            return Err(Error::NotReady);
        }
        if self.pending[slot].as_ref().is_some_and(|entry| {
            entry.completion_owner == BlockCompletionOwner::Physical && !entry.completion_claimed
        }) {
            return Err(Error::WrongToken);
        }
        let status = self.retire_pending_slot(slot)?;
        status.into()
    }

    /// Returns the number of published pending requests that have not yet been
    /// drained from the used ring.
    pub fn pending_request_count(&self) -> usize {
        self.pending_count
    }

    /// Returns whether a physical request still owns completion authority.
    /// Count-only drains must not claim its used entry.
    pub fn physical_pending(&self) -> bool {
        self.physical_pending_count != 0
    }

    /// Records a synchronous wait performed outside this object while an
    /// already-published pending request was in flight.
    pub fn record_external_queue_wait(&mut self, polls: u64, notified: bool) {
        record_queue_sync_wait(polls, notified);
    }

    /// Returns the size of the device's VirtQueue.
    ///
    /// This can be used to tell the caller how many channels to monitor on.
    pub fn virt_queue_size(&self) -> u16 {
        QUEUE_SIZE
    }

    /// Returns the current generation-safe queue identity while the queue is
    /// live. A retired reset queue is never returned as a reusable handle
    /// without a complete transport reinitialization.
    pub fn queue_handle(&self) -> Option<BlockQueueHandle> {
        if self.quarantined || self.retired {
            return None;
        }
        Some(BlockQueueHandle {
            generation: self.generation,
            queue: QUEUE,
        })
    }

    /// Attempts a bounded transport reset and releases pending DMA mappings
    /// only after quiescence has been observed.  A failed proof leaves the
    /// request slots and mappings owned by this object and returns the typed
    /// `Quarantined` outcome.
    pub fn reset_device(&mut self) -> ResetOutcome {
        // Stop producing device-side notifications before any early
        // quarantine return.  This is independent of the quiescence proof:
        // a late IRQ must not keep a poisoned queue alive.
        self.queue.set_dev_notify(false);
        if self.quarantined {
            return ResetOutcome::Quarantined;
        }
        if self.retired {
            return ResetOutcome::Retired;
        }
        if self.queue.outstanding_descriptor_count() != self.tracked_pending_descriptor_count() {
            // A legacy/untracked descriptor or a queue-accounting mismatch
            // means the driver cannot enumerate every descriptor and DMA
            // owner. Resetting and dropping the queue would be a use-after-
            // free risk, so retain the complete owner instead.
            self.quarantined = true;
            return ResetOutcome::Quarantined;
        }
        self.transport.set_status(DeviceStatus::empty());
        let mut polls = 0usize;
        while !self.transport.get_status().is_empty() && polls < RESET_POLL_BUDGET {
            polls = polls.checked_add(1).expect("reset poll-count overflow");
            spin_loop();
        }
        if !self.transport.get_status().is_empty() {
            self.quarantined = true;
            return ResetOutcome::Quarantined;
        }
        self.transport.mark_reset_complete();
        for entry in (&mut *self.pending).iter_mut() {
            let Some(request) = entry.take() else {
                continue;
            };
            if !request.done {
                let token = request.token.expect("live pending request lost token");
                // SAFETY: reset status was observed empty, proving that the
                // device no longer accesses this descriptor chain.
                unsafe { request.recycle_after_quiescence(&mut self.queue, token) };
            }
        }
        if !self.queue.is_empty() || self.queue.has_live_indirect_lists() {
            self.quarantined = true;
            return ResetOutcome::Quarantined;
        }
        self.token_slots.fill(None);
        self.notified_slots.fill(false);
        self.pending_count = 0;
        self.async_pending_count = 0;
        self.physical_pending_count = 0;
        self.transport.queue_unset(QUEUE);
        if self.generation == u64::MAX {
            // A generation must never wrap and make an old handle valid
            // again.  Keep the retired owner fail-closed instead.
            self.quarantined = true;
            return ResetOutcome::Quarantined;
        }
        let Some(next_generation) = self.generation.checked_add(1) else {
            self.quarantined = true;
            return ResetOutcome::Quarantined;
        };
        self.generation = next_generation;
        self.retired = true;
        ResetOutcome::Retired
    }
}

impl<H: Hal, T: Transport> Drop for VirtIOBlk<H, T> {
    fn drop(&mut self) {
        if self.quarantined {
            // The transport did not prove quiescence.  The resource fields
            // are ManuallyDrop, so returning here deliberately leaks the
            // queue, transport, pending slots, and DMA mappings instead of
            // allowing Rust's drop glue to free memory the device may still
            // access.  This is the quarantine owner, not a synthetic error.
            self.queue.set_dev_notify(false);
            return;
        }
        if self.retired {
            // `reset_device` already proved quiescence and cleared the
            // queue. Do not ask the transport to reset a third time while
            // dropping the retired owner.
            unsafe {
                ManuallyDrop::drop(&mut self.pending);
                ManuallyDrop::drop(&mut self.queue);
                ManuallyDrop::drop(&mut self.transport);
            }
            return;
        }
        let live_pending = self.pending.iter().filter(|entry| entry.is_some()).count();
        record_blk_async_resource_leaks(live_pending);
        // A pending request still contains caller-owned DMA pointers.  Reset
        // the device before tearing down the queue so it cannot touch those
        // pointers after this object is gone.  The pending requests are
        // deliberately not reported as completed; teardown is fail-closed.
        self.queue.set_dev_notify(false);
        self.transport.set_status(DeviceStatus::empty());
        let mut polls = 0usize;
        while !self.transport.get_status().is_empty() && polls < RESET_POLL_BUDGET {
            polls = polls.checked_add(1).expect("reset poll-count overflow");
            spin_loop();
        }
        if !self.transport.get_status().is_empty() {
            self.quarantined = true;
            // Keep every possible DMA owner in its ManuallyDrop fields.  The
            // quarantine branch above will retain them when this destructor
            // returns.
            return;
        }
        self.transport.mark_reset_complete();
        if self.queue.outstanding_descriptor_count() != self.tracked_pending_descriptor_count() {
            // Do not destroy a queue containing a descriptor that is not
            // represented by a pending owner. The ManuallyDrop fields retain
            // both the queue and all caller/DMA state in this branch.
            self.quarantined = true;
            return;
        }
        // Physical mappings are owned by their pending entries until the
        // used-ring reap. After reset the device is quiescent, so release any
        // still-live mappings exactly once; completed entries already unmapped
        // themselves in `PendingBlkRequest::complete`.
        for entry in (&mut *self.pending).iter_mut() {
            let Some(request) = entry.take() else {
                continue;
            };
            if !request.done {
                let token = request.token.expect("live pending request lost token");
                // SAFETY: the bounded reset proof above established
                // quiescence before descriptor recycling.
                unsafe { request.recycle_after_quiescence(&mut self.queue, token) };
            }
        }
        if !self.queue.is_empty() || self.queue.has_live_indirect_lists() {
            self.quarantined = true;
            return;
        }
        // Clear any queue pointers after the device has acknowledged reset.
        self.transport.queue_unset(QUEUE);
        // The owner fields are ManuallyDrop specifically so this is the only
        // normal path that releases them.  Quarantine returns above retain the
        // complete owner instead.
        unsafe {
            ManuallyDrop::drop(&mut self.pending);
            ManuallyDrop::drop(&mut self.queue);
            ManuallyDrop::drop(&mut self.transport);
        }
    }
}

#[repr(C)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity_low: Volatile<u32>,
    capacity_high: Volatile<u32>,
    size_max: Volatile<u32>,
    seg_max: Volatile<u32>,
    cylinders: Volatile<u16>,
    heads: Volatile<u8>,
    sectors: Volatile<u8>,
    blk_size: Volatile<u32>,
    physical_block_exp: Volatile<u8>,
    alignment_offset: Volatile<u8>,
    min_io_size: Volatile<u16>,
    opt_io_size: Volatile<u32>,
    // ... ignored
}

/// A VirtIO block device request.
#[repr(C)]
#[derive(AsBytes, Debug)]
pub struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

impl Default for BlkReq {
    fn default() -> Self {
        Self {
            type_: ReqType::In,
            reserved: 0,
            sector: 0,
        }
    }
}

/// Response of a VirtIOBlk request.
#[repr(C)]
#[derive(AsBytes, Debug, FromBytes, FromZeroes)]
pub struct BlkResp {
    status: RespStatus,
}

impl BlkResp {
    /// Return the status of a VirtIOBlk request.
    pub fn status(&self) -> RespStatus {
        self.status
    }
}

#[repr(u32)]
#[derive(AsBytes, Clone, Copy, Debug, Eq, PartialEq)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    GetId = 8,
    GetLifetime = 10,
    Discard = 11,
    WriteZeroes = 13,
    SecureErase = 14,
}

/// Status of a VirtIOBlk request.
#[repr(transparent)]
#[derive(AsBytes, Copy, Clone, Debug, Eq, FromBytes, FromZeroes, PartialEq)]
pub struct RespStatus(u8);

impl RespStatus {
    /// Ok.
    pub const OK: RespStatus = RespStatus(0);
    /// IoErr.
    pub const IO_ERR: RespStatus = RespStatus(1);
    /// Unsupported yet.
    pub const UNSUPPORTED: RespStatus = RespStatus(2);
    /// Not ready.
    pub const NOT_READY: RespStatus = RespStatus(3);

    /// Returns the raw device status byte for completion plumbing.
    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl From<RespStatus> for Result {
    fn from(status: RespStatus) -> Self {
        match status {
            RespStatus::OK => Ok(()),
            RespStatus::IO_ERR => Err(Error::IoError),
            RespStatus::UNSUPPORTED => Err(Error::Unsupported),
            RespStatus::NOT_READY => Err(Error::NotReady),
            _ => Err(Error::IoError),
        }
    }
}

impl Default for BlkResp {
    fn default() -> Self {
        BlkResp {
            status: RespStatus::NOT_READY,
        }
    }
}

/// The standard sector size of a VirtIO block device. Data is read and written in multiples of this
/// size.
pub const SECTOR_SIZE: usize = 512;

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device supports multiqueue.
        const MQ            = 1 << 12;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;
        /// Device supports providing storage lifetime information.
        const LIFETIME      = 1 << 15;
        /// Device can support the secure erase command.
        const SECURE_ERASE  = 1 << 16;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};
    use core::{mem::size_of, ptr::NonNull};
    use std::{sync::Mutex, thread};

    use super::*;
    use crate::{
        hal::fake::FakeHal,
        transport::{
            fake::{FakeTransport, QueueStatus, State},
            DeviceType,
        },
    };

    #[repr(align(512))]
    struct AlignedBlock([u8; SECTOR_SIZE]);

    #[test]
    fn pending_handle_round_trips_generation_and_cookie() {
        let handle = PendingBlkHandle {
            generation: 0x1234_5678,
            cookie: 99,
            slot: 17,
            token: 99,
            notified: true,
        };
        let encoded = handle.into_raw();
        let decoded = PendingBlkHandle::from_raw(encoded);
        assert_eq!(decoded.completion_cookie(), handle.completion_cookie());
        assert_eq!(decoded.notified(), handle.notified());
        // Raw handles deliberately carry only the opaque cookie and notify
        // hint; queue generation/slot/token remain private to the owner.
        assert_eq!(decoded.generation(), 0);
        assert_ne!(decoded, handle);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn pending_slab_is_heap_backed_with_fixed_capacity() {
        type TestBlk = VirtIOBlk<FakeHal, FakeTransport<BlkConfig>>;

        // The queue and bookkeeping arrays are intentionally bounded, but a
        // 128-entry request owner slab must not inflate every device value or
        // any caller stack frame.
        assert!(size_of::<TestBlk>() < 16 * 1024);
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, _| {
            assert_eq!(blk.pending.len(), QUEUE_SIZE as usize);
            assert_eq!(
                blk.pending.iter().filter(|entry| entry.is_some()).count(),
                0
            );
        });
    }

    fn with_test_blk<R>(
        device_features: u64,
        f: impl FnOnce(&mut VirtIOBlk<FakeHal, FakeTransport<BlkConfig>>, Arc<Mutex<State>>) -> R,
    ) -> R {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();
        f(&mut blk, state)
    }

    #[test]
    fn prepared_physical_drop_rolls_back_without_publication() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let physical = [PhysicalSegment {
                paddr: SECTOR_SIZE,
                len: SECTOR_SIZE,
            }];
            let mut request = PendingBlkPhysicalBatchRequest {
                block_id: 0,
                buffer: PendingBlkPhysicalBatchBuffer::Read(&physical),
                handle: None,
            };
            let descriptor_budget = blk.async_descriptor_budget();
            {
                let prepared =
                    unsafe { blk.prepare_physical_batch(core::slice::from_mut(&mut request)) }
                        .unwrap();
                drop(prepared);
            }

            assert_eq!(blk.pending_request_count(), 0);
            assert_eq!(blk.async_descriptor_budget(), descriptor_budget);
            assert!(!State::poll_queue_notified(&state, QUEUE));
        });
    }

    #[test]
    fn legacy_nb_is_tracked_and_stale_handle_cannot_claim_reused_token() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let mut request = BlkReq::default();
            let mut buffer = [0u8; SECTOR_SIZE];
            let mut response = BlkResp::default();
            let old =
                unsafe { blk.read_blocks_nb(0, &mut request, &mut buffer, &mut response) }.unwrap();
            let mut used = vec![0x5a; SECTOR_SIZE];
            used.extend_from_slice(
                BlkResp {
                    status: RespStatus::OK,
                }
                .as_bytes(),
            );
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| used));
            assert_eq!(
                blk.drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                    .unwrap()
                    .drained(),
                1
            );
            assert!(blk.pending_request_done(old));
            assert_eq!(blk.retire_completion_records(), None);
            assert_eq!(
                unsafe { blk.complete_read_blocks(old, &request, &mut buffer, &mut response) },
                Ok(())
            );

            let new =
                unsafe { blk.read_blocks_nb(1, &mut request, &mut buffer, &mut response) }.unwrap();
            assert_ne!(old.completion_cookie(), new.completion_cookie());
            assert_eq!(
                unsafe { blk.complete_read_blocks(old, &request, &mut buffer, &mut response) },
                Err(Error::WrongToken)
            );
            let mut used = vec![0x6b; SECTOR_SIZE];
            used.extend_from_slice(
                BlkResp {
                    status: RespStatus::OK,
                }
                .as_bytes(),
            );
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| used));
            assert_eq!(
                unsafe { blk.complete_read_blocks(new, &request, &mut buffer, &mut response) },
                Ok(())
            );
        });
    }

    #[test]
    fn physical_prepare_validation_failure_publishes_nothing() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, _state| {
            let physical = [PhysicalSegment {
                paddr: 0,
                len: SECTOR_SIZE,
            }];
            let mut request = PendingBlkPhysicalBatchRequest {
                block_id: 0,
                buffer: PendingBlkPhysicalBatchBuffer::Read(&physical),
                handle: None,
            };
            let descriptor_budget = blk.async_descriptor_budget();
            assert!(matches!(
                unsafe { blk.prepare_physical_batch(core::slice::from_mut(&mut request)) },
                Err(Error::InvalidParam)
            ));
            assert_eq!(blk.pending_request_count(), 0);
            assert_eq!(blk.async_descriptor_budget(), descriptor_budget);
        });
    }

    #[test]
    fn no_drain_prepare_leaves_ordinary_used_entry_for_global_owner() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let ordinary_buf = [0u8; SECTOR_SIZE];
            let mut ordinary = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&ordinary_buf),
                handle: None,
            };
            let ordinary_report =
                unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut ordinary)) }.unwrap();
            assert_eq!(ordinary_report.submitted, 1);
            let ordinary_handle = ordinary.handle.expect("ordinary handle");
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    BlkResp {
                        status: RespStatus::OK,
                    }
                    .as_bytes()
                    .to_vec()
                }));

            let ordinary_buf_2 = [0u8; SECTOR_SIZE];
            let mut ordinary_2 = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&ordinary_buf_2),
                handle: None,
            };
            let ordinary_report_2 = unsafe {
                blk.submit_pending_batch_no_drain(core::slice::from_mut(&mut ordinary_2))
            }
            .unwrap();
            assert_eq!(ordinary_report_2.submitted, 1);
            let ordinary_handle_2 = ordinary_2.handle.expect("ordinary handle 2");
            // The ordinary no-drain submit must leave the first used entry
            // at the lower FIFO head for the global owner as well.
            assert!(!blk.pending_request_done(ordinary_handle));
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    BlkResp {
                        status: RespStatus::OK,
                    }
                    .as_bytes()
                    .to_vec()
                }));

            let mut backing = AlignedBlock([0; SECTOR_SIZE]);
            let physical = [PhysicalSegment {
                paddr: backing.0.as_mut_ptr() as usize,
                len: SECTOR_SIZE,
            }];
            let mut request = PendingBlkPhysicalBatchRequest {
                block_id: 1,
                buffer: PendingBlkPhysicalBatchBuffer::Read(&physical),
                handle: None,
            };
            let prepared =
                unsafe { blk.prepare_physical_batch_no_drain(core::slice::from_mut(&mut request)) }
                    .unwrap();
            let (report, handles, count) = prepared.publish_with_handles();
            assert_eq!(report.submitted, 1);
            assert_eq!(count, 1);
            let physical_handle = handles[0];
            // Preparation must not let a synchronous submitter consume the
            // ordinary used-ring head behind the broker's back.
            assert!(!blk.pending_request_done(ordinary_handle));
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    let mut used = vec![0x5a; SECTOR_SIZE];
                    used.extend_from_slice(
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes(),
                    );
                    used
                }));

            let mut output = [BlockCompletion {
                handle: PendingBlkHandle::from_raw(0),
                cookie: 0,
                owner: BlockCompletionOwner::Physical,
                status: RespStatus::NOT_READY,
                bytes: 0,
            }; 3];
            let drained = blk.drain_pending_completions_all_into(&mut output).unwrap();
            assert_eq!(drained.completed, 3);
            assert_eq!(output[0].owner, BlockCompletionOwner::Ordinary);
            assert_eq!(output[0].handle.into_raw(), ordinary_handle.into_raw());
            assert_eq!(output[1].owner, BlockCompletionOwner::Ordinary);
            assert_eq!(output[1].handle.into_raw(), ordinary_handle_2.into_raw());
            assert_eq!(output[2].owner, BlockCompletionOwner::Physical);
            assert_eq!(output[2].handle.into_raw(), physical_handle.into_raw());
        });
    }

    #[test]
    fn physical_drain_does_not_consume_ordinary_completions() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let buffers = vec![[0u8; SECTOR_SIZE], [0u8; SECTOR_SIZE]];
            let mut requests = buffers
                .iter()
                .enumerate()
                .map(|(index, buffer)| PendingBlkBatchRequest {
                    block_id: index,
                    buffer: PendingBlkBatchBuffer::Write(buffer),
                    handle: None,
                })
                .collect::<Vec<_>>();
            let report = unsafe { blk.submit_pending_batch(&mut requests) }.unwrap();
            assert_eq!(report.submitted, 2);
            let handles = requests
                .iter()
                .map(|request| request.handle.unwrap())
                .collect::<Vec<_>>();

            for status in [RespStatus::IO_ERR, RespStatus::OK] {
                assert!(state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| BlkResp { status }
                        .as_bytes()
                        .to_vec(),));
            }

            let mut output = [BlockCompletion {
                handle: PendingBlkHandle::from_raw(0),
                cookie: 0,
                owner: BlockCompletionOwner::Physical,
                status: RespStatus::NOT_READY,
                bytes: 0,
            }; 2];
            let drained = blk.drain_pending_completions_into(&mut output).unwrap();
            assert_eq!(drained.completed, 0);
            assert!(drained.continuation);
            assert_eq!(blk.pending_request_count(), 2);

            assert_eq!(
                blk.drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                    .unwrap()
                    .drained(),
                2
            );
            assert!(blk.pending_request_done(handles[0]));
            assert!(blk.pending_request_done(handles[1]));
            assert_eq!(
                blk.complete_pending_request(handles[0]),
                Err(Error::IoError)
            );
            assert_eq!(blk.complete_pending_request(handles[1]), Ok(()));
            assert_eq!(blk.pending_request_count(), 0);
        });
    }

    #[test]
    fn count_poll_before_exact_physical_drain_preserves_cookie_and_status() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let mut backing = AlignedBlock([0; SECTOR_SIZE]);
            let physical = [PhysicalSegment {
                paddr: backing.0.as_mut_ptr() as usize,
                len: SECTOR_SIZE,
            }];
            let handle = unsafe { blk.submit_read_blocks_physical_pending(0, &physical) }.unwrap();

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    let mut response = vec![0x5a; SECTOR_SIZE];
                    response.extend_from_slice(
                        BlkResp {
                            status: RespStatus::IO_ERR,
                        }
                        .as_bytes(),
                    );
                    response
                },));

            // A count-only poll must not claim a physical used entry. The
            // exact drain below remains the sole owner of its cookie/status.
            assert_eq!(
                blk.drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                    .unwrap(),
                PendingBlkDrainStatus::Continuation { drained: 0 }
            );
            let mut output = [BlockCompletion {
                handle: PendingBlkHandle::from_raw(0),
                cookie: 0,
                owner: BlockCompletionOwner::Physical,
                status: RespStatus::NOT_READY,
                bytes: 0,
            }];
            let drained = blk.drain_pending_completions_into(&mut output).unwrap();
            assert_eq!(drained.completed, 1);
            assert_eq!(output[0].handle.into_raw(), handle.into_raw());
            assert_eq!(output[0].cookie, handle.completion_cookie());
            assert_eq!(output[0].status, RespStatus::IO_ERR);
            assert_eq!(output[0].bytes, SECTOR_SIZE as u32);
            assert_eq!(blk.pending_request_count(), 0);
        });
    }

    #[test]
    fn count_only_retirement_caps_a_large_budget_to_one_fixed_pass() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let buffers = [[0u8; SECTOR_SIZE]; 6];
            let mut requests = buffers
                .iter()
                .enumerate()
                .map(|(index, buffer)| PendingBlkBatchRequest {
                    block_id: index,
                    buffer: PendingBlkBatchBuffer::Write(buffer),
                    handle: None,
                })
                .collect::<Vec<_>>();
            let report = unsafe { blk.submit_pending_batch(&mut requests) }.expect("submit batch");
            assert_eq!(report.submitted, buffers.len());

            for _ in 0..buffers.len() {
                assert!(state
                    .lock()
                    .unwrap()
                    .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes()
                        .to_vec()
                    }));
            }

            let first = blk
                .drain_pending_completions_bounded(usize::MAX)
                .expect("first bounded drain");
            assert_eq!(
                first,
                PendingBlkDrainStatus::Continuation {
                    drained: PENDING_COMPLETION_DRAIN_BUDGET,
                }
            );
            let second = blk
                .drain_pending_completions_bounded(usize::MAX)
                .expect("second bounded drain");
            assert_eq!(
                second,
                PendingBlkDrainStatus::Complete {
                    drained: buffers.len() - PENDING_COMPLETION_DRAIN_BUDGET,
                }
            );

            // All six records are done, but the poll surface passes only one
            // fixed count-only budget. The remaining two owners must stay
            // visible for the next poll.
            assert_eq!(blk.pending_request_count(), 0);
            assert_eq!(blk.pending.iter().flatten().count(), buffers.len());
            assert!(blk.pending.iter().flatten().all(|entry| entry.done));
            assert_eq!(
                blk.retire_completion_records_bounded(PENDING_COMPLETION_DRAIN_BUDGET),
                None
            );
            assert_eq!(
                blk.pending.iter().flatten().count(),
                buffers.len() - PENDING_COMPLETION_DRAIN_BUDGET
            );
            assert_eq!(
                blk.retire_completion_records_bounded(PENDING_COMPLETION_DRAIN_BUDGET),
                None
            );
            assert_eq!(blk.pending.iter().flatten().count(), 0);
        });
    }

    #[test]
    fn ordinary_batch_drain_preserves_notified_raw_handle() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let buffer = [0u8; SECTOR_SIZE];
            let mut request = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&buffer),
                handle: None,
            };
            let report =
                unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut request)) }.unwrap();
            assert_eq!(report.submitted, 1);
            assert!(report.notified);
            let submitted = request.handle.expect("ordinary request lost handle");

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    BlkResp {
                        status: RespStatus::OK,
                    }
                    .as_bytes()
                    .to_vec()
                }));
            let mut output = [BlockCompletion {
                handle: PendingBlkHandle::from_raw(0),
                cookie: 0,
                owner: BlockCompletionOwner::Physical,
                status: RespStatus::NOT_READY,
                bytes: 0,
            }];
            let drained = blk.drain_pending_completions_all_into(&mut output).unwrap();
            assert_eq!(drained.completed, 1);
            assert_eq!(output[0].owner, BlockCompletionOwner::Ordinary);
            assert_eq!(output[0].handle.into_raw(), submitted.into_raw());
            assert_eq!(output[0].cookie, submitted.completion_cookie());
        });
    }

    #[test]
    fn malformed_used_length_quarantines_without_releasing_owner() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let mut buffer = [0u8; SECTOR_SIZE];
            let mut request = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Read(&mut buffer),
                handle: None,
            };
            let report =
                unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut request)) }.unwrap();
            assert_eq!(report.submitted, 1);

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| {
                    BlkResp {
                        status: RespStatus::OK,
                    }
                    .as_bytes()
                    .to_vec()
                }));
            let mut output = [BlockCompletion {
                handle: PendingBlkHandle::from_raw(0),
                cookie: 0,
                owner: BlockCompletionOwner::Ordinary,
                status: RespStatus::NOT_READY,
                bytes: 0,
            }];
            assert_eq!(
                blk.drain_pending_completions_all_into(&mut output),
                Err(Error::Quarantined)
            );
            assert_eq!(blk.pending_request_count(), 1);
            // A malformed used entry is not a successful completion: keep the
            // request owner in the reset/quarantine custody path.
            assert!(!blk.pending_request_done(request.handle.unwrap()));
            assert!(blk.queue_handle().is_none());
        });
    }

    #[test]
    fn reset_rejects_stale_generation_and_quarantine_retains_owner() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, _state| {
            let buffer = [0u8; SECTOR_SIZE];
            let mut request = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&buffer),
                handle: None,
            };
            unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut request)) }.unwrap();
            let old_handle = request.handle.unwrap();
            assert_eq!(blk.reset_device(), ResetOutcome::Retired);
            // A second reset observes the permanently retired queue; it does
            // not reopen notifications or pretend that submission is live.
            assert_eq!(blk.reset_device(), ResetOutcome::Retired);
            assert!(blk.queue_handle().is_none());
            assert!(!blk.pending_request_done(old_handle));
            assert_eq!(
                blk.complete_pending_request(old_handle),
                Err(Error::WrongToken)
            );
            let buffer = [0u8; SECTOR_SIZE];
            let mut retired_request = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&buffer),
                handle: None,
            };
            assert_eq!(
                unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut retired_request)) },
                Err(Error::Quarantined)
            );
        });

        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state,
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();
        let physical = [PhysicalSegment {
            paddr: SECTOR_SIZE,
            len: SECTOR_SIZE,
        }];
        unsafe { blk.submit_read_blocks_physical_pending(0, &physical) }.unwrap();
        blk.quarantined = true;
        assert_eq!(blk.reset_device(), ResetOutcome::Quarantined);
        assert_eq!(blk.pending_request_count(), 1);
        assert!(blk.queue_handle().is_none());
        core::mem::forget(blk);
    }

    #[test]
    fn config() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(0x42),
            capacity_high: Volatile::new(0x02),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RO.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        assert_eq!(blk.capacity(), 0x02_0000_0042);
        assert_eq!(blk.readonly(), true);
    }

    #[test]
    fn bounded_pending_drain_requires_task_continuation() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();
        let buffers = (0..6).map(|_| [0u8; SECTOR_SIZE]).collect::<Vec<_>>();
        let mut requests = buffers
            .iter()
            .enumerate()
            .map(|(index, buffer)| PendingBlkBatchRequest {
                block_id: index,
                buffer: PendingBlkBatchBuffer::Write(buffer),
                handle: None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            blk.drain_pending_completions_bounded(0).unwrap(),
            PendingBlkDrainStatus::Complete { drained: 0 }
        );
        let report = unsafe { blk.submit_pending_batch(requests.as_mut_slice()) }.unwrap();
        assert_eq!(report.submitted, buffers.len());
        let handles = requests
            .iter()
            .map(|request| request.handle.expect("submitted request lost handle"))
            .collect::<Vec<_>>();

        let mut error_first = true;
        for _ in 0..report.submitted {
            let status = if error_first {
                error_first = false;
                RespStatus::IO_ERR
            } else {
                RespStatus::OK
            };
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| BlkResp { status }
                    .as_bytes()
                    .to_vec(),));
        }

        assert_eq!(
            blk.drain_pending_completions_bounded(0).unwrap(),
            PendingBlkDrainStatus::Continuation { drained: 0 }
        );
        assert_eq!(blk.pending_request_count(), buffers.len());
        assert_eq!(
            blk.drain_pending_completions_bounded(2).unwrap(),
            PendingBlkDrainStatus::Continuation { drained: 2 }
        );
        assert_eq!(
            blk.drain_pending_completions_bounded(2).unwrap(),
            PendingBlkDrainStatus::Continuation { drained: 2 }
        );
        assert_eq!(
            blk.drain_pending_completions_bounded(2).unwrap(),
            PendingBlkDrainStatus::Complete { drained: 2 }
        );
        assert_eq!(blk.pending_request_count(), 0);
        for (index, handle) in handles.into_iter().enumerate() {
            if index == 0 {
                assert!(matches!(
                    blk.complete_pending_request(handle),
                    Err(Error::IoError)
                ));
            } else {
                blk.complete_pending_request(handle).unwrap();
            }
        }
    }

    #[test]
    fn count_drain_retires_owned_records_and_reuses_slot() {
        with_test_blk(BlkFeature::RING_INDIRECT_DESC.bits(), |blk, state| {
            let buffer = [0u8; SECTOR_SIZE];
            let mut request = PendingBlkBatchRequest {
                block_id: 0,
                buffer: PendingBlkBatchBuffer::Write(&buffer),
                handle: None,
            };
            assert_eq!(
                unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut request)) }
                    .unwrap()
                    .submitted,
                1
            );
            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |_| BlkResp {
                    status: RespStatus::IO_ERR
                }
                .as_bytes()
                .to_vec(),));
            assert_eq!(
                blk.drain_pending_completions_bounded(PENDING_COMPLETION_DRAIN_BUDGET)
                    .unwrap(),
                PendingBlkDrainStatus::Complete { drained: 1 }
            );
            assert_eq!(blk.pending_request_count(), 0);
            assert_eq!(blk.retire_completion_records(), Some(RespStatus::IO_ERR));

            // No handle-specific waiter consumed the completion. The slot was
            // nevertheless retired by the single completion owner and is
            // immediately reusable.
            let replacement = [0u8; SECTOR_SIZE];
            let mut replacement_request = PendingBlkBatchRequest {
                block_id: 1,
                buffer: PendingBlkBatchBuffer::Write(&replacement),
                handle: None,
            };
            assert_eq!(
                unsafe {
                    blk.submit_pending_batch(core::slice::from_mut(&mut replacement_request))
                }
                .unwrap()
                .submitted,
                1
            );
        });
    }

    #[test]
    fn inflight_drop_resets_transport_before_queue_teardown() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut buffer = [0u8; SECTOR_SIZE];
        let mut request = PendingBlkBatchRequest {
            block_id: 0,
            buffer: PendingBlkBatchBuffer::Read(&mut buffer),
            handle: None,
        };
        {
            let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();
            let report = unsafe { blk.submit_pending_batch(core::slice::from_mut(&mut request)) };
            assert_eq!(report.unwrap().submitted, 1);
        }
        let state = state.lock().unwrap();
        assert!(state.status.is_empty());
        assert_eq!(state.queues[QUEUE as usize].descriptors, 0);
    }

    #[test]
    fn read() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a read request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                    assert_eq!(
                        request,
                        BlkReq {
                            type_: ReqType::In,
                            reserved: 0,
                            sector: 42
                        }
                        .as_bytes()
                    );

                    let mut response = vec![0; SECTOR_SIZE];
                    response[0..9].copy_from_slice(b"Test data");
                    response.extend_from_slice(
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes(),
                    );

                    response
                }));
        });

        // Read a block from the device.
        let mut buffer = [0; 512];
        blk.read_blocks(42, &mut buffer).unwrap();
        assert_eq!(&buffer[0..9], b"Test data");

        handle.join().unwrap();
    }

    #[test]
    fn write() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a write request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                    assert_eq!(
                        &request[0..size_of::<BlkReq>()],
                        BlkReq {
                            type_: ReqType::Out,
                            reserved: 0,
                            sector: 42
                        }
                        .as_bytes()
                    );
                    let data = &request[size_of::<BlkReq>()..];
                    assert_eq!(data.len(), SECTOR_SIZE);
                    assert_eq!(&data[0..9], b"Test data");

                    let mut response = Vec::new();
                    response.extend_from_slice(
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes(),
                    );

                    response
                }));
        });

        // Write a block to the device.
        let mut buffer = [0; 512];
        buffer[0..9].copy_from_slice(b"Test data");
        blk.write_blocks(42, &mut buffer).unwrap();

        // Request to flush should be ignored as the device doesn't support it.
        blk.flush().unwrap();

        handle.join().unwrap();
    }

    #[test]
    fn flush() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: (BlkFeature::RING_INDIRECT_DESC | BlkFeature::FLUSH).bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a flush request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                    assert_eq!(
                        request,
                        BlkReq {
                            type_: ReqType::Flush,
                            reserved: 0,
                            sector: 0,
                        }
                        .as_bytes()
                    );

                    let mut response = Vec::new();
                    response.extend_from_slice(
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes(),
                    );

                    response
                }));
        });

        // Request to flush.
        blk.flush().unwrap();

        handle.join().unwrap();
    }

    #[test]
    fn device_id() {
        let mut config_space = BlkConfig {
            capacity_low: Volatile::new(66),
            capacity_high: Volatile::new(0),
            size_max: Volatile::new(0),
            seg_max: Volatile::new(0),
            cylinders: Volatile::new(0),
            heads: Volatile::new(0),
            sectors: Volatile::new(0),
            blk_size: Volatile::new(0),
            physical_block_exp: Volatile::new(0),
            alignment_offset: Volatile::new(0),
            min_io_size: Volatile::new(0),
            opt_io_size: Volatile::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: QUEUE_SIZE.into(),
            device_features: BlkFeature::RING_INDIRECT_DESC.bits(),
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let mut blk = VirtIOBlk::<FakeHal, FakeTransport<BlkConfig>>::new(transport).unwrap();

        // Start a thread to simulate the device waiting for a flush request.
        let handle = thread::spawn(move || {
            println!("Device waiting for a request.");
            State::wait_until_queue_notified(&state, QUEUE);
            println!("Transmit queue was notified.");

            assert!(state
                .lock()
                .unwrap()
                .read_write_queue::<{ QUEUE_SIZE as usize }>(QUEUE, |request| {
                    assert_eq!(
                        request,
                        BlkReq {
                            type_: ReqType::GetId,
                            reserved: 0,
                            sector: 0,
                        }
                        .as_bytes()
                    );

                    let mut response = Vec::new();
                    response.extend_from_slice(b"device_id\0\0\0\0\0\0\0\0\0\0\0");
                    response.extend_from_slice(
                        BlkResp {
                            status: RespStatus::OK,
                        }
                        .as_bytes(),
                    );

                    response
                }));
        });

        let mut id = [0; 20];
        let length = blk.device_id(&mut id).unwrap();
        assert_eq!(&id[0..length], b"device_id");

        handle.join().unwrap();
    }
}
