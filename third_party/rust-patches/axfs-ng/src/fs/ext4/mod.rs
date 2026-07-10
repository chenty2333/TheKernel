mod fs;
mod inode;
mod util;

use alloc::{sync::Arc, vec::Vec};

use axdriver::prelude::{
    BlockAsyncOp, BlockDriverOps, BlockQueueRequest, BlockRequestHandle, BlockSegment, DevError,
};
pub use fs::*;
pub use inode::*;
use lwext4_rust::{
    AsyncReadStats, AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, Ext4Error, Ext4Result,
    ffi::EIO,
};

use crate::MountedBlockDevice;

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
        let handles = handles
            .into_iter()
            .map(|raw| BlockRequestHandle { raw })
            .collect::<Vec<_>>();
        if handles.is_empty() {
            return Ok(());
        }
        self.inner
            .device()
            .lock()
            .wait_async_all(&handles)
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

    fn try_read_blocks_vectored_async(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> Ext4Result<Option<AsyncReadStats>> {
        let mut dev = self.inner.device().lock();
        let Some(caps) = dev.async_queue_caps() else {
            return Ok(None);
        };
        if caps.max_requests == 0 || caps.max_descriptors == 0 {
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
            return Ok(None);
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
                .ok_or_else(|| Ext4Error::new(EIO as _, None))?;
            start = end;
        }

        let mut next = 0usize;
        let mut submit_batches = 0usize;
        let mut handles: Vec<BlockRequestHandle> = Vec::new();
        while next < requests.len() {
            let report = match dev.submit_async_batch(&mut requests[next..]) {
                Ok(report) => report,
                Err(DevError::Unsupported) if handles.is_empty() => return Ok(None),
                Err(_) => return Err(Ext4Error::new(EIO as _, None)),
            };
            if report.submitted == 0 {
                if handles.is_empty() {
                    return Ok(None);
                }
                dev.wait_async_all(&handles)
                    .map_err(|_| Ext4Error::new(EIO as _, None))?;
                handles.clear();
                continue;
            }

            submit_batches += 1;
            for request in requests[next..next + report.submitted].iter() {
                handles.push(
                    request
                        .handle
                        .ok_or_else(|| Ext4Error::new(EIO as _, None))?,
                );
            }
            next += report.submitted;
            if report.queue_full && next < requests.len() {
                dev.wait_async_all(&handles)
                    .map_err(|_| Ext4Error::new(EIO as _, None))?;
                handles.clear();
            }
        }

        if !handles.is_empty() {
            dev.wait_async_all(&handles)
                .map_err(|_| Ext4Error::new(EIO as _, None))?;
        }
        Ok(Some(AsyncReadStats { submit_batches }))
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
                .ok_or_else(|| Ext4Error::new(EIO as _, None))?;
            start = end;
        }
        if requests.len() > caps.default_depth {
            return Ok(None);
        }

        let report = match dev.submit_async_batch(&mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(_) => return Err(Ext4Error::new(EIO as _, None)),
        };
        if report.submitted != requests.len() || report.queue_full {
            let handles = requests
                .iter()
                .take(report.submitted)
                .filter_map(|request| request.handle)
                .collect::<Vec<_>>();
            if !handles.is_empty() {
                dev.wait_async_all(&handles)
                    .map_err(|_| Ext4Error::new(EIO as _, None))?;
            }
            return Ok(None);
        }

        Ok(Some(AsyncReadSubmission {
            handles: requests
                .iter()
                .map(|request| {
                    request
                        .handle
                        .ok_or_else(|| Ext4Error::new(EIO as _, None))
                        .map(|handle| handle.raw)
                })
                .collect::<Ext4Result<Vec<_>>>()?,
            bytes: report.bytes,
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
                .ok_or_else(|| Ext4Error::new(EIO as _, None))?;
            start = end;
        }
        if requests.len() > caps.default_depth {
            return Ok(None);
        }

        let report = match dev.submit_async_batch(&mut requests) {
            Ok(report) => report,
            Err(DevError::Unsupported) => return Ok(None),
            Err(_) => return Err(Ext4Error::new(EIO as _, None)),
        };
        if report.submitted != requests.len() || report.queue_full {
            let handles = requests
                .iter()
                .take(report.submitted)
                .filter_map(|request| request.handle)
                .collect::<Vec<_>>();
            if !handles.is_empty() {
                dev.wait_async_all(&handles)
                    .map_err(|_| Ext4Error::new(EIO as _, None))?;
            }
            return Ok(None);
        }

        Ok(Some(AsyncWriteSubmission {
            handles: requests
                .iter()
                .map(|request| {
                    request
                        .handle
                        .ok_or_else(|| Ext4Error::new(EIO as _, None))
                        .map(|handle| handle.raw)
                })
                .collect::<Ext4Result<Vec<_>>>()?,
            bytes: report.bytes,
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
