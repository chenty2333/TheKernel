//! Driver for VirtIO block devices.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::hint::spin_loop;

use bitflags::bitflags;
use log::info;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::{
    Error, Result,
    hal::Hal,
    queue::VirtQueue,
    stats::{
        record_blk_async_adaptive_completion, record_blk_async_admission_stall,
        record_blk_async_completion, record_blk_async_completion_error,
        record_blk_async_flush_completion, record_blk_async_flush_request,
        record_blk_async_queue_full, record_blk_async_resource_leaks,
        record_blk_async_submit_batch, record_blk_flush, record_blk_flush_unsupported,
        record_blk_pending_depth, record_blk_pending_drain, record_blk_pending_queue_full,
        record_blk_read, record_blk_write, record_queue_sync_wait,
    },
    transport::Transport,
    volatile::{Volatile, volread},
};

const QUEUE: u16 = 0;
const QUEUE_SIZE: u16 = 16;
// LA currently uses a bounce-buffered HAL for block I/O. Keep the block queue on the
// simple split-ring path there; indirect descriptors and event-index are optional and
// have been the source of corrupted request chains under QEMU.
#[cfg(target_arch = "loongarch64")]
const SUPPORTED_FEATURES: BlkFeature = BlkFeature::RO.union(BlkFeature::FLUSH);

#[cfg(not(target_arch = "loongarch64"))]
const SUPPORTED_FEATURES: BlkFeature = BlkFeature::RO
    .union(BlkFeature::FLUSH)
    .union(BlkFeature::RING_INDIRECT_DESC)
    .union(BlkFeature::RING_EVENT_IDX);

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
/// # use virtio_drivers::{Error, Hal};
/// # use virtio_drivers::transport::Transport;
/// use virtio_drivers::device::blk::{SECTOR_SIZE, VirtIOBlk};
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
    transport: T,
    queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    pending: [Option<PendingBlkRequest>; QUEUE_SIZE as usize],
    token_slots: [Option<usize>; QUEUE_SIZE as usize],
    pending_count: usize,
    async_pending_count: usize,
    capacity: u64,
    negotiated_features: BlkFeature,
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
}

/// Handle for a submitted pending block request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingBlkHandle {
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
        u64::from(self.slot) | (u64::from(self.token) << 16) | ((self.notified as u64) << 32)
    }

    /// Decodes a handle previously returned by [`Self::into_raw`].
    pub fn from_raw(raw: u64) -> Self {
        Self {
            slot: raw as u16,
            token: (raw >> 16) as u16,
            notified: ((raw >> 32) & 1) != 0,
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
        }
    }

    fn mark_async_accounted(&mut self) {
        self.async_accounted = true;
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn is_flush(&self) -> bool {
        self.req.type_ == ReqType::Flush
    }

    unsafe fn complete<H: Hal, const SIZE: usize>(
        &mut self,
        queue: &mut VirtQueue<H, SIZE>,
        token: u16,
    ) -> Result {
        if self.token != Some(token) {
            return Err(Error::WrongToken);
        }
        match &self.buffer {
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
                    )?;
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
                    )?;
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
                unsafe {
                    queue.pop_used(token, &[self.req.as_bytes()], outputs.as_mut_slice())?;
                }
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
                unsafe {
                    queue.pop_used(token, inputs.as_slice(), &mut [self.resp.as_bytes_mut()])?;
                }
            }
            PendingBlkBuffer::Flush => {
                // SAFETY: These are exactly the buffers passed to
                // `add_unpublished` for this token.
                unsafe {
                    queue.pop_used(
                        token,
                        &[self.req.as_bytes()],
                        &mut [self.resp.as_bytes_mut()],
                    )?;
                }
            }
        }

        self.done = true;
        Ok(())
    }
}

impl<H: Hal, T: Transport> VirtIOBlk<H, T> {
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

