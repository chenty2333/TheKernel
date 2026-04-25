use axdriver_base::{BaseDriverOps, DevResult, DeviceType};
use axdriver_block::BlockDriverOps;
use spin::Mutex;
use virtio_drivers::{device::blk::VirtIOBlk as InnerDev, transport::Transport, Hal};

use crate::as_dev_err;

/// The VirtIO block device driver.
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    inner: Mutex<InnerDev<H, T>>,
}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DevResult<Self> {
        let inner = InnerDev::new(transport).map_err(as_dev_err)?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }
}

impl<H: Hal, T: Transport + Send> BaseDriverOps for VirtIoBlkDev<H, T> {
    fn device_name(&self) -> &str {
        "virtio-blk"
    }

    fn device_type(&self) -> DeviceType {
        DeviceType::Block
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
        self.inner
            .lock()
            .read_blocks(block_id as _, buf)
            .map_err(as_dev_err)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DevResult {
        self.inner
            .lock()
            .write_blocks(block_id as _, buf)
            .map_err(as_dev_err)
    }

    fn flush(&mut self) -> DevResult {
        self.inner.lock().flush().map_err(as_dev_err)
    }
}
