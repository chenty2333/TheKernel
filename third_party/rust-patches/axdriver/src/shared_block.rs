use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
};
use core::cmp::min;

use axsync::{Mutex, MutexGuard};

use crate::{
    AxBlockDevice,
    prelude::{
        BaseDriverOps, BlockDriverOps, BlockQueueCaps, BlockQueueRequest, BlockRequestHandle,
        BlockSubmitReport, DevError, DevResult, DeviceType,
    },
};

struct SharedBlockDeviceInner {
    device: Mutex<AxBlockDevice>,
    name: String,
    device_type: DeviceType,
    irq: Option<usize>,
}

/// A cloneable, serialized handle to one block device.
///
/// Filesystems and raw block-device files can share this handle without
/// duplicating descriptor queues or bypassing driver synchronization.
#[derive(Clone)]
pub struct SharedBlockDevice {
    inner: Arc<SharedBlockDeviceInner>,
}

impl SharedBlockDevice {
    pub fn new(device: AxBlockDevice) -> Self {
        let name = device.device_name().to_string();
        let device_type = device.device_type();
        let irq = device.irq_num();
        Self {
            inner: Arc::new(SharedBlockDeviceInner {
                device: Mutex::new(device),
                name,
                device_type,
                irq,
            }),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, AxBlockDevice> {
        self.inner.device.lock()
    }

    pub fn byte_len(&self) -> u64 {
        self.num_blocks().saturating_mul(self.block_size() as u64)
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> DevResult<usize> {
        if buf.is_empty() || offset >= self.byte_len() {
            return Ok(0);
        }
        let len = min(buf.len() as u64, self.byte_len() - offset) as usize;
        let mut device = self.lock();
        read_at_locked(&mut *device, offset, &mut buf[..len])?;
        Ok(len)
    }

    pub fn write_at(&self, offset: u64, buf: &[u8]) -> DevResult<usize> {
        if buf.is_empty() || offset >= self.byte_len() {
            return Ok(0);
        }
        let len = min(buf.len() as u64, self.byte_len() - offset) as usize;
        let mut device = self.lock();
        write_at_locked(&mut *device, offset, &buf[..len])?;
        Ok(len)
    }
}

fn read_at_locked(
    device: &mut (impl BlockDriverOps + ?Sized),
    offset: u64,
    buf: &mut [u8],
) -> DevResult {
    let block_size = device.block_size();
    if block_size == 0 {
        return Err(DevError::InvalidParam);
    }

    let mut done = 0;
    let mut block = offset / block_size as u64;
    let block_offset = offset as usize % block_size;
    if block_offset != 0 {
        let mut scratch = vec![0; block_size];
        device.read_block(block, &mut scratch)?;
        let copied = min(buf.len(), block_size - block_offset);
        buf[..copied].copy_from_slice(&scratch[block_offset..block_offset + copied]);
        done += copied;
        block += 1;
    }

    let full_bytes = (buf.len() - done) / block_size * block_size;
    if full_bytes != 0 {
        device.read_block(block, &mut buf[done..done + full_bytes])?;
        done += full_bytes;
        block += (full_bytes / block_size) as u64;
    }

    if done != buf.len() {
        let mut scratch = vec![0; block_size];
        device.read_block(block, &mut scratch)?;
        let tail = buf.len() - done;
        buf[done..].copy_from_slice(&scratch[..tail]);
    }
    Ok(())
}

fn write_at_locked(
    device: &mut (impl BlockDriverOps + ?Sized),
    offset: u64,
    buf: &[u8],
) -> DevResult {
    let block_size = device.block_size();
    if block_size == 0 {
        return Err(DevError::InvalidParam);
    }

    let mut done = 0;
    let mut block = offset / block_size as u64;
    let block_offset = offset as usize % block_size;
    if block_offset != 0 {
        let mut scratch = vec![0; block_size];
        device.read_block(block, &mut scratch)?;
        let copied = min(buf.len(), block_size - block_offset);
        scratch[block_offset..block_offset + copied].copy_from_slice(&buf[..copied]);
        device.write_block(block, &scratch)?;
        done += copied;
        block += 1;
    }

    let full_bytes = (buf.len() - done) / block_size * block_size;
    if full_bytes != 0 {
        device.write_block(block, &buf[done..done + full_bytes])?;
        done += full_bytes;
        block += (full_bytes / block_size) as u64;
    }

    if done != buf.len() {
        let mut scratch = vec![0; block_size];
        device.read_block(block, &mut scratch)?;
        let tail = buf.len() - done;
        scratch[..tail].copy_from_slice(&buf[done..]);
        device.write_block(block, &scratch)?;
    }
    Ok(())
}

impl BaseDriverOps for SharedBlockDevice {
    fn device_name(&self) -> &str {
        &self.inner.name
    }

    fn device_type(&self) -> DeviceType {
        self.inner.device_type
    }

    fn irq_num(&self) -> Option<usize> {
        self.inner.irq
    }
}

impl BlockDriverOps for SharedBlockDevice {
    fn num_blocks(&self) -> u64 {
        self.lock().num_blocks()
    }

    fn block_size(&self) -> usize {
        self.lock().block_size()
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DevResult {
        self.lock().read_block(block_id, buf)
    }

    fn read_block_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> DevResult {
        self.lock().read_block_vectored(block_id, bufs)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        self.lock().write_block(block_id, buf)
    }

    fn write_block_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> DevResult {
        self.lock().write_block_vectored(block_id, bufs)
    }

    fn flush(&mut self) -> DevResult {
        self.lock().flush()
    }

    fn async_queue_caps(&self) -> Option<BlockQueueCaps> {
        self.lock().async_queue_caps()
    }

    fn submit_async_batch(
        &mut self,
        requests: &mut [BlockQueueRequest<'_>],
    ) -> DevResult<BlockSubmitReport> {
        self.lock().submit_async_batch(requests)
    }

    fn poll_async_complete(&mut self, budget: usize) -> DevResult<usize> {
        self.lock().poll_async_complete(budget)
    }

    fn wait_async_all(&mut self, handles: &[BlockRequestHandle]) -> DevResult {
        self.lock().wait_async_all(handles)
    }

    fn enable_irq(&mut self) -> DevResult {
        BlockDriverOps::enable_irq(&mut *self.lock())
    }

    fn disable_irq(&mut self) -> DevResult {
        BlockDriverOps::disable_irq(&mut *self.lock())
    }

    fn is_irq_enabled(&self) -> bool {
        self.lock().is_irq_enabled()
    }

    fn handle_irq(&mut self) -> DevResult<usize> {
        self.lock().handle_irq()
    }

    fn fence_async(&mut self) -> DevResult {
        self.lock().fence_async()
    }
}

#[cfg(all(test, block_dev = "ramdisk"))]
mod tests {
    use alloc::vec::Vec;

    use axdriver_block::ramdisk::RamDisk;

    use super::*;

    fn patterned_device(bytes: usize) -> (SharedBlockDevice, Vec<u8>) {
        let contents = (0..bytes)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let device = SharedBlockDevice::new(RamDisk::from(contents.as_slice()));
        (device, contents)
    }

    #[test]
    fn unaligned_read_crosses_blocks_and_stops_at_eof() {
        let (device, contents) = patterned_device(1024);
        let mut crossing = [0u8; 8];
        assert_eq!(device.read_at(509, &mut crossing).unwrap(), crossing.len());
        assert_eq!(&crossing, &contents[509..517]);

        let mut eof = [0xa5; 16];
        assert_eq!(device.read_at(1020, &mut eof).unwrap(), 4);
        assert_eq!(&eof[..4], &contents[1020..]);
        assert_eq!(&eof[4..], &[0xa5; 12]);
        assert_eq!(device.read_at(1024, &mut eof).unwrap(), 0);
    }

    #[test]
    fn unaligned_write_preserves_neighbors_and_stops_at_eof() {
        let (device, mut expected) = patterned_device(1024);
        let crossing = [0xf1, 0xf2, 0xf3, 0xf4, 0xf5];
        assert_eq!(device.write_at(510, &crossing).unwrap(), crossing.len());
        expected[510..515].copy_from_slice(&crossing);

        let tail = [0xe1, 0xe2, 0xe3, 0xe4];
        assert_eq!(device.write_at(1022, &tail).unwrap(), 2);
        expected[1022..].copy_from_slice(&tail[..2]);

        let mut actual = vec![0u8; expected.len()];
        assert_eq!(device.read_at(0, &mut actual).unwrap(), actual.len());
        assert_eq!(actual, expected);
        assert_eq!(device.write_at(1024, &tail).unwrap(), 0);
    }

    #[test]
    fn block_flush_is_forwarded() {
        let (mut device, _) = patterned_device(512);
        BlockDriverOps::flush(&mut device).unwrap();
    }
}