        Ok(VirtIOBlk {
            transport,
            queue,
            pending: core::array::from_fn(|_| None),
            token_slots: [None; QUEUE_SIZE as usize],
            pending_count: 0,
            async_pending_count: 0,
            capacity,
            negotiated_features,
        })
    }

    fn alloc_pending_slot(&self) -> Result<usize> {
        self.pending
            .iter()
            .position(Option::is_none)
            .ok_or(Error::QueueFull)
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

    /// Waits for a pending request handle and reaps its response.
    pub fn wait_pending_request(&mut self, handle: PendingBlkHandle) -> Result {
        let mut polls = 0u64;
        loop {
            self.drain_pending_completions()?;
            if self.pending_request_done(handle) {
                self.record_external_queue_wait(polls, handle.notified());
                return self.complete_pending_request(handle);
            }
            polls = polls.saturating_add(1);
            spin_loop();
        }
    }

    fn configured_async_depth_cap(&self) -> usize {
        #[cfg(target_arch = "loongarch64")]
        {
            (crate::stats::async_block_la_depth() as usize).clamp(1, QUEUE_SIZE as usize)
        }
        #[cfg(not(target_arch = "loongarch64"))]
        {
            let configured = crate::stats::async_block_depth() as usize;
            if configured == 0 {
                usize::from(QUEUE_SIZE / 2)
            } else {
                configured
            }
            .clamp(1, QUEUE_SIZE as usize)
        }
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
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [resp.as_bytes_mut()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Sends the given request to the device and waits for a response, including the given data.
    fn request_read(&mut self, request: BlkReq, data: &mut [u8]) -> Result {
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            &mut [data, resp.as_bytes_mut()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    #[cfg(feature = "alloc")]
    fn request_read_vectored(&mut self, request: BlkReq, data: &mut [&mut [u8]]) -> Result {
        let mut resp = BlkResp::default();
        let mut outputs = Vec::with_capacity(data.len() + 1);
        outputs.extend(data.iter_mut().map(|buf| &mut **buf));
        outputs.push(resp.as_bytes_mut());
        self.queue.add_notify_wait_pop(
            &[request.as_bytes()],
            outputs.as_mut_slice(),
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Sends the given request and data to the device and waits for a response.
    fn request_write(&mut self, request: BlkReq, data: &[u8]) -> Result {
        let mut resp = BlkResp::default();
        self.queue.add_notify_wait_pop(
            &[request.as_bytes(), data],
            &mut [resp.as_bytes_mut()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    #[cfg(feature = "alloc")]
    fn request_write_vectored(&mut self, request: BlkReq, data: &[&[u8]]) -> Result {
        let mut resp = BlkResp::default();
        let mut inputs = Vec::with_capacity(data.len() + 1);
        inputs.push(request.as_bytes());
        inputs.extend(data.iter().copied());
        self.queue.add_notify_wait_pop(
            inputs.as_slice(),
            &mut [resp.as_bytes_mut()],
            &mut self.transport,
        )?;
        resp.status.into()
    }

    /// Requests the device to flush any pending writes to storage.
    ///
    /// This will be ignored if the device doesn't support the `VIRTIO_BLK_F_FLUSH` feature.
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
        let token_idx = usize::from(token);
        debug_assert!(self.token_slots[token_idx].is_none());
        self.token_slots[token_idx] = Some(slot);
        self.pending[slot].as_mut().ok_or(Error::WrongToken)?.token = Some(token);
        self.pending_count += 1;
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_read(buf.len(), 0);

        let notified = self.queue.should_notify();
        if notified {
            self.transport.notify(QUEUE);
        }
        Ok(PendingBlkHandle {
            slot: slot as u16,
            token,
            notified,
        })
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
    /// It will submit request to the VirtIO block device and return a token identifying
    /// the position of the first Descriptor in the chain. If there are not enough
    /// Descriptors to allocate, then it returns [`Error::QueueFull`].
    ///
    /// The caller can then call `peek_used` with the returned token to check whether the device has
    /// finished handling the request. Once it has, the caller must call `complete_read_blocks` with
    /// the same buffers before reading the response.
    ///
    /// ```
    /// # use virtio_drivers::{Error, Hal};
    /// # use virtio_drivers::device::blk::VirtIOBlk;
    /// # use virtio_drivers::transport::Transport;
    /// use virtio_drivers::device::blk::{BlkReq, BlkResp, RespStatus};
    ///
    /// # fn example<H: Hal, T: Transport>(blk: &mut VirtIOBlk<H, T>) -> Result<(), Error> {
    /// let mut request = BlkReq::default();
    /// let mut buffer = [0; 512];
    /// let mut response = BlkResp::default();
    /// let token = unsafe { blk.read_blocks_nb(42, &mut request, &mut buffer, &mut response) }?;
    ///
    /// // Wait for an interrupt to tell us that the request completed...
    /// assert_eq!(blk.peek_used(), Some(token));
    ///
    /// unsafe {
    ///     blk.complete_read_blocks(token, &request, &mut buffer, &mut response)?;
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
    ) -> Result<u16> {
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        record_blk_read(buf.len(), 0);
        *req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let token = self
            .queue
            .add(&[req.as_bytes()], &mut [buf, resp.as_bytes_mut()])?;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        Ok(token)
    }

    /// Completes a read operation which was started by `read_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers must be passed in again as were passed to `read_blocks_nb` when it returned
    /// the token.
    pub unsafe fn complete_read_blocks(
        &mut self,
        token: u16,
        req: &BlkReq,
        buf: &mut [u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        self.queue
            .pop_used(token, &[req.as_bytes()], &mut [buf, resp.as_bytes_mut()])?;
        resp.status.into()
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
        let token_idx = usize::from(token);
        debug_assert!(self.token_slots[token_idx].is_none());
        self.token_slots[token_idx] = Some(slot);
        self.pending[slot].as_mut().ok_or(Error::WrongToken)?.token = Some(token);
        self.pending_count += 1;
        record_blk_pending_depth(self.pending_count);
        self.queue.publish_unpublished(token);
        record_blk_write(buf.len(), 0);

        let notified = self.queue.should_notify();
        if notified {
            self.transport.notify(QUEUE);
        }
        Ok(PendingBlkHandle {
            slot: slot as u16,
            token,
            notified,
        })
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
        let mut report = PendingBlkBatchReport::default();
        if requests.is_empty() {
            return Ok(report);
        }
        for request in requests.iter() {
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
        }

        self.drain_pending_completions()?;

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
            self.pending[slot]
                .as_mut()
                .ok_or(Error::WrongToken)?
                .mark_async_accounted();

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
                    return Err(err);
                }
            };

            let token_idx = usize::from(token);
            debug_assert!(self.token_slots[token_idx].is_none());
            self.token_slots[token_idx] = Some(slot);
            self.pending[slot].as_mut().ok_or(Error::WrongToken)?.token = Some(token);
            self.pending_count += 1;
            self.async_pending_count += 1;
            record_blk_pending_depth(self.pending_count);
            accepted_heads[report.submitted] = token;

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
            request.handle = Some(PendingBlkHandle {
                slot: slot as u16,
                token,
                notified: false,
            });
            report.submitted += 1;
            report.bytes += bytes;
        }

        for head in accepted_heads.iter().copied().take(report.submitted) {
            self.queue.publish_unpublished(head);
        }

        if report.submitted != 0 {
            report.notified = self.queue.should_notify();
            if report.notified {
                self.transport.notify(QUEUE);
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
    ) -> Result<u16> {
        assert_ne!(buf.len(), 0);
        assert_eq!(buf.len() % SECTOR_SIZE, 0);
        record_blk_write(buf.len(), 0);
        *req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let token = self
            .queue
            .add(&[req.as_bytes(), buf], &mut [resp.as_bytes_mut()])?;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE);
        }
        Ok(token)
    }

    /// Completes a write operation which was started by `write_blocks_nb`.
    ///
    /// # Safety
    ///
    /// The same buffers must be passed in again as were passed to `write_blocks_nb` when it
    /// returned the token.
    pub unsafe fn complete_write_blocks(
        &mut self,
        token: u16,
        req: &BlkReq,
        buf: &[u8],
        resp: &mut BlkResp,
    ) -> Result<()> {
        self.queue
            .pop_used(token, &[req.as_bytes(), buf], &mut [resp.as_bytes_mut()])?;
        resp.status.into()
    }

    /// Fetches the token of the next completed request from the used ring and returns it, without
    /// removing it from the used ring. If there are no pending completed requests returns `None`.
    pub fn peek_used(&mut self) -> Option<u16> {
        self.queue.peek_used()
    }

    /// Drains all currently completed pending block requests.
    pub fn drain_pending_completions(&mut self) -> Result<usize> {
        let mut drained = 0;
        let mut async_drained = 0;
        let mut async_drained_bytes = 0;
        while let Some(token) = self.queue.peek_used() {
            let idx = usize::from(token);
            let Some(slot) = self.token_slots[idx] else {
                return Err(Error::WrongToken);
            };
            let Some(entry) = self.pending[slot].as_mut() else {
                return Err(Error::WrongToken);
            };
            let async_flush = entry.async_accounted && entry.is_flush();
            // SAFETY: The pending entry was installed before its descriptor was
            // published, and stores the original buffers for this token.
            if let Err(err) = unsafe { entry.complete(&mut self.queue, token) } {
                record_blk_async_completion_error();
                return Err(err);
            }
            if entry.async_accounted {
                async_drained += 1;
                async_drained_bytes += entry.bytes();
                self.async_pending_count = self.async_pending_count.saturating_sub(1);
                if async_flush {
                    record_blk_async_flush_completion();
                }
            }
            self.token_slots[idx] = None;
            self.pending_count -= 1;
            drained += 1;
        }
        record_blk_pending_drain(drained);
        record_blk_async_completion(async_drained, async_drained_bytes, self.async_pending_count);
        record_blk_async_adaptive_completion(async_drained, self.configured_async_depth_cap());
        Ok(drained)
    }

    /// Returns whether a pending block request has completed.
    pub fn pending_request_done(&self, handle: PendingBlkHandle) -> bool {
        self.pending
            .get(usize::from(handle.slot))
            .and_then(Option::as_ref)
            .is_some_and(|entry| entry.token == Some(handle.token) && entry.done)
    }

    /// Reaps a completed pending request and returns its device status.
    pub fn complete_pending_request(&mut self, handle: PendingBlkHandle) -> Result {
        let slot = usize::from(handle.slot);
        let Some(entry) = self.pending.get(slot).and_then(Option::as_ref) else {
            return Err(Error::WrongToken);
        };
        if entry.token != Some(handle.token) {
            return Err(Error::WrongToken);
        }
        if !entry.done {
            return Err(Error::NotReady);
        }
        let status = entry.resp.status();
        self.pending[slot] = None;
        status.into()
    }

    /// Returns the number of published pending requests that have not yet been
    /// drained from the used ring.
    pub fn pending_request_count(&self) -> usize {
        self.pending_count
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
}

impl<H: Hal, T: Transport> Drop for VirtIOBlk<H, T> {
    fn drop(&mut self) {
        let live_pending = self.pending.iter().filter(|entry| entry.is_some()).count();
        record_blk_async_resource_leaks(live_pending);
        debug_assert_eq!(live_pending, 0, "dropping VirtIOBlk with live requests");
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(QUEUE);
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
    In          = 0,
    Out         = 1,
    Flush       = 4,
    GetId       = 8,
    GetLifetime = 10,
    Discard     = 11,
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
            DeviceType,
            fake::{FakeTransport, QueueStatus, State},
        },
    };

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

            assert!(
                state
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
                    })
            );
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

            assert!(
                state
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
                    })
            );
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

            assert!(
                state
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
                    })
            );
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

            assert!(
                state
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
                    })
            );
        });

        let mut id = [0; 20];
        let length = blk.device_id(&mut id).unwrap();
        assert_eq!(&id[0..length], b"device_id");

        handle.join().unwrap();
    }
}
