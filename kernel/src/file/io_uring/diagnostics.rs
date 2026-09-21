//! Direct-DMA fallback classification shared by the io_uring physical path.

/// Why a request left the synchronous physical-DMA fast path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoUringDmaFallbackReason {
    Geometry,
    Provenance,
    SgCap,
    Extent,
    DeviceAdmission,
}

#[inline(always)]
pub(crate) fn record_io_uring_physical_submitted(_bytes: usize, _qd: usize, _extents: usize) {}

#[inline(always)]
pub(crate) fn record_io_uring_physical_child_completed() {}

#[inline(always)]
pub(crate) fn record_io_uring_physical_completed(_bytes: usize) {}

#[inline(always)]
pub(crate) fn record_io_uring_physical_quarantine() {}

#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_read_hit(_bytes: usize) {}

#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_read_fallback(_reason: IoUringDmaFallbackReason) {}

#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_write_hit(_bytes: usize) {}

#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_write_fallback(_reason: IoUringDmaFallbackReason) {}
