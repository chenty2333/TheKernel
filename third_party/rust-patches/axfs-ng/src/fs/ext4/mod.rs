mod fs;
mod inode;
mod util;

use alloc::{sync::Arc, vec::Vec};

use axdriver::prelude::{
    BlockAsyncOp, BlockCompletion, BlockCompletionOwner, BlockCompletionStatus, BlockDriverOps,
    BlockPhysicalRequest, BlockPhysicalSegment, BlockPhysicalSgOutcome, BlockQueueRequest,
    BlockRequestHandle, BlockResetOutcome, BlockSegment, DevError, DevResult,
};
pub use fs::*;
pub use inode::*;
use lwext4_rust::{
    AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, Ext4Error, Ext4Result,
    PhysicalIoBatchRequest, PhysicalIoBatchSubmitOutcome, PhysicalIoBatchSubmission,
    PhysicalIoCompletion, PhysicalIoCompletionDrain, PhysicalIoNotSubmittedReason,
    PhysicalIoPublication, PhysicalIoSegment, PhysicalIoSgOutcome,
    ffi::EIO,
};

use crate::MountedBlockDevice;

fn accepted_block_request_handles<'a>(
    requests: &'a [BlockQueueRequest<'_>],
    submitted: usize,
) -> impl Iterator<Item = BlockRequestHandle> + 'a {
    let accepted = requests.get(..submitted).unwrap_or_else(|| {
        panic!(
            "block driver overreported {submitted} accepted requests for {} entries",
            requests.len()
        )
    });
    accepted.iter().map(|request| {
        request
            .handle
            .expect("accepted block request is missing its completion handle")
    })
}

