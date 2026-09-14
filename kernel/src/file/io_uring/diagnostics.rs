//! Optional direct-DMA counters; not part of the completion protocol.

#[cfg(feature = "test-io-control")]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Counters for the synchronous physical-DMA fast path used by fixed-buffer
/// io_uring requests.  The counters are deliberately kept next to the ring
/// lease because the path is a lease-owned optimization rather than a generic
/// user-I/O property.
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_STATS_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_HITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACK_GEOMETRY: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACK_PROVENANCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACK_SG_CAP: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACK_EXTENT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_READ_FALLBACK_DEVICE_ADMISSION: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_HITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACK_GEOMETRY: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACK_PROVENANCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACK_SG_CAP: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACK_EXTENT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_DMA_DIRECT_WRITE_FALLBACK_DEVICE_ADMISSION: AtomicU64 =
    AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_SUBMITTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_CHILD_SUBMITTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_COMPLETED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_CHILD_COMPLETED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_DIRECT_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_QD_HIGHWATER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_EXTENT_HIGHWATER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
pub(super) static IO_URING_PHYSICAL_QUARANTINE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IoUringDmaFallbackReason {
    Geometry,
    Provenance,
    SgCap,
    Extent,
    DeviceAdmission,
}

#[cfg(feature = "test-io-control")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IoUringDmaDirectStats {
    pub read_hits: u64,
    pub read_bytes: u64,
    pub read_fallbacks: u64,
    pub read_fallback_geometry: u64,
    pub read_fallback_provenance: u64,
    pub read_fallback_sg_cap: u64,
    pub read_fallback_extent: u64,
    pub read_fallback_device_admission: u64,
    pub write_hits: u64,
    pub write_bytes: u64,
    pub write_fallbacks: u64,
    pub write_fallback_geometry: u64,
    pub write_fallback_provenance: u64,
    pub write_fallback_sg_cap: u64,
    pub write_fallback_extent: u64,
    pub write_fallback_device_admission: u64,
    pub physical_submitted: u64,
    pub physical_child_submitted: u64,
    pub physical_completed: u64,
    pub physical_child_completed: u64,
    pub physical_direct_bytes: u64,
    pub physical_qd_highwater: u64,
    pub physical_extent_highwater: u64,
    pub physical_quarantine: u64,
}

#[cfg(feature = "test-io-control")]
pub(crate) fn set_io_uring_dma_direct_stats_enabled(enabled: bool) {
    IO_URING_DMA_DIRECT_STATS_ENABLED.store(enabled, Ordering::Relaxed);
}

