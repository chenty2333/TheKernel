use alloc::{boxed::Box, vec::Vec};
use core::{
    ffi::{c_int, c_void},
    mem, ptr, slice,
};

use crate::{Ext4Error, Ext4Result, error::Context, ffi::*};

/// Device block size.
pub const EXT4_DEV_BSIZE: usize = 512;

/// One caller-owned physical-memory range for a synchronous direct request.
/// This is kept dependency-neutral so the ext4 core does not depend on the
/// VFS crate or on a particular DMA implementation.
/// The owner must keep it pinned, DMA-accessible, and disjoint from every
/// other range for the complete call; concurrent CPU/device content races are
/// the caller's responsibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoSegment {
    pub paddr: usize,
    pub len: usize,
}

/// Terminal result of a direct physical-SG request.  `Quarantined` means the
/// lower driver published the descriptor but could not prove DMA retirement;
/// the caller must retain/transfer its pin owner to quarantine and must not
/// fall back through a virtual buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoSgOutcome {
    Completed(usize),
    NotSubmitted,
    Quarantined,
}

/// Why a physical batch was not published.  Every variant is a proof that no
/// descriptor became visible to the device, so the caller may still choose a
/// pre-publication fallback.  `Backpressure` is transient queue admission;
/// the other variants describe permanent capability, allocation, or request
/// validation failures and must not be conflated with queue pressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoNotSubmittedReason {
    Backpressure,
    Unsupported,
    NoMemory,
    Invalid,
}

/// One owned request description produced from a [`PhysicalIoPlan`].  The
/// fixed SG array is copied before publication, so a worker never borrows a
/// caller's registered-buffer descriptor while waiting for the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoBatchRequest {
    pub block_id: u64,
    pub operation: crate::fs::PhysicalIoOperation,
    pub segments: [PhysicalIoSegment; crate::fs::MAX_PHYSICAL_IO_SEGMENTS],
    pub segment_count: usize,
    pub bytes: usize,
}

impl PhysicalIoBatchRequest {
    pub const fn empty() -> Self {
        Self {
            block_id: 0,
            operation: crate::fs::PhysicalIoOperation::Read,
            segments: [PhysicalIoSegment { paddr: 0, len: 0 }; crate::fs::MAX_PHYSICAL_IO_SEGMENTS],
            segment_count: 0,
            bytes: 0,
        }
    }

    pub fn from_plan(plan: crate::fs::PhysicalIoPlan, extent_index: usize) -> Option<Self> {
        let extent = plan.extent(extent_index)?;
        let start = extent.segment_start();
        let count = extent.segment_count();
        let end = start.checked_add(count)?;
        if end > plan.segment_count() || count == 0 || count > crate::fs::MAX_PHYSICAL_IO_SEGMENTS {
            return None;
        }
        let mut segments =
            [PhysicalIoSegment { paddr: 0, len: 0 }; crate::fs::MAX_PHYSICAL_IO_SEGMENTS];
        for (index, segment) in plan.segments()[start..end].iter().copied().enumerate() {
            segments[index] = segment;
        }
        Some(Self {
            block_id: extent.physical_block_id(),
            operation: plan.operation(),
            segments,
            segment_count: count,
            bytes: extent.bytes(),
        })
    }

    pub fn physical_segments(&self) -> &[PhysicalIoSegment] {
        &self.segments[..self.segment_count]
    }
}

/// Handles returned by one physical batch publication.  The raw value is an
/// opaque generation-safe driver handle; completion cookies are reported by
/// the driver's exact completion drain and must be paired with these handles
/// by the effect owner.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PhysicalIoBatchSubmission {
    pub handles: Vec<u64>,
    /// Non-zero cookies assigned by the driver before publication, one per
    /// accepted handle.  A non-empty accepted submission with missing or
    /// zero cookies is malformed and must remain terminal rather than
    /// guessing an identity from a later completion.
    pub cookies: Vec<u64>,
    pub bytes: usize,
    pub submitted: usize,
    /// A partial report is terminal: accepted handles remain owned by the
    /// caller for quiescence and the operation must not fall back.
    pub terminal: bool,
}

/// Result of one all-or-none physical batch admission attempt.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PhysicalIoBatchSubmitOutcome {
    /// No descriptor was published and the reason is explicit.
    NotSubmitted(PhysicalIoNotSubmittedReason),
    /// The returned submission owns every descriptor accepted by the lower
    /// route.  A partial or malformed submission remains terminal inside the
    /// effect state machine and cannot become a fallback.
    Submitted(PhysicalIoBatchSubmission),
}

