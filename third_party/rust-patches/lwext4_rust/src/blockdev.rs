use alloc::{boxed::Box, vec::Vec};
use core::{
    ffi::{c_int, c_void},
    mem, ptr, slice,
};

use crate::{Ext4Error, Ext4Result, error::Context, ffi::*};

/// Device block size.
pub const EXT4_DEV_BSIZE: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AsyncReadStats {
    pub submit_batches: usize,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct AsyncReadSubmission {
    pub handles: Vec<u64>,
    pub bytes: usize,
    pub submit_batches: usize,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct AsyncWriteSubmission {
    pub handles: Vec<u64>,
    pub bytes: usize,
    pub submit_batches: usize,
}

pub trait BlockDevice {
    /// Writes blocks to the device, starting from the given block ID.
    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize>;

    /// Reads blocks from the device, starting from the given block ID.
    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize>;

    /// Reads blocks into a scatter list, starting from the given block ID.
    fn read_blocks_vectored(&mut self, block_id: u64, bufs: &mut [&mut [u8]]) -> Ext4Result<usize> {
        let mut total = 0usize;
        let mut cur_block = block_id;
        for buf in bufs.iter_mut() {
            if buf.is_empty() {
                continue;
            }
            let read = self.read_blocks(cur_block, buf)?;
            total += read;
            cur_block += (read / EXT4_DEV_BSIZE) as u64;
            if read < buf.len() || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts to read a scatter list through an async block queue.
    ///
    /// Returns `Ok(None)` when the underlying device cannot accept this request
    /// through its async queue. Callers must keep all buffers alive until this
    /// method returns, at which point accepted requests are complete.
    fn try_read_blocks_vectored_async(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> Ext4Result<Option<AsyncReadStats>> {
        let _ = block_id;
        let _ = bufs;
        Ok(None)
    }

    /// Attempts to submit a scatter-list read through an async block queue.
    ///
    /// Returns `Ok(None)` when the request cannot be submitted without a
    /// lock-internal wait. Callers must keep all buffers alive until the
    /// returned handles have completed.
    fn try_read_blocks_vectored_async_submit(
        &mut self,
        block_id: u64,
        bufs: &mut [&mut [u8]],
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        let _ = block_id;
        let _ = bufs;
        Ok(None)
    }

    /// Attempts to submit a scatter-list write through an async block queue.
    ///
    /// Returns `Ok(None)` when the request cannot be submitted without a
    /// lock-internal wait. Callers must keep all buffers alive until the
    /// returned handles have completed.
    fn try_write_blocks_vectored_async_submit(
        &mut self,
        block_id: u64,
        bufs: &[&[u8]],
    ) -> Ext4Result<Option<AsyncWriteSubmission>> {
        let _ = block_id;
        let _ = bufs;
        Ok(None)
    }

    /// Gets the number of blocks on the device.
    fn num_blocks(&self) -> Ext4Result<u64>;

    /// Flushes the underlying block device after filesystem cache writeback.
    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }

    /// Writes blocks from a scatter list, starting from the given block ID.
    fn write_blocks_vectored(&mut self, block_id: u64, bufs: &[&[u8]]) -> Ext4Result<usize> {
        let mut total = 0usize;
        let mut cur_block = block_id;
        for buf in bufs.iter().copied() {
            if buf.is_empty() {
                continue;
            }
            let written = self.write_blocks(cur_block, buf)?;
            total += written;
            cur_block += (written / EXT4_DEV_BSIZE) as u64;
            if written < buf.len() || written == 0 {
                break;
            }
        }
        Ok(total)
    }
}

/// Holds necessary resources for the ext4 block device, and automatically frees
/// them when the instance is dropped.
#[allow(dead_code)]
struct ResourceGuard<Dev> {
    dev: Box<Dev>,
    block_buf: Box<[u8; EXT4_DEV_BSIZE]>,
    block_cache_buf: Box<ext4_bcache>,
    block_dev_iface: Box<ext4_blockdev_iface>,
}

pub struct Ext4BlockDevice<Dev: BlockDevice> {
    pub(crate) inner: Box<ext4_blockdev>,
    _guard: ResourceGuard<Dev>,
}

impl<Dev: BlockDevice> Ext4BlockDevice<Dev> {
    pub fn new(dev: Dev) -> Ext4Result<Self> {
        let allocation_error =
            || Ext4Error::new(ENOMEM as _, "ext4 block-device allocation failed");
        let mut dev = Box::try_new(dev).map_err(|_| allocation_error())?;

        // Block size buffer
        let mut block_buf = Box::try_new([0u8; EXT4_DEV_BSIZE]).map_err(|_| allocation_error())?;
        let mut block_dev_iface = Box::try_new(ext4_blockdev_iface {
            open: Some(Self::dev_open),
            bread: Some(Self::dev_bread),
            bwrite: Some(Self::dev_bwrite),
            close: Some(Self::dev_close),
            flush: Some(Self::dev_flush),
            lock: None,
            unlock: None,
            ph_bsize: EXT4_DEV_BSIZE as u32,
            ph_bcnt: 0,
            ph_bbuf: block_buf.as_mut_ptr(),
            ph_refctr: 0,
            bread_ctr: 0,
            bwrite_ctr: 0,
            p_user: dev.as_mut() as *mut _ as *mut c_void,
        })
        .map_err(|_| allocation_error())?;

        let mut block_cache_buf: Box<ext4_bcache> =
            Box::try_new(unsafe { mem::zeroed() }).map_err(|_| allocation_error())?;
        let mut blockdev = Box::try_new(ext4_blockdev {
            bdif: block_dev_iface.as_mut(),
            part_offset: 0,
            part_size: 0,
            bc: block_cache_buf.as_mut(),
            lg_bsize: 0,
            lg_bcnt: 0,
            cache_write_back: 0,
            writeback_error: 0,
            fs: ptr::null_mut(),
            journal: ptr::null_mut(),
        })
        .map_err(|_| allocation_error())?;

        unsafe {
            ext4_block_init(blockdev.as_mut()).context("ext4_block_init")?;
            ext4_block_cache_write_back(blockdev.as_mut(), 1)
                .context("ext4_block_cache_write_back")
                .inspect_err(|_| {
                    ext4_block_fini(blockdev.as_mut());
                })?;
        }
        Ok(Self {
            inner: blockdev,
            _guard: ResourceGuard {
                dev,
                block_buf,
                block_cache_buf,
                block_dev_iface,
            },
        })
    }

    pub(crate) fn dev_mut(&mut self) -> &mut Dev {
        self._guard.dev.as_mut()
    }

    pub(crate) fn direct_physical_block_id(&self, logical_block_id: u64) -> u64 {
        let bdev = self.inner.as_ref();
        let bdif = unsafe { &*bdev.bdif };
        (logical_block_id * bdev.lg_bsize as u64 + bdev.part_offset) / bdif.ph_bsize as u64
    }

    pub(crate) fn invalidate_logical_block_range(
        &mut self,
        logical_block_id: u64,
        block_count: u32,
    ) {
        if block_count == 0 {
            return;
        }
        unsafe {
            ext4_bcache_invalidate_lba(self.inner.as_mut().bc, logical_block_id, block_count);
        }
    }

    unsafe fn dev_read_fields<'a>(
        bdev: *mut ext4_blockdev,
    ) -> (
        &'a mut ext4_blockdev,
        &'a mut ext4_blockdev_iface,
        &'a mut Dev,
    ) {
        let bdev = unsafe { &mut *bdev };
        let bdif = unsafe { &mut *bdev.bdif };
        let dev = unsafe { &mut *(bdif.p_user as *mut Dev) };
        (bdev, bdif, dev)
    }
    unsafe extern "C" fn dev_open(bdev: *mut ext4_blockdev) -> c_int {
        debug!("open ext4 block device");
        let (bdev, bdif, dev) = unsafe { Self::dev_read_fields(bdev) };

        bdif.ph_bcnt = match dev.num_blocks() {
            Ok(cur) => cur,
            Err(err) => {
                error!("num_blocks failed: {err:?}");
                return EIO as _;
            }
        };

        bdev.part_offset = 0;
        bdev.part_size = bdif.ph_bcnt * bdif.ph_bsize as u64;
        EOK as _
    }
    unsafe extern "C" fn dev_bread(
        bdev: *mut ext4_blockdev,
        buf: *mut c_void,
        blk_id: u64,
        blk_cnt: u32,
    ) -> c_int {
        trace!("read ext4 block id={blk_id} count={blk_cnt}");
        if blk_cnt == 0 {
            return EOK as _;
        }

        let (_bdev, bdif, dev) = unsafe { Self::dev_read_fields(bdev) };
        let buf_len = (bdif.ph_bsize * blk_cnt) as usize;
        let buffer = unsafe { slice::from_raw_parts_mut(buf as *mut u8, buf_len) };
        if let Err(err) = dev.read_blocks(blk_id, buffer) {
            error!("read_blocks failed: {err:?}");
            return EIO as _;
        }

        EOK as _
    }
    unsafe extern "C" fn dev_bwrite(
        bdev: *mut ext4_blockdev,
        buf: *const c_void,
        blk_id: u64,
        blk_cnt: u32,
    ) -> c_int {
        trace!("write ext4 block id={blk_id} count={blk_cnt}");
        if blk_cnt == 0 {
            return EOK as _;
        }

        let (_bdev, bdif, dev) = unsafe { Self::dev_read_fields(bdev) };
        let buf_len = (bdif.ph_bsize * blk_cnt) as usize;
        let buffer = unsafe { slice::from_raw_parts(buf as *const u8, buf_len) };
        if let Err(err) = dev.write_blocks(blk_id, buffer) {
            error!("read_blocks failed: {err:?}");
            return EIO as _;
        }

        // drop_cache();
        // sync

        EOK as _
    }
    unsafe extern "C" fn dev_close(_bdev: *mut ext4_blockdev) -> c_int {
        debug!("close ext4 block device");
        EOK as _
    }

    unsafe extern "C" fn dev_flush(bdev: *mut ext4_blockdev) -> c_int {
        let (_bdev, _bdif, dev) = unsafe { Self::dev_read_fields(bdev) };
        if let Err(err) = dev.flush() {
            error!("flush failed: {err:?}");
            return EIO as _;
        }
        EOK as _
    }
}

impl<Dev: BlockDevice> Drop for Ext4BlockDevice<Dev> {
    fn drop(&mut self) {
        unsafe {
            let bdev = self.inner.as_mut();
            let result = ext4_block_fini(bdev);
            if result != EOK as _ {
                error!("failed to close ext4 block device: {result}");
            }
        }
    }
}
