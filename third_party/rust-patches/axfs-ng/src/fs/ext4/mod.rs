mod fs;
mod inode;
mod util;

use alloc::{sync::Arc, vec::Vec};

use axdriver::prelude::{
    BlockAsyncOp, BlockDriverOps, BlockPhysicalSegment, BlockQueueRequest, BlockRequestHandle,
    BlockSegment, DevError,
};
pub use fs::*;
pub use inode::*;
use lwext4_rust::{
    AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, Ext4Error, Ext4Result,
    PhysicalIoSegment, ffi::EIO,
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

    pub(crate) fn wait_async_write(&self, submission: &AsyncWriteSubmission) -> Ext4Result<()> {
        self.wait_async_handles(submission.handles.iter().copied())
    }

    pub(crate) fn wait_async_read(&self, submission: &AsyncReadSubmission) -> Ext4Result<()> {
        self.wait_async_handles(submission.handles.iter().copied())
    }

    fn wait_async_handles(&self, handles: impl IntoIterator<Item = u64>) -> Ext4Result<()> {
        let mut dev = self.inner.device().lock();
        reap_all_raw_handles(handles, |handle| {
            dev.wait_async_all(core::slice::from_ref(&handle))
        })
        .map_err(|_| Ext4Error::new(EIO as _, None))
    }
}

impl BlockDevice for Ext4Disk {
    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        self.inner
            .device()
            .lock()
            .read_block(block_id, buf)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(buf.len())
    }

    fn read_blocks_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> Ext4Result<usize> {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        self.inner
            .device()
            .lock()
            .read_block_vectored(block_id, bufs)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(bytes)
    }

    unsafe fn read_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<Option<usize>> {
        const MAX_PHYSICAL_SG: usize = 4;
        if segments.is_empty() || segments.len() > MAX_PHYSICAL_SG {
            return Ok(None);
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
        let mut dev = self.inner.device().lock();
        match unsafe { dev.read_block_physical_sg(block_id, &physical[..segments.len()]) } {
            Ok(()) => Ok(Some(bytes)),
            Err(DevError::Unsupported) => Ok(None),
            Err(_) => Err(Ext4Error::new(EIO as _, Some("physical SG read failed"))),
        }
    }

    fn try_read_blocks_vectored_async_submit(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        let mut dev = self.inner.device().lock();
        let Some(caps) = dev.async_queue_caps() else {
            return Ok(None);
        };
        if caps.default_depth == 0 || caps.max_requests == 0 || caps.max_descriptors == 0 {
            return Ok(None);
        }

        let block_size = dev.block_size();
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

        let report = match dev.submit_async_batch(&mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(_) => return Err(Ext4Error::new(EIO as _, None)),
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
            let _ = dev.wait_async_all(core::slice::from_ref(&handle));
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
        let mut dev = self.inner.device().lock();
        let Some(caps) = dev.async_queue_caps() else {
            return Ok(None);
        };
        if caps.default_depth == 0 || caps.max_requests == 0 || caps.max_descriptors == 0 {
            return Ok(None);
        }

        let block_size = dev.block_size();
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

        let report = match dev.submit_async_batch(&mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(_) => return Err(Ext4Error::new(EIO as _, None)),
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
            let _ = dev.wait_async_all(core::slice::from_ref(&handle));
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
        self.inner
            .device()
            .lock()
            .write_block(block_id, buf)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(buf.len())
    }

    fn write_blocks_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> Ext4Result<usize> {
        let bytes = bufs.iter().map(|buf| buf.len()).sum();
        self.inner
            .device()
            .lock()
            .write_block_vectored(block_id, bufs)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(bytes)
    }

    unsafe fn write_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<Option<usize>> {
        const MAX_PHYSICAL_SG: usize = 4;
        if segments.is_empty() || segments.len() > MAX_PHYSICAL_SG {
            return Ok(None);
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
        let mut dev = self.inner.device().lock();
        match unsafe { dev.write_block_physical_sg(block_id, &physical[..segments.len()]) } {
            Ok(()) => Ok(Some(bytes)),
            Err(DevError::Unsupported) => Ok(None),
            Err(_) => Err(Ext4Error::new(EIO as _, Some("physical SG write failed"))),
        }
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.inner.device().num_blocks())
    }

    fn flush(&mut self) -> Ext4Result<()> {
        self.inner
            .device()
            .lock()
            .flush()
            .map_err(|_| Ext4Error::new(EIO as _, None))
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
}