fn reap_all_raw_handles<E>(
    handles: impl IntoIterator<Item = u64>,
    mut wait_one: impl FnMut(BlockRequestHandle) -> Result<(), E>,
) -> Result<(), E> {
    let mut first_error = None;
    for raw in handles {
        if let Err(error) = wait_one(BlockRequestHandle { raw }) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Classifies lower admission failures before the first physical descriptor
/// is published.  Queue pressure is transient and therefore distinct from
/// permanent capability, allocation, and validation failures; all four
/// classes retain the all-or-none no-publication proof needed by a prepared
/// effect.
fn physical_batch_not_submitted_reason(
    error: &DevError,
) -> Option<PhysicalIoNotSubmittedReason> {
    match error {
        DevError::Again | DevError::ResourceBusy => {
            Some(PhysicalIoNotSubmittedReason::Backpressure)
        }
        DevError::Unsupported => Some(PhysicalIoNotSubmittedReason::Unsupported),
        DevError::NoMemory => Some(PhysicalIoNotSubmittedReason::NoMemory),
        DevError::InvalidParam => Some(PhysicalIoNotSubmittedReason::Invalid),
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) struct Ext4Disk {
    inner: Arc<MountedBlockDevice>,
}

impl Ext4Disk {
    pub(crate) fn new(dev: MountedBlockDevice) -> Self {
        Self {
            inner: Arc::new(dev),
        }
    }

    pub(crate) fn reset_device(&self) -> DevResult<BlockResetOutcome> {
        let mut device = self.inner.device().clone();
        BlockDriverOps::reset_device(&mut device)
    }

    /// Convert a lower queue failure while preserving reset-required physical
    /// custody.  A quarantined/retired outcome is not an ordinary EIO: the
    /// caller's effect/pin owner must remain retained until a supervisor or a
    /// complete transport reinitialization takes custody.
    fn reset_for_physical_error(&self, fallback_context: &'static str) -> Ext4Error {
        let (context, quarantined) = match self.reset_device() {
            Ok(BlockResetOutcome::Quiesced) => (fallback_context, false),
            Ok(BlockResetOutcome::Retired) => (
                "physical completion queue retired; owner requires transport reinit",
                true,
            ),
            Ok(BlockResetOutcome::Quarantined) => (
                "physical completion owner remains quarantined after reset",
                true,
            ),
            Err(_) => (
                "physical completion reset failed; owner remains retained",
                true,
            ),
        };
        Ext4Error::new(EIO as _, Some(context)).with_physical_quarantined(quarantined)
    }

    fn physical_wait_error(&self, error: DevError) -> Ext4Error {
        if matches!(error, DevError::BadState) {
            self.reset_for_physical_error("physical completion queue reset")
        } else {
            // A physical owner is still live when a wait cannot make
            // progress (for example a non-blocking caller).  Preserve typed
            // custody even when reset was not attempted here.
            Ext4Error::new(EIO as _, Some("physical completion wait failed"))
                .with_physical_quarantined(true)
        }
    }

    fn device_error(&self, error: DevError, context: &'static str) -> Ext4Error {
        if matches!(error, DevError::BadState) {
            self.reset_for_physical_error(context)
        } else {
            Ext4Error::new(EIO as _, Some(context))
        }
    }

    pub(crate) fn wait_async_write(&self, submission: &AsyncWriteSubmission) -> Ext4Result<()> {
        self.wait_async_handles(submission.handles.iter().copied())
    }

    pub(crate) fn wait_async_read(&self, submission: &AsyncReadSubmission) -> Ext4Result<()> {
        self.wait_async_handles(submission.handles.iter().copied())
    }

    /// Waits for a concrete physical completion routed to the device-global
    /// kernel owner.  Once the lower broker is installed, exact synchronous
    /// effect records remain in the lower mailbox and must be consumed through
    /// [`Self::wait_physical_completions_exact`] instead of this adapter.
    pub(crate) fn wait_any_physical_completion(
        &self,
        output: &mut [PhysicalIoCompletion],
    ) -> Ext4Result<PhysicalIoCompletionDrain> {
        const MAX_COMPLETIONS: usize = 32;
        if output.is_empty() {
            let mut empty: [BlockCompletion; 0] = [];
            let drain = self
                .inner
                .device()
                .wait_any_physical_completion(&mut empty)
                .map_err(|error| self.physical_wait_error(error))?;
            return Ok(PhysicalIoCompletionDrain {
                completed: drain.completed,
                continuation: drain.continuation,
            });
        }

        let output_len = output.len().min(MAX_COMPLETIONS);
        let mut driver_output = [BlockCompletion {
            handle: BlockRequestHandle { raw: 0 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0,
            status: BlockCompletionStatus::DeviceError(0),
            bytes: 0,
        }; MAX_COMPLETIONS];
        let drain = self
            .inner
            .device()
            .wait_any_physical_completion(&mut driver_output[..output_len])
            .map_err(|error| self.physical_wait_error(error))?;
        if drain.completed > output_len {
            return Err(Ext4Error::new(
                EIO as _,
                Some("physical completion wait overreported"),
            ));
        }
        for (dst, src) in output
            .iter_mut()
            .zip(driver_output.iter())
            .take(drain.completed)
        {
            if src.handle.raw == 0 || src.cookie == 0 || src.owner != BlockCompletionOwner::Physical
            {
                // The lower driver retired an unidentifiable completion. Do
                // not hand a zero identity to effect demux; close the queue
                // and retain/quarantine any remaining DMA owners instead.
                return Err(self.reset_for_physical_error("physical completion has zero identity"));
            }
            let success = match src.status {
                BlockCompletionStatus::Success => true,
                BlockCompletionStatus::DeviceError(_) => false,
                BlockCompletionStatus::Quarantined => {
                    // Quarantine is a typed reset-required condition, not a
                    // device I/O failure. Do not turn it into `success=false`
                    // and let the physical effect release its owner.
                    return Err(self.reset_for_physical_error("physical completion is quarantined"));
                }
            };
            *dst = PhysicalIoCompletion {
                handle: src.handle.raw,
                cookie: src.cookie,
                bytes: src.bytes as usize,
                success,
            };
        }
        Ok(PhysicalIoCompletionDrain {
            completed: drain.completed,
            continuation: drain.continuation,
        })
    }

    /// Waits for only the handles/cookies published by one effect.  The
    /// shared device owner still performs a mixed any-drain on every pass and
    /// retains foreign physical records in its fixed mailbox, so concurrent
    /// effects cannot steal or lose an out-of-order completion.
    pub(crate) fn wait_physical_completions_exact(
        &self,
        publication: PhysicalIoPublication,
        output: &mut [PhysicalIoCompletion],
    ) -> Ext4Result<PhysicalIoCompletionDrain> {
        const MAX_COMPLETIONS: usize = 32;
        let count = publication.count();
        if count == 0 || output.is_empty() || count > MAX_COMPLETIONS {
            return if count == 0 || count > MAX_COMPLETIONS {
                Err(Ext4Error::new(
                    EIO as _,
                    Some("invalid physical completion publication"),
                ))
            } else {
                Ok(PhysicalIoCompletionDrain::default())
            };
        }
        let mut handles = [BlockRequestHandle { raw: 0 }; MAX_COMPLETIONS];
        let mut cookies = [0u64; MAX_COMPLETIONS];
        for index in 0..count {
            handles[index] = BlockRequestHandle {
                raw: publication.handle(index).ok_or_else(|| {
                    Ext4Error::new(EIO as _, Some("physical completion handle missing"))
                })?,
            };
            cookies[index] = publication.cookie(index).ok_or_else(|| {
                Ext4Error::new(EIO as _, Some("physical completion cookie missing"))
            })?;
        }

        let output_len = output.len().min(MAX_COMPLETIONS);
        let mut driver_output = [BlockCompletion {
            handle: BlockRequestHandle { raw: 0 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0,
            status: BlockCompletionStatus::DeviceError(0),
            bytes: 0,
        }; MAX_COMPLETIONS];
        let drain = self
            .inner
            .device()
            .wait_physical_completions_exact(
                &handles[..count],
                &cookies[..count],
                &mut driver_output[..output_len],
            )
            .map_err(|error| self.physical_wait_error(error))?;
        if drain.completed > output_len {
            return Err(
                self.reset_for_physical_error("physical exact completion wait overreported")
            );
        }
        for (dst, src) in output
            .iter_mut()
            .zip(driver_output.iter())
            .take(drain.completed)
        {
            if src.owner != BlockCompletionOwner::Physical
                || src.handle.raw == 0
                || src.cookie == 0
                || !handles[..count]
                    .iter()
                    .zip(cookies[..count].iter().copied())
                    .any(|(handle, cookie)| handle.raw == src.handle.raw && cookie == src.cookie)
            {
                return Err(
                    self.reset_for_physical_error("physical exact completion identity mismatch")
                );
            }
            let success = match src.status {
                BlockCompletionStatus::Success => true,
                BlockCompletionStatus::DeviceError(_) => false,
                BlockCompletionStatus::Quarantined => {
                    return Err(
                        self.reset_for_physical_error("physical exact completion is quarantined")
                    );
                }
            };
            *dst = PhysicalIoCompletion {
                handle: src.handle.raw,
                cookie: src.cookie,
                bytes: src.bytes as usize,
                success,
            };
        }
        Ok(PhysicalIoCompletionDrain {
            completed: drain.completed,
            continuation: drain.continuation,
        })
    }

    fn wait_async_handles(&self, handles: impl IntoIterator<Item = u64>) -> Ext4Result<()> {
        reap_all_raw_handles(handles, |handle| {
            self.inner
                .device()
                .wait_async_all_owned(core::slice::from_ref(&handle))
        })
        .map_err(|error| self.device_error(error, "async completion queue reset"))
    }
}

impl BlockDevice for Ext4Disk {
    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        let mut device = self.inner.device().lock();
        device
            .read_block(block_id, buf)
            .map_err(|error| self.device_error(error, "block read failed"))?;
        Ok(buf.len())
    }

    fn read_blocks_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> Ext4Result<usize> {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        let mut device = self.inner.device().lock();
        device
            .read_block_vectored(block_id, bufs)
            .map_err(|error| self.device_error(error, "vectored block read failed"))?;
        Ok(bytes)
    }

    unsafe fn read_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<PhysicalIoSgOutcome> {
        const MAX_PHYSICAL_SG: usize = 4;
        if segments.is_empty() || segments.len() > MAX_PHYSICAL_SG {
            return Ok(PhysicalIoSgOutcome::NotSubmitted);
        }
        let mut physical = [BlockPhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        let mut bytes = 0usize;
        for (index, segment) in segments.iter().copied().enumerate() {
            physical[index] = BlockPhysicalSegment {
                paddr: segment.paddr,
                len: segment.len,
            };
            bytes = bytes
                .checked_add(segment.len)
                .ok_or_else(|| Ext4Error::new(EIO as _, Some("physical SG length overflow")))?;
        }
        match unsafe {
            self.inner
                .device()
                .read_block_physical_sg(block_id, &physical[..segments.len()])
        } {
            Ok(BlockPhysicalSgOutcome::Completed) => Ok(PhysicalIoSgOutcome::Completed(bytes)),
            Ok(BlockPhysicalSgOutcome::NotSubmitted) => Ok(PhysicalIoSgOutcome::NotSubmitted),
            Ok(BlockPhysicalSgOutcome::Quarantined) => Ok(PhysicalIoSgOutcome::Quarantined),
            Err(error) => Err(self.device_error(error, "physical SG read failed")),
        }
    }

    fn try_read_blocks_vectored_async_submit(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        let mut dev = self.inner.device().clone();
        let Some(caps) = BlockDriverOps::async_queue_caps(&dev) else {
            return Ok(None);
        };
        if caps.default_depth == 0 || caps.max_requests == 0 || caps.max_descriptors == 0 {
            return Ok(None);
        }

        let block_size = BlockDriverOps::block_size(&dev);
        if block_size == 0 {
            return Ok(None);
        }

        let mut segments = Vec::with_capacity(bufs.len());
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            if buf.len() % block_size != 0 {
                return Ok(None);
            }
            segments.push(BlockSegment::from_read_buf(buf));
        }
        if segments.is_empty() {
            return Ok(Some(AsyncReadSubmission {
                bytes: 0,
                ..AsyncReadSubmission::default()
            }));
        }

        let max_segments_per_request = 4usize;
        // Submit-only APIs must never accept a prefix and then wait under the
        // caller's filesystem lock. Keep this boundary atomic by admitting a
        // single hardware request; larger scatter lists use the synchronous
        // fallback until an owned partial-submission outcome exists.
        if segments.len() > max_segments_per_request {
            return Ok(None);
        }
        let bytes = segments.iter().map(|segment| segment.len).sum();
        let mut handles = Vec::with_capacity(1);
        let mut requests = [BlockQueueRequest {
            op: BlockAsyncOp::Read,
            block_id,
            segments: &segments,
            handle: None,
        }];

        let report = match BlockDriverOps::submit_async_batch(&mut dev, &mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(error) => return Err(self.device_error(error, "async read submit failed")),
        };
        if report.submitted == 0 {
            return Ok(None);
        }
        let handle = accepted_block_request_handles(&requests, report.submitted)
            .next()
            .expect("one accepted ext4 read request lost its handle");
        assert_eq!(
            report.submitted, 1,
            "ext4 read submit accepted a non-atomic prefix"
        );
        if report.bytes != bytes {
            if let Err(error) = self
                .inner
                .device()
                .wait_async_all_owned(core::slice::from_ref(&handle))
            {
                return Err(self.device_error(error, "async read completion failed"));
            }
            return Err(Ext4Error::new(EIO as _, None));
        }
        assert!(
            handles.len() < handles.capacity(),
            "preallocated ext4 read handle storage exhausted"
        );
        handles.push(handle.raw);

        Ok(Some(AsyncReadSubmission {
            handles,
            bytes,
            submit_batches: 1,
        }))
    }

    fn try_write_blocks_vectored_async_submit(
        &mut self,
        block_id: u64,
        bufs: &[&[u8]],
    ) -> Ext4Result<Option<AsyncWriteSubmission>> {
        let mut dev = self.inner.device().clone();
        let Some(caps) = BlockDriverOps::async_queue_caps(&dev) else {
            return Ok(None);
        };
        if caps.default_depth == 0 || caps.max_requests == 0 || caps.max_descriptors == 0 {
            return Ok(None);
        }

        let block_size = BlockDriverOps::block_size(&dev);
        if block_size == 0 {
            return Ok(None);
        }

        let mut segments = Vec::with_capacity(bufs.len());
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            if buf.len() % block_size != 0 {
                return Ok(None);
            }
            segments.push(BlockSegment::from_write_buf(buf));
        }
        if segments.is_empty() {
            return Ok(Some(AsyncWriteSubmission {
                bytes: 0,
                ..AsyncWriteSubmission::default()
            }));
        }

        let max_segments_per_request = 4usize;
        if segments.len() > max_segments_per_request {
            return Ok(None);
        }
        let bytes = segments.iter().map(|segment| segment.len).sum();
        let mut handles = Vec::with_capacity(1);
        let mut requests = [BlockQueueRequest {
            op: BlockAsyncOp::Write,
            block_id,
            segments: &segments,
            handle: None,
        }];

        let report = match BlockDriverOps::submit_async_batch(&mut dev, &mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(error) => return Err(self.device_error(error, "async write submit failed")),
        };
        if report.submitted == 0 {
            return Ok(None);
        }
        let handle = accepted_block_request_handles(&requests, report.submitted)
            .next()
            .expect("one accepted ext4 write request lost its handle");
        assert_eq!(
            report.submitted, 1,
            "ext4 write submit accepted a non-atomic prefix"
        );
        if report.bytes != bytes {
            if let Err(error) = self
                .inner
                .device()
                .wait_async_all_owned(core::slice::from_ref(&handle))
            {
                return Err(self.device_error(error, "async write completion failed"));
            }
            return Err(Ext4Error::new(EIO as _, None));
        }
        assert!(
            handles.len() < handles.capacity(),
            "preallocated ext4 write handle storage exhausted"
        );
        handles.push(handle.raw);

        Ok(Some(AsyncWriteSubmission {
            handles,
            bytes,
            submit_batches: 1,
        }))
    }

    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        let mut device = self.inner.device().lock();
        device
            .write_block(block_id, buf)
            .map_err(|error| self.device_error(error, "block write failed"))?;
        Ok(buf.len())
    }

    fn write_blocks_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> Ext4Result<usize> {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        let mut device = self.inner.device().lock();
        device
            .write_block_vectored(block_id, bufs)
            .map_err(|error| self.device_error(error, "vectored block write failed"))?;
        Ok(bytes)
    }

    unsafe fn write_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<PhysicalIoSgOutcome> {
        const MAX_PHYSICAL_SG: usize = 4;
        if segments.is_empty() || segments.len() > MAX_PHYSICAL_SG {
            return Ok(PhysicalIoSgOutcome::NotSubmitted);
        }
        let mut physical = [BlockPhysicalSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_SG];
        let mut bytes = 0usize;
        for (index, segment) in segments.iter().copied().enumerate() {
            physical[index] = BlockPhysicalSegment {
                paddr: segment.paddr,
                len: segment.len,
            };
            bytes = bytes
                .checked_add(segment.len)
                .ok_or_else(|| Ext4Error::new(EIO as _, Some("physical SG length overflow")))?;
        }
        match unsafe {
            self.inner
                .device()
                .write_block_physical_sg(block_id, &physical[..segments.len()])
        } {
            Ok(BlockPhysicalSgOutcome::Completed) => Ok(PhysicalIoSgOutcome::Completed(bytes)),
            Ok(BlockPhysicalSgOutcome::NotSubmitted) => Ok(PhysicalIoSgOutcome::NotSubmitted),
            Ok(BlockPhysicalSgOutcome::Quarantined) => Ok(PhysicalIoSgOutcome::Quarantined),
            Err(error) => Err(self.device_error(error, "physical SG write failed")),
        }
    }

    unsafe fn submit_physical_batch_with_route(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
        kernel_worker: bool,
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        const MAX_REQUESTS: usize = 32;
        if requests.is_empty() || requests.len() > MAX_REQUESTS {
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::Invalid,
            ));
        }

        // The request descriptors borrow only the plan-owned fixed arrays for
        // the duration of this call.  The plan/effect owner retains those
        // arrays after publication until exact completion retirement.
        let mut driver_requests = Vec::new();
        if driver_requests.try_reserve_exact(requests.len()).is_err() {
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::NoMemory,
            ));
        }
        let mut expected_bytes = 0usize;
        for request in requests {
            if request.segment_count == 0
                || request.segment_count > lwext4_rust::MAX_PHYSICAL_IO_SEGMENTS
            {
                return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                    PhysicalIoNotSubmittedReason::Invalid,
                ));
            }
            let segments = request.physical_segments();
            if segments.iter().any(|segment| segment.len == 0) {
                return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                    PhysicalIoNotSubmittedReason::Invalid,
                ));
            }
            let bytes = segments
                .iter()
                .try_fold(0usize, |total, segment| total.checked_add(segment.len));
            if bytes != Some(request.bytes) {
                return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                    PhysicalIoNotSubmittedReason::Invalid,
                ));
            }
            expected_bytes = match expected_bytes.checked_add(request.bytes) {
                Some(bytes) => bytes,
                None => {
                    return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                        PhysicalIoNotSubmittedReason::Invalid,
                    ));
                }
            };
            let op = match request.operation {
                lwext4_rust::PhysicalIoOperation::Read => BlockAsyncOp::Read,
                lwext4_rust::PhysicalIoOperation::Write => BlockAsyncOp::Write,
            };
            let mut physical = Vec::new();
            if physical.try_reserve_exact(segments.len()).is_err() {
                return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                    PhysicalIoNotSubmittedReason::NoMemory,
                ));
            }
            physical.extend(
                segments
                    .iter()
                    .copied()
                    .map(|segment| BlockPhysicalSegment {
                        paddr: segment.paddr,
                        len: segment.len,
                    }),
            );
            // Keep one owned conversion per descriptor.  The physical driver
            // copies the numbers into its queue-owned request state before
            // publishing; no Rust reference escapes this call.
            driver_requests.push((op, request.block_id, physical));
        }

        let mut requests_for_driver = Vec::new();
        if requests_for_driver
            .try_reserve_exact(driver_requests.len())
            .is_err()
        {
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::NoMemory,
            ));
        }
        for (op, block_id, segments) in &driver_requests {
            requests_for_driver.push(BlockPhysicalRequest {
                block_id: *block_id,
                op: *op,
                segments,
                handle: None,
                cookie: None,
            });
        }

        // Reserve the returned-handle owner before publication.  Once the
        // driver accepts a prefix, allocation failure cannot turn the result
        // into a fallback: the accepted handles must remain available for
        // quiescence and terminal reporting.
        let mut handles = Vec::new();
        if handles
            .try_reserve_exact(requests_for_driver.len())
            .is_err()
        {
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::NoMemory,
            ));
        }
        let mut cookies = Vec::new();
        if cookies
            .try_reserve_exact(requests_for_driver.len())
            .is_err()
        {
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::NoMemory,
            ));
        }

        let report = if kernel_worker {
            unsafe {
                self.inner
                    .device()
                    .submit_physical_batch_kernel(&mut requests_for_driver)
            }
        } else {
            unsafe {
                self.inner
                    .device()
                    .submit_physical_batch_exact(&mut requests_for_driver)
            }
        };
        let report = match report {
            Ok(report) => report,
            // These errors are all returned before lower publication. Keep
            // their exact class so only transient queue pressure is retried;
            // permanent capability/allocation/validation failures remain
            // observable to the high-level fallback policy.
            Err(error) => {
                let Some(reason) = physical_batch_not_submitted_reason(&error) else {
                    return Err(self.device_error(error, "physical batch submit failed"));
                };
                return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(reason));
            }
        };
        if report.submitted > requests.len() {
            // The driver has claimed an impossible accepted prefix.  Close
            // the queue before exposing the malformed publication.  There
            // is no safe exact owner for the impossible suffix, so this is a
            // terminal typed failure and never a fallback result.
            return Err(self.reset_for_physical_error("malformed physical batch report"));
        }
        if report.submitted == 0 {
            if report.bytes != 0 {
                return Err(
                    self.reset_for_physical_error("malformed physical batch zero-prefix bytes")
                );
            }
            return Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::Invalid,
            ));
        }

        for request in requests_for_driver.iter().take(report.submitted) {
            let (Some(handle), Some(cookie)) = (request.handle, request.cookie) else {
                // The accepted prefix is no longer safely identifiable.  Do
                // not expose a fallback result; reset/quarantine the lower
                // device before retaining the terminal malformed report.
                return Err(self.reset_for_physical_error("physical batch handle identity missing"));
            };
            if handle.raw == 0 || cookie == 0 {
                return Err(self.reset_for_physical_error("physical batch handle identity is zero"));
            }
            handles.push(handle.raw);
            cookies.push(cookie);
        }
        let terminal = report.submitted != requests.len() || report.bytes != expected_bytes;
        Ok(PhysicalIoBatchSubmitOutcome::Submitted(PhysicalIoBatchSubmission {
            handles,
            cookies,
            bytes: report.bytes,
            submitted: report.submitted,
            terminal,
        }))
    }

    unsafe fn submit_physical_batch(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        // Synchronous direct-I/O effects own their exact handle/cookie wait;
        // they remain in the lower mailbox and never become kernel-worker
        // routes.
        unsafe { self.submit_physical_batch_with_route(requests, false) }
    }

    /// Publishes an io_uring-owned effect to the device-global task worker.
    /// The lower broker then drains the used ring once and returns this
    /// route's exact raw handle/cookie record to the upper router.
    unsafe fn submit_physical_batch_kernel(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        unsafe { self.submit_physical_batch_with_route(requests, true) }
    }

    fn wait_any_physical_completion(
        &mut self,
        output: &mut [PhysicalIoCompletion],
    ) -> Ext4Result<PhysicalIoCompletionDrain> {
        Ext4Disk::wait_any_physical_completion(self, output)
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.inner.device().num_blocks())
    }

    fn flush(&mut self) -> Ext4Result<()> {
        let mut device = self.inner.device().lock();
        device
            .flush()
            .map_err(|error| self.device_error(error, "block flush failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn accepted_handles_preserve_the_reported_prefix() {
        let requests = [request(Some(21)), request(Some(22)), request(None)];
        let handles = accepted_block_request_handles(&requests, 2);
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.raw)
                .collect::<Vec<_>>(),
            [21, 22]
        );
    }

    #[test]
    #[should_panic(expected = "missing its completion handle")]
    fn accepted_handles_fail_closed_on_a_missing_handle() {
        let requests = [request(None)];
        let _ = accepted_block_request_handles(&requests, 1).next();
    }

    #[test]
    #[should_panic(expected = "overreported")]
    fn accepted_handles_fail_closed_on_an_overreported_count() {
        let requests = [request(Some(21))];
        let _ = accepted_block_request_handles(&requests, 2);
    }

    #[test]
    fn raw_handle_reap_continues_after_completion_errors() {
        let mut waited = Vec::new();
        let result = reap_all_raw_handles([41, 42, 43], |handle| {
            waited.push(handle.raw);
            if handle.raw == 41 { Err(()) } else { Ok(()) }
        });

        assert_eq!(result, Err(()));
        assert_eq!(waited, [41, 42, 43]);
    }

    #[test]
    fn physical_batch_admission_preserves_typed_failure_classes() {
        assert_eq!(
            physical_batch_not_submitted_reason(&DevError::Again),
            Some(PhysicalIoNotSubmittedReason::Backpressure)
        );
        assert_eq!(
            physical_batch_not_submitted_reason(&DevError::ResourceBusy),
            Some(PhysicalIoNotSubmittedReason::Backpressure)
        );
        assert_eq!(
            physical_batch_not_submitted_reason(&DevError::Unsupported),
            Some(PhysicalIoNotSubmittedReason::Unsupported)
        );
        assert_eq!(
            physical_batch_not_submitted_reason(&DevError::NoMemory),
            Some(PhysicalIoNotSubmittedReason::NoMemory)
        );
        assert_eq!(
            physical_batch_not_submitted_reason(&DevError::InvalidParam),
            Some(PhysicalIoNotSubmittedReason::Invalid)
        );
        assert_eq!(physical_batch_not_submitted_reason(&DevError::Io), None);
    }
}