#[cfg(feature = "test-io-control")]
pub(crate) fn reset_io_uring_dma_direct_stats() {
    for counter in [
        &IO_URING_DMA_DIRECT_READ_HITS,
        &IO_URING_DMA_DIRECT_READ_BYTES,
        &IO_URING_DMA_DIRECT_READ_FALLBACKS,
        &IO_URING_DMA_DIRECT_READ_FALLBACK_GEOMETRY,
        &IO_URING_DMA_DIRECT_READ_FALLBACK_PROVENANCE,
        &IO_URING_DMA_DIRECT_READ_FALLBACK_SG_CAP,
        &IO_URING_DMA_DIRECT_READ_FALLBACK_EXTENT,
        &IO_URING_DMA_DIRECT_READ_FALLBACK_DEVICE_ADMISSION,
        &IO_URING_DMA_DIRECT_WRITE_HITS,
        &IO_URING_DMA_DIRECT_WRITE_BYTES,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACKS,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACK_GEOMETRY,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACK_PROVENANCE,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACK_SG_CAP,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACK_EXTENT,
        &IO_URING_DMA_DIRECT_WRITE_FALLBACK_DEVICE_ADMISSION,
        &IO_URING_PHYSICAL_SUBMITTED,
        &IO_URING_PHYSICAL_CHILD_SUBMITTED,
        &IO_URING_PHYSICAL_COMPLETED,
        &IO_URING_PHYSICAL_CHILD_COMPLETED,
        &IO_URING_PHYSICAL_DIRECT_BYTES,
        &IO_URING_PHYSICAL_QD_HIGHWATER,
        &IO_URING_PHYSICAL_EXTENT_HIGHWATER,
        &IO_URING_PHYSICAL_QUARANTINE,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

#[cfg(feature = "test-io-control")]
pub(crate) fn io_uring_dma_direct_stats_snapshot() -> IoUringDmaDirectStats {
    IoUringDmaDirectStats {
        read_hits: IO_URING_DMA_DIRECT_READ_HITS.load(Ordering::Relaxed),
        read_bytes: IO_URING_DMA_DIRECT_READ_BYTES.load(Ordering::Relaxed),
        read_fallbacks: IO_URING_DMA_DIRECT_READ_FALLBACKS.load(Ordering::Relaxed),
        read_fallback_geometry: IO_URING_DMA_DIRECT_READ_FALLBACK_GEOMETRY.load(Ordering::Relaxed),
        read_fallback_provenance: IO_URING_DMA_DIRECT_READ_FALLBACK_PROVENANCE
            .load(Ordering::Relaxed),
        read_fallback_sg_cap: IO_URING_DMA_DIRECT_READ_FALLBACK_SG_CAP.load(Ordering::Relaxed),
        read_fallback_extent: IO_URING_DMA_DIRECT_READ_FALLBACK_EXTENT.load(Ordering::Relaxed),
        read_fallback_device_admission: IO_URING_DMA_DIRECT_READ_FALLBACK_DEVICE_ADMISSION
            .load(Ordering::Relaxed),
        write_hits: IO_URING_DMA_DIRECT_WRITE_HITS.load(Ordering::Relaxed),
        write_bytes: IO_URING_DMA_DIRECT_WRITE_BYTES.load(Ordering::Relaxed),
        write_fallbacks: IO_URING_DMA_DIRECT_WRITE_FALLBACKS.load(Ordering::Relaxed),
        write_fallback_geometry: IO_URING_DMA_DIRECT_WRITE_FALLBACK_GEOMETRY
            .load(Ordering::Relaxed),
        write_fallback_provenance: IO_URING_DMA_DIRECT_WRITE_FALLBACK_PROVENANCE
            .load(Ordering::Relaxed),
        write_fallback_sg_cap: IO_URING_DMA_DIRECT_WRITE_FALLBACK_SG_CAP.load(Ordering::Relaxed),
        write_fallback_extent: IO_URING_DMA_DIRECT_WRITE_FALLBACK_EXTENT.load(Ordering::Relaxed),
        write_fallback_device_admission: IO_URING_DMA_DIRECT_WRITE_FALLBACK_DEVICE_ADMISSION
            .load(Ordering::Relaxed),
        physical_submitted: IO_URING_PHYSICAL_SUBMITTED.load(Ordering::Relaxed),
        physical_child_submitted: IO_URING_PHYSICAL_CHILD_SUBMITTED.load(Ordering::Relaxed),
        physical_completed: IO_URING_PHYSICAL_COMPLETED.load(Ordering::Relaxed),
        physical_child_completed: IO_URING_PHYSICAL_CHILD_COMPLETED.load(Ordering::Relaxed),
        physical_direct_bytes: IO_URING_PHYSICAL_DIRECT_BYTES.load(Ordering::Relaxed),
        physical_qd_highwater: IO_URING_PHYSICAL_QD_HIGHWATER.load(Ordering::Relaxed),
        physical_extent_highwater: IO_URING_PHYSICAL_EXTENT_HIGHWATER.load(Ordering::Relaxed),
        physical_quarantine: IO_URING_PHYSICAL_QUARANTINE.load(Ordering::Relaxed),
    }
}

#[cfg(feature = "test-io-control")]
pub(crate) fn record_io_uring_physical_submitted(bytes: usize, qd: usize, extents: usize) {
    if !IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    IO_URING_PHYSICAL_SUBMITTED.fetch_add(1, Ordering::Relaxed);
    IO_URING_PHYSICAL_CHILD_SUBMITTED.fetch_add(extents as u64, Ordering::Relaxed);
    let _ =
        IO_URING_PHYSICAL_QD_HIGHWATER.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (qd as u64 > current).then_some(qd as u64)
        });
    let _ = IO_URING_PHYSICAL_EXTENT_HIGHWATER.try_update(
        Ordering::AcqRel,
        Ordering::Acquire,
        |current| (extents as u64 > current).then_some(extents as u64),
    );
    let _ = bytes;
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_physical_submitted(_bytes: usize, _qd: usize, _extents: usize) {}

#[cfg(feature = "test-io-control")]
pub(crate) fn record_io_uring_physical_child_completed() {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        IO_URING_PHYSICAL_CHILD_COMPLETED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_physical_child_completed() {}

#[cfg(feature = "test-io-control")]
pub(crate) fn record_io_uring_physical_completed(bytes: usize) {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        IO_URING_PHYSICAL_COMPLETED.fetch_add(1, Ordering::Relaxed);
        IO_URING_PHYSICAL_DIRECT_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_physical_completed(_bytes: usize) {}

#[cfg(feature = "test-io-control")]
pub(crate) fn record_io_uring_physical_quarantine() {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        IO_URING_PHYSICAL_QUARANTINE.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_physical_quarantine() {}

#[cfg(feature = "test-io-control")]
#[inline]
pub(crate) fn record_io_uring_dma_direct_read_hit(bytes: usize) {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) && bytes != 0 {
        IO_URING_DMA_DIRECT_READ_HITS.fetch_add(1, Ordering::Relaxed);
        IO_URING_DMA_DIRECT_READ_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_read_hit(_bytes: usize) {}

#[cfg(feature = "test-io-control")]
#[inline]
pub(crate) fn record_io_uring_dma_direct_read_fallback(reason: IoUringDmaFallbackReason) {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        IO_URING_DMA_DIRECT_READ_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        let counter = match reason {
            IoUringDmaFallbackReason::Geometry => &IO_URING_DMA_DIRECT_READ_FALLBACK_GEOMETRY,
            IoUringDmaFallbackReason::Provenance => &IO_URING_DMA_DIRECT_READ_FALLBACK_PROVENANCE,
            IoUringDmaFallbackReason::SgCap => &IO_URING_DMA_DIRECT_READ_FALLBACK_SG_CAP,
            IoUringDmaFallbackReason::Extent => &IO_URING_DMA_DIRECT_READ_FALLBACK_EXTENT,
            IoUringDmaFallbackReason::DeviceAdmission => {
                &IO_URING_DMA_DIRECT_READ_FALLBACK_DEVICE_ADMISSION
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_read_fallback(_reason: IoUringDmaFallbackReason) {}

#[cfg(feature = "test-io-control")]
#[inline]
pub(crate) fn record_io_uring_dma_direct_write_hit(bytes: usize) {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) && bytes != 0 {
        IO_URING_DMA_DIRECT_WRITE_HITS.fetch_add(1, Ordering::Relaxed);
        IO_URING_DMA_DIRECT_WRITE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_write_hit(_bytes: usize) {}

#[cfg(feature = "test-io-control")]
#[inline]
pub(crate) fn record_io_uring_dma_direct_write_fallback(reason: IoUringDmaFallbackReason) {
    if IO_URING_DMA_DIRECT_STATS_ENABLED.load(Ordering::Relaxed) {
        IO_URING_DMA_DIRECT_WRITE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        let counter = match reason {
            IoUringDmaFallbackReason::Geometry => &IO_URING_DMA_DIRECT_WRITE_FALLBACK_GEOMETRY,
            IoUringDmaFallbackReason::Provenance => &IO_URING_DMA_DIRECT_WRITE_FALLBACK_PROVENANCE,
            IoUringDmaFallbackReason::SgCap => &IO_URING_DMA_DIRECT_WRITE_FALLBACK_SG_CAP,
            IoUringDmaFallbackReason::Extent => &IO_URING_DMA_DIRECT_WRITE_FALLBACK_EXTENT,
            IoUringDmaFallbackReason::DeviceAdmission => {
                &IO_URING_DMA_DIRECT_WRITE_FALLBACK_DEVICE_ADMISSION
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "test-io-control"))]
#[inline(always)]
pub(crate) fn record_io_uring_dma_direct_write_fallback(_reason: IoUringDmaFallbackReason) {}