/// Result metadata for a bounded device-level physical completion wait.  The
/// completion records themselves remain in the caller-owned output slice so a
/// worker can demultiplex them among multiple published effects.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysicalIoCompletionDrain {
    pub completed: usize,
    pub continuation: bool,
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
            let read = match self.read_blocks(cur_block, buf) {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += read;
            cur_block = match cur_block.checked_add((read / EXT4_DEV_BSIZE) as u64) {
                Some(cur_block) => cur_block,
                None if total != 0 => break,
                None => return Err(Ext4Error::new(EIO as _, "vectored read block overflow")),
            };
            if read < buf.len() || read == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts a direct read into caller-pinned physical SG memory.
    /// `NotSubmitted` means that the underlying device has no physical-SG
    /// path. `Quarantined` is reset-required custody, never fallback.
    ///
    /// # Safety
    ///
    /// Every segment must remain pinned, DMA-accessible, writable, and disjoint
    /// from every other segment until this synchronous call returns. Concurrent
    /// CPU/device content races are the caller's responsibility.
    unsafe fn read_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<PhysicalIoSgOutcome> {
        let _ = (block_id, segments);
        Ok(PhysicalIoSgOutcome::NotSubmitted)
    }

    /// Attempts to submit a scatter-list read through an async block queue.
    ///
    /// Returns `Ok(None)` when the request cannot be submitted without a
    /// lock-internal wait. This is an all-or-none submit boundary: `Some` must
    /// cover the complete scatter list, and `None` or `Err` must leave no
    /// request able to access the buffers. The method itself must not wait for
    /// completion. Callers keep all buffers alive until the returned handles
    /// have completed.
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
    /// lock-internal wait. This is an all-or-none submit boundary: `Some` must
    /// cover the complete scatter list, and `None` or `Err` must leave no
    /// request able to access the buffers. The method itself must not wait for
    /// completion. Callers keep all buffers alive until the returned handles
    /// have completed.
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
            let written = match self.write_blocks(cur_block, buf) {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            total += written;
            cur_block = match cur_block.checked_add((written / EXT4_DEV_BSIZE) as u64) {
                Some(cur_block) => cur_block,
                None if total != 0 => break,
                None => return Err(Ext4Error::new(EIO as _, "vectored write block overflow")),
            };
            if written < buf.len() || written == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// Attempts a direct overwrite from caller-pinned physical SG memory.
    /// `NotSubmitted` means that the underlying device has no physical-SG
    /// path. `Quarantined` is reset-required custody, never fallback.
    ///
    /// # Safety
    ///
    /// Every segment must remain pinned, DMA-accessible, readable, and disjoint
    /// from every other segment until this synchronous call returns. Concurrent
    /// CPU/device content races are the caller's responsibility.
    unsafe fn write_blocks_physical_sg(
        &mut self,
        block_id: u64,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<PhysicalIoSgOutcome> {
        let _ = (block_id, segments);
        Ok(PhysicalIoSgOutcome::NotSubmitted)
    }

    /// Atomically publishes all requests from one owned physical plan.  A
    /// `NotSubmitted` result means no descriptor was published and the caller
    /// may use its synchronous fallback.  A returned submission is never a
    /// fallback:
    /// when `terminal` is true it contains the accepted prefix whose handles
    /// must be retained until exact completion/quiescence.
    unsafe fn submit_physical_batch(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        let _ = requests;
        Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
            PhysicalIoNotSubmittedReason::Unsupported,
        ))
    }

    /// Publishes an owned physical batch for the device-global kernel worker.
    /// Implementations with a shared lower broker may select its kernel route;
    /// the default preserves the exact-route behavior of simple devices.
    unsafe fn submit_physical_batch_with_route(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
        _kernel_worker: bool,
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        unsafe { self.submit_physical_batch(requests) }
    }

    unsafe fn submit_physical_batch_kernel(
        &mut self,
        requests: &[PhysicalIoBatchRequest],
    ) -> Ext4Result<PhysicalIoBatchSubmitOutcome> {
        unsafe { self.submit_physical_batch_with_route(requests, true) }
    }

    /// Waits for at least one concrete physical completion from the shared
    /// device owner and returns a bounded, device-level batch.  This method is
    /// intentionally effect-agnostic: callers must match the returned handle
    /// and cookie to their own published effect.  The default is an explicit
    /// unsupported error and must not look like a successful empty drain.
    fn wait_any_physical_completion(
        &mut self,
        output: &mut [crate::fs::PhysicalIoCompletion],
    ) -> Ext4Result<PhysicalIoCompletionDrain> {
        let _ = output;
        Err(Ext4Error::new(
            EIO as _,
            "physical completion wait unsupported",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FaultingBlockDevice {
        fail_read_call: usize,
        fail_write_call: usize,
        read_calls: usize,
        write_calls: usize,
    }

    impl FaultingBlockDevice {
        fn new(fail_read_call: usize, fail_write_call: usize) -> Self {
            Self {
                fail_read_call,
                fail_write_call,
                read_calls: 0,
                write_calls: 0,
            }
        }
    }

    impl BlockDevice for FaultingBlockDevice {
        fn write_blocks(&mut self, _block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
            let call = self.write_calls;
            self.write_calls += 1;
            if call == self.fail_write_call {
                Err(Ext4Error::new(EIO as _, "injected write failure"))
            } else {
                Ok(buf.len())
            }
        }

        fn read_blocks(&mut self, _block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
            let call = self.read_calls;
            self.read_calls += 1;
            if call == self.fail_read_call {
                Err(Ext4Error::new(EIO as _, "injected read failure"))
            } else {
                buf.fill(call as u8 + 1);
                Ok(buf.len())
            }
        }

        fn num_blocks(&self) -> Ext4Result<u64> {
            Ok(u64::MAX)
        }
    }

    #[test]
    fn default_vectored_read_preserves_prefix_before_later_error() {
        let mut device = FaultingBlockDevice::new(1, usize::MAX);
        let mut first = [0u8; EXT4_DEV_BSIZE];
        let mut second = [0u8; EXT4_DEV_BSIZE];
        let mut bufs: [&mut [u8]; 2] = [&mut first, &mut second];

        assert_eq!(
            device.read_blocks_vectored(0, &mut bufs).unwrap(),
            EXT4_DEV_BSIZE
        );
        assert!(first.iter().all(|byte| *byte == 1));
        assert!(second.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn default_vectored_write_preserves_prefix_before_later_error() {
        let mut device = FaultingBlockDevice::new(usize::MAX, 1);
        let first = [1u8; EXT4_DEV_BSIZE];
        let second = [2u8; EXT4_DEV_BSIZE];
        let bufs: [&[u8]; 2] = [&first, &second];

        assert_eq!(
            device.write_blocks_vectored(0, &bufs).unwrap(),
            EXT4_DEV_BSIZE
        );
        assert_eq!(device.write_calls, 2);
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
