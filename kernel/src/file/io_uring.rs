//! Concrete TheKernel adapter for Linux `io_uring` ring ownership.

use alloc::{
    borrow::Cow,
    boxed::Box,
    sync::{Arc, Weak},
    task::Wake,
    vec::Vec,
};
use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Waker},
    time::Duration,
};

use axdriver::{
    SharedBlockDevice,
    prelude::{
        BlockCompletion, BlockCompletionAvailability, BlockCompletionNotifier,
        BlockCompletionOwner, BlockCompletionStatus, BlockCompletionTerminalNotifier,
        BlockDriverOps, BlockRequestHandle, BlockResetOutcome, DevError,
    },
};
use axerrno::{AxError, AxResult, LinuxError};
use axfs::{
    PhysicalIoCompletion, PhysicalIoEffectState, PhysicalIoNotSubmittedReason, PhysicalIoOperation,
    PhysicalIoPendingReason, PhysicalIoPublication, PhysicalIoPublishOutcome, PhysicalIoResetProof,
    PhysicalIoSettleOutcome, PreparedPhysicalIoEffect,
};
use axfs_ng_vfs::PhysicalIoSegment;
use axhal::paging::PageSize;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    POLLERR, POLLHUP, POLLIN, POLLMSG, POLLNVAL, POLLOUT, POLLPRI, POLLRDBAND, POLLRDHUP,
    POLLRDNORM, POLLREMOVE, POLLWRBAND, POLLWRNORM,
};
use ouroboros::self_referencing;
use spin::Once;
use thekernel_linux_io_uring::{
    BufferLeaseRelease, BufferSlot, BufferTableId, CancelSelector, CompletionPublication,
    CompletionToken, CopiedSubmission, FileSlot, FileTableId, IoUringError, IssuedRequest,
    LeaseRelease, MappingRegion, ParsedSubmission, PreparedRequest, ReadWriteRequest,
    RegisteredBufferLease, RegisteredBufferTable, RegisteredFileLease, RegisteredFileTable,
    RequestId, RequestIssueError, RequestRegistry, RequestReservation, RingId, RingLayout,
    TerminalCause,
};

use super::{
    DescriptionResource, FileDescription, FileHandle, FileLike, FileMmapRequest,
    FixedSharedMmapRegion, IoOperationContext, Kstat, PreparedFileMmap, SharedPages,
    anon_inode_stat, memfd::MemfdMutationGuard, privilege_metadata::ContentWritePrivilegeGuard,
};
use crate::mm::{
    PinnedUserSegmentsMut, SharedAtomicU32, UserIoPinProvenance, UserIoPinSegment,
    UserMemoryCapability, physical_segments_are_disjoint,
    try_pin_user_segments_to_user_longterm_with,
};

/// Counters for the synchronous physical-DMA fast path used by fixed-buffer
/// io_uring requests.  The counters are deliberately kept next to the ring
/// lease because the path is a lease-owned optimization rather than a generic
/// user-I/O property.
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_STATS_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_HITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACK_GEOMETRY: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACK_PROVENANCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACK_SG_CAP: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACK_EXTENT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_READ_FALLBACK_DEVICE_ADMISSION: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_HITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACK_GEOMETRY: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACK_PROVENANCE: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACK_SG_CAP: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACK_EXTENT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_DMA_DIRECT_WRITE_FALLBACK_DEVICE_ADMISSION: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_SUBMITTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_CHILD_SUBMITTED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_COMPLETED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_CHILD_COMPLETED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_DIRECT_BYTES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_QD_HIGHWATER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_EXTENT_HIGHWATER: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "test-io-control")]
static IO_URING_PHYSICAL_QUARANTINE: AtomicU64 = AtomicU64::new(0);

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

const RING_WAITER_SLOTS: usize = 64;
const PAGE_BYTES: usize = PageSize::Size4K as usize;
const IO_URING_GLOBAL_REQUEST_SLOTS: usize = 65_536;
const IO_URING_GLOBAL_FIXED_FILE_SLOTS: usize = 65_536;
const IO_URING_GLOBAL_REGISTERED_BUFFER_SLOTS: usize = 65_536;
/// The physical effect path is deliberately bounded independently from the
/// SQ/CQ geometry.  A ring can advertise a larger SQ, but only this many
/// device-owned effects may wait for completion at once.
pub(crate) const IO_URING_PHYSICAL_MAX_QD: usize = 32;
pub(crate) const IO_URING_PHYSICAL_MAX_SEGMENTS: usize = 16;
pub(crate) const IO_URING_PHYSICAL_MAX_BYTES: usize = 256 * 1024;
const IO_URING_PHYSICAL_MAX_EXTENTS: usize = 16;
/// A transient filesystem finalization is retried only a small, fixed number
/// of times per worker activation. Persistent Busy leaves the exact work in
/// its slot for a later physical-worker wake instead of spinning forever.
const PHYSICAL_FINALIZATION_RETRY_BUDGET: usize = 3;
/// Round-robin state for the bounded finalization retry selector.  The
/// cursor indexes the fixed, router-derived ring list for the current
/// activation; the slot cursor is shared across rings so a low slot number
/// cannot monopolize repeated activations.
static PHYSICAL_FINALIZATION_RETRY_RING_CURSOR: AtomicUsize = AtomicUsize::new(0);
static PHYSICAL_FINALIZATION_RETRY_SLOT_CURSOR: AtomicUsize = AtomicUsize::new(0);
// Registered fixed buffers retain MM pins until explicit unregister or ring
// teardown. Keep this accounting independent from the slot count: a small
// table must not be able to pin an unbounded virtual range. The global budget
// is 64 MiB of page-cover, while one ring is limited to 16 MiB; each
// descriptor is charged independently, so overlapping descriptors consume the
// same page-cover twice as Linux's separate registered resources do.
const IO_URING_GLOBAL_REGISTERED_BUFFER_PAGES: usize = 16_384;
const IO_URING_RING_REGISTERED_BUFFER_PAGES: usize = 4_096;
const FINAL_CLOSE_STEP_BUDGET: usize = 64;
const POLL_ALWAYS_REPORTED: IoEvents = IoEvents::ALWAYS;
/// Pending stream reads are intentionally a small, ring-local bounded slice.
/// The owner is transferred only after a nonblocking attempt returns
/// `WouldBlock`; no unbounded queue or generic async executor is introduced.
pub(crate) const IO_URING_PENDING_STREAM_CAPACITY: usize = 64;
const IO_URING_PENDING_STREAM_BUDGET: usize = 8;

fn pending_stream_events() -> IoEvents {
    IoEvents::READABLE | IoEvents::HANGUP | IoEvents::ERROR
}

static NEXT_RING_ID: AtomicU64 = AtomicU64::new(1);
static IO_URING_REQUEST_SLOTS: AtomicUsize = AtomicUsize::new(0);
static IO_URING_FIXED_FILE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static IO_URING_REGISTERED_BUFFER_SLOTS: AtomicUsize = AtomicUsize::new(0);
static IO_URING_REGISTERED_BUFFER_PAGES: AtomicUsize = AtomicUsize::new(0);
static IO_URING_REGISTERED_BUFFER_BYTES: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_IO_URING_WORK: AtomicPtr<IoUring> = AtomicPtr::new(ptr::null_mut());
/// The lower handle/cookie namespace is device-local. Keep the exact shared
/// queue identity alongside every upper route instead of trying to infer it
/// from a raw completion handle. This fixed table is deliberately small: the
/// admission path may scan it, while completion routing remains a direct
/// identity carried by the route group.
const PHYSICAL_COMPLETION_MAX_DEVICES: usize = 8;

type PhysicalCompletionDeviceIdentities = [usize; PHYSICAL_COMPLETION_MAX_DEVICES];

fn physical_completion_device_identities() -> (PhysicalCompletionDeviceIdentities, usize) {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let mut identities = [0; PHYSICAL_COMPLETION_MAX_DEVICES];
    let mut len = 0;
    for slot in registry.slots.iter().flatten() {
        if len == identities.len() {
            break;
        }
        identities[len] = slot.identity;
        len += 1;
    }
    (identities, len)
}

struct PhysicalCompletionDeviceSlot {
    identity: usize,
    /// Monotonic process-lifetime callback context for this slot.  Lower
    /// callbacks may already have loaded an old context when unregister
    /// starts; a replacement slot therefore must never reuse the old token,
    /// even if allocator address reuse makes `identity` equal.
    callback_context: usize,
    device: SharedBlockDevice,
    generation: u64,
    configured: bool,
    active: bool,
    /// Admission is fenced independently from lower transport activity.  A
    /// removal/reset may close this bit while an already published owner is
    /// still drained by the completion worker.
    admission_open: bool,
    /// Removal is a per-device intent.  Reset retirement must not reopen a
    /// slot that a concurrent unregister has already fenced.
    removal_pending: bool,
    reset_pending: bool,
    in_flight: usize,
    /// Progress notifications are accounted per exact lower device.  The
    /// generation is the lower progress generation (not the transport
    /// generation stored above); a worker may clear this marker only after it
    /// has observed the same generation again.
    progress_pending: bool,
    progress_generation: u64,
    /// A progress-sequence overflow is terminal for this slot.  Never wrap
    /// the sequence: once the bounded marker can no longer be advanced, keep
    /// the device fenced in reset custody until the slot is removed.
    progress_overflowed: bool,
    /// Terminal sequence exhaustion is a separate stable fence.  It cannot
    /// be represented by the progress sequence because a terminal proof must
    /// retain its own compare/consume identity.
    terminal_sequence_overflowed: bool,
    /// Terminal notifications belong to this device's transport generation.
    /// Keeping the mailbox in the registry prevents a vda reset from
    /// consuming (or reopening) vdb's generation.
    terminal_state: u8,
    terminal_generation: u64,
    terminal_sequence: u64,
    terminal_consumed_sequence: u64,
}

struct PhysicalCompletionDeviceRegistry {
    slots: [Option<PhysicalCompletionDeviceSlot>; PHYSICAL_COMPLETION_MAX_DEVICES],
}

impl PhysicalCompletionDeviceRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { None }; PHYSICAL_COMPLETION_MAX_DEVICES],
        }
    }
}

static PHYSICAL_COMPLETION_DEVICE_REGISTRY: SpinNoIrq<PhysicalCompletionDeviceRegistry> =
    SpinNoIrq::new(PhysicalCompletionDeviceRegistry::new());
/// Callback contexts are process-lifetime incarnation tokens rather than raw
/// device identities.  Never reuse one: an IRQ that loaded an old context
/// after unregister must be rejected by a newly installed slot.
static PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT: AtomicUsize = AtomicUsize::new(1);
static PHYSICAL_COMPLETION_DEFAULT_IDENTITY: AtomicUsize = AtomicUsize::new(0);
static PHYSICAL_COMPLETION_DEVICE_ACTIVE: AtomicBool = AtomicBool::new(false);
static PHYSICAL_COMPLETION_DEVICE_GENERATION: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_COMPLETION_WORKER_STOPPED: AtomicBool = AtomicBool::new(false);
static PHYSICAL_COMPLETION_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static PHYSICAL_COMPLETION_WORKER_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Fixed-slot round-robin cursor for one bounded completion pass. Each
/// activation still visits every registered device at most once, but a busy
/// first slot cannot permanently receive the first lower-drain opportunity.
static PHYSICAL_COMPLETION_DEVICE_CURSOR: AtomicUsize = AtomicUsize::new(0);
/// A wake is a bounded generation hand-off to the dedicated completion task.
/// It is intentionally separate from the generic deferred-work list: the
/// completion task may block in the lower wait, while policy/fanotify work
/// must continue on its own worker.
static PHYSICAL_COMPLETION_WORK_PENDING: AtomicBool = AtomicBool::new(false);
/// A reset request is kept live until the lower reset proves quiescence.  In
/// particular, a `ResourceBusy` admission result must not look like a stopped
/// worker: the submitter that owns the admission guard may still commit its
/// route after the first reset attempt returns.
static PHYSICAL_COMPLETION_RESET_PENDING: AtomicBool = AtomicBool::new(false);
/// Lower reset/retirement notifications are delivered by the shared-device
/// terminal callback.  The callback only publishes this bounded marker and
/// wakes the upper worker; it never drains or retires an upper owner itself.
const PHYSICAL_COMPLETION_TERMINAL_NONE: u8 = 0;
const PHYSICAL_COMPLETION_TERMINAL_QUIESCED: u8 = 1;
const PHYSICAL_COMPLETION_TERMINAL_RETIRED: u8 = 2;
const PHYSICAL_COMPLETION_TERMINAL_QUARANTINED: u8 = 3;
static PHYSICAL_COMPLETION_TERMINAL_STATE: AtomicU8 =
    AtomicU8::new(PHYSICAL_COMPLETION_TERMINAL_NONE);
static PHYSICAL_COMPLETION_TERMINAL_GENERATION: AtomicU64 = AtomicU64::new(0);
/// Monotonic mailbox sequence for lower terminal notifications.  The state
/// and generation fields are read/written under the short pair lock below;
/// the sequence lets the upper worker prove that the event it retired is
/// still the current event before clearing custody or reopening admission.
static PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED: AtomicBool = AtomicBool::new(false);
static PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
/// The notifier context is process-lifetime storage, so uninstalling the
/// callback never depends on a ring or a mount remaining alive.
static PHYSICAL_COMPLETION_TERMINAL_CONTEXT: u8 = 0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PhysicalCompletionProgressState {
    pending: bool,
    generation: u64,
    overflowed: bool,
}

/// Advances one exact device's durable progress marker. Keeping this small
/// state transition separate makes the checked (non-wrapping) arithmetic and
/// snapshot/clear protocol directly testable without constructing a hardware
/// block device in host unit tests.
fn advance_physical_completion_progress(
    progress: &mut PhysicalCompletionProgressState,
) -> AxResult<()> {
    progress.pending = true;
    if progress.overflowed {
        // Overflow is a stable fence, not a transient arithmetic error.  A
        // later callback must not keep publishing wakes or retrying a reset
        // merely because the marker can no longer advance.
        return Err(AxError::BadState);
    }
    match progress.generation.checked_add(1) {
        Some(next) => {
            progress.generation = next;
            Ok(())
        }
        None => {
            progress.overflowed = true;
            Err(AxError::BadState)
        }
    }
}

fn allocate_physical_completion_callback_context() -> AxResult<usize> {
    let mut current = PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT.load(Ordering::Acquire);
    loop {
        let next = current.checked_add(1).ok_or(AxError::BadState)?;
        match PHYSICAL_COMPLETION_CALLBACK_CONTEXT_NEXT.compare_exchange(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

fn advance_physical_completion_terminal_sequence(sequence: u64) -> AxResult<u64> {
    sequence.checked_add(1).ok_or(AxError::BadState)
}

fn clear_physical_completion_progress_if_unchanged(
    progress: &mut PhysicalCompletionProgressState,
    observed_generation: Option<u64>,
) {
    if observed_generation == Some(progress.generation) && !progress.overflowed {
        progress.pending = false;
    }
}

/// Serializes the short lifetime hand-off between submitter admission and
/// device teardown.  A route reservation is not enough by itself: it is
/// possible to reserve a ring slot, then lose the worker/device before the
/// route reservation is installed.  Production admission therefore holds a
/// counted guard from the ring-slot reservation through route commit; stop
/// must reject while that guard is live.
struct PhysicalCompletionAdmissionState {
    /// `configured` distinguishes the production device owner from unit-test
    /// ring reservations made before any device is installed.  The latter
    /// retain their local capacity semantics without opening production I/O.
    configured: bool,
    open: bool,
    in_flight: usize,
    /// A worker-stop notification observed while a submitter owns the
    /// publication/commit fence.  The generation change is deferred until
    /// the last guard drops, so a published effect cannot be stamped stale
    /// between lower publication and route custody.
    generation_bump_pending: bool,
    /// Serializes the fallible lower broker installation without holding this
    /// IRQ-disabling lock.  Teardown and a second installer wait for the
    /// final slot publication/rollback instead of observing a half-installed
    /// owner.
    install_in_progress: bool,
}

static PHYSICAL_COMPLETION_ADMISSION_STATE: SpinNoIrq<PhysicalCompletionAdmissionState> =
    SpinNoIrq::new(PhysicalCompletionAdmissionState {
        configured: false,
        open: false,
        in_flight: 0,
        generation_bump_pending: false,
        install_in_progress: false,
    });

#[inline]
fn physical_completion_default_identity() -> usize {
    PHYSICAL_COMPLETION_DEFAULT_IDENTITY.load(Ordering::Acquire)
}

fn physical_completion_device_slot(identity: usize) -> Option<usize> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|slot| slot.identity == identity))
}

fn physical_completion_device_for(identity: usize) -> Option<SharedBlockDevice> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == identity)
        .map(|slot| slot.device.clone())
}

fn physical_completion_generation_for(identity: usize) -> Option<u64> {
    PHYSICAL_COMPLETION_DEVICE_REGISTRY
        .lock()
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == identity)
        .map(|slot| slot.generation)
        .or_else(|| {
            // Only zero is the synthetic identity used by allocation-free
            // unit tests. A non-zero production identity remains authorized
            // exclusively by its live registry slot; falling back to the
            // root generation would alias a torn-down vda with stale routes.
            (identity == 0).then(|| PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire))
        })
}

fn physical_completion_progress_notifier(context: usize) {
    // The context is a process-lifetime slot incarnation token, not a raw
    // device address.  Account the progress edge before waking the owner;
    // the registry lock makes worker clear/recheck and this callback one
    // protocol, so a lower IRQ cannot disappear in the PollSet arm window.
    let _ = mark_physical_completion_device_progress_from_callback(context);
}

/// Publishes one durable progress edge for an exact lower device.
///
/// The transport generation identifies reset/reinitialization and normally
/// remains zero for the lifetime of a live queue.  It is therefore not a
/// usable wake sequence.  This slot-local sequence advances for every lower
/// callback and every upper publication edge, and is compared under the same
/// registry lock by the worker before it clears `progress_pending`.
///
/// A sequence overflow is impossible in normal operation but must not wrap:
/// doing so could make an old worker snapshot equal a new edge.  Instead the
/// exact device is fenced into reset custody and the marker remains pending.
fn mark_physical_completion_device_progress(device_identity: usize) -> AxResult<()> {
    mark_physical_completion_device_progress_matching(|slot| slot.identity == device_identity)
}

fn mark_physical_completion_device_progress_from_callback(context: usize) -> AxResult<()> {
    mark_physical_completion_device_progress_matching(|slot| slot.callback_context == context)
}

fn mark_physical_completion_device_progress_matching(
    matches: impl Fn(&PhysicalCompletionDeviceSlot) -> bool,
) -> AxResult<()> {
    let (identity, result, first_overflow) = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| matches(slot))
        else {
            // A late callback for an unregistered/incarnation-mismatched
            // device must not wake or mutate a sibling slot that happens to
            // reuse the same raw identity.
            return Err(AxError::BadState);
        };

        let was_overflowed = slot.progress_overflowed;
        let mut progress = PhysicalCompletionProgressState {
            pending: slot.progress_pending,
            generation: slot.progress_generation,
            overflowed: slot.progress_overflowed,
        };
        let result = advance_physical_completion_progress(&mut progress);
        slot.progress_pending = progress.pending;
        slot.progress_generation = progress.generation;
        slot.progress_overflowed = progress.overflowed;
        if result.is_err() && !was_overflowed {
            slot.active = false;
            slot.admission_open = false;
            slot.reset_pending = true;
        }
        (
            slot.identity,
            result,
            !was_overflowed && progress.overflowed,
        )
    };

    if first_overflow {
        // Only the first overflow is a liveness edge. Once fenced, repeated
        // callbacks are rejected without rearming reset/wake state.
        if identity == physical_completion_default_identity() {
            PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
            PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
        }
        // The registry lock is deliberately released before waking.  IRQ
        // callers never retain it across task notification.
        wake_physical_completion_worker();
        return Err(AxError::BadState);
    }
    if result.is_err() {
        return Err(AxError::BadState);
    }
    // The registry lock is deliberately released before waking.  IRQ callers
    // never retain it across task notification, and the release/acquire pair
    // makes the marker visible before the worker runs.
    wake_physical_completion_worker();
    Ok(())
}

fn register_physical_completion_device_slot(
    device: SharedBlockDevice,
    generation: u64,
    _active: bool,
) -> AxResult<usize> {
    let identity = device.identity_token();
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if registry
        .slots
        .iter()
        .flatten()
        .any(|slot| slot.identity == identity)
    {
        // A live (or removal-fenced) slot owns this exact lower identity.
        // Never reset its progress/terminal sequence underneath published
        // routes; the caller must wait for exact removal before reinstalling.
        return Err(AxError::AlreadyExists);
    }
    let callback_context = allocate_physical_completion_callback_context()?;
    let Some(slot) = registry.slots.iter_mut().find(|slot| slot.is_none()) else {
        return Err(AxError::ResourceBusy);
    };
    *slot = Some(PhysicalCompletionDeviceSlot {
        identity,
        callback_context,
        device,
        generation,
        configured: false,
        active: false,
        admission_open: false,
        removal_pending: false,
        reset_pending: false,
        in_flight: 0,
        progress_pending: false,
        progress_generation: 0,
        progress_overflowed: false,
        terminal_sequence_overflowed: false,
        terminal_state: PHYSICAL_COMPLETION_TERMINAL_NONE,
        terminal_generation: generation,
        terminal_sequence: 0,
        terminal_consumed_sequence: 0,
    });
    Ok(callback_context)
}

fn remove_physical_completion_device_slot(
    identity: usize,
    callback_context: usize,
) -> Option<SharedBlockDevice> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let index = registry.slots.iter().position(|slot| {
        slot.as_ref().is_some_and(|slot| {
            slot.identity == identity && slot.callback_context == callback_context
        })
    })?;
    registry.slots[index].take().map(|slot| slot.device)
}

fn publish_physical_completion_device_slot(
    identity: usize,
    active: bool,
    admission_open: bool,
) -> AxResult<bool> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == identity)
        .ok_or(AxError::BadState)?;
    let publishable = slot.terminal_state == PHYSICAL_COMPLETION_TERMINAL_NONE
        && slot.terminal_sequence == slot.terminal_consumed_sequence
        && !slot.reset_pending
        && !slot.progress_overflowed
        && !slot.terminal_sequence_overflowed
        && !slot.removal_pending;
    slot.configured = true;
    slot.active = active && publishable;
    slot.admission_open = admission_open && publishable;
    slot.removal_pending = false;
    Ok(slot.active)
}

fn physical_completion_terminal_context() -> usize {
    (&PHYSICAL_COMPLETION_TERMINAL_CONTEXT as *const u8) as usize
}

fn physical_completion_terminal_code(availability: BlockCompletionAvailability) -> (u8, u64) {
    match availability {
        BlockCompletionAvailability::Live { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_QUIESCED, generation)
        }
        BlockCompletionAvailability::Retired { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_RETIRED, generation)
        }
        BlockCompletionAvailability::Quarantined { generation } => {
            (PHYSICAL_COMPLETION_TERMINAL_QUARANTINED, generation)
        }
    }
}

#[derive(Clone, Copy)]
struct PhysicalCompletionTerminalEvent {
    sequence: u64,
    consumed_sequence: u64,
    state: u8,
    generation: u64,
}

fn physical_completion_terminal_event() -> Option<PhysicalCompletionTerminalEvent> {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    let consumed_sequence = PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
    let state = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire);
    if state == PHYSICAL_COMPLETION_TERMINAL_NONE || sequence == consumed_sequence {
        return None;
    }
    Some(PhysicalCompletionTerminalEvent {
        sequence,
        consumed_sequence,
        state,
        generation: PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
    })
}

fn clear_physical_completion_terminal_event() {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.store(sequence, Ordering::Release);
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
}

/// Reads the terminal mailbox for one exact device. The registry lock is also
/// the publication fence for slot teardown, so a late lower callback cannot
/// be mistaken for a replacement device that happens to reuse its raw
/// handles.
fn physical_completion_terminal_event_for_device(
    device_identity: usize,
) -> Option<PhysicalCompletionTerminalEvent> {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)?;
    if slot.terminal_state == PHYSICAL_COMPLETION_TERMINAL_NONE
        || slot.terminal_sequence == slot.terminal_consumed_sequence
    {
        return None;
    }
    Some(PhysicalCompletionTerminalEvent {
        sequence: slot.terminal_sequence,
        consumed_sequence: slot.terminal_consumed_sequence,
        state: slot.terminal_state,
        generation: slot.terminal_generation,
    })
}

fn clear_physical_completion_terminal_event_for_device(device_identity: usize) {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if let Some(slot) = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)
    {
        slot.terminal_consumed_sequence = slot.terminal_sequence;
        slot.terminal_state = PHYSICAL_COMPLETION_TERMINAL_NONE;
    }
}

fn physical_completion_terminal_event_for_device_reset(
    device_identity: usize,
    outcome: BlockResetOutcome,
    generation: u64,
) -> Option<PhysicalCompletionTerminalEvent> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let slot = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)?;
    if slot.terminal_state != PHYSICAL_COMPLETION_TERMINAL_NONE
        && slot.terminal_sequence != slot.terminal_consumed_sequence
    {
        return Some(PhysicalCompletionTerminalEvent {
            sequence: slot.terminal_sequence,
            consumed_sequence: slot.terminal_consumed_sequence,
            state: slot.terminal_state,
            generation: slot.terminal_generation,
        });
    }
    if slot.terminal_sequence_overflowed {
        return Some(PhysicalCompletionTerminalEvent {
            sequence: slot.terminal_sequence,
            consumed_sequence: slot.terminal_consumed_sequence,
            state: PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
            generation,
        });
    }
    let state = match outcome {
        BlockResetOutcome::Quiesced => PHYSICAL_COMPLETION_TERMINAL_QUIESCED,
        BlockResetOutcome::Retired => PHYSICAL_COMPLETION_TERMINAL_RETIRED,
        BlockResetOutcome::Quarantined => PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
    };
    Some(PhysicalCompletionTerminalEvent {
        sequence: slot.terminal_sequence,
        consumed_sequence: slot.terminal_consumed_sequence,
        state,
        generation,
    })
}

fn physical_completion_terminal_event_for_reset(
    outcome: BlockResetOutcome,
    generation: u64,
) -> PhysicalCompletionTerminalEvent {
    let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    let consumed_sequence = PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
    let state = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire);
    if state != PHYSICAL_COMPLETION_TERMINAL_NONE && sequence != consumed_sequence {
        return PhysicalCompletionTerminalEvent {
            sequence,
            consumed_sequence,
            state,
            generation: PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
        };
    }
    if PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire) {
        return PhysicalCompletionTerminalEvent {
            sequence,
            consumed_sequence,
            state: PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
            generation,
        };
    }
    let state = match outcome {
        BlockResetOutcome::Quiesced => PHYSICAL_COMPLETION_TERMINAL_QUIESCED,
        BlockResetOutcome::Retired => PHYSICAL_COMPLETION_TERMINAL_RETIRED,
        BlockResetOutcome::Quarantined => PHYSICAL_COMPLETION_TERMINAL_QUARANTINED,
    };
    PhysicalCompletionTerminalEvent {
        sequence,
        consumed_sequence,
        state,
        generation,
    }
}

/// Receives a lower transport reset/retirement event without taking any
/// upper ownership lock.  The shared-device implementation invokes this
/// callback synchronously from its reset path, including when the reset was
/// initiated by this module; taking the admission lock here would deadlock
/// that path.  The upper worker later consumes the exact sequence snapshot
/// and performs route/work retirement after admitted submitters have left the
/// publication fence.
fn physical_completion_terminal_notifier(
    context: usize,
    availability: BlockCompletionAvailability,
) {
    let (state, generation) = physical_completion_terminal_code(availability);
    // Production callbacks carry the exact slot incarnation token. Keep
    // the old process-lifetime mailbox as a test-only/legacy fallback when a
    // caller uses the historical terminal context.
    let device_context = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.callback_context == context)
        {
            slot.active = false;
            slot.admission_open = false;
            if slot.terminal_sequence_overflowed {
                // A terminal proof can no longer be represented without
                // risking sequence aliasing.  Keep this slot fenced and wait
                // for exact removal/reinstall; do not manufacture an event.
                Some((slot.identity, false))
            } else if let Ok(next) =
                advance_physical_completion_terminal_sequence(slot.terminal_sequence)
            {
                slot.terminal_generation = generation;
                slot.terminal_state = state;
                slot.terminal_sequence = next;
                slot.reset_pending = false;
                Some((slot.identity, true))
            } else {
                slot.terminal_sequence_overflowed = true;
                slot.reset_pending = false;
                Some((slot.identity, false))
            }
        } else {
            None
        }
    };
    if let Some((device_identity, published)) = device_context {
        // A terminal notification is also a lower progress edge.  Publish it
        // through the same exact-device sequence as an IRQ callback so a
        // worker snapshot cannot clear it merely because the transport
        // generation stayed unchanged.
        if published {
            let _ = mark_physical_completion_device_progress_from_callback(context);
        }
        if device_identity == physical_completion_default_identity() {
            PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
            if published {
                PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
            }
        }
        // A valid terminal edge is an actual liveness event even when the
        // progress marker is already fenced.  The wake occurs after releasing
        // the registry lock and is emitted once for this callback only.
        if published {
            wake_physical_completion_worker();
        }
        return;
    }
    if context != physical_completion_terminal_context() {
        return;
    }
    let terminal_overflow_wake = {
        let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
        let sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
        match advance_physical_completion_terminal_sequence(sequence) {
            Ok(sequence) => {
                PHYSICAL_COMPLETION_TERMINAL_GENERATION.store(generation, Ordering::Release);
                PHYSICAL_COMPLETION_TERMINAL_STATE.store(state, Ordering::Release);
                PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.store(sequence, Ordering::Release);
                false
            }
            Err(_) => {
                PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.store(true, Ordering::Release);
                PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
                PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
                let pending = PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire)
                    != PHYSICAL_COMPLETION_TERMINAL_NONE
                    && PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire)
                        != PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
                PHYSICAL_COMPLETION_WORK_PENDING.store(pending, Ordering::Release);
                pending
            }
        }
    };
    if terminal_overflow_wake {
        // Preserve the already-published proof; only its existing event is
        // actionable once sequence space is exhausted.
        crate::deferred_work::wake_physical_completion_worker();
        return;
    }
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    // Keep pending asserted while upper route/effect custody is unresolved;
    // `has_physical_completion_work` recognizes the terminal marker even
    // though the lower queue is no longer live.
    PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    crate::deferred_work::wake_physical_completion_worker();
}

fn physical_completion_terminal_outcome(state: u8) -> Option<BlockResetOutcome> {
    match state {
        PHYSICAL_COMPLETION_TERMINAL_QUIESCED => {
            Some(axdriver::prelude::BlockResetOutcome::Quiesced)
        }
        PHYSICAL_COMPLETION_TERMINAL_RETIRED => Some(axdriver::prelude::BlockResetOutcome::Retired),
        _ => None,
    }
}

/// A production physical admission guard.  It is deliberately held by the
/// fixed worker reservation, not by a temporary submitter local, so the
/// stop/generation gate covers the complete publish-to-route-commit window.
struct PhysicalCompletionAdmissionGuard {
    device_identity: usize,
    /// Whether this guard mirrored the legacy root admission counter when it
    /// was acquired. The registry slot can disappear during teardown, and
    /// the current default identity may already be zero by then; cleanup
    /// must still release the counter it actually charged.
    mirrors_global: bool,
}

impl PhysicalCompletionAdmissionGuard {
    fn begin() -> AxResult<Option<Self>> {
        Self::begin_for(physical_completion_default_identity())
    }

    fn begin_for(device_identity: usize) -> AxResult<Option<Self>> {
        // Keep admission before registry in this path. Worker lifecycle
        // publication takes the same order, so a submitter cannot deadlock
        // with a stop/start transition while mirroring the root counters.
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        let mirrors_global = device_identity == physical_completion_default_identity();
        if admission_state.install_in_progress {
            return Err(AxError::ResourceBusy);
        }
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        {
            if !slot.configured
                || !slot.active
                || !slot.admission_open
                || slot.reset_pending
                || slot.progress_overflowed
                || slot.terminal_sequence_overflowed
                || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                || !PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
            {
                return Err(AxError::BadState);
            }
            if mirrors_global {
                admission_state.in_flight = admission_state
                    .in_flight
                    .checked_add(1)
                    .ok_or(AxError::BadState)?;
            }
            slot.in_flight = match slot.in_flight.checked_add(1) {
                Some(in_flight) => in_flight,
                None => {
                    if mirrors_global {
                        admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
                    }
                    return Err(AxError::BadState);
                }
            };
            drop(registry);
            drop(admission_state);
            return Ok(Some(Self {
                device_identity,
                mirrors_global,
            }));
        }
        drop(registry);
        // A production identity must never fall through to the legacy test
        // admission state after its exact registry slot has disappeared.
        // Production identities are registry-authorized only. The
        // zero-identity branch is retained solely for allocation-free unit
        // tests that model the old root gate without a real device slot.
        if device_identity != 0 {
            return Err(AxError::BadState);
        }
        if !admission_state.configured {
            // No installed production owner: direct ring capacity tests and
            // non-production callers may still exercise local reservations,
            // but they cannot publish a physical effect without readiness.
            return Ok(None);
        }
        if !admission_state.open
            || !PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire)
            || PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire)
            || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
            || !PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
        {
            return Err(AxError::BadState);
        }
        admission_state.in_flight = admission_state
            .in_flight
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        drop(admission_state);
        Ok(Some(Self {
            device_identity,
            mirrors_global,
        }))
    }

    /// Check-and-call is kept under the same lifecycle lock used by stop and
    /// worker-failure notification.  This closes the last check-then-publish
    /// window: once publication starts, teardown cannot invalidate its owner
    /// before the reservation commits the route and worker item.
    fn with_publish<T>(f: impl FnOnce() -> T) -> Option<T> {
        Self::with_publish_for(physical_completion_default_identity(), f)
    }

    fn with_publish_for<T>(device_identity: usize, f: impl FnOnce() -> T) -> Option<T> {
        let registered_ready = {
            let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            registry
                .slots
                .iter()
                .flatten()
                .find(|slot| slot.identity == device_identity)
                .map(|slot| {
                    slot.configured
                        && slot.active
                        && slot.admission_open
                        && !slot.reset_pending
                        && !slot.progress_overflowed
                        && !slot.terminal_sequence_overflowed
                        && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                        && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
                })
        };
        if let Some(ready) = registered_ready {
            // The counted admission guard prevents teardown/reset from
            // retiring this device while publication is in flight.  Do not
            // retain the IRQ-disabling registry lock across filesystem and
            // driver publication: that path may legitimately contend on a
            // sleeping mutex.  The lower queue's generation/availability
            // gate remains the final pre-descriptor publication check if a
            // terminal notification races this snapshot.
            return ready.then(f);
        }
        if device_identity != 0 {
            return None;
        }
        let (configured, ready) = {
            let state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            (
                state.configured,
                state.configured
                    && state.open
                    && PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire)
                    && !PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire)
                    && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
                    && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire),
            )
        };
        if ready {
            Some(f())
        } else if configured {
            None
        } else {
            // This branch is reachable only for test/non-production local
            // reservations.  The real physical path is gated by readiness,
            // but retaining the permissive behavior keeps the reservation
            // helper independently testable.
            Some(f())
        }
    }
}

impl Drop for PhysicalCompletionAdmissionGuard {
    fn drop(&mut self) {
        // Match begin_for's admission -> registry order. This is also the
        // order used by worker lifecycle transitions.
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if registry
            .slots
            .iter()
            .flatten()
            .any(|slot| slot.identity == self.device_identity)
        {
            let mut wake_reset = false;
            if let Some(slot) = registry
                .slots
                .iter_mut()
                .flatten()
                .find(|slot| slot.identity == self.device_identity)
            {
                slot.in_flight = slot.in_flight.saturating_sub(1);
                wake_reset = slot.in_flight == 0 && (slot.reset_pending || slot.removal_pending);
            }
            if self.mirrors_global {
                admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
                if admission_state.in_flight == 0 && admission_state.generation_bump_pending {
                    admission_state.generation_bump_pending = false;
                    PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
                }
            }
            drop(registry);
            drop(admission_state);
            if wake_reset {
                PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
                crate::deferred_work::wake_physical_completion_worker();
            }
            return;
        }
        drop(registry);
        admission_state.in_flight = admission_state.in_flight.saturating_sub(1);
        if admission_state.in_flight == 0 && admission_state.generation_bump_pending {
            admission_state.generation_bump_pending = false;
            PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
        }
        let wake_reset = admission_state.in_flight == 0
            && (PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire)
                || physical_completion_terminal_event().is_some());
        drop(admission_state);
        if wake_reset {
            PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
            crate::deferred_work::wake_physical_completion_worker();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalCompletionChildState {
    Empty,
    Reserved,
    Owner,
    /// A published effect whose driver report did not provide a unique,
    /// usable handle.  It remains reset/teardown custody and is never
    /// converted into an EIO CQE by the route lookup path.
    Quarantined,
}

#[derive(Clone, Copy)]
struct PhysicalCompletionRouteChild {
    handle: Option<u64>,
    cookie: Option<u64>,
    state: PhysicalCompletionChildState,
}

impl PhysicalCompletionRouteChild {
    const fn empty() -> Self {
        Self {
            handle: None,
            cookie: None,
            state: PhysicalCompletionChildState::Empty,
        }
    }

    const fn reserved() -> Self {
        Self {
            handle: None,
            cookie: None,
            state: PhysicalCompletionChildState::Reserved,
        }
    }

    const fn quarantined(handle: Option<u64>, cookie: Option<u64>) -> Self {
        Self {
            handle,
            cookie,
            state: PhysicalCompletionChildState::Quarantined,
        }
    }

    const fn owner(handle: u64, cookie: Option<u64>) -> Self {
        Self {
            handle: Some(handle),
            cookie,
            state: PhysicalCompletionChildState::Owner,
        }
    }
}

/// A single physical request group owns one ring/request identity and up to
/// sixteen lower descriptors.  Keeping the Arc and request metadata here,
/// instead of in every child, makes the hot completion lookup bounded at 32
/// groups without multiplying the ownership footprint by extent count.
struct PhysicalCompletionRouteGroup {
    ring: Option<Arc<IoUring>>,
    request: Option<RequestId>,
    slot: usize,
    /// Exact lower queue identity. Raw handles/cookies are only meaningful
    /// within this device namespace.
    device_identity: usize,
    generation: u64,
    /// A pending publication owns the logical ring/request identity but has
    /// no lower descriptor yet.  Keep it out of the child route set: handle
    /// lookup must never mistake a pending owner for a published request.
    pending_publication: bool,
    child_len: usize,
    children: [PhysicalCompletionRouteChild; IO_URING_PHYSICAL_MAX_EXTENTS],
}

impl PhysicalCompletionRouteGroup {
    fn reserved(device_identity: usize, generation: u64, child_len: usize) -> Self {
        let mut children =
            [const { PhysicalCompletionRouteChild::empty() }; IO_URING_PHYSICAL_MAX_EXTENTS];
        for child in &mut children[..child_len] {
            *child = PhysicalCompletionRouteChild::reserved();
        }
        Self {
            ring: None,
            request: None,
            slot: 0,
            device_identity,
            generation,
            pending_publication: false,
            child_len,
            children,
        }
    }

    fn is_committed(&self) -> bool {
        self.ring.is_some() && self.request.is_some()
    }

    fn has_custody(&self) -> bool {
        self.is_committed()
            && (self.pending_publication
                || self.children[..self.child_len].iter().any(|child| {
                    matches!(
                        child.state,
                        PhysicalCompletionChildState::Owner
                            | PhysicalCompletionChildState::Quarantined
                    )
                }))
    }

    fn has_quarantined_child(&self) -> bool {
        self.children[..self.child_len]
            .iter()
            .any(|child| child.state == PhysicalCompletionChildState::Quarantined)
    }
}

#[derive(Clone, Copy)]
struct QuarantinedPhysicalCompletion {
    completion: PhysicalIoCompletion,
    device_identity: usize,
    /// A completion observed while its handle route was not yet visible may
    /// have raced the submitter's route commit. Keep it replayable after the
    /// exact route becomes visible. Protocol failures observed for an
    /// already-owned route are diagnostic custody only and must not be fed
    /// back into the effect a second time.
    replayable: bool,
}

/// Exact metadata for a pre-publication owner.  Pending publication is not a
/// lower route and therefore cannot be discovered through a raw completion
/// handle.  This fixed table is only metadata; the IssuedRequest and every
/// admission lease remain in the ring's matching logical slot.
struct PhysicalCompletionPendingOwner {
    device_identity: usize,
    generation: u64,
    ring: Arc<IoUring>,
    request: RequestId,
    slot: usize,
    claimed: bool,
}

struct PhysicalCompletionRouter {
    groups: [Option<PhysicalCompletionRouteGroup>; IO_URING_PHYSICAL_MAX_QD],
    pending: [Option<PhysicalCompletionPendingOwner>; IO_URING_PHYSICAL_MAX_QD],
    quarantine: [Option<QuarantinedPhysicalCompletion>; IO_URING_PHYSICAL_MAX_QD],
    quarantine_len: usize,
    work_count: usize,
    pending_count: usize,
}

impl PhysicalCompletionRouter {
    const fn new() -> Self {
        Self {
            groups: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            pending: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            quarantine: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            quarantine_len: 0,
            work_count: 0,
            pending_count: 0,
        }
    }
}

static PHYSICAL_COMPLETION_ROUTER: SpinNoIrq<PhysicalCompletionRouter> =
    SpinNoIrq::new(PhysicalCompletionRouter::new());
static PHYSICAL_PUBLICATION_RETRY_CURSOR: AtomicUsize = AtomicUsize::new(0);
const PHYSICAL_PUBLICATION_RETRY_BUDGET: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalPublicationRetryDisposition {
    /// No pending owner remains and this pass did not publish new work.
    Quiescent,
    /// At least one pending owner became device-visible. Its freshly
    /// installed routes require an immediate bounded follow-up pass.
    Republished,
    /// Pending owners remain, but an existing published route owns the next
    /// real completion edge. Do not turn lower backpressure into a busy loop.
    WaitingForCompletion,
    /// Pending owners remain without any published route that can generate a
    /// future completion edge. A delayed task-context retry is the sole
    /// liveness source.
    PendingOnly,
}

const fn physical_publication_retry_disposition(
    republished: bool,
    pending_remaining: bool,
    published_routes_remaining: bool,
) -> PhysicalPublicationRetryDisposition {
    if republished {
        PhysicalPublicationRetryDisposition::Republished
    } else if !pending_remaining {
        PhysicalPublicationRetryDisposition::Quiescent
    } else if published_routes_remaining {
        PhysicalPublicationRetryDisposition::WaitingForCompletion
    } else {
        PhysicalPublicationRetryDisposition::PendingOnly
    }
}

fn register_physical_completion_pending_owner(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    if router.pending.iter().flatten().any(|owner| {
        owner.device_identity == device_identity
            && owner.generation == generation
            && owner.request == request
            && owner.slot == slot
            && Arc::ptr_eq(&owner.ring, ring)
    }) {
        return Err(AxError::BadState);
    }
    let Some(entry) = router.pending.iter_mut().find(|entry| entry.is_none()) else {
        return Err(AxError::ResourceBusy);
    };
    *entry = Some(PhysicalCompletionPendingOwner {
        device_identity,
        generation,
        ring: Arc::clone(ring),
        request,
        slot,
        claimed: false,
    });
    router.pending_count = router
        .pending_count
        .checked_add(1)
        .ok_or(AxError::BadState)?;
    Ok(())
}

fn clear_physical_completion_pending_owner(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> bool {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    clear_physical_completion_pending_owner_locked(
        &mut router,
        ring,
        request,
        slot,
        device_identity,
        generation,
    )
}

fn set_physical_completion_pending_claim(
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
    claimed: bool,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
        owner.device_identity == device_identity
            && owner.generation == generation
            && owner.request == request
            && owner.slot == slot
            && Arc::ptr_eq(&owner.ring, ring)
    }) else {
        return Err(AxError::BadState);
    };
    if owner.claimed && claimed {
        return Err(AxError::ResourceBusy);
    }
    owner.claimed = claimed;
    Ok(())
}

fn clear_physical_completion_pending_owner_locked(
    router: &mut PhysicalCompletionRouter,
    ring: &Arc<IoUring>,
    request: RequestId,
    slot: usize,
    device_identity: usize,
    generation: u64,
) -> bool {
    let Some(index) = router.pending.iter().position(|entry| {
        entry.as_ref().is_some_and(|owner| {
            owner.device_identity == device_identity
                && owner.generation == generation
                && owner.request == request
                && owner.slot == slot
                && Arc::ptr_eq(&owner.ring, ring)
        })
    }) else {
        return false;
    };
    router.pending[index] = None;
    router.pending_count = router.pending_count.saturating_sub(1);
    true
}

fn physical_completion_pending_owner_snapshot(
    index: usize,
) -> Option<(Arc<IoUring>, RequestId, usize, usize, u64)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    let owner = router.pending.get(index)?.as_ref()?;
    Some((
        Arc::clone(&owner.ring),
        owner.request,
        owner.slot,
        owner.device_identity,
        owner.generation,
    ))
}

fn physical_completion_pending_owner_count_for_device(device_identity: usize) -> usize {
    PHYSICAL_COMPLETION_ROUTER
        .lock()
        .pending
        .iter()
        .flatten()
        .filter(|owner| owner.device_identity == device_identity)
        .count()
}

/// A route reservation is made before the vendor effect is published.  One
/// reservation occupies exactly one fixed request group, regardless of its
/// extent count. It is deliberately separate from the per-ring QD
/// reservation: completion ownership is device-global, so two rings must
/// compete for the same fixed group table even when each ring still has local
/// QD credit.
struct PhysicalCompletionRouteReservation {
    group: usize,
    len: usize,
    device_identity: usize,
    work_reserved: bool,
    committed: bool,
}

impl PhysicalCompletionRouteReservation {
    fn new(count: usize) -> AxResult<Self> {
        Self::new_for_device(count, physical_completion_default_identity())
    }

    fn new_for_device(count: usize, device_identity: usize) -> AxResult<Self> {
        if count == 0 || count > IO_URING_PHYSICAL_MAX_EXTENTS {
            return Err(AxError::BadState);
        }
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let device_work_count = router
            .groups
            .iter()
            .flatten()
            .filter(|group| group.device_identity == device_identity)
            .count();
        if device_work_count >= IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::ResourceBusy);
        }
        let Some(group) = router.groups.iter().position(Option::is_none) else {
            return Err(AxError::ResourceBusy);
        };
        let generation =
            physical_completion_generation_for(device_identity).ok_or(AxError::BadState)?;
        router.groups[group] = Some(PhysicalCompletionRouteGroup::reserved(
            device_identity,
            generation,
            count,
        ));
        router.work_count += 1;
        Ok(Self {
            group,
            len: count,
            device_identity,
            work_reserved: true,
            committed: false,
        })
    }

    fn activate_locked(
        &mut self,
        router: &mut PhysicalCompletionRouter,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        publication: Option<PhysicalIoPublication>,
    ) -> bool {
        let mut handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let mut cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let force_quarantine = publication.is_none_or(|publication| publication.count() == 0);
        let accepted = publication.map_or(0, |publication| {
            let count = publication.count();
            for index in 0..count.min(IO_URING_PHYSICAL_MAX_EXTENTS) {
                handles[index] = publication.handle(index);
                cookies[index] = publication.cookie(index);
            }
            count
        });
        self.activate_handles_locked(
            router,
            ring,
            request,
            slot,
            &handles,
            &cookies,
            accepted,
            force_quarantine,
        )
    }

    /// Installs only the handles that the lower publication actually accepted.
    /// A terminal short batch is still a valid publication for its accepted
    /// prefix; every reserved suffix is a pre-publication rollback and must
    /// not become reset custody.  A missing/duplicate handle in the accepted
    /// prefix remains quarantine custody because it cannot be retired by an
    /// exact completion.
    fn activate_handles_locked(
        &mut self,
        router: &mut PhysicalCompletionRouter,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handles: &[Option<u64>; IO_URING_PHYSICAL_MAX_EXTENTS],
        cookies: &[Option<u64>; IO_URING_PHYSICAL_MAX_EXTENTS],
        accepted: usize,
        force_quarantine: bool,
    ) -> bool {
        let mut quarantined = force_quarantine || accepted > self.len;
        if !quarantined {
            for index in 0..accepted {
                let Some(handle) = handles[index] else {
                    quarantined = true;
                    break;
                };
                if handle == 0
                    || handles[..index]
                        .iter()
                        .flatten()
                        .any(|existing| *existing == handle)
                    || router
                        .groups
                        .iter()
                        .enumerate()
                        .any(|(group_index, group)| {
                            group_index != self.group
                                && group.as_ref().is_some_and(|group| {
                                    group.device_identity == self.device_identity
                                        && group.children[..group.child_len].iter().any(|child| {
                                            matches!(
                                                child.state,
                                                PhysicalCompletionChildState::Owner
                                                    | PhysicalCompletionChildState::Quarantined
                                            ) && child.handle == Some(handle)
                                        })
                                })
                        })
                {
                    quarantined = true;
                    break;
                }
            }
        }

        let Some(group) = router.groups.get_mut(self.group).and_then(Option::as_mut) else {
            // A reservation must normally remain installed until this commit
            // point.  If an internal owner was already removed, preserve the
            // fail-stop result rather than exposing a route without a group.
            self.committed = true;
            self.work_reserved = false;
            return true;
        };
        group.ring = Some(Arc::clone(ring));
        group.request = Some(request);
        group.slot = slot;
        if quarantined {
            // Any malformed accepted prefix makes the complete reserved
            // group reset custody.  The lower owner may have accepted an
            // extent beyond the reported prefix, so retaining every reserved
            // child is safer than releasing a suffix whose ownership is
            // ambiguous.  A valid short prefix is handled below and alone
            // releases its never-published suffix.
            for index in 0..self.len {
                let handle = (index < accepted).then_some(handles[index]).flatten();
                let cookie = (index < accepted).then_some(cookies[index]).flatten();
                group.children[index] = PhysicalCompletionRouteChild::quarantined(handle, cookie);
            }
        } else {
            for index in 0..self.len {
                group.children[index] = if index < accepted {
                    PhysicalCompletionRouteChild::owner(
                        handles[index].expect("validated physical completion handle"),
                        cookies[index],
                    )
                } else {
                    // The lower device never owned this suffix.  Roll its
                    // upper child reservation back exactly as a legal short
                    // publication; the operation's one group charge remains
                    // for the accepted prefix until it settles.
                    PhysicalCompletionRouteChild::empty()
                };
            }
        }
        self.committed = true;
        self.work_reserved = false;
        quarantined
    }

    // legacy implementation removed
    // group.children[..group.child_len].iter().any(|child| {
    // matches!(
    // child.state,
    // PhysicalCompletionChildState::Owner
    // | PhysicalCompletionChildState::Quarantined
    // ) && child.handle == Some(handle)
    // })
    // })

    fn activate(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        publication: Option<PhysicalIoPublication>,
    ) {
        let quarantined = self.activate_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            publication,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
    }

    #[cfg(test)]
    fn activate_test(self, ring: &Arc<IoUring>, request: RequestId, slot: usize, handle: u64) {
        self.activate_test_with_cookie(ring, request, slot, handle, None);
    }

    #[cfg(test)]
    fn activate_test_with_cookie(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handle: u64,
        cookie: Option<u64>,
    ) {
        let mut handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let mut cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        handles[0] = Some(handle);
        cookies[0] = cookie;
        let quarantined = self.activate_handles_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            &handles,
            &cookies,
            1,
            false,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
    }

    #[cfg(test)]
    fn activate_test_with_handles(
        mut self,
        ring: &Arc<IoUring>,
        request: RequestId,
        slot: usize,
        handles: &[Option<u64>],
    ) -> bool {
        let mut accepted_handles = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let accepted = handles.len();
        let copied = accepted.min(accepted_handles.len());
        accepted_handles[..copied].copy_from_slice(&handles[..copied]);
        let cookies = [None; IO_URING_PHYSICAL_MAX_EXTENTS];
        let quarantined = self.activate_handles_locked(
            &mut PHYSICAL_COMPLETION_ROUTER.lock(),
            ring,
            request,
            slot,
            &accepted_handles,
            &cookies,
            accepted,
            false,
        );
        if quarantined {
            record_io_uring_physical_quarantine();
        }
        quarantined
    }
}

impl Drop for PhysicalCompletionRouteReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        if self.work_reserved {
            router.groups[self.group] = None;
            router.work_count = router.work_count.saturating_sub(1);
        }
    }
}

fn physical_completion_route_count() -> usize {
    physical_completion_route_count_for_device(physical_completion_default_identity())
}

fn physical_completion_route_count_for_device(device_identity: usize) -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .map(|group| {
            group.children[..group.child_len]
                .iter()
                .filter(|child| child.state == PhysicalCompletionChildState::Owner)
                .count()
        })
        .sum()
}

fn physical_completion_custody_count() -> usize {
    physical_completion_custody_count_for_device(physical_completion_default_identity())
}

fn physical_completion_custody_count_for_device(device_identity: usize) -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .filter(|group| group.has_custody())
        .count()
        + router
            .pending
            .iter()
            .flatten()
            .filter(|owner| owner.device_identity == device_identity)
            .count()
}

fn physical_completion_has_quarantined_route() -> bool {
    physical_completion_has_quarantined_route_for_device(physical_completion_default_identity())
}

fn physical_completion_has_quarantined_route_for_device(device_identity: usize) -> bool {
    PHYSICAL_COMPLETION_ROUTER
        .lock()
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .any(PhysicalCompletionRouteGroup::has_quarantined_child)
}

fn lookup_physical_completion_route_for_device(
    device_identity: usize,
    handle: u64,
) -> Option<(Arc<IoUring>, usize)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter().flatten() {
        if group.ring.is_none() || group.device_identity != device_identity {
            continue;
        }
        if group.children[..group.child_len].iter().any(|child| {
            child.state == PhysicalCompletionChildState::Owner && child.handle == Some(handle)
        }) {
            return Some((
                Arc::clone(group.ring.as_ref().expect("committed route ring")),
                group.slot,
            ));
        }
    }
    None
}

fn lookup_physical_completion_route(handle: u64) -> Option<(Arc<IoUring>, usize)> {
    lookup_physical_completion_route_for_device(physical_completion_default_identity(), handle)
}

fn lookup_physical_completion_route_identity(
    handle: u64,
) -> Option<(Arc<IoUring>, RequestId, u64)> {
    lookup_physical_completion_route_identity_for_device(
        physical_completion_default_identity(),
        handle,
    )
    .map(|(ring, request, generation, _slot)| (ring, request, generation))
}

fn lookup_physical_completion_route_identity_for_device(
    device_identity: usize,
    handle: u64,
) -> Option<(Arc<IoUring>, RequestId, u64, usize)> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter().flatten() {
        let (Some(ring), Some(request)) = (group.ring.as_ref(), group.request) else {
            continue;
        };
        if group.device_identity != device_identity {
            continue;
        }
        if group.children[..group.child_len].iter().any(|child| {
            child.state == PhysicalCompletionChildState::Owner && child.handle == Some(handle)
        }) {
            return Some((Arc::clone(ring), request, group.generation, group.slot));
        }
    }
    None
}

/// Releases the complete route set for one exact request only after its
/// completion handle has matched.  Ring/worker slots are deliberately not an
/// identity: a stale completion can race a new request that reuses both.
fn release_physical_completion_routes(
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) -> bool {
    release_physical_completion_routes_for_device(
        physical_completion_default_identity(),
        ring,
        request,
        handle,
    )
}

fn release_physical_completion_routes_for_device(
    device_identity: usize,
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) -> bool {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    // Require an exact handle match before clearing the request's complete
    // extent set.  In particular, an old cleanup with a reused worker slot
    // cannot observe a different generation and decrement its QD charge.
    let Some(group_index) = router.groups.iter().position(|group| {
        let Some(group) = group.as_ref() else {
            return false;
        };
        let (Some(owner), Some(route_request)) = (group.ring.as_ref(), group.request) else {
            return false;
        };
        if group.device_identity != device_identity {
            return false;
        }
        Arc::ptr_eq(owner, ring)
            && route_request == request
            && group.children[..group.child_len].iter().any(|child| {
                matches!(
                    child.state,
                    PhysicalCompletionChildState::Owner | PhysicalCompletionChildState::Quarantined
                ) && handle.is_none_or(|expected| child.handle == Some(expected))
            })
    }) else {
        return false;
    };
    // The matching route proves that this request still owns one global work
    // charge.  Refuse a corrupted zero count before clearing the routes;
    // saturating here would hide a double release and strand the remaining
    // reset/close accounting.
    if router.work_count == 0 {
        return false;
    }
    router.groups[group_index] = None;
    router.work_count -= 1;
    true
}

struct PhysicalCompletionResetOwner {
    device_identity: usize,
    ring: Arc<IoUring>,
    request: RequestId,
    slot: usize,
    generation: u64,
}

struct PhysicalCompletionResetOwners {
    owners: [Option<PhysicalCompletionResetOwner>; IO_URING_PHYSICAL_MAX_QD],
    len: usize,
}

impl PhysicalCompletionResetOwners {
    const fn new() -> Self {
        Self {
            owners: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            len: 0,
        }
    }
}

struct PhysicalCompletionResetWork {
    owner: PhysicalCompletionResetOwner,
    work: PhysicalIoWork,
}

struct PhysicalCompletionResetWorks {
    works: [Option<PhysicalCompletionResetWork>; IO_URING_PHYSICAL_MAX_QD],
    len: usize,
}

impl PhysicalCompletionResetWorks {
    const fn new() -> Self {
        Self {
            works: [const { None }; IO_URING_PHYSICAL_MAX_QD],
            len: 0,
        }
    }
}

/// Captures one exact ring/request identity for every published route that
/// must be retired by a global device reset.  The storage is fixed-capacity:
/// after the lower reset proves `Quiesced`, retirement cannot fail because a
/// recovery allocation ran out of memory.
fn collect_physical_completion_owners_for_device(
    device_identity: usize,
) -> AxResult<PhysicalCompletionResetOwners> {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut owners = PhysicalCompletionResetOwners::new();
    for group in router.groups.iter().flatten() {
        if group.device_identity != device_identity {
            continue;
        }
        let (Some(ring), Some(request)) = (group.ring.as_ref(), group.request) else {
            continue;
        };
        if !group.has_custody() {
            continue;
        }
        if owners.len == IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::BadState);
        }
        owners.owners[owners.len] = Some(PhysicalCompletionResetOwner {
            device_identity,
            ring: Arc::clone(ring),
            request,
            slot: group.slot,
            generation: group.generation,
        });
        owners.len += 1;
    }
    for pending in router.pending.iter().flatten() {
        if pending.device_identity != device_identity {
            continue;
        }
        if owners.len == IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::BadState);
        }
        owners.owners[owners.len] = Some(PhysicalCompletionResetOwner {
            device_identity,
            ring: Arc::clone(&pending.ring),
            request: pending.request,
            slot: pending.slot,
            generation: pending.generation,
        });
        owners.len += 1;
    }
    Ok(owners)
}

fn collect_physical_completion_owners() -> AxResult<PhysicalCompletionResetOwners> {
    collect_physical_completion_owners_for_device(physical_completion_default_identity())
}

fn clear_physical_completion_quarantine() {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    router.quarantine.fill(None);
    router.quarantine_len = 0;
}

fn clear_physical_completion_quarantine_for_device(device_identity: usize) {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut removed = 0;
    for index in 0..router.quarantine.len() {
        if router.quarantine[index]
            .as_ref()
            .is_some_and(|entry| entry.device_identity == device_identity)
        {
            router.quarantine[index] = None;
            removed += 1;
        }
    }
    router.quarantine_len = router.quarantine_len.saturating_sub(removed);
}

fn route_matches_reset_owner(
    group: &PhysicalCompletionRouteGroup,
    owner: &PhysicalCompletionResetOwner,
) -> bool {
    group.has_custody()
        && group.device_identity == owner.device_identity
        && group.request == Some(owner.request)
        && group.generation == owner.generation
        && group
            .ring
            .as_ref()
            .is_some_and(|ring| Arc::ptr_eq(ring, &owner.ring))
}

fn pending_matches_reset_owner(
    pending: &PhysicalCompletionPendingOwner,
    owner: &PhysicalCompletionResetOwner,
) -> bool {
    pending.device_identity == owner.device_identity
        && pending.generation == owner.generation
        && pending.request == owner.request
        && pending.slot == owner.slot
        && Arc::ptr_eq(&pending.ring, &owner.ring)
}

/// Removes every route for the reset owner set as one router transaction.
/// All identities are validated before any route is cleared, so a malformed
/// owner set cannot partially decrement the global work charge.
fn release_physical_completion_owner_set(owners: &PhysicalCompletionResetOwners) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut group_releases = 0usize;
    let mut pending_releases = 0usize;
    for owner in owners.owners[..owners.len].iter().flatten() {
        let group_match = router
            .groups
            .iter()
            .flatten()
            .any(|group| route_matches_reset_owner(group, owner));
        let pending_match = router
            .pending
            .iter()
            .flatten()
            .any(|pending| pending_matches_reset_owner(pending, owner));
        if !group_match && !pending_match {
            return Err(AxError::BadState);
        }
        group_releases += usize::from(group_match);
        pending_releases += usize::from(pending_match);
    }
    if router.work_count < group_releases || router.pending_count < pending_releases {
        return Err(AxError::BadState);
    }
    for group in &mut router.groups {
        if group.as_ref().is_some_and(|group| {
            owners.owners[..owners.len]
                .iter()
                .flatten()
                .any(|owner| route_matches_reset_owner(group, owner))
        }) {
            *group = None;
        }
    }
    for pending in &mut router.pending {
        if pending.as_ref().is_some_and(|pending| {
            owners.owners[..owners.len]
                .iter()
                .flatten()
                .any(|owner| pending_matches_reset_owner(pending, owner))
        }) {
            *pending = None;
        }
    }
    router.work_count -= group_releases;
    router.pending_count -= pending_releases;
    Ok(())
}

fn restore_physical_completion_reset_works(mut works: PhysicalCompletionResetWorks) {
    for entry in works.works[..works.len].iter_mut() {
        let Some(entry) = entry.take() else {
            continue;
        };
        let _ = entry.owner.ring.retain_physical_worker_work(entry.work);
    }
}

/// Completes the upper reset protocol only after the lower device has
/// returned a quiescent outcome (`Quiesced` or `Retired`).  Every route and
/// ring work owner is still held while reset runs; once quiescence is proven,
/// each exact request receives a typed reset failure and its route/work charge
/// is released exactly once.
fn retire_physical_completion_after_reset_for_device(
    device_identity: usize,
    outcome: axdriver::prelude::BlockResetOutcome,
) -> AxResult<()> {
    let proof = PhysicalIoResetProof::from_lower_reset(outcome).ok_or(AxError::BadState)?;
    let owners = collect_physical_completion_owners_for_device(device_identity)?;
    // Validate the route/work pairing before mutating either table.  A
    // missing ring owner is an internal custody violation; leaving routes in
    // place is safer than decrementing the global charge and stranding the
    // unmatched ring work on a retry.
    let mut works = PhysicalCompletionResetWorks::new();
    for owner in owners.owners[..owners.len].iter().flatten() {
        if !owner.ring.has_physical_worker_request_for_device(
            device_identity,
            owner.request,
            owner.slot,
            owner.generation,
        ) {
            restore_physical_completion_reset_works(works);
            return Err(AxError::BadState);
        }
        let Some(work) = owner.ring.take_physical_worker_for_reset_for_device(
            device_identity,
            owner.request,
            owner.slot,
            owner.generation,
        ) else {
            restore_physical_completion_reset_works(works);
            return Err(AxError::BadState);
        };
        works.works[works.len] = Some(PhysicalCompletionResetWork {
            owner: PhysicalCompletionResetOwner {
                device_identity,
                ring: Arc::clone(&owner.ring),
                request: owner.request,
                slot: owner.slot,
                generation: owner.generation,
            },
            work,
        });
        works.len += 1;
    }
    if let Err(error) = release_physical_completion_owner_set(&owners) {
        restore_physical_completion_reset_works(works);
        return Err(error);
    }
    for entry in works.works[..works.len].iter_mut() {
        let Some(entry) = entry.take() else {
            continue;
        };
        entry
            .owner
            .ring
            .finish_physical_worker_after_reset(entry.work, proof);
    }
    clear_physical_completion_quarantine_for_device(device_identity);
    if physical_completion_work_count_for_device(device_identity) == 0 {
        Ok(())
    } else {
        Err(AxError::BadState)
    }
}

fn retire_physical_completion_after_reset(
    outcome: axdriver::prelude::BlockResetOutcome,
) -> AxResult<()> {
    retire_physical_completion_after_reset_for_device(
        physical_completion_default_identity(),
        outcome,
    )
}

fn quarantine_physical_completion_routes(
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) {
    quarantine_physical_completion_routes_for_device(
        physical_completion_default_identity(),
        ring,
        request,
        handle,
    )
}

fn quarantine_physical_completion_routes_for_device(
    device_identity: usize,
    ring: &Arc<IoUring>,
    request: RequestId,
    handle: Option<u64>,
) {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    for group in router.groups.iter_mut().flatten() {
        let Some(owner) = group.ring.as_ref() else {
            continue;
        };
        if group.device_identity != device_identity
            || group.request != Some(request)
            || !Arc::ptr_eq(owner, ring)
        {
            continue;
        }
        for child in &mut group.children[..group.child_len] {
            if child.state == PhysicalCompletionChildState::Owner
                && handle.is_none_or(|expected| child.handle == Some(expected))
            {
                child.state = PhysicalCompletionChildState::Quarantined;
            }
        }
    }
}

fn quarantine_physical_completion(
    completion: PhysicalIoCompletion,
    replayable: bool,
) -> AxResult<()> {
    quarantine_physical_completion_for_device(
        physical_completion_default_identity(),
        completion,
        replayable,
    )
}

fn quarantine_physical_completion_for_device(
    device_identity: usize,
    completion: PhysicalIoCompletion,
    replayable: bool,
) -> AxResult<()> {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let Some(index) = router.quarantine.iter().position(Option::is_none) else {
        // The effect route remains installed and its owner remains pinned;
        // callers must take a typed reset/quarantine path instead of turning
        // this bounded-storage failure into an I/O error or fallback.
        return Err(AxError::ResourceBusy);
    };
    router.quarantine[index] = Some(QuarantinedPhysicalCompletion {
        completion,
        device_identity,
        replayable,
    });
    router.quarantine_len += 1;
    // A completion that beats the Reserved -> Owner route commit is an
    // expected publication race, not a safety quarantine.  Keep it in the
    // same bounded custody slab for exact handle/cookie replay, but reserve
    // the externally visible quarantine counter for observations that can no
    // longer be replayed safely.
    if !replayable {
        record_io_uring_physical_quarantine();
    }
    Ok(())
}

/// Removes only completion records that were observed before their exact
/// route became visible. Protocol failures from an already-owned route stay
/// in bounded custody and are never replayed into the effect a second time.
fn take_replayable_physical_completions(output: &mut [PhysicalIoCompletion]) -> usize {
    take_replayable_physical_completions_for_device(physical_completion_default_identity(), output)
}

fn take_replayable_physical_completions_for_device(
    device_identity: usize,
    output: &mut [PhysicalIoCompletion],
) -> usize {
    let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
    let mut count = 0;
    for index in 0..router.quarantine.len() {
        if count == output.len() {
            break;
        }
        let Some(record) = router.quarantine[index] else {
            continue;
        };
        if !record.replayable {
            continue;
        }
        if record.device_identity != device_identity {
            continue;
        }
        let mut matches_owner = false;
        let mut wrong_cookie_owner = false;
        for group in router.groups.iter_mut().flatten() {
            if group.device_identity != device_identity {
                continue;
            }
            for child in &mut group.children[..group.child_len] {
                if child.state != PhysicalCompletionChildState::Owner
                    || child.handle != Some(record.completion.handle)
                {
                    continue;
                }
                if child
                    .cookie
                    .is_none_or(|expected| expected == record.completion.cookie)
                {
                    matches_owner = true;
                    continue;
                }
                // The raw handle was reused with a different publication
                // cookie. Retain the stale record as diagnostic custody and
                // quarantine the new child so a lower reset, not a replay,
                // resolves the ABA.
                wrong_cookie_owner = true;
                child.state = PhysicalCompletionChildState::Quarantined;
            }
        }
        if matches_owner {
            output[count] = record.completion;
            count += 1;
            router.quarantine[index] = None;
            router.quarantine_len = router.quarantine_len.saturating_sub(1);
        } else if wrong_cookie_owner
            && let Some(record) = router.quarantine[index].as_mut()
            && record.replayable
        {
            record.replayable = false;
            // The apparent early completion has now been proven to
            // belong to an older publication of the reused raw
            // handle.  Count the transition exactly once when its
            // cookie mismatch turns replay custody into fail-stop
            // quarantine.
            record_io_uring_physical_quarantine();
        }
    }
    count
}

#[inline]
fn retained_completion_needs_quarantine(reason: PhysicalIoPendingReason) -> bool {
    !matches!(reason, PhysicalIoPendingReason::MissingCompletion { .. })
}

fn route_physical_completion(
    completion: PhysicalIoCompletion,
) -> AxResult<PhysicalIoCompletionDisposition> {
    route_physical_completion_for_device(physical_completion_default_identity(), completion)
}

fn route_physical_completion_for_device(
    device_identity: usize,
    completion: PhysicalIoCompletion,
) -> AxResult<PhysicalIoCompletionDisposition> {
    let Some((ring, request, generation, slot)) =
        lookup_physical_completion_route_identity_for_device(device_identity, completion.handle)
    else {
        // This may be a completion racing the submitter's Reserved -> Owner
        // route commit. Keep it replayable; a later task-context pass will
        // match the exact handle after the owner is atomically installed.
        quarantine_physical_completion_for_device(device_identity, completion, true)?;
        return Ok(PhysicalIoCompletionDisposition::Unknown);
    };
    if generation != physical_completion_generation_for(device_identity).unwrap_or(0) {
        // The lower transport generation changed after publication. Keep the
        // ring/work owner in reset custody; a stale late IRQ is never allowed
        // to settle a handle that a replacement transport may reuse.
        quarantine_physical_completion_routes_for_device(
            device_identity,
            &ring,
            request,
            Some(completion.handle),
        );
        quarantine_physical_completion_for_device(device_identity, completion, false)?;
        return Ok(PhysicalIoCompletionDisposition::Unknown);
    }
    ring.consume_physical_completion_for_device_at_slot(device_identity, slot, completion)
}

fn map_block_completion_error(error: DevError) -> AxError {
    match error {
        DevError::AlreadyExists => AxError::AlreadyExists,
        DevError::Again => AxError::WouldBlock,
        DevError::BadState => AxError::BadState,
        DevError::InvalidParam => AxError::InvalidInput,
        DevError::Io => AxError::Io,
        DevError::NoMemory => AxError::NoMemory,
        DevError::ResourceBusy => AxError::ResourceBusy,
        DevError::Unsupported => AxError::OperationNotSupported,
    }
}

fn convert_block_completion(completion: BlockCompletion) -> AxResult<PhysicalIoCompletion> {
    if completion.owner != BlockCompletionOwner::Physical
        || completion.handle.raw == 0
        || completion.cookie == 0
    {
        // A lower owner/type violation is a reset/quarantine condition, never
        // a synthetic EIO completion. Published effect owners stay installed
        // until an explicit device reset proves quiescence.
        return Err(AxError::BadState);
    }
    let success = match completion.status {
        BlockCompletionStatus::Success => true,
        BlockCompletionStatus::DeviceError(_) => false,
        BlockCompletionStatus::Quarantined => return Err(AxError::BadState),
    };
    Ok(PhysicalIoCompletion {
        handle: completion.handle.raw,
        cookie: completion.cookie,
        bytes: completion.bytes as usize,
        success,
    })
}

/// Keeps every lower record that was removed before a generation/type error
/// became visible.  Returning the first typed error makes the caller enter
/// the device reset path, while the bounded upper quarantine retains exact
/// handle/cookie custody instead of silently losing an earlier record in the
/// same batch.
fn quarantine_drained_block_completions(records: &[BlockCompletion], count: usize) -> AxError {
    quarantine_drained_block_completions_for_device(
        physical_completion_default_identity(),
        records,
        count,
    )
}

fn quarantine_drained_block_completions_for_device(
    device_identity: usize,
    records: &[BlockCompletion],
    count: usize,
) -> AxError {
    let mut first_error = None;
    for record in records.iter().copied().take(count.min(records.len())) {
        match convert_block_completion(record) {
            Ok(completion) => {
                if let Err(error) =
                    quarantine_physical_completion_for_device(device_identity, completion, false)
                {
                    first_error.get_or_insert(error);
                }
            }
            Err(error) => {
                first_error.get_or_insert(error);
                record_io_uring_physical_quarantine();
            }
        }
    }
    first_error.unwrap_or(AxError::BadState)
}

/// Converts one bounded lower-device drain into the exact completion records
/// consumed by the physical effect router. The lower wait is device-global,
/// so ordinary completions are never removed by this callback; the lower
/// mailbox's physical-owner filter guarantees that every record here belongs
/// to a published physical request.
fn shared_block_completion_waiter_for_device(
    device_identity: usize,
    output: &mut [PhysicalIoCompletion],
    blocking: bool,
) -> AxResult<(usize, bool)> {
    if output.is_empty() {
        return Ok((0, false));
    }
    let device =
        physical_completion_device_for(device_identity).ok_or(AxError::OperationNotSupported)?;
    let generation = device.completion_generation();
    let upper_generation = physical_completion_generation_for(device_identity).unwrap_or(0);
    let mut lower = [BlockCompletion {
        handle: BlockRequestHandle { raw: 0 },
        owner: BlockCompletionOwner::Physical,
        cookie: 0,
        status: BlockCompletionStatus::Quarantined,
        bytes: 0,
    }; IO_URING_PHYSICAL_MAX_QD];
    let limit = lower.len().min(output.len());
    let drain = if blocking {
        device
            .wait_any_physical_completion(&mut lower[..limit])
            .map_err(map_block_completion_error)?
    } else {
        device
            .drain_physical_completions(&mut lower[..limit])
            .map_err(map_block_completion_error)?
    };
    let lower_live = matches!(
        device.completion_availability(),
        BlockCompletionAvailability::Live {
            generation: observed
        } if observed == generation
    );
    if upper_generation != generation
        || !lower_live
        || !physical_completion_device_ready_for(device_identity)
    {
        // A reset/stop crossed the wait boundary. Do not route records from
        // the cancelled generation into a newly installed device owner, but
        // also do not drop records already removed from the lower mailbox:
        // retain them as non-replayable diagnostic custody with the upper
        // route/effect still installed.
        return Err(quarantine_drained_block_completions_for_device(
            device_identity,
            &lower,
            drain.completed,
        ));
    }
    if drain.completed > limit {
        let _ = quarantine_drained_block_completions_for_device(
            device_identity,
            &lower,
            drain.completed,
        );
        return Err(AxError::BadState);
    }
    for (destination, completion) in output
        .iter_mut()
        .zip(lower.iter().copied())
        .take(drain.completed)
    {
        let converted = match convert_block_completion(completion) {
            Ok(converted) => converted,
            Err(error) => {
                let _ = quarantine_drained_block_completions_for_device(
                    device_identity,
                    &lower,
                    drain.completed,
                );
                return Err(error);
            }
        };
        *destination = converted;
    }
    Ok((drain.completed, drain.continuation))
}

/// Installs the one device-global task-context completion bridge for the
/// default filesystem block device. The device handle is independent of all
/// rings, so two rings share one lower completion owner and exact global
/// handle router. A second installation is rejected rather than replacing a
/// live generation underneath published effects.
pub(crate) fn install_physical_completion_device(mut device: SharedBlockDevice) -> AxResult<()> {
    if BlockDriverOps::async_queue_caps(&device).is_none() {
        return Err(AxError::OperationNotSupported);
    }
    let identity = device.identity_token();
    {
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        if PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire) {
            return Err(AxError::BadState);
        }
        // The root role is single-owner, but an additional device may have
        // configured the shared worker before the root was available.  Only
        // an existing root role (or this exact identity) is a duplicate.
        if PHYSICAL_COMPLETION_DEFAULT_IDENTITY.load(Ordering::Acquire) != 0
            || physical_completion_device_slot(identity).is_some()
        {
            return Err(AxError::AlreadyExists);
        }
        if admission_state.install_in_progress {
            return Err(AxError::ResourceBusy);
        }
        admission_state.install_in_progress = true;
    }

    // Broker installation takes a sleeping lower mutex and is fallible. Keep
    // it outside the IRQ-disabling admission lock; only the final slot/state
    // publication takes the `admission -> registry` order.
    let mut slot_callback_context = None;
    let result = (|| {
        let generation = if device.physical_completion_broker_installed() {
            device.completion_generation()
        } else {
            device
                .install_physical_completion_broker()
                .map_err(map_block_completion_error)?
        };
        let callback_context =
            register_physical_completion_device_slot(device.clone(), generation, false)?;
        slot_callback_context = Some(callback_context);
        device
            .install_completion_progress_notifier(
                Some(physical_completion_progress_notifier as BlockCompletionNotifier),
                callback_context,
            )
            .map_err(map_block_completion_error)?;
        device
            .install_completion_terminal_notifier(
                Some(physical_completion_terminal_notifier as BlockCompletionTerminalNotifier),
                callback_context,
            )
            .map_err(map_block_completion_error)?;
        BlockDriverOps::enable_irq(&mut device).map_err(map_block_completion_error)?;
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        if !admission_state.install_in_progress
            || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
        {
            return Err(AxError::BadState);
        }
        let worker_live = PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
            && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire);
        let published_active =
            publish_physical_completion_device_slot(identity, worker_live, worker_live)?;
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
        PHYSICAL_COMPLETION_DEFAULT_IDENTITY.store(identity, Ordering::Release);
        admission_state.configured = true;
        admission_state.open = published_active;
        admission_state.generation_bump_pending = false;
        admission_state.install_in_progress = false;
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.store(false, Ordering::Release);
        clear_physical_completion_terminal_event();
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(published_active, Ordering::Release);
        drop(admission_state);
        Ok(())
    })();
    if result.is_err() {
        // No descriptor can be admitted before this point. Roll back every
        // upper callback/slot publication; an installed lower broker remains
        // a harmless owner and can be adopted by a later retry.
        if let Some(callback_context) = slot_callback_context {
            let _ = BlockDriverOps::disable_irq(&mut device);
            let _ = device.install_completion_terminal_notifier(None, 0);
            let _ = device.install_completion_progress_notifier(None, 0);
            let _ = remove_physical_completion_device_slot(identity, callback_context);
        }
        PHYSICAL_COMPLETION_ADMISSION_STATE
            .lock()
            .install_in_progress = false;
        return result;
    }
    wake_physical_completion_worker();
    Ok(())
}

/// Installs an additional axfs-registered device into the bounded completion
/// registry. It has independent admission/reset state and is never aliased to
/// the legacy root waiter or generation mailbox.
fn install_additional_physical_completion_device(mut device: SharedBlockDevice) -> AxResult<()> {
    if BlockDriverOps::async_queue_caps(&device).is_none() {
        return Err(AxError::OperationNotSupported);
    }
    {
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        if PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire) {
            return Err(AxError::BadState);
        }
        if admission_state.install_in_progress {
            return Err(AxError::ResourceBusy);
        }
        admission_state.install_in_progress = true;
    }

    let identity = device.identity_token();
    let mut slot_callback_context = None;
    let result = (|| {
        let generation = if device.physical_completion_broker_installed() {
            device.completion_generation()
        } else {
            device
                .install_physical_completion_broker()
                .map_err(map_block_completion_error)?
        };
        let callback_context = register_physical_completion_device_slot(
            device.clone(),
            generation,
            PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
                && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire),
        )?;
        slot_callback_context = Some(callback_context);
        device
            .install_completion_progress_notifier(
                Some(physical_completion_progress_notifier as BlockCompletionNotifier),
                callback_context,
            )
            .map_err(map_block_completion_error)?;
        device
            .install_completion_terminal_notifier(
                Some(physical_completion_terminal_notifier as BlockCompletionTerminalNotifier),
                callback_context,
            )
            .map_err(map_block_completion_error)?;
        BlockDriverOps::enable_irq(&mut device).map_err(map_block_completion_error)?;
        let live = PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
            && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
            && matches!(
                device.completion_availability(),
                BlockCompletionAvailability::Live { generation: observed }
                    if observed == generation
            );
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        if !admission_state.install_in_progress
            || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
        {
            return Err(AxError::BadState);
        }
        publish_physical_completion_device_slot(identity, live, live)?;
        // `configured` means that at least one exact device owner exists; it
        // is deliberately independent of the legacy root's global `open`
        // bit.  This lets an additional-only installation start and service
        // the worker even when the root device has no async queue.
        admission_state.configured = true;
        admission_state.install_in_progress = false;
        drop(admission_state);
        Ok(())
    })();
    if result.is_err() {
        if let Some(callback_context) = slot_callback_context {
            let _ = BlockDriverOps::disable_irq(&mut device);
            let _ = device.install_completion_terminal_notifier(None, 0);
            let _ = device.install_completion_progress_notifier(None, 0);
            let _ = remove_physical_completion_device_slot(identity, callback_context);
        }
        PHYSICAL_COMPLETION_ADMISSION_STATE
            .lock()
            .install_in_progress = false;
        return result;
    }
    wake_physical_completion_worker();
    Ok(())
}

/// Called once from deferred-work initialization after axfs has published its
/// root block-device registry. A missing/unsupported root device leaves
/// physical admission disabled and preserves the ordinary io_uring path.
pub(crate) fn install_default_physical_completion_device() {
    let device = match axfs::raw_block_device(axfs::ROOT_BLOCK_DEVICE_NAME) {
        Ok(device) => device,
        Err(error) => {
            debug!("io_uring physical completion disabled: root device unavailable: {error:?}");
            return;
        }
    };
    match install_physical_completion_device(device) {
        Ok(()) => debug!("io_uring physical completion owner installed for root block device"),
        Err(AxError::OperationNotSupported) => {
            debug!("io_uring physical completion disabled: root device has no async queue")
        }
        Err(AxError::AlreadyExists) => {
            debug!("io_uring physical completion owner was already installed")
        }
        Err(error) => error!("io_uring physical completion installation failed: {error:?}"),
    }
    for name in axfs::block_device_names() {
        if name == axfs::ROOT_BLOCK_DEVICE_NAME {
            continue;
        }
        let Ok(device) = axfs::raw_block_device(&name) else {
            continue;
        };
        match install_additional_physical_completion_device(device) {
            Ok(()) => debug!("io_uring physical completion owner installed for {name}"),
            Err(AxError::OperationNotSupported) => {
                debug!("io_uring physical completion disabled for {name}: no async queue")
            }
            Err(error) => {
                error!("io_uring physical completion installation failed for {name}: {error:?}")
            }
        }
    }
}

/// Stops the production bridge only after every route has retired. This is a
/// teardown hook for a future block-device unregister path; refusing a live
/// stop is essential because dropping the shared handle must not make a
/// published effect appear unowned. The lower device's notifier is cleared
/// when the final SharedBlockDevice Arc is released, making late IRQ wakes
/// harmless.
pub(crate) fn stop_physical_completion_device() -> AxResult<()> {
    // Admission is the first lock in every guard/lifecycle transition. Keep
    // it held from the per-device close through the final registry delete so
    // a guard cannot enter between the in-flight/work check and removal.
    let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    if admission_state.install_in_progress {
        return Err(AxError::ResourceBusy);
    }
    admission_state.open = false;
    let (identities, len) = physical_completion_device_identities();
    if len != 0 {
        {
            let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            for slot in registry.slots.iter_mut().flatten() {
                slot.admission_open = false;
                slot.removal_pending = true;
            }
        }

        // The admission lock fences all new guards. A running worker is also
        // fenced by the same lock at drain entry, so the following snapshot
        // cannot be invalidated by a later worker activation.
        let worker_active = PHYSICAL_COMPLETION_WORKER_ACTIVE.load(Ordering::Acquire);
        let mut busy = worker_active;
        for identity in identities[..len].iter().copied() {
            let in_flight = physical_completion_in_flight_for_device(identity);
            let work = physical_completion_work_count_for_device(identity);
            if in_flight != 0 || work != 0 {
                busy = true;
                let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
                if let Some(slot) = registry
                    .slots
                    .iter_mut()
                    .flatten()
                    .find(|slot| slot.identity == identity)
                {
                    if !slot.progress_overflowed && !slot.terminal_sequence_overflowed {
                        slot.reset_pending = true;
                    }
                    if worker_active || in_flight != 0 || work != 0 {
                        slot.active = false;
                    }
                }
            }
        }
        if busy {
            PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
            drop(admission_state);
            crate::deferred_work::wake_physical_completion_worker();
            return Err(AxError::ResourceBusy);
        }

        // No guard/work owner remains. Uninstall lower callbacks before the
        // exact slot is removed; a late callback then finds no matching
        // identity and cannot target a replacement device.
        let mut devices = [const { None }; PHYSICAL_COMPLETION_MAX_DEVICES];
        {
            let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            for (index, identity) in identities[..len].iter().copied().enumerate() {
                devices[index] = registry
                    .slots
                    .iter()
                    .flatten()
                    .find(|slot| slot.identity == identity)
                    .map(|slot| slot.device.clone());
            }
        }
        for (index, identity) in identities[..len].iter().copied().enumerate() {
            if let Some(device) = devices[index].as_ref() {
                let _ = device.install_completion_terminal_notifier(None, 0);
                let _ = device.install_completion_progress_notifier(None, 0);
            }
            clear_physical_completion_quarantine_for_device(identity);
            let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            if let Some(index) = registry
                .slots
                .iter()
                .position(|slot| slot.as_ref().is_some_and(|slot| slot.identity == identity))
            {
                registry.slots[index] = None;
            }
        }
        PHYSICAL_COMPLETION_DEFAULT_IDENTITY.store(0, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        admission_state.configured = false;
        admission_state.open = false;
        admission_state.generation_bump_pending = false;
        admission_state.in_flight = 0;
        drop(admission_state);
        clear_physical_completion_work_pending_with_recheck();
        crate::deferred_work::wake_physical_completion_worker();
        return Ok(());
    }

    let work_count = physical_completion_work_count();
    if admission_state.in_flight != 0
        || work_count != 0
        || PHYSICAL_COMPLETION_WORKER_ACTIVE.load(Ordering::Acquire)
    {
        // The legacy zero-identity test gate follows the same close-before-
        // check rule even though it has no registry slot.
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        drop(admission_state);
        crate::deferred_work::wake_physical_completion_worker();
        return Err(AxError::ResourceBusy);
    }
    admission_state.configured = false;
    admission_state.open = false;
    admission_state.generation_bump_pending = false;
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
    PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    clear_physical_completion_terminal_event();
    drop(admission_state);
    clear_physical_completion_work_pending_with_recheck();
    crate::deferred_work::wake_physical_completion_worker();
    Ok(())
}

/// Records a terminal failure of the dedicated task itself. Keep the device
/// and every published route in custody so typed reset/quiescence can prove
/// ownership; only disable new admission and cancel the generation here.
pub(crate) fn note_physical_completion_worker_stopped() {
    let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    admission_state.open = false;
    admission_state.install_in_progress = false;
    PHYSICAL_COMPLETION_WORKER_STOPPED.store(true, Ordering::Release);
    PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    if admission_state.in_flight == 0 {
        PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
    } else {
        // Do not invalidate a generation while an admitted submitter may
        // still be between lower publication and route/worker commit.  The
        // guard drop performs the deferred bump after typed custody exists.
        admission_state.generation_bump_pending = true;
    }
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let mut progress_identities = [0usize; PHYSICAL_COMPLETION_MAX_DEVICES];
    let mut progress_len = 0;
    for slot in registry.slots.iter_mut().flatten() {
        slot.active = false;
        slot.admission_open = false;
        if !slot.progress_overflowed && !slot.terminal_sequence_overflowed {
            slot.reset_pending = true;
        }
        if progress_len < progress_identities.len() {
            progress_identities[progress_len] = slot.identity;
            progress_len += 1;
        }
    }
    let root_reset_pending =
        registry.slots.iter().flatten().any(|slot| {
            slot.identity == physical_completion_default_identity() && slot.reset_pending
        });
    let device_pending = registry
        .slots
        .iter()
        .flatten()
        .any(physical_completion_device_pending_locked);
    drop(registry);
    for identity in progress_identities[..progress_len].iter().copied() {
        let _ = mark_physical_completion_device_progress(identity);
    }
    if root_reset_pending || (admission_state.configured && progress_len == 0) {
        PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
    } else {
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    }
    // Keep exact custody visible until reset/quiescence proves it is retired.
    // Clearing this bit here would let a failed owner strand a published
    // completion forever.
    PHYSICAL_COMPLETION_WORK_PENDING.store(device_pending, Ordering::Release);
    drop(admission_state);
}

pub(crate) fn note_physical_completion_worker_started() {
    let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    for slot in registry.slots.iter_mut().flatten() {
        if slot.configured
            && matches!(
                slot.device.completion_availability(),
                BlockCompletionAvailability::Live { generation }
                    if generation == slot.generation
            )
            && slot.terminal_state == PHYSICAL_COMPLETION_TERMINAL_NONE
            && !slot.progress_overflowed
        {
            slot.active = true;
            // A worker takeover inherits reset custody published by the
            // failed owner.  Do not reopen that device until its exact reset
            // marker has been serviced.
            slot.admission_open = !slot.reset_pending && !slot.removal_pending;
        } else if slot.configured {
            slot.active = false;
            slot.admission_open = false;
            if !slot.progress_overflowed && !slot.terminal_sequence_overflowed {
                slot.reset_pending = true;
            }
        }
    }
    // The worker contract is established by any configured exact device, not
    // only by the legacy root role.  In particular, an additional-only
    // installation must not leave the worker's entry predicate false.
    admission_state.configured =
        admission_state.configured || registry.slots.iter().flatten().any(|slot| slot.configured);
    let root_live = registry.slots.iter().flatten().any(|slot| {
        slot.identity == physical_completion_default_identity()
            && slot.configured
            && slot.active
            && !slot.reset_pending
            && !slot.progress_overflowed
            && !slot.terminal_sequence_overflowed
            && !slot.removal_pending
    });
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(root_live, Ordering::Release);
    admission_state.open = admission_state.configured && root_live;
}

pub(crate) fn physical_completion_worker_is_stopped() -> bool {
    PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
}

/// Resets the installed lower device through its typed quiescence path. A
/// quarantined result keeps admission disabled and leaves every upper route
/// in custody until reset proves quiescence.  A quiescent reset then retires
/// the old generation's routes/work; only a reusable (`Quiesced`) queue is
/// re-enabled.  A permanently dismantled (`Retired`) queue stays closed, and
/// late completions from a cancelled generation cannot reach a new request.
pub(crate) fn reset_physical_completion_device() -> AxResult<()> {
    if let Some(identity) = (physical_completion_default_identity() != 0)
        .then(physical_completion_default_identity)
        .filter(|identity| physical_completion_generation_for(*identity).is_some())
    {
        mark_physical_completion_device_reset_pending(identity);
        return service_physical_completion_reset_for_device(identity);
    }
    let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    if admission_state.in_flight != 0 {
        // Close new admission, but keep the generation and every existing
        // route/work owner intact.  The admitted submitter may already have
        // published below this fence and still needs to commit its upper
        // route before the next reset attempt can prove custody.
        admission_state.open = false;
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        drop(admission_state);
        return Err(AxError::ResourceBusy);
    }
    let device = physical_completion_device_for(physical_completion_default_identity())
        .ok_or(AxError::OperationNotSupported)?;
    admission_state.open = false;
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_GENERATION.fetch_add(1, Ordering::AcqRel);
    // Keep the pending bit asserted until lower reset plus upper retirement
    // has completed.  Clearing it before that proof lets the worker return
    // while a route/effect owner is still live.
    PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    drop(admission_state);
    let mut device = device;
    BlockDriverOps::reset_device(&mut device).map_err(map_block_completion_error)?;
    let Some(event) = physical_completion_terminal_event() else {
        // The installed lower broker must publish the terminal proof before
        // reset returns.  Without that exact callback event, neither the
        // local return value nor a device generation read is sufficient to
        // retire upper custody or reopen admission safely.
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        admission_state.open = false;
        drop(admission_state);
        return Err(AxError::BadState);
    };
    let Some(event_outcome) = physical_completion_terminal_outcome(event.state) else {
        // Quarantine is a terminal notification without a physical
        // quiescence proof. Keep all upper route/effect owners in custody and
        // leave the marker for a later recovery event; do not reinterpret the
        // local reset return as a quiescent result.
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
        admission_state.open = false;
        drop(admission_state);
        return Err(AxError::BadState);
    };

    // The callback's exact generation/outcome is the only proof used here.
    // Do not release any route, ring slot, or registered-buffer pin before
    // this point; after it, retire every old-generation owner so the global
    // route/work counters cannot strand close.  A newer callback may replace
    // `event` while this exact owner set is being retired; the finish step
    // then preserves that newer event and keeps admission fenced.
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(event.generation, Ordering::Release);
    if let Err(error) = retire_physical_completion_after_reset(event_outcome) {
        PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        return Err(error);
    }

    finish_physical_completion_external_terminal_event(event, event_outcome)?;
    if matches!(event_outcome, BlockResetOutcome::Retired)
        || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
    {
        // Quiescence released all ring custody. A caller may perform final
        // device close even when the old worker cannot be restarted.
        return if matches!(event_outcome, BlockResetOutcome::Retired) {
            Ok(())
        } else {
            Err(AxError::BadState)
        };
    }
    Ok(())
}

fn physical_completion_in_flight() -> usize {
    PHYSICAL_COMPLETION_ADMISSION_STATE.lock().in_flight
}

/// Schedules a later reset/terminal-custody pass.  The one-millisecond task
/// sleep is deliberately outside the lower/device lock and gives an admitted
/// submitter time to finish its publication/route commit; a direct wake here
/// would turn a persistent `ResourceBusy` into a busy loop.
fn defer_physical_completion_reset_retry() {
    clear_physical_completion_work_pending_with_recheck();
    if let Err(error) = axtask::sleep(Duration::from_millis(1)) {
        // There is no safe fallback after a published effect has lost its
        // lower reset progress edge.  Stop the kernel rather than clearing
        // custody or manufacturing a terminal completion.
        panic!("io_uring physical reset retry sleep failed; upper custody remains live: {error:?}");
    }
    PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    crate::deferred_work::wake_physical_completion_worker();
}

/// Consumes one terminal notification from the lower shared broker.  This is
/// an upper-only path: the lower reset already proved the supplied outcome,
/// so invoking `reset_device` again would race the owner that delivered the
/// notification and could lose the generation boundary.
fn retire_physical_completion_after_external_terminal() -> AxResult<()> {
    let Some(event) = physical_completion_terminal_event() else {
        return Ok(());
    };
    let state = event.state;
    if state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
        // A quarantined lower queue provides no PhysicalIoResetProof. Keep
        // every upper route/effect owner installed, but suppress the worker
        // predicate until a later lower recovery event supplies proof.
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        return Err(AxError::BadState);
    }
    if physical_completion_in_flight() != 0 {
        defer_physical_completion_reset_retry();
        return Err(AxError::ResourceBusy);
    }
    let outcome = physical_completion_terminal_outcome(state).ok_or(AxError::BadState)?;
    // Fence all old-generation route lookups before retiring their exact
    // owners.  `retire_physical_completion_after_reset` still performs the
    // route/work transaction and drops each effect only after this proof.
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(event.generation, Ordering::Release);
    retire_physical_completion_after_reset(outcome)?;
    finish_physical_completion_external_terminal_event(event, outcome)
}

/// Commits one exact terminal event after its upper routes/work have retired.
/// The admission lock is taken before the event lock everywhere this helper
/// participates in lifecycle publication.  A newer lower reset can publish
/// while route retirement runs; in that case the sequence no longer matches,
/// so the old event leaves admission fenced and re-wakes the worker instead of
/// clearing the newer marker or reopening the old generation.
fn finish_physical_completion_external_terminal_event(
    event: PhysicalCompletionTerminalEvent,
    outcome: BlockResetOutcome,
) -> AxResult<()> {
    let mut admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    let _event_lock = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
    let current_sequence = PHYSICAL_COMPLETION_TERMINAL_EVENT_SEQUENCE.load(Ordering::Acquire);
    let consumed_sequence = PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.load(Ordering::Acquire);
    if current_sequence != event.sequence || consumed_sequence != event.consumed_sequence {
        drop(_event_lock);
        admission_state.open = false;
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        drop(admission_state);
        crate::deferred_work::wake_physical_completion_worker();
        return Err(AxError::ResourceBusy);
    }

    let reusable = matches!(outcome, BlockResetOutcome::Quiesced)
        && admission_state.configured
        && !PHYSICAL_COMPLETION_TERMINAL_SEQUENCE_OVERFLOWED.load(Ordering::Acquire)
        && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
        && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire);
    PHYSICAL_COMPLETION_TERMINAL_CONSUMED_SEQUENCE.store(event.sequence, Ordering::Release);
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
    PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(reusable, Ordering::Release);
    admission_state.open = reusable;
    drop(_event_lock);
    drop(admission_state);
    clear_physical_completion_work_pending_with_recheck();
    Ok(())
}

/// Services either an external terminal marker or a reset requested by the
/// upper worker.  A Busy result leaves both marker and custody live and is
/// retried only through the delayed wake above.
fn service_physical_completion_reset() -> AxResult<()> {
    if physical_completion_terminal_event().is_some() {
        return retire_physical_completion_after_external_terminal();
    }
    if !PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire) {
        return Ok(());
    }
    match reset_physical_completion_device() {
        Ok(()) => Ok(()),
        Err(AxError::ResourceBusy) => {
            defer_physical_completion_reset_retry();
            Err(AxError::ResourceBusy)
        }
        Err(error) => {
            // Lower reset failure retains route/effect custody.  If the
            // lower notifier produced a terminal marker, the next worker
            // pass will consume it; otherwise retry with the same bounded
            // delayed edge rather than dropping the owner.
            if physical_completion_terminal_event().is_none() {
                defer_physical_completion_reset_retry();
            }
            Err(error)
        }
    }
}

fn physical_completion_in_flight_for_device(device_identity: usize) -> usize {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .map_or(0, |slot| slot.in_flight)
}

fn finish_physical_completion_external_terminal_event_for_device(
    device_identity: usize,
    event: PhysicalCompletionTerminalEvent,
    outcome: BlockResetOutcome,
) -> AxResult<()> {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let Some(slot) = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)
    else {
        return Err(AxError::BadState);
    };
    if slot.terminal_sequence != event.sequence
        || slot.terminal_consumed_sequence != event.consumed_sequence
    {
        slot.active = false;
        slot.reset_pending = true;
        return Err(AxError::ResourceBusy);
    }
    let reusable = matches!(outcome, BlockResetOutcome::Quiesced)
        && slot.configured
        && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
        && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire);
    slot.terminal_consumed_sequence = event.sequence;
    slot.terminal_state = PHYSICAL_COMPLETION_TERMINAL_NONE;
    slot.generation = event.generation;
    slot.reset_pending = false;
    let mut slot_active = reusable && !slot.removal_pending;
    slot.active = slot_active;
    slot.admission_open = slot_active;
    slot.progress_pending = false;
    // Keep the durable sequence monotonic across a transport reset.  A new
    // lower edge after this clear must advance it, rather than reusing the
    // transport generation that normally remains zero.
    if slot.progress_overflowed || slot.terminal_sequence_overflowed {
        slot_active = false;
        slot.active = false;
        slot.admission_open = false;
        // Overflow is a stable fail-closed fence.  Do not re-arm reset after
        // consuming one exact lower proof; removal/reinstall is required to
        // obtain a fresh bounded sequence namespace.
        slot.reset_pending = false;
    }
    let slot_reset_pending = slot.reset_pending;
    drop(registry);
    if device_identity == physical_completion_default_identity() {
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(event.generation, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(slot_active, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(slot_reset_pending, Ordering::Release);
        clear_physical_completion_work_pending_with_recheck();
    }
    Ok(())
}

fn stabilize_physical_completion_overflow(device_identity: usize) {
    let stable = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        else {
            return;
        };
        if slot.progress_overflowed || slot.terminal_sequence_overflowed {
            slot.active = false;
            slot.admission_open = false;
            slot.reset_pending = false;
            true
        } else {
            false
        }
    };
    if stable && device_identity == physical_completion_default_identity() {
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    }
    if stable {
        clear_physical_completion_work_pending_with_recheck();
    }
}

/// Services one registry slot without consulting root-device globals.  The
/// lower SharedBlockDevice callback supplies the exact terminal generation;
/// only after that proof are this device's routes and ring owners retired.
fn service_physical_completion_reset_for_device(device_identity: usize) -> AxResult<()> {
    if let Some(event) = physical_completion_terminal_event_for_device(device_identity) {
        if event.state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
            if physical_completion_device_progress_overflowed(device_identity)
                || physical_completion_device_terminal_sequence_overflowed(device_identity)
            {
                // Quarantine is an explicit lower terminal state but not a
                // physical retirement proof.  For an already-overflowed
                // marker, consume only the notification and remain fenced;
                // repeated worker wakes cannot produce a new proof.
                clear_physical_completion_terminal_event_for_device(device_identity);
                stabilize_physical_completion_overflow(device_identity);
            }
            return Err(AxError::BadState);
        }
        let outcome = physical_completion_terminal_outcome(event.state).ok_or(AxError::BadState)?;
        if physical_completion_in_flight_for_device(device_identity) != 0 {
            defer_physical_completion_reset_retry();
            return Err(AxError::ResourceBusy);
        }
        physical_completion_generation_store_for_device(device_identity, event.generation);
        retire_physical_completion_after_reset_for_device(device_identity, outcome)?;
        return finish_physical_completion_external_terminal_event_for_device(
            device_identity,
            event,
            outcome,
        );
    }

    let device = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        else {
            return Err(AxError::OperationNotSupported);
        };
        if !slot.reset_pending {
            return Ok(());
        }
        if slot.in_flight != 0 {
            return Err(AxError::ResourceBusy);
        }
        slot.active = false;
        slot.device.clone()
    };

    if device_identity == physical_completion_default_identity() {
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
    }
    let mut device = device;
    let reset_result =
        BlockDriverOps::reset_device(&mut device).map_err(map_block_completion_error);
    if let Err(error) = reset_result {
        if physical_completion_device_progress_overflowed(device_identity)
            || physical_completion_device_terminal_sequence_overflowed(device_identity)
        {
            stabilize_physical_completion_overflow(device_identity);
        }
        return Err(error);
    }
    let Some(event) = physical_completion_terminal_event_for_device(device_identity) else {
        // Reset without the exact lower terminal callback is not enough to
        // retire an upper route.  An overflowed marker has now spent its one
        // reset attempt and enters stable custody until an external exact
        // proof or slot removal/reinstall; ordinary markers remain pending
        // for a later recovery edge.
        if physical_completion_device_progress_overflowed(device_identity)
            || physical_completion_device_terminal_sequence_overflowed(device_identity)
        {
            stabilize_physical_completion_overflow(device_identity);
        }
        return Err(AxError::BadState);
    };
    if event.state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
        if physical_completion_device_progress_overflowed(device_identity)
            || physical_completion_device_terminal_sequence_overflowed(device_identity)
        {
            clear_physical_completion_terminal_event_for_device(device_identity);
            stabilize_physical_completion_overflow(device_identity);
        }
        return Err(AxError::BadState);
    }
    let outcome = physical_completion_terminal_outcome(event.state).ok_or(AxError::BadState)?;
    physical_completion_generation_store_for_device(device_identity, event.generation);
    retire_physical_completion_after_reset_for_device(device_identity, outcome)?;
    finish_physical_completion_external_terminal_event_for_device(device_identity, event, outcome)
}

fn physical_completion_generation_store_for_device(device_identity: usize, generation: u64) {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if let Some(slot) = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)
    {
        slot.generation = generation;
    }
    if device_identity == physical_completion_default_identity() {
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }
}

#[cfg(feature = "test-io-control")]
pub(crate) fn io_uring_physical_quarantine_len() -> usize {
    PHYSICAL_COMPLETION_ROUTER.lock().quarantine_len
}

fn physical_completion_work_count() -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router.work_count.saturating_add(router.pending_count)
}

fn physical_completion_work_count_for_device(device_identity: usize) -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router
        .groups
        .iter()
        .flatten()
        .filter(|group| group.device_identity == device_identity)
        .count()
        + router
            .pending
            .iter()
            .flatten()
            .filter(|owner| owner.device_identity == device_identity)
            .count()
}

fn mark_physical_completion_device_reset_pending(device_identity: usize) {
    let (found, stable_overflow) = {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        {
            slot.active = false;
            slot.admission_open = false;
            let stable_overflow = slot.progress_overflowed || slot.terminal_sequence_overflowed;
            if !stable_overflow {
                slot.reset_pending = true;
            }
            (true, stable_overflow)
        } else {
            (false, false)
        }
    };
    if found && !stable_overflow {
        let _ = mark_physical_completion_device_progress(device_identity);
    }
    if device_identity == physical_completion_default_identity() && !stable_overflow {
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
    }
    if !found {
        // Keep legacy zero-identity lifecycle tests wakeable without letting
        // an unknown production identity mutate a sibling registry slot.
        wake_physical_completion_worker();
    }
}

fn physical_completion_device_reset_pending(device_identity: usize) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.reset_pending)
}

fn physical_completion_device_progress_generation(device_identity: usize) -> Option<u64> {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .map(|slot| slot.progress_generation)
}

fn physical_completion_device_progress_overflowed(device_identity: usize) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.progress_overflowed)
}

fn physical_completion_device_terminal_sequence_overflowed(device_identity: usize) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.terminal_sequence_overflowed)
}

fn clear_physical_completion_device_progress_if_unchanged(
    device_identity: usize,
    observed_generation: Option<u64>,
) {
    let Some(observed_generation) = observed_generation else {
        return;
    };
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    if let Some(slot) = registry
        .slots
        .iter_mut()
        .flatten()
        .find(|slot| slot.identity == device_identity)
    {
        let mut progress = PhysicalCompletionProgressState {
            pending: slot.progress_pending,
            generation: slot.progress_generation,
            overflowed: slot.progress_overflowed,
        };
        clear_physical_completion_progress_if_unchanged(&mut progress, Some(observed_generation));
        slot.progress_pending = progress.pending;
        slot.progress_generation = progress.generation;
        slot.progress_overflowed = progress.overflowed;
    }
}

fn physical_completion_device_pending_locked(slot: &PhysicalCompletionDeviceSlot) -> bool {
    let terminal_pending = slot.terminal_state != PHYSICAL_COMPLETION_TERMINAL_NONE
        && slot.terminal_sequence != slot.terminal_consumed_sequence;
    // A sequence-overflow fence is stable terminal state.  It remains
    // admission-closed and retains custody, but must not make the worker
    // repeatedly reset/wake forever.  A later exact terminal proof is still
    // visible through `terminal_pending` and may retire custody once.
    if slot.terminal_sequence_overflowed && !terminal_pending {
        return false;
    }
    if slot.progress_overflowed && !terminal_pending {
        // The first overflow keeps `reset_pending` set so one typed reset may
        // seek a lower proof.  The service path clears it after that attempt,
        // converting the slot to stable fenced state.
        return slot.reset_pending;
    }
    slot.progress_pending || slot.reset_pending || terminal_pending
}

pub(crate) fn physical_completion_device_ready() -> bool {
    let identity = physical_completion_default_identity();
    identity != 0 && physical_completion_device_ready_for(identity)
}

/// Checks readiness for the exact mounted SharedBlockDevice selected by the
/// filesystem identity. A ready sibling device cannot authorize this file.
pub(crate) fn physical_completion_device_ready_for(device_identity: usize) -> bool {
    if device_identity == 0 {
        return false;
    }
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry.slots.iter().flatten().any(|slot| {
        slot.identity == device_identity
            && slot.configured
            && slot.active
            && !slot.reset_pending
            && !slot.progress_overflowed
            && !slot.terminal_sequence_overflowed
            && PHYSICAL_COMPLETION_WORKER_STARTED.load(Ordering::Acquire)
            && !PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire)
            && slot.device.physical_completion_broker_installed()
            && matches!(
                slot.device.completion_availability(),
                BlockCompletionAvailability::Live { generation }
                    if generation == slot.generation
            )
    })
}

/// Returns whether the dedicated completion owner has a wakeable physical
/// queue.  Published work without a mount-bound waiter remains in custody,
/// but does not make the scheduler spin; the filesystem integration must
/// install/wake its exact device bridge before waiting can begin.
pub(crate) fn has_physical_completion_work() -> bool {
    let terminal = physical_completion_terminal_event()
        .map_or(PHYSICAL_COMPLETION_TERMINAL_NONE, |event| event.state);
    let global_pending = PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire);
    let legacy_pending = PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire)
        || terminal != PHYSICAL_COMPLETION_TERMINAL_NONE;
    if !global_pending && !legacy_pending {
        let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        if !registry
            .slots
            .iter()
            .flatten()
            .any(physical_completion_device_pending_locked)
        {
            return false;
        }
    }
    if legacy_pending && terminal != PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
        return true;
    }
    let (identities, len) = physical_completion_device_identities();
    (0..len).any(|index| {
        let identity = identities[index];
        if let Some(event) = physical_completion_terminal_event_for_device(identity) {
            return event.state != PHYSICAL_COMPLETION_TERMINAL_QUARANTINED;
        }
        let pending = {
            let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            registry
                .slots
                .iter()
                .flatten()
                .find(|slot| slot.identity == identity)
                .is_some_and(physical_completion_device_pending_locked)
        };
        pending
            || (physical_completion_device_ready_for(identity)
                && physical_completion_custody_count_for_device(identity) != 0)
    })
}

#[inline]
fn physical_completion_reset_or_terminal_pending() -> bool {
    if PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire)
        || physical_completion_terminal_event().is_some()
    {
        return true;
    }
    let (identities, len) = physical_completion_device_identities();
    (0..len).any(|index| {
        let identity = identities[index];
        let progress = {
            let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
            registry
                .slots
                .iter()
                .flatten()
                .find(|slot| slot.identity == identity)
                .is_some_and(physical_completion_device_pending_locked)
        };
        progress
            || (physical_completion_device_reset_pending(identity)
                && !physical_completion_device_progress_overflowed(identity)
                && !physical_completion_device_terminal_sequence_overflowed(identity))
            || physical_completion_terminal_event_for_device(identity).is_some()
    })
}

/// Clears one worker activation's pending bit, then rechecks the lifecycle
/// markers before allowing the caller to arm a lower wait or return.  A
/// notifier can publish a marker between the caller's first check and this
/// clear; republishing both the bit and the wake closes that check/clear
/// lost-wake window.  If the notifier races immediately after the recheck it
/// owns the release-store to `WORK_PENDING`, so the false state cannot remain
/// stable without the next wake edge.
fn clear_physical_completion_work_pending_with_recheck() -> bool {
    // Hold both marker locks while deciding whether the global fast bit may
    // be cleared. Device progress callbacks take the registry lock; legacy
    // terminal callbacks take the mailbox lock. Thus a notifier racing this
    // section either publishes before the recheck or after the clear and owns
    // the next wake, with no empty PollSet window.
    let keep_pending = {
        let _event = PHYSICAL_COMPLETION_TERMINAL_EVENT_LOCK.lock();
        let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let legacy_pending = PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire)
            || PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire)
                != PHYSICAL_COMPLETION_TERMINAL_NONE;
        let device_pending = registry
            .slots
            .iter()
            .flatten()
            .any(physical_completion_device_pending_locked);
        let keep_pending = legacy_pending || device_pending;
        if !keep_pending {
            PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        }
        keep_pending
    };
    if keep_pending {
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        crate::deferred_work::wake_physical_completion_worker();
    }
    keep_pending
}

/// Publishes one generation to the dedicated completion task.  IRQ code and
/// submitter code only call this allocation-free wake; all device waiting and
/// exact demultiplexing stays in task context.
pub(crate) fn wake_physical_completion_worker() {
    PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    crate::deferred_work::wake_physical_completion_worker();
}

/// Selects a bounded set of distinct pending owners in ring/slot
/// round-robin order.  The callback is only an observation; the caller still
/// takes each exact owner by `(ring, slot)` before retrying it.  Keeping the
/// cursor state as two indexes makes the selector allocation-free and avoids
/// coupling retry fairness to the order in which route extents happen to
/// occupy the global table.
fn select_physical_finalization_round_robin(
    ring_count: usize,
    ring_cursor: &mut usize,
    slot_cursor: &mut usize,
    selected: &mut [(usize, usize); PHYSICAL_FINALIZATION_RETRY_BUDGET],
    mut is_pending: impl FnMut(usize, usize) -> bool,
) -> usize {
    if ring_count == 0 {
        return 0;
    }

    let ring_start = *ring_cursor % ring_count;
    let mut next_slot = *slot_cursor % IO_URING_PHYSICAL_MAX_QD;
    let mut selected_len = 0;
    let mut last_ring = ring_start;
    let scan_budget = ring_count.saturating_mul(PHYSICAL_FINALIZATION_RETRY_BUDGET);

    for position in 0..scan_budget {
        if selected_len == PHYSICAL_FINALIZATION_RETRY_BUDGET {
            break;
        }
        let ring_index = (ring_start + position) % ring_count;
        let slot = (0..IO_URING_PHYSICAL_MAX_QD).find_map(|offset| {
            let slot = (next_slot + offset) % IO_URING_PHYSICAL_MAX_QD;
            if selected[..selected_len]
                .iter()
                .any(|&(selected_ring, selected_slot)| {
                    selected_ring == ring_index && selected_slot == slot
                })
            {
                return None;
            }
            is_pending(ring_index, slot).then_some(slot)
        });
        let Some(slot) = slot else {
            continue;
        };
        selected[selected_len] = (ring_index, slot);
        selected_len += 1;
        last_ring = ring_index;
        next_slot = (slot + 1) % IO_URING_PHYSICAL_MAX_QD;
    }

    if selected_len != 0 {
        *ring_cursor = (last_ring + 1) % ring_count;
    }
    *slot_cursor = next_slot;
    selected_len
}

/// The finalization timer is a liveness edge, not a correctness edge.  If
/// task sleep itself fails, immediate self-wake would turn a scheduler fault
/// into an unbounded retry loop.  Stop this owner and leave exact physical
/// custody for the typed reset supervisor instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PhysicalFinalizationSleepErrorAction {
    FailStop,
}

const fn physical_finalization_sleep_error_action() -> PhysicalFinalizationSleepErrorAction {
    PhysicalFinalizationSleepErrorAction::FailStop
}

fn fail_stop_physical_finalization_worker(error: AxError) {
    match physical_finalization_sleep_error_action() {
        PhysicalFinalizationSleepErrorAction::FailStop => {
            note_physical_completion_worker_stopped();
            let reset = reset_physical_completion_device();
            clear_physical_completion_work_pending_with_recheck();
            PHYSICAL_COMPLETION_WORKER_ACTIVE.store(false, Ordering::Release);
            match reset {
                Ok(()) => error!(
                    "io_uring physical finalization retry sleep failed; worker fail-stopped: \
                     {error:?}"
                ),
                Err(reset_error) => panic!(
                    "io_uring physical finalization retry sleep failed ({error:?}); reset could \
                     not retire exact owners: {reset_error:?}"
                ),
            }
        }
    }
}

fn complete_pending_physical_prepublication_error(
    mut work: PhysicalIoWork,
    error: LinuxError,
) -> AxResult<()> {
    let ring = Arc::clone(&work.ring);
    let request = work.request_id().ok_or(AxError::BadState)?;
    let device_identity = work.device_identity();
    let generation = work.device_generation();
    let slot = work.slot();
    if !clear_physical_completion_pending_owner(&ring, request, slot, device_identity, generation) {
        return Err(AxError::BadState);
    }
    let issued = work.take_issued().ok_or(AxError::BadState)?;
    let admission = work.take_admission().ok_or(AxError::BadState)?;
    work.pending_publication = false;
    drop(admission);
    // The pending effect has never been device-visible. Releasing the empty
    // work drops the exact logical QD charge before publishing its CQE.
    drop(work);
    ring.complete_issued(issued, TerminalCause::Completed, -error.code(), 0)
}

fn handoff_pending_physical_publication(
    mut work: PhysicalIoWork,
) -> AxResult<(IssuedRequest, PreparedPhysicalIoAdmission)> {
    let issued = work.take_issued().ok_or(AxError::BadState)?;
    let admission = work.take_admission().ok_or(AxError::BadState)?;
    // The retry reservation continues to own the charged slot. Prevent the
    // now-empty shell from releasing that charge before commit installs the
    // published owner in the same slot.
    work.pending_publication = false;
    work.slot = usize::MAX;
    drop(work);
    Ok((issued, admission))
}

/// Retries at most two exact pending owners for one device after a completion
/// pass has returned descriptor credit. A queue-full result simply restores
/// the same Prepared owner. The caller distinguishes a pending-only tail that
/// needs a delayed task-context retry from work that owns a real completion
/// edge; neither case may use synchronous I/O fallback.
fn retry_pending_physical_publications_for_device(
    device_identity: usize,
) -> AxResult<PhysicalPublicationRetryDisposition> {
    let start =
        PHYSICAL_PUBLICATION_RETRY_CURSOR.fetch_add(1, Ordering::AcqRel) % IO_URING_PHYSICAL_MAX_QD;
    let mut attempts = 0usize;
    let mut published = false;

    for offset in 0..IO_URING_PHYSICAL_MAX_QD {
        if attempts == PHYSICAL_PUBLICATION_RETRY_BUDGET {
            break;
        }
        let index = (start + offset) % IO_URING_PHYSICAL_MAX_QD;
        let Some((ring, request, slot, identity, generation)) =
            physical_completion_pending_owner_snapshot(index)
        else {
            continue;
        };
        if identity != device_identity {
            continue;
        }
        let mut reservation = match ring
            .reserve_pending_physical_worker_slot_for_retry(identity, request, slot, generation)
        {
            Ok(reservation) => reservation,
            Err(AxError::ResourceBusy) => continue,
            Err(AxError::BadState) => {
                mark_physical_completion_device_reset_pending(identity);
                continue;
            }
            Err(error) => return Err(error),
        };
        attempts += 1;
        let Some(mut work) =
            ring.take_pending_physical_worker_for_retry(identity, request, slot, generation)
        else {
            drop(reservation);
            mark_physical_completion_device_reset_pending(identity);
            continue;
        };
        let extent_count = match work
            .admission()
            .ok_or(AxError::BadState)
            .and_then(PreparedPhysicalIoAdmission::physical_extent_count)
        {
            Ok(count) => count,
            Err(_) => {
                drop(reservation);
                complete_pending_physical_prepublication_error(work, LinuxError::EIO)?;
                continue;
            }
        };
        match reservation.reserve_completion_routes(extent_count) {
            Ok(()) => {}
            Err(AxError::ResourceBusy) => {
                ring.retain_physical_worker_work(work)?;
                drop(reservation);
                continue;
            }
            Err(_) => {
                drop(reservation);
                complete_pending_physical_prepublication_error(work, LinuxError::EIO)?;
                continue;
            }
        }
        if reservation
            .bind_admission(work.admission_mut().ok_or(AxError::BadState)?)
            .is_err()
        {
            drop(reservation);
            complete_pending_physical_prepublication_error(work, LinuxError::EIO)?;
            continue;
        }
        let outcome = reservation.with_physical_publish(|| unsafe {
            work.admission_mut().ok_or(AxError::BadState)?.publish()
        });
        match outcome {
            Ok(Ok(PhysicalIoPublishOutcome::NotSubmitted(
                PhysicalIoNotSubmittedReason::Backpressure,
            ))) => {
                ring.retain_physical_worker_work(work)?;
                drop(reservation);
            }
            Ok(Ok(PhysicalIoPublishOutcome::NotSubmitted(reason))) => {
                drop(reservation);
                let error = match reason {
                    PhysicalIoNotSubmittedReason::Unsupported => LinuxError::EOPNOTSUPP,
                    PhysicalIoNotSubmittedReason::NoMemory => LinuxError::ENOMEM,
                    PhysicalIoNotSubmittedReason::Invalid => LinuxError::EINVAL,
                    PhysicalIoNotSubmittedReason::Backpressure => unreachable!(),
                };
                complete_pending_physical_prepublication_error(work, error)?;
            }
            Ok(Ok(
                PhysicalIoPublishOutcome::Published(_) | PhysicalIoPublishOutcome::Terminal(_),
            )) => {
                let (issued, admission) = handoff_pending_physical_publication(work)?;
                reservation.commit(issued, admission)?;
                published = true;
            }
            Ok(Err(error)) => {
                let (issued, admission) = handoff_pending_physical_publication(work)?;
                reservation.commit(issued, admission)?;
                error!("io_uring pending physical publication entered quarantine: {error:?}");
                published = true;
            }
            Err(_) => {
                drop(reservation);
                complete_pending_physical_prepublication_error(work, LinuxError::EIO)?;
            }
        }
    }
    Ok(physical_publication_retry_disposition(
        published,
        physical_completion_pending_owner_count_for_device(device_identity) != 0,
        physical_completion_route_count_for_device(device_identity) != 0,
    ))
}

fn retry_pending_physical_finalizations() -> AxResult<bool> {
    let mut rings = [const { None }; IO_URING_PHYSICAL_MAX_QD];
    let mut ring_len = 0usize;
    {
        let router = PHYSICAL_COMPLETION_ROUTER.lock();
        for group in router.groups.iter().flatten() {
            let Some(ring) = group.ring.as_ref() else {
                continue;
            };
            if !group.children[..group.child_len]
                .iter()
                .any(|child| child.state == PhysicalCompletionChildState::Owner)
            {
                continue;
            }
            if rings[..ring_len]
                .iter()
                .flatten()
                .any(|existing| Arc::ptr_eq(existing, ring))
            {
                continue;
            }
            if ring_len == rings.len() {
                return Err(AxError::BadState);
            }
            rings[ring_len] = Some(Arc::clone(ring));
            ring_len += 1;
        }
    }

    let mut ring_cursor = PHYSICAL_FINALIZATION_RETRY_RING_CURSOR.load(Ordering::Acquire);
    let mut slot_cursor = PHYSICAL_FINALIZATION_RETRY_SLOT_CURSOR.load(Ordering::Acquire);
    let mut selected = [(0usize, 0usize); PHYSICAL_FINALIZATION_RETRY_BUDGET];
    let selected_len = select_physical_finalization_round_robin(
        ring_len,
        &mut ring_cursor,
        &mut slot_cursor,
        &mut selected,
        |ring_index, slot| {
            rings[ring_index]
                .as_ref()
                .is_some_and(|ring| ring.has_physical_finalization_retry_at_slot(slot))
        },
    );
    PHYSICAL_FINALIZATION_RETRY_RING_CURSOR.store(ring_cursor, Ordering::Release);
    PHYSICAL_FINALIZATION_RETRY_SLOT_CURSOR.store(slot_cursor, Ordering::Release);

    for (attempt_index, &(ring_index, slot)) in selected[..selected_len].iter().enumerate() {
        let ring = rings[ring_index].as_ref().ok_or(AxError::BadState)?;
        // A concurrent reset may remove an item after selection.  In that
        // case the exact owner is already in reset custody; never synthesize
        // a replacement attempt or discard another slot's owner.
        let disposition = ring.retry_physical_finalization_at_slot(slot)?;
        if disposition.is_some() && attempt_index + 1 < selected_len {
            axtask::yield_now();
        }
    }

    Ok(rings[..ring_len]
        .iter()
        .flatten()
        .any(|ring| ring.has_physical_finalization_retry()))
}

/// The single task-context consumer for all registered shared block devices.
/// Each activation scans the fixed device registry and performs one
/// non-blocking bounded drain per slot. Sleeping on one idle root queue would
/// strand a completion on vdb, so IRQ/progress notifications wake this owner
/// and the next activation selects the exact device again.
pub(crate) fn drain_physical_completion_work() {
    // Lifecycle teardown holds this same short lock while closing every
    // device's admission fence and checking worker ownership.  Taking it
    // before the active-owner CAS prevents a worker from entering the gap
    // between that check and registry removal.
    let admission_state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
    if !admission_state.configured || PHYSICAL_COMPLETION_WORKER_STOPPED.load(Ordering::Acquire) {
        drop(admission_state);
        return;
    }
    if PHYSICAL_COMPLETION_WORKER_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        drop(admission_state);
        return;
    }
    drop(admission_state);
    match retry_pending_physical_finalizations() {
        Ok(true) => {
            // Finalization Busy has no new device completion to wait for.
            // This activation has already spent its fixed retry budget;
            // leave the work in its slot and use the existing task timer for
            // a delayed next activation. The timer edge is the liveness
            // source when no further device IRQ is expected; the fixed delay
            // keeps this self-continuation from becoming a busy loop.
            clear_physical_completion_work_pending_with_recheck();
            if let Err(error) = axtask::sleep(Duration::from_millis(1)) {
                // Do not leave the exact finalization owner with both the
                // pending bit and worker inactive: a failed sleep would
                // otherwise strand it forever.  A direct wake here could
                // busy-loop if the scheduler keeps rejecting sleep, so the
                // bounded fail-stop path disables admission and hands all
                // owners to reset/quiescence custody.
                fail_stop_physical_finalization_worker(error);
                return;
            }
            PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
            crate::deferred_work::wake_physical_completion_worker();
            PHYSICAL_COMPLETION_WORKER_ACTIVE.store(false, Ordering::Release);
            return;
        }
        Ok(false) => {}
        Err(error) => {
            // The slot-level retry path fences the exact work device before
            // returning this error. A selector invariant failure has no
            // device identity to broaden safely, so leave existing custody
            // untouched and let the next bounded pass re-evaluate it.
            PHYSICAL_COMPLETION_WORKER_ACTIVE.store(false, Ordering::Release);
            error!("io_uring physical finalization retry failed: {error:?}");
            return;
        }
    }
    let (identities, len) = physical_completion_device_identities();
    let mut output = [PhysicalIoCompletion {
        handle: 0,
        cookie: 0,
        bytes: 0,
        success: false,
    }; IO_URING_PHYSICAL_MAX_QD];
    let mut continuation = false;
    let mut delayed_publication_retry = false;
    let device_start = if len == 0 {
        0
    } else {
        PHYSICAL_COMPLETION_DEVICE_CURSOR.fetch_add(1, Ordering::AcqRel) % len
    };
    for offset in 0..len {
        let identity = identities[(device_start + offset) % len];
        let progress_generation = physical_completion_device_progress_generation(identity);
        if physical_completion_terminal_event_for_device(identity).is_some()
            || physical_completion_device_reset_pending(identity)
        {
            if let Err(error) = service_physical_completion_reset_for_device(identity)
                && !matches!(error, AxError::ResourceBusy)
            {
                error!(
                    "io_uring physical reset/terminal custody for device {identity:#x}: {error:?}"
                );
            }
            continue;
        }
        if !physical_completion_device_ready_for(identity)
            || physical_completion_custody_count_for_device(identity) == 0
        {
            clear_physical_completion_device_progress_if_unchanged(identity, progress_generation);
            continue;
        }
        if physical_completion_has_quarantined_route_for_device(identity) {
            mark_physical_completion_device_reset_pending(identity);
            let _ = service_physical_completion_reset_for_device(identity);
            continue;
        }
        let pass = run_physical_completion_pass_for_device(
            identity,
            &mut output,
            IO_URING_PHYSICAL_MAX_QD,
            |output| shared_block_completion_waiter_for_device(identity, output, false),
        );
        match pass {
            Ok(pass) => {
                continuation |= pass.continuation;
                match retry_pending_physical_publications_for_device(identity) {
                    Ok(PhysicalPublicationRetryDisposition::Republished) => continuation = true,
                    Ok(PhysicalPublicationRetryDisposition::PendingOnly) => {
                        delayed_publication_retry = true;
                    }
                    Ok(
                        PhysicalPublicationRetryDisposition::Quiescent
                        | PhysicalPublicationRetryDisposition::WaitingForCompletion,
                    ) => {}
                    Err(error) => {
                        mark_physical_completion_device_reset_pending(identity);
                        let _ = service_physical_completion_reset_for_device(identity);
                        error!(
                            "io_uring physical publication retry failed for device {identity:#x}: \
                             {error:?}"
                        );
                        continue;
                    }
                }
                if !pass.continuation {
                    clear_physical_completion_device_progress_if_unchanged(
                        identity,
                        progress_generation,
                    );
                }
            }
            Err(error) => {
                // Device-local failures fence only this device. A vda
                // quarantine must not close vdb's admission generation.
                mark_physical_completion_device_reset_pending(identity);
                let _ = service_physical_completion_reset_for_device(identity);
                error!(
                    "io_uring physical completion worker stopped for device {identity:#x}: \
                     {error:?}"
                );
            }
        }
    }
    if continuation {
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        crate::deferred_work::wake_physical_completion_worker();
    } else if delayed_publication_retry {
        // PendingPublication owns no lower route, so once a queue-full retry
        // observes no published sibling there is no device IRQ left to wake
        // this owner. Use the same bounded task timer as finalization retry;
        // an immediate self-wake would spin while another queue owner retains
        // the descriptor credit.
        clear_physical_completion_work_pending_with_recheck();
        if let Err(error) = axtask::sleep(Duration::from_millis(1)) {
            // A persistent scheduler failure cannot safely abandon an issued
            // request with no device-visible effect. Fence every pending-only
            // device into the existing typed reset path instead.
            for identity in identities.iter().copied().take(len) {
                if physical_completion_pending_owner_count_for_device(identity) != 0
                    && physical_completion_route_count_for_device(identity) == 0
                {
                    mark_physical_completion_device_reset_pending(identity);
                }
            }
            error!("io_uring pending physical publication retry sleep failed: {error:?}");
        } else {
            PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
            crate::deferred_work::wake_physical_completion_worker();
        }
    } else {
        // A non-blocking pass with no lower record (or a fully drained batch)
        // hands ownership back to the PollSet. The per-device notifier owns
        // the next liveness edge; the lock-coupled recheck preserves a sibling
        // device's wake and never clears its work.
        clear_physical_completion_work_pending_with_recheck();
    }
    PHYSICAL_COMPLETION_WORKER_ACTIVE.store(false, Ordering::Release);
}

struct RequestSlotCharge(usize);

impl RequestSlotCharge {
    fn try_new(slots: usize) -> AxResult<Self> {
        IO_URING_REQUEST_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_REQUEST_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENOSPC))?;
        Ok(Self(slots))
    }
}

impl Drop for RequestSlotCharge {
    fn drop(&mut self) {
        IO_URING_REQUEST_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

struct FixedFileSlotCharge(usize);

impl FixedFileSlotCharge {
    fn try_new(slots: usize) -> AxResult<Self> {
        if slots > crate::task::AX_FILE_LIMIT {
            return Err(AxError::from(LinuxError::EMFILE));
        }
        IO_URING_FIXED_FILE_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_FIXED_FILE_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENFILE))?;
        Ok(Self(slots))
    }
}

impl Drop for FixedFileSlotCharge {
    fn drop(&mut self) {
        IO_URING_FIXED_FILE_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

struct RegisteredBufferSlotCharge(usize);

impl RegisteredBufferSlotCharge {
    fn try_new(slots: usize) -> AxResult<Self> {
        IO_URING_REGISTERED_BUFFER_SLOTS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(slots)
                    .filter(|next| *next <= IO_URING_GLOBAL_REGISTERED_BUFFER_SLOTS)
            })
            .map_err(|_| AxError::from(LinuxError::ENFILE))?;
        Ok(Self(slots))
    }
}

impl Drop for RegisteredBufferSlotCharge {
    fn drop(&mut self) {
        IO_URING_REGISTERED_BUFFER_SLOTS.fetch_sub(self.0, Ordering::AcqRel);
    }
}

struct RegisteredBufferPinBudget {
    pages: AtomicUsize,
    bytes: AtomicUsize,
}

impl RegisteredBufferPinBudget {
    const fn new() -> Self {
        Self {
            pages: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        }
    }

    fn try_reserve(&self, pages: usize, bytes: usize, page_limit: usize) -> bool {
        if self
            .pages
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(pages).filter(|next| *next <= page_limit)
            })
            .is_err()
        {
            return false;
        }
        let byte_limit = page_limit.saturating_mul(PAGE_BYTES);
        if self
            .bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|next| *next <= byte_limit)
            })
            .is_err()
        {
            self.pages.fetch_sub(pages, Ordering::AcqRel);
            return false;
        }
        true
    }

    fn release(&self, pages: usize, bytes: usize) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.pages.fetch_sub(pages, Ordering::AcqRel);
    }

    fn try_charge(self: &Arc<Self>, pages: usize) -> AxResult<RegisteredBufferPinCharge> {
        let bytes = pages.checked_mul(PAGE_BYTES).ok_or(AxError::NoMemory)?;
        if !self.try_reserve(pages, bytes, IO_URING_RING_REGISTERED_BUFFER_PAGES) {
            return Err(AxError::ResourceBusy);
        }
        if !try_reserve_global_registered_buffer_pin(pages, bytes) {
            self.release(pages, bytes);
            return Err(AxError::ResourceBusy);
        }
        Ok(RegisteredBufferPinCharge {
            budget: Arc::clone(self),
            pages,
            bytes,
        })
    }
}

struct RegisteredBufferPinCharge {
    budget: Arc<RegisteredBufferPinBudget>,
    pages: usize,
    bytes: usize,
}

impl Drop for RegisteredBufferPinCharge {
    fn drop(&mut self) {
        self.budget.release(self.pages, self.bytes);
        IO_URING_REGISTERED_BUFFER_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        IO_URING_REGISTERED_BUFFER_PAGES.fetch_sub(self.pages, Ordering::AcqRel);
    }
}

struct PinBeforeCharge<P, C> {
    pin: Option<P>,
    _charge: C,
}

impl<P, C> PinBeforeCharge<P, C> {
    fn new(pin: P, charge: C) -> Self {
        Self {
            pin: Some(pin),
            _charge: charge,
        }
    }
}

impl<P, C> Drop for PinBeforeCharge<P, C> {
    fn drop(&mut self) {
        drop(self.pin.take());
    }
}

fn try_reserve_global_registered_buffer_pin(pages: usize, bytes: usize) -> bool {
    if IO_URING_REGISTERED_BUFFER_PAGES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(pages)
                .filter(|next| *next <= IO_URING_GLOBAL_REGISTERED_BUFFER_PAGES)
        })
        .is_err()
    {
        return false;
    }
    let byte_limit = IO_URING_GLOBAL_REGISTERED_BUFFER_PAGES.saturating_mul(PAGE_BYTES);
    if IO_URING_REGISTERED_BUFFER_BYTES
        .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= byte_limit)
        })
        .is_err()
    {
        IO_URING_REGISTERED_BUFFER_PAGES.fetch_sub(pages, Ordering::AcqRel);
        return false;
    }
    true
}

struct RegisteredFiles {
    table: RegisteredFileTable<FileDescription>,
    _charge: FixedFileSlotCharge,
}

/// Registered-buffer owner. The pin is retained until the table owner and
/// every request lease have retired. Actual file I/O still uses the existing
/// direct-or-copy fallback; this pin establishes lifetime and mapping fences,
/// not a claim of hardware DMA support.
struct RegisteredBuffer {
    address: usize,
    length: usize,
    pin_start: usize,
    pin_len: usize,
    segment_ends: Vec<usize>,
    pin_segments_disjoint: bool,
    capability: UserMemoryCapability,
    // Release the lower VM/frame/page-cache pin before making this ring's
    // admission charge reusable. A large unpin can yield enough observable
    // time for another registration to consume the io_uring budget while the
    // shared lower pin budget is still occupied.
    _pin_owner: PinBeforeCharge<PinnedUserSegmentsMut, RegisteredBufferPinCharge>,
}

// The owner retains the explicit address-space capability alongside the
// kernel-side pin/fence state and opaque userspace address. It never relies on
// current-task state; the address-space pin registry serializes mapping
// changes for the selected capability.
unsafe impl Send for RegisteredBuffer {}
unsafe impl Sync for RegisteredBuffer {}

struct RegisteredBuffers {
    table: RegisteredBufferTable<RegisteredBuffer>,
    _charge: RegisteredBufferSlotCharge,
}

struct IoUringFinalizer {
    ring: Option<Arc<IoUring>>,
}

impl Drop for IoUringFinalizer {
    fn drop(&mut self) {
        let Some(ring) = self.ring.take() else {
            return;
        };
        ring.request_final_close();
    }
}

pub(crate) enum IoUringFileLease {
    Descriptor(Arc<FileDescription>),
    Registered {
        ring: Weak<IoUring>,
        lease: Option<RegisteredFileLease<FileDescription>>,
    },
}

pub(crate) struct IoUringBufferLease {
    ring: Arc<IoUring>,
    lease: Option<RegisteredBufferLease<RegisteredBuffer>>,
}

/// The only operations which may consume a prepared physical admission.
/// Generic streams and pseudo-files never construct this token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparedPhysicalIoOperation {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhysicalIoCompletionDisposition {
    Settled,
    Retained,
    Unknown,
}

fn physical_io_completion_result(result: axfs_ng_vfs::VfsResult<usize>) -> i32 {
    match result {
        Ok(bytes) => i32::try_from(bytes).unwrap_or(-LinuxError::EOVERFLOW.code()),
        Err(error) => -LinuxError::from(error).code(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PhysicalIoCompletionPass {
    pub(crate) drained: usize,
    pub(crate) continuation: bool,
}

/// Owned physical plan captured while the registered buffer lease is held.
/// The lease itself remains part of [`PreparedPhysicalIoAdmission`], so these
/// descriptors cannot outlive the pin or be paired with a different buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedPhysicalIoPlan {
    operation: PreparedPhysicalIoOperation,
    offset: u64,
    address: usize,
    requested_len: usize,
    allowed_len: usize,
    physical: [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
    physical_len: usize,
    /// Opaque SharedBlockDevice identity selected from the mounted
    /// filesystem binding. Zero is retained for unit-test-only reservations;
    /// production admission must bind a non-zero device before publication.
    device_identity: usize,
    device_generation: u64,
}

impl PreparedPhysicalIoPlan {
    const fn new(
        operation: PreparedPhysicalIoOperation,
        offset: u64,
        address: usize,
        requested_len: usize,
        allowed_len: usize,
        physical: [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
        physical_len: usize,
    ) -> Self {
        Self {
            operation,
            offset,
            address,
            requested_len,
            allowed_len,
            physical,
            physical_len,
            device_identity: 0,
            device_generation: 0,
        }
    }

    pub(crate) fn bind_device(&mut self, identity: usize, generation: u64) -> AxResult<()> {
        if identity == 0 || self.device_identity != 0 {
            return Err(AxError::BadState);
        }
        self.device_identity = identity;
        self.device_generation = generation;
        Ok(())
    }

    pub(crate) fn physical_segments(&self) -> AxResult<&[PhysicalIoSegment]> {
        if self.physical_len == 0 || self.physical_len > self.physical.len() {
            return Err(AxError::BadState);
        }
        Ok(&self.physical[..self.physical_len])
    }

    pub(crate) const fn operation(&self) -> PreparedPhysicalIoOperation {
        self.operation
    }

    pub(crate) const fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) const fn address(&self) -> usize {
        self.address
    }

    pub(crate) const fn requested_len(&self) -> usize {
        self.requested_len
    }

    pub(crate) const fn allowed_len(&self) -> usize {
        self.allowed_len
    }

    pub(crate) const fn device_identity(&self) -> usize {
        self.device_identity
    }

    pub(crate) const fn device_generation(&self) -> u64 {
        self.device_generation
    }
}

/// Checks that two ordered physical SG lists describe the same byte stream.
///
/// The registered-buffer side and the lower filesystem plan may split a
/// physical range at different boundaries (for example, an extent boundary
/// may split one registered-buffer segment).  Compare the ranges with two
/// cursors instead of requiring descriptor-count or descriptor-boundary
/// equality.  Every descriptor is still checked for a non-zero length and a
/// representable physical end, and each compared chunk must begin at the
/// same physical address.  Thus a gap, overlap, reorder, truncation, or
/// overflow cannot be hidden by re-segmentation.
fn physical_byte_streams_equivalent<Upper, Lower, UpperFields, LowerFields>(
    upper: &[Upper],
    lower: &[Lower],
    mut upper_fields: UpperFields,
    mut lower_fields: LowerFields,
) -> AxResult<()>
where
    UpperFields: FnMut(&Upper) -> (usize, usize),
    LowerFields: FnMut(&Lower) -> (usize, usize),
{
    fn checked_stream_len<Segment, Fields>(
        segments: &[Segment],
        fields: &mut Fields,
    ) -> AxResult<usize>
    where
        Fields: FnMut(&Segment) -> (usize, usize),
    {
        if segments.is_empty() {
            return Err(AxError::BadState);
        }
        segments.iter().try_fold(0usize, |total, segment| {
            let (paddr, len) = fields(segment);
            if len == 0 {
                return Err(AxError::BadState);
            }
            paddr.checked_add(len).ok_or(AxError::BadState)?;
            total.checked_add(len).ok_or(AxError::BadState)
        })
    }

    let upper_len = checked_stream_len(upper, &mut upper_fields)?;
    let lower_len = checked_stream_len(lower, &mut lower_fields)?;
    if upper_len != lower_len {
        return Err(AxError::BadState);
    }

    let mut upper_index = 0usize;
    let mut lower_index = 0usize;
    let mut upper_offset = 0usize;
    let mut lower_offset = 0usize;
    while upper_index < upper.len() && lower_index < lower.len() {
        let (upper_segment_paddr, upper_segment_len) = upper_fields(&upper[upper_index]);
        let (lower_segment_paddr, lower_segment_len) = lower_fields(&lower[lower_index]);
        let upper_paddr = upper_segment_paddr
            .checked_add(upper_offset)
            .ok_or(AxError::BadState)?;
        let lower_paddr = lower_segment_paddr
            .checked_add(lower_offset)
            .ok_or(AxError::BadState)?;
        if upper_paddr != lower_paddr {
            return Err(AxError::BadState);
        }

        let upper_remaining = upper_segment_len
            .checked_sub(upper_offset)
            .ok_or(AxError::BadState)?;
        let lower_remaining = lower_segment_len
            .checked_sub(lower_offset)
            .ok_or(AxError::BadState)?;
        let matched = upper_remaining.min(lower_remaining);
        if matched == 0 {
            return Err(AxError::BadState);
        }
        upper_offset = upper_offset.checked_add(matched).ok_or(AxError::BadState)?;
        lower_offset = lower_offset.checked_add(matched).ok_or(AxError::BadState)?;
        if upper_offset == upper_segment_len {
            upper_index = upper_index.checked_add(1).ok_or(AxError::BadState)?;
            upper_offset = 0;
        }
        if lower_offset == lower_segment_len {
            lower_index = lower_index.checked_add(1).ok_or(AxError::BadState)?;
            lower_offset = 0;
        }
    }

    if upper_index == upper.len()
        && lower_index == lower.len()
        && upper_offset == 0
        && lower_offset == 0
    {
        Ok(())
    } else {
        Err(AxError::BadState)
    }
}

/// A worker-safe physical operation after submitter-side policy admission.
///
/// Construction retains both the exact file lease and the exact registered
/// buffer lease.  The worker API accepts this token only; it cannot receive a
/// raw borrowed SG tuple, a numeric fd, or a task-derived credential view.
pub(crate) struct PreparedPhysicalIoAdmission {
    file: Option<IoUringFileLease>,
    buffer: Option<IoUringBufferLease>,
    context: Option<IoOperationContext>,
    plan: PreparedPhysicalIoPlan,
    /// Write-side policy reservations stay live through physical retirement.
    /// In particular, a memfd seal or set-id cleanup cannot race the device
    /// while the effect still owns the destination range.
    memfd: Option<MemfdMutationGuard>,
    privilege: Option<ContentWritePrivilegeGuard>,
    /// Bound before lower publication so a defensive Drop can transfer the
    /// exact published owner into the ring's typed custody table.
    worker_slot: Option<usize>,
    /// The vendor-owned effect contains the inode, range-cache lease, and
    /// staged cache transaction.  It must travel with the exact file/buffer
    /// leases until physical retirement; a worker must never reconstruct it
    /// from an fd, offset, or raw SG tuple.
    effect: Option<PreparedPhysicalIoEffect>,
}

impl PreparedPhysicalIoAdmission {
    pub(crate) fn new(
        file: IoUringFileLease,
        buffer: IoUringBufferLease,
        context: IoOperationContext,
        plan: PreparedPhysicalIoPlan,
        memfd: Option<MemfdMutationGuard>,
        privilege: Option<ContentWritePrivilegeGuard>,
        effect: PreparedPhysicalIoEffect,
    ) -> AxResult<Self> {
        if plan.device_identity == 0 {
            return Err(AxError::BadState);
        }
        if plan.allowed_len == 0 || plan.allowed_len > plan.requested_len {
            return Err(AxError::BadState);
        }
        if plan.allowed_len != plan.requested_len {
            return Err(AxError::BadState);
        }
        let (address, length) = buffer.range()?;
        let address = usize::try_from(address).map_err(|_| AxError::BadAddress)?;
        let length = usize::try_from(length).map_err(|_| AxError::BadAddress)?;
        if address != plan.address || length != plan.requested_len {
            return Err(AxError::BadAddress);
        }
        let description = file.description()?;
        context.validate_for(description)?;
        let physical = plan.physical_segments()?;
        buffer.physical_segments_for_plan(physical, plan.allowed_len)?;
        let effect_plan = effect.plan();
        let effect_operation = match effect_plan.operation() {
            PhysicalIoOperation::Read => PreparedPhysicalIoOperation::Read,
            PhysicalIoOperation::Write => PreparedPhysicalIoOperation::Write,
        };
        let effect_offset_matches = effect_plan
            .extent(0)
            .is_some_and(|extent| extent.file_offset() == plan.offset);
        if effect_operation != plan.operation
            || !effect_offset_matches
            || effect_plan.io_bytes() != plan.allowed_len
            || physical_byte_streams_equivalent(
                physical,
                effect_plan.segments(),
                |segment| (segment.paddr, segment.len),
                |segment| (segment.paddr, segment.len),
            )
            .is_err()
        {
            return Err(AxError::BadState);
        }
        if plan.operation == PreparedPhysicalIoOperation::Write
            && (memfd.is_none() || privilege.is_none())
        {
            return Err(AxError::BadState);
        }
        Ok(Self {
            file: Some(file),
            buffer: Some(buffer),
            context: Some(context),
            plan,
            memfd,
            privilege,
            worker_slot: None,
            effect: Some(effect),
        })
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        IoUringFileLease,
        IoUringBufferLease,
        IoOperationContext,
        PreparedPhysicalIoPlan,
        Option<MemfdMutationGuard>,
        Option<ContentWritePrivilegeGuard>,
        PreparedPhysicalIoEffect,
    ) {
        let mut this = self;
        (
            this.file.take().expect("admission file lease missing"),
            this.buffer.take().expect("admission buffer lease missing"),
            this.context.take().expect("admission context missing"),
            this.plan,
            this.memfd.take(),
            this.privilege.take(),
            this.effect.take().expect("admission effect missing"),
        )
    }

    pub(crate) fn operation(&self) -> PreparedPhysicalIoOperation {
        self.plan.operation
    }

    pub(crate) fn plan(&self) -> PreparedPhysicalIoPlan {
        self.plan
    }

    pub(crate) fn effect(&self) -> &PreparedPhysicalIoEffect {
        self.effect.as_ref().expect("admission effect missing")
    }

    pub(crate) fn effect_mut(&mut self) -> &mut PreparedPhysicalIoEffect {
        self.effect.as_mut().expect("admission effect missing")
    }

    /// Publishes the already prepared vendor effect. This is intentionally
    /// the only unsafe operation exposed by the admission token; callers must
    /// reserve a worker slot immediately before invoking it and must not
    /// fallback after a Published/Terminal outcome. io_uring uses the lower
    /// kernel-destination route so the device-global broker can drain this
    /// effect without stealing synchronous exact-route waiters.
    pub(crate) unsafe fn publish(&mut self) -> AxResult<PhysicalIoPublishOutcome> {
        unsafe { self.effect_mut().publish_kernel() }
    }

    pub(crate) fn into_effect(self) -> PreparedPhysicalIoEffect {
        let mut this = self;
        this.effect.take().expect("admission effect missing")
    }

    fn is_published_unretired(&self) -> bool {
        let Some(effect) = self.effect.as_ref() else {
            // `into_parts`/`into_effect` take ownership of the effect before
            // this value's destructor runs.  An empty option is therefore a
            // moved-out, drop-safe admission rather than a quarantined one.
            return false;
        };
        // `publish()` may return an error after the lower hook has observed
        // an unknown publication; the wrapper marks that condition sticky
        // even if its inner state is still `Prepared`.  Conversely,
        // `is_published()` remains true after the typed settlement proof
        // transitions the effect to `Finalized`.  Ordinary destructors are
        // therefore allowed only for a genuinely never-published Prepared
        // effect or an exact Finalized effect. Every other state, including
        // a publish error/quarantine, keeps the composite owner fail-stop so
        // the registered-buffer pin cannot outlive DMA.
        match effect.state() {
            PhysicalIoEffectState::Finalized => false,
            PhysicalIoEffectState::Prepared => effect.is_published() || effect.is_quarantined(),
            _ => true,
        }
    }

    pub(crate) fn physical_extent_count(&self) -> AxResult<usize> {
        let count = self.effect().plan().extent_count();
        if count == 0 || count > IO_URING_PHYSICAL_MAX_EXTENTS {
            return Err(AxError::BadState);
        }
        Ok(count)
    }

    fn bind_worker_slot(&mut self, slot: usize) -> AxResult<()> {
        if self.worker_slot.replace(slot).is_some() {
            return Err(AxError::BadState);
        }
        Ok(())
    }

    /// Releases the upper ring/file/buffer custody after a lower reset has
    /// proved that no device access remains.  The lower effect itself has no
    /// logical completion to finalize after reset; its fail-stop destructor
    /// retains any lower-owned cache/inode custody while the registered-buffer
    /// and policy leases in this admission are released.
    fn retire_after_reset(mut self, proof: PhysicalIoResetProof) {
        if let Some(mut effect) = self.effect.take() {
            // The lower reset/retired result is the only evidence that the
            // device can no longer touch this effect.  Transition the
            // high-level owner first so its range lease, staged cache
            // transaction, inode, and location all release normally.
            effect.abort_after_reset(proof);
            drop(effect);
        }
    }
}

impl Drop for PreparedPhysicalIoAdmission {
    fn drop(&mut self) {
        if !self.is_published_unretired() {
            return;
        }
        // A published effect's vendor Drop deliberately fail-stops its
        // range/cache owner, but the registered-buffer and policy leases are
        // siblings here. Re-home every owner together in the ring's bounded
        // custody table so DMA can never outlive only the pin or only the
        // cache transaction. This path is defensive: normal publication
        // always moves the admission into `PhysicalIoWork` before the guard
        // is released.
        record_io_uring_physical_quarantine();
        let Some(buffer) = self.buffer.as_ref() else {
            panic!("published io_uring admission lost its buffer owner");
        };
        let Some(slot) = self.worker_slot else {
            panic!("published io_uring admission lost its worker slot");
        };
        let ring = Arc::clone(&buffer.ring);
        let admission = PreparedPhysicalIoAdmission {
            file: self.file.take(),
            buffer: self.buffer.take(),
            context: self.context.take(),
            plan: self.plan,
            memfd: self.memfd.take(),
            privilege: self.privilege.take(),
            worker_slot: Some(slot),
            effect: self.effect.take(),
        };
        ring.park_physical_worker_custody(PhysicalIoWork {
            ring: Arc::clone(&ring),
            slot,
            issued: None,
            admission: Some(admission),
            pending_publication: false,
            #[cfg(test)]
            test_handle: None,
        });
    }
}

impl Drop for IoUringBufferLease {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        self.ring.release_registered_buffer(lease);
    }
}

fn clip_registered_physical_segments(
    segments: &[UserIoPinSegment],
    offset: usize,
    length: usize,
    output: &mut [PhysicalIoSegment; IO_URING_PHYSICAL_MAX_SEGMENTS],
) -> AxResult<usize> {
    if length == 0 || length > IO_URING_PHYSICAL_MAX_BYTES {
        return Err(AxError::InvalidInput);
    }
    let end = offset.checked_add(length).ok_or(AxError::BadAddress)?;
    let mut logical = 0usize;
    let mut count = 0usize;
    for segment in segments.iter().copied() {
        let segment_end = logical
            .checked_add(segment.len)
            .ok_or(AxError::BadAddress)?;
        let clip_start = offset.max(logical);
        let clip_end = end.min(segment_end);
        if clip_start < clip_end {
            let paddr = segment
                .paddr
                .checked_add(clip_start.checked_sub(logical).ok_or(AxError::BadAddress)?)
                .ok_or(AxError::BadAddress)?;
            let clipped_len = clip_end - clip_start;
            if let Some(previous) = count.checked_sub(1).and_then(|index| output.get_mut(index))
                && previous.paddr.checked_add(previous.len) == Some(paddr)
            {
                previous.len = previous
                    .len
                    .checked_add(clipped_len)
                    .ok_or(AxError::BadAddress)?;
            } else {
                if count == output.len() {
                    return Err(AxError::InvalidInput);
                }
                output[count] = PhysicalIoSegment::new(paddr, clipped_len);
                count += 1;
            }
        }
        logical = segment_end;
        if logical >= end {
            break;
        }
    }
    if logical < end || count == 0 {
        return Err(AxError::BadAddress);
    }
    Ok(count)
}

fn locate_physical_segment(segment_ends: &[usize], offset: usize) -> AxResult<(usize, usize)> {
    let segment_index = segment_ends.partition_point(|segment_end| *segment_end <= offset);
    let preceding = segment_index
        .checked_sub(1)
        .map_or(0, |index| segment_ends[index]);
    let segment_offset = offset.checked_sub(preceding).ok_or(AxError::BadAddress)?;
    Ok((segment_index, segment_offset))
}

impl IoUringBufferLease {
    /// Derives the physical descriptor array from this exact registered
    /// buffer lease. Callers can only provide operation metadata; the SG
    /// addresses themselves are never accepted from an unowned tuple.
    pub(crate) fn prepared_physical_plan(
        &self,
        operation: PreparedPhysicalIoOperation,
        offset: u64,
        address: usize,
        requested_len: usize,
        allowed_len: usize,
    ) -> AxResult<PreparedPhysicalIoPlan> {
        let (lease_address, lease_length) = self.range()?;
        if usize::try_from(lease_address).map_err(|_| AxError::BadAddress)? != address
            || usize::try_from(lease_length).map_err(|_| AxError::BadAddress)? != requested_len
        {
            return Err(AxError::BadAddress);
        }
        if allowed_len == 0 || allowed_len > requested_len {
            return Err(AxError::BadAddress);
        }
        let (segments, offset_in_segments, fixed_len, _) = self.physical_range()?;
        if allowed_len > fixed_len {
            return Err(AxError::BadAddress);
        }
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_PHYSICAL_MAX_SEGMENTS];
        let physical_len = clip_registered_physical_segments(
            segments,
            offset_in_segments,
            allowed_len,
            &mut physical,
        )?;
        Ok(PreparedPhysicalIoPlan::new(
            operation,
            offset,
            address,
            requested_len,
            allowed_len,
            physical,
            physical_len,
        ))
    }

    /// Returns the address-space capability captured when this buffer was
    /// registered. Fixed I/O must never substitute the caller's current
    /// capability: the ring may be submitted through a shared descriptor by
    /// another task or address space.
    pub(crate) fn capability(&self) -> AxResult<UserMemoryCapability> {
        self.lease
            .as_ref()
            .map(|lease| lease.owner().capability.clone())
            .ok_or(AxError::BadState)
    }

    /// Returns the exact subrange validated by the table lookup. Fixed I/O
    /// must derive its address and length from this lease rather than reuse
    /// the caller's raw SQE geometry after admission.
    pub(crate) fn range(&self) -> AxResult<(u64, u32)> {
        self.lease
            .as_ref()
            .map(|lease| {
                let range = lease.range();
                (range.address(), range.length())
            })
            .ok_or(AxError::BadState)
    }

    /// Returns the selected fixed-buffer bytes from the physical SG captured
    /// at registration. The lease must remain alive while the returned view is
    /// consumed; it is the owner of the underlying pin.
    pub(crate) fn physical_range(&self) -> AxResult<(&[UserIoPinSegment], usize, usize, bool)> {
        let lease = self.lease.as_ref().ok_or(AxError::BadState)?;
        let range = lease.range();
        let owner = lease.owner();
        let address = usize::try_from(range.address()).map_err(|_| AxError::BadAddress)?;
        let length = usize::try_from(range.length()).map_err(|_| AxError::BadAddress)?;
        let offset = address
            .checked_sub(owner.pin_start)
            .ok_or(AxError::BadAddress)?;
        let pin = owner._pin_owner.pin.as_ref().ok_or(AxError::BadState)?;
        let end = offset.checked_add(length).ok_or(AxError::BadAddress)?;
        if end > owner.pin_len {
            return Err(AxError::BadAddress);
        }
        let (segment_index, segment_offset) = locate_physical_segment(&owner.segment_ends, offset)?;
        let segments = pin
            .segments()
            .get(segment_index..)
            .ok_or(AxError::BadAddress)?;
        Ok((
            segments,
            segment_offset,
            length,
            owner.pin_segments_disjoint,
        ))
    }

    /// Validates that an already clipped physical plan remains covered by
    /// this exact registered-buffer lease. The returned lease is retained by
    /// the prepared token; callers never retain this borrowed tuple.
    pub(crate) fn physical_segments_for_plan(
        &self,
        expected: &[PhysicalIoSegment],
        length: usize,
    ) -> AxResult<()> {
        let (segments, offset, fixed_len, _) = self.physical_range()?;
        if length == 0 || length > fixed_len {
            return Err(AxError::BadAddress);
        }
        let _ = self.physical_provenance()?;
        let mut actual = [PhysicalIoSegment::new(0, 0); IO_URING_PHYSICAL_MAX_SEGMENTS];
        let actual_len = clip_registered_physical_segments(segments, offset, length, &mut actual)?;
        if expected != &actual[..actual_len] {
            return Err(AxError::BadAddress);
        }
        Ok(())
    }

    /// Returns the provenance captured by the registered-buffer pin.  The
    /// direct physical-DMA path only accepts private anonymous pages; callers
    /// must continue to hold this lease while the lower filesystem call runs.
    pub(crate) fn physical_provenance(&self) -> AxResult<UserIoPinProvenance> {
        let lease = self.lease.as_ref().ok_or(AxError::BadState)?;
        let pin = lease
            .owner()
            ._pin_owner
            .pin
            .as_ref()
            .ok_or(AxError::BadState)?;
        Ok(pin.provenance())
    }
}

impl IoUringFileLease {
    pub(crate) fn description(&self) -> AxResult<&Arc<FileDescription>> {
        match self {
            Self::Descriptor(description) => Ok(description),
            Self::Registered { lease, .. } => lease
                .as_ref()
                .map(RegisteredFileLease::owner)
                .ok_or(AxError::BadState),
        }
    }
}

impl Drop for IoUringFileLease {
    fn drop(&mut self) {
        let Self::Registered { ring, lease } = self else {
            return;
        };
        let Some(lease) = lease.take() else {
            return;
        };
        if let Some(ring) = ring.upgrade() {
            ring.release_registered_file(lease);
        }
    }
}

/// A published physical effect retained by the single task-context completion
/// owner.  The request token is kept by value until the owner has a typed
/// settlement proof; merely dropping this item must never synthesize a CQE.
pub(crate) struct PhysicalIoWork {
    ring: Arc<IoUring>,
    slot: usize,
    issued: Option<IssuedRequest>,
    admission: Option<PreparedPhysicalIoAdmission>,
    /// The logical slot is charged before publication. A pending owner has
    /// no lower child route yet, but retains the same request/admission lease
    /// until a bounded retry publishes it or teardown cancels it.
    pending_publication: bool,
    #[cfg(test)]
    test_handle: Option<u64>,
}

/// A terminal publication split that moves the complete physical owner out of
/// `PhysicalIoWork` before exposing a CQE. The payload keeps the issued proof
/// and ring alive while it first drops the DMA/cache admission, then drops the
/// now-empty work to release the physical slot, and only then publishes.
struct PhysicalIoTerminalPayload {
    ring: Arc<IoUring>,
    work: Option<PhysicalIoWork>,
    issued: Option<IssuedRequest>,
    admission: Option<PreparedPhysicalIoAdmission>,
}

impl PhysicalIoTerminalPayload {
    fn from_valid_work(mut work: PhysicalIoWork) -> Self {
        debug_assert!(work.issued().is_some() && work.admission().is_some());
        let ring = Arc::clone(&work.ring);
        let issued = work.take_issued();
        let admission = work.take_admission();
        Self {
            ring,
            work: Some(work),
            issued,
            admission,
        }
    }

    fn retire(mut self) -> AxResult<(Arc<IoUring>, IssuedRequest)> {
        // This is deliberately before releasing the worker slot or publishing
        // the IssuedRequest: effect/range, file, registered-buffer, and
        // write-policy owners must all be gone before the user can observe
        // the CQE.
        drop(self.admission.take());
        // The work item is now empty, so its Drop only releases the exact
        // physical worker slot/QD charge. IssuedRequest remains in this typed
        // payload until complete_issued consumes it.
        drop(self.work.take());
        let issued = self.issued.take().ok_or(AxError::BadState)?;
        Ok((self.ring, issued))
    }
}

/// Keeps the reset terminal boundary explicit: all upper owners are retired,
/// then the physical work slot is released, and only then may the EIO CQE be
/// made visible.  The small state helper is shared by the production path and
/// the pure ordering test below so a future refactor cannot move publication
/// ahead of retirement accidentally.
fn run_physical_reset_terminal_order(
    retire_owners: impl FnOnce(),
    release_work: impl FnOnce(),
    publish_cqe: impl FnOnce(),
) {
    retire_owners();
    release_work();
    publish_cqe();
}

impl PhysicalIoWork {
    pub(crate) fn issued(&self) -> Option<&IssuedRequest> {
        self.issued.as_ref()
    }

    pub(crate) fn admission(&self) -> Option<&PreparedPhysicalIoAdmission> {
        self.admission.as_ref()
    }

    pub(crate) fn admission_mut(&mut self) -> Option<&mut PreparedPhysicalIoAdmission> {
        self.admission.as_mut()
    }

    pub(crate) fn take_issued(&mut self) -> Option<IssuedRequest> {
        self.issued.take()
    }

    pub(crate) fn take_admission(&mut self) -> Option<PreparedPhysicalIoAdmission> {
        self.admission.take()
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    fn device_identity(&self) -> usize {
        self.admission
            .as_ref()
            .map_or(0, |admission| admission.plan().device_identity())
    }

    fn request_id(&self) -> Option<RequestId> {
        self.issued.as_ref().map(IssuedRequest::id)
    }

    fn device_generation(&self) -> u64 {
        self.admission
            .as_ref()
            .map_or(0, |admission| admission.plan().device_generation())
    }

    fn is_pending_publication(&self) -> bool {
        self.pending_publication
    }

    fn matches_reset_identity(&self, request: RequestId, slot: usize) -> bool {
        self.slot == slot && self.request_id().is_none_or(|current| current == request)
    }

    fn matches_reset_identity_with_generation(
        &self,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> bool {
        self.matches_reset_identity(request, slot) && self.device_generation() == generation
    }

    pub(crate) fn publication(&self) -> Option<PhysicalIoPublication> {
        self.admission.as_ref()?.effect().publication()
    }

    fn owns_handle(&self, handle: u64) -> bool {
        #[cfg(test)]
        if self.test_handle == Some(handle) {
            return true;
        }
        let Some(publication) = self.publication() else {
            return false;
        };
        (0..publication.count()).any(|index| publication.handle(index) == Some(handle))
    }

    fn needs_finalization_retry(&self) -> bool {
        self.admission.as_ref().is_some_and(|admission| {
            matches!(
                admission.effect().state(),
                PhysicalIoEffectState::Completed | PhysicalIoEffectState::SettledFailure
            )
        })
    }
}

impl Drop for PhysicalIoWork {
    fn drop(&mut self) {
        if self
            .admission
            .as_ref()
            .is_some_and(PreparedPhysicalIoAdmission::is_published_unretired)
        {
            // Keep all DMA owners, including the registered-buffer pin and
            // issued proof, in fail-stop custody. Re-home the complete work
            // owner instead of dropping/leaking individual fields; releasing
            // the fixed slot here would let final close proceed while the
            // device still owns the physical ranges.
            record_io_uring_physical_quarantine();
            let work = PhysicalIoWork {
                ring: Arc::clone(&self.ring),
                slot: self.slot,
                issued: self.issued.take(),
                admission: self.admission.take(),
                pending_publication: self.pending_publication,
                #[cfg(test)]
                test_handle: None,
            };
            self.ring.park_physical_worker_custody(work);
            return;
        }
        if self.pending_publication
            && let (Some(request), Some(admission)) = (self.request_id(), self.admission.as_ref())
        {
            clear_physical_completion_pending_owner(
                &self.ring,
                request,
                self.slot,
                admission.plan().device_identity(),
                admission.plan().device_generation(),
            );
        }
        // A work item is only dropped after the completion owner has consumed
        // the exact device settlement (or moved the item into quarantine).
        // Clearing the bounded slot here makes the lease/drop ordering
        // explicit and wakes a parked final close without a polling retry.
        self.ring.release_physical_worker_slot(self.slot);
    }
}

/// Reversible reservation for one fixed-capacity physical worker slot.  The
/// reservation must be acquired before descriptor publication; commit only
/// installs already-owned effect/request tokens and cannot allocate.
pub(crate) struct PhysicalIoWorkerReservation<'a> {
    ring: &'a IoUring,
    owner: Arc<IoUring>,
    slot: usize,
    device_identity: usize,
    routes: Option<PhysicalCompletionRouteReservation>,
    admission_gate: Option<PhysicalCompletionAdmissionGuard>,
    /// Initial reservations own the QD charge and release it if dropped.
    /// A retry claims an already charged pending slot and must leave that
    /// charge with the pending work when the lower queue remains full.
    owns_slot_charge: bool,
    pending_claim: Option<(RequestId, u64)>,
    committed: bool,
}

impl PhysicalIoWorkerReservation<'_> {
    pub(crate) fn bind_admission(
        &self,
        admission: &mut PreparedPhysicalIoAdmission,
    ) -> AxResult<()> {
        let plan = admission.plan();
        if plan.device_identity() != self.device_identity
            || physical_completion_generation_for(self.device_identity)
                != Some(plan.device_generation())
        {
            return Err(AxError::BadState);
        }
        if admission
            .worker_slot
            .is_some_and(|bound| bound != self.slot)
        {
            return Err(AxError::BadState);
        }
        if admission.worker_slot.is_none() {
            admission.bind_worker_slot(self.slot)?;
        }
        Ok(())
    }

    pub(crate) fn reserve_completion_routes(&mut self, extent_count: usize) -> AxResult<()> {
        if self.routes.is_some() {
            return Err(AxError::BadState);
        }
        self.routes = Some(PhysicalCompletionRouteReservation::new_for_device(
            extent_count,
            self.device_identity,
        )?);
        Ok(())
    }

    /// Installs a pre-publication owner in the same fixed logical slot used
    /// by published work.  A route reservation, if any, is deliberately
    /// dropped first: PendingPublication owns no lower child route and must
    /// not consume a handle/cookie route while waiting for device credit.
    #[allow(clippy::result_large_err)]
    pub(crate) fn commit_pending(
        mut self,
        issued: IssuedRequest,
        mut admission: PreparedPhysicalIoAdmission,
    ) -> Result<(), (AxError, IssuedRequest, PreparedPhysicalIoAdmission)> {
        let request = issued.id();
        let generation = admission.plan().device_generation();
        let device_identity = admission.plan().device_identity();
        if admission.worker_slot.is_none() {
            admission.worker_slot = Some(self.slot);
        }
        drop(self.routes.take());

        let pending_claim = self.pending_claim.take();
        let mut state = self.ring.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let slot_reserved = state
            .physical_slot_reserved
            .get(self.slot)
            .copied()
            .unwrap_or(false);
        let slot_available = state
            .physical_work
            .get(self.slot)
            .is_some_and(Option::is_none);
        if (!slot_reserved || !slot_available)
            && let Some((claimed_request, claimed_generation)) = pending_claim
            && let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                owner.device_identity == device_identity
                    && owner.generation == claimed_generation
                    && owner.request == claimed_request
                    && owner.slot == self.slot
                    && Arc::ptr_eq(&owner.ring, &self.owner)
            })
        {
            owner.claimed = false;
        }
        if !slot_reserved || !slot_available {
            drop(router);
            drop(state);
            return Err((AxError::BadState, issued, admission));
        }

        if let Some((claimed_request, claimed_generation)) = pending_claim {
            let valid = router.pending.iter_mut().flatten().any(|owner| {
                let valid = owner.device_identity == device_identity
                    && owner.generation == claimed_generation
                    && owner.request == claimed_request
                    && owner.request == request
                    && owner.slot == self.slot
                    && Arc::ptr_eq(&owner.ring, &self.owner)
                    && owner.claimed;
                if valid {
                    owner.claimed = false;
                }
                valid
            });
            if !valid {
                if let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                    owner.device_identity == device_identity
                        && owner.generation == claimed_generation
                        && owner.request == claimed_request
                        && owner.slot == self.slot
                        && Arc::ptr_eq(&owner.ring, &self.owner)
                }) {
                    owner.claimed = false;
                }
                drop(router);
                drop(state);
                return Err((AxError::BadState, issued, admission));
            }
            if claimed_generation != generation {
                if let Some(owner) = router.pending.iter_mut().flatten().find(|owner| {
                    owner.device_identity == device_identity
                        && owner.generation == claimed_generation
                        && owner.request == claimed_request
                        && owner.slot == self.slot
                        && Arc::ptr_eq(&owner.ring, &self.owner)
                }) {
                    owner.claimed = false;
                }
                drop(router);
                drop(state);
                return Err((AxError::BadState, issued, admission));
            }
        } else {
            if router.pending.iter().flatten().count() >= IO_URING_PHYSICAL_MAX_QD {
                drop(router);
                drop(state);
                return Err((AxError::ResourceBusy, issued, admission));
            }
            let Some(entry) = router.pending.iter_mut().find(|entry| entry.is_none()) else {
                drop(router);
                drop(state);
                return Err((AxError::ResourceBusy, issued, admission));
            };
            *entry = Some(PhysicalCompletionPendingOwner {
                device_identity,
                generation,
                ring: Arc::clone(&self.owner),
                request,
                slot: self.slot,
                claimed: false,
            });
            router.pending_count = router.pending_count.saturating_add(1);
        }

        *state
            .physical_work
            .get_mut(self.slot)
            .expect("validated slot") = Some(PhysicalIoWork {
            ring: Arc::clone(&self.owner),
            slot: self.slot,
            issued: Some(issued),
            admission: Some(admission),
            pending_publication: true,
            #[cfg(test)]
            test_handle: None,
        });
        state.physical_slot_reserved[self.slot] = false;
        self.committed = true;
        drop(router);
        drop(state);
        self.ring
            .physical_work_pending
            .store(true, Ordering::Release);
        wake_physical_completion_worker();
        Ok(())
    }

    pub(crate) fn commit(
        mut self,
        issued: IssuedRequest,
        mut admission: PreparedPhysicalIoAdmission,
    ) -> AxResult<()> {
        // The normal path always owns a route reservation made before
        // publication.  If that invariant is violated after an effect has
        // already been published, retain the effect in fail-stop custody
        // rather than letting this `Reservation` destructor release the slot
        // and making a synchronous fallback look valid.
        let request = issued.id();
        // The slot is bound before lower publication on the normal path. Keep
        // this idempotent defensive assignment for typed post-publication
        // custody if an internal caller reaches commit without that bind.
        if admission.worker_slot.is_none() {
            admission.worker_slot = Some(self.slot);
        }
        let pending_claim = self.pending_claim.take();
        let mut routes = self.routes.take();
        if routes.is_none() {
            let extent_count = admission.physical_extent_count().unwrap_or(1);
            routes = PhysicalCompletionRouteReservation::new_for_device(
                extent_count,
                self.device_identity,
            )
            .ok();
        }
        let had_routes = routes.is_some();
        let bytes = admission.plan().allowed_len();
        let admission_generation = admission.plan().device_generation();
        // A terminal publication may be a valid accepted prefix.  Its exact
        // handles still have to drain before the typed effect can settle the
        // terminal logical failure; only an absent/invalid handle is routed
        // into custody by `activate_locked`.  Do not use the sticky vendor
        // `is_quarantined` bit here: the vendor sets it for every terminal
        // publication, including the prefix whose handles remain observable.
        let publication = admission.effect().publication();
        let published_extent_count = publication.map_or(0, |published| published.count());
        // Route visibility and work-slot visibility form one publication
        // transaction. A completion worker may already be draining the
        // device while this submitter commits, so both owners stay locked
        // until the Work item and its routes are visible. Acquire the
        // sleeping ring mutex before the IRQ-disabling router lock: QD
        // contention may legitimately wait for the completion task to
        // release RingState, and doing that with interrupts/preemption
        // disabled is both illegal and hostile to the cache-hot completion
        // owner. The completion paths never retain RingState while acquiring
        // the router, so this order preserves the atomic publication without
        // introducing an inverse lock edge.
        let mut state = self.ring.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let slot_reserved = state
            .physical_slot_reserved
            .get(self.slot)
            .copied()
            .unwrap_or(false);
        let Some(entry) = state.physical_work.get_mut(self.slot) else {
            if pending_claim.is_some() {
                clear_physical_completion_pending_owner_locked(
                    &mut router,
                    &self.owner,
                    request,
                    self.slot,
                    self.device_identity,
                    admission_generation,
                );
            }
            drop(state);
            if let Some(mut routes) = routes {
                // The work slot could not be installed, so even a valid
                // publication handle has no matching owner. Keep every
                // reserved extent in typed quarantine rather than exposing
                // an Owner route that would later manufacture an unknown/EIO
                // completion.
                let quarantined =
                    routes.activate_locked(&mut router, &self.owner, request, self.slot, None);
                if quarantined {
                    record_io_uring_physical_quarantine();
                }
            }
            drop(router);
            self.committed = true;
            record_io_uring_physical_quarantine();
            self.ring.park_physical_worker_custody(PhysicalIoWork {
                ring: Arc::clone(&self.owner),
                slot: self.slot,
                issued: Some(issued),
                admission: Some(admission),
                pending_publication: false,
                #[cfg(test)]
                test_handle: None,
            });
            return Err(AxError::BadState);
        };
        if entry.is_some() || !slot_reserved {
            if pending_claim.is_some() {
                clear_physical_completion_pending_owner_locked(
                    &mut router,
                    &self.owner,
                    request,
                    self.slot,
                    self.device_identity,
                    admission_generation,
                );
            }
            drop(state);
            if let Some(mut routes) = routes {
                let quarantined =
                    routes.activate_locked(&mut router, &self.owner, request, self.slot, None);
                if quarantined {
                    record_io_uring_physical_quarantine();
                }
            }
            drop(router);
            self.committed = true;
            record_io_uring_physical_quarantine();
            self.ring.park_physical_worker_custody(PhysicalIoWork {
                ring: Arc::clone(&self.owner),
                slot: self.slot,
                issued: Some(issued),
                admission: Some(admission),
                pending_publication: false,
                #[cfg(test)]
                test_handle: None,
            });
            return Err(AxError::BadState);
        }
        let work = PhysicalIoWork {
            ring: Arc::clone(&self.owner),
            slot: self.slot,
            issued: Some(issued),
            admission: Some(admission),
            pending_publication: false,
            #[cfg(test)]
            test_handle: None,
        };
        *entry = Some(work);
        state.physical_slot_reserved[self.slot] = false;
        let qd = state.physical_work_count;
        self.committed = true;
        let route_quarantined = if let Some(mut routes) = routes {
            routes.activate_locked(&mut router, &self.owner, request, self.slot, publication)
        } else {
            false
        };
        if pending_claim.is_some() {
            clear_physical_completion_pending_owner_locked(
                &mut router,
                &self.owner,
                request,
                self.slot,
                self.device_identity,
                admission_generation,
            );
        }
        drop(state);
        drop(router);
        if route_quarantined {
            record_io_uring_physical_quarantine();
        }
        if !had_routes {
            // A published effect without a route reservation can never be
            // demultiplexed.  The work item remains in its fixed slot and its
            // leases remain live until a typed reset path takes custody.
            record_io_uring_physical_quarantine();
        }
        record_io_uring_physical_submitted(bytes, qd, published_extent_count);
        self.ring
            .physical_work_pending
            .store(true, Ordering::Release);
        // Publish a durable edge only after both the ring work slot and all
        // exact lower routes are visible and their locks are released.  A
        // lower completion may have arrived before this commit and its IRQ
        // edge may already have been observed/cleared by the worker; this
        // exact identity edge forces the worker to revisit the newly
        // committed custody without aliasing a sibling device.
        let _ = mark_physical_completion_device_progress(self.device_identity);
        self.owner.enqueue_deferred();
        Ok(())
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    pub(crate) fn with_physical_publish<T>(&self, publish: impl FnOnce() -> T) -> AxResult<T> {
        if self.admission_gate.is_some() {
            PhysicalCompletionAdmissionGuard::with_publish_for(self.device_identity, publish)
                .ok_or(AxError::BadState)
        } else {
            Ok(publish())
        }
    }
}

impl Drop for PhysicalIoWorkerReservation<'_> {
    fn drop(&mut self) {
        if let Some((request, generation)) = self.pending_claim.take() {
            let _ = set_physical_completion_pending_claim(
                &self.owner,
                request,
                self.slot,
                self.device_identity,
                generation,
                false,
            );
            let mut state = self.ring.state.lock();
            if state
                .physical_work
                .get(self.slot)
                .is_some_and(Option::is_some)
                && state
                    .physical_slot_reserved
                    .get(self.slot)
                    .copied()
                    .unwrap_or(false)
            {
                state.physical_slot_reserved[self.slot] = false;
            }
        }
        if self.committed {
            return;
        }
        if self.owns_slot_charge {
            self.ring.release_reserved_physical_worker_slot(self.slot);
        }
    }
}

#[self_referencing]
struct OwnedPollRegistration {
    file: FileHandle<dyn FileLike>,
    #[borrows(file)]
    #[covariant]
    registration: PollRegistration<'this>,
}

#[derive(Default)]
struct PollRegistrationState {
    arming: bool,
    woke_during_arm: bool,
    registration: Option<OwnedPollRegistration>,
}

struct PollCallbackState {
    ring: Weak<IoUring>,
    request: RequestId,
    enabled: AtomicBool,
    source_woke: AtomicBool,
}

impl PollCallbackState {
    fn publish_source_wake(&self) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        self.source_woke.store(true, Ordering::Release);
        if let Some(ring) = self.ring.upgrade() {
            ring.publish_poll_hint(self.request);
        }
    }
}

struct PollWake(Arc<PollCallbackState>);

impl Wake for PollWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.publish_source_wake();
    }
}

struct PollControl {
    request: RequestId,
    events: IoEvents,
    active: AtomicBool,
    callback: Arc<PollCallbackState>,
    waker: Once<Waker>,
    registration: SpinNoIrq<PollRegistrationState>,
    lease: SpinNoIrq<Option<IoUringFileLease>>,
}

impl PollControl {
    fn try_new(
        ring: Weak<IoUring>,
        request: RequestId,
        lease: IoUringFileLease,
        events: IoEvents,
    ) -> Result<Arc<Self>, (AxError, IoUringFileLease)> {
        let callback = match Arc::try_new(PollCallbackState {
            ring,
            request,
            enabled: AtomicBool::new(true),
            source_woke: AtomicBool::new(false),
        }) {
            Ok(callback) => callback,
            Err(_) => return Err((AxError::NoMemory, lease)),
        };
        let control = match Arc::try_new(Self {
            request,
            events,
            active: AtomicBool::new(true),
            callback: Arc::clone(&callback),
            waker: Once::new(),
            registration: SpinNoIrq::new(PollRegistrationState::default()),
            lease: SpinNoIrq::new(None),
        }) {
            Ok(control) => control,
            Err(_) => return Err((AxError::NoMemory, lease)),
        };
        let wake = match Arc::try_new(PollWake(callback)) {
            Ok(wake) => wake,
            Err(_) => return Err((AxError::NoMemory, lease)),
        };
        control.waker.call_once(|| Waker::from(wake));
        *control.lease.lock() = Some(lease);
        Ok(control)
    }

    fn file_handle(&self) -> AxResult<FileHandle<dyn FileLike>> {
        self.lease
            .lock()
            .as_ref()
            .ok_or(AxError::BadState)?
            .description()
            .map(|description| description.file_handle())
    }

    fn description(&self) -> AxResult<Arc<FileDescription>> {
        self.lease
            .lock()
            .as_ref()
            .ok_or(AxError::BadState)
            .and_then(|lease| lease.description().cloned())
    }

    fn begin_registration(&self) -> bool {
        let mut state = self.registration.lock();
        if !self.active.load(Ordering::Acquire) || state.arming || state.registration.is_some() {
            return false;
        }
        state.arming = true;
        state.woke_during_arm = false;
        true
    }

    fn finish_registration(&self, registration: OwnedPollRegistration) {
        let retired = {
            let mut state = self.registration.lock();
            state.arming = false;
            if self.active.load(Ordering::Acquire) && !state.woke_during_arm {
                state.registration = Some(registration);
                None
            } else {
                Some(registration)
            }
        };
        drop(retired);
    }

    fn abort_registration(&self) {
        let mut state = self.registration.lock();
        state.arming = false;
        state.woke_during_arm = false;
    }

    fn registration_fired(&self) {
        let retired = {
            let mut state = self.registration.lock();
            if state.arming {
                state.woke_during_arm = true;
                None
            } else {
                state.registration.take()
            }
        };
        drop(retired);
    }

    fn ensure_armed(&self) -> AxResult<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        let file = self.file_handle()?;
        if !self.begin_registration() {
            return Ok(());
        }
        let Some(waker) = self.waker.get() else {
            self.abort_registration();
            return Err(AxError::BadState);
        };
        let events = self.events;
        match OwnedPollRegistration::try_new(file, |file| {
            let mut context = Context::from_waker(waker);
            file.register(&mut context, events)
        }) {
            Ok(registration) => {
                self.finish_registration(registration);
                Ok(())
            }
            Err(error) => {
                self.abort_registration();
                Err(crate::readiness::registration_error(error))
            }
        }
    }

    fn check_arm_check(&self) -> AxResult<IoEvents> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(IoEvents::empty());
        }
        let before = self.file_handle()?.poll_events_for_poll() & self.events;
        if !before.is_empty() {
            return Ok(before);
        }
        self.ensure_armed()?;
        let after = self.file_handle()?.poll_events_for_poll() & self.events;
        Ok(before | after)
    }

    fn take_source_wake(&self) -> bool {
        self.callback.source_woke.swap(false, Ordering::AcqRel)
    }

    fn has_source_wake(&self) -> bool {
        self.callback.source_woke.load(Ordering::Acquire)
    }

    fn deactivate(&self) -> Option<IoUringFileLease> {
        self.active.store(false, Ordering::Release);
        self.callback.enabled.store(false, Ordering::Release);
        self.callback.source_woke.store(false, Ordering::Release);
        let registration = self.registration.lock().registration.take();
        drop(registration);
        self.lease.lock().take()
    }
}

/// One bounded pending zero-offset FIFO read. The callback/control path is
/// intentionally weak; the exact buffer lease keeps the ring alive after the
/// ring fd is closed, and it is removed only after the issued request has won
/// its terminal transition and the CQE has been published.
struct PendingStreamWork {
    slot: usize,
    issued: Option<IssuedRequest>,
    request: ReadWriteRequest,
    control: Arc<PollControl>,
    buffer: IoUringBufferLease,
    context: IoOperationContext,
    capability: UserMemoryCapability,
}

impl PendingStreamWork {
    fn request_id(&self) -> RequestId {
        self.issued
            .as_ref()
            .expect("pending stream owner lost issued request")
            .id()
    }
}

/// Resources returned when pending-stream publication cannot be completed
/// before the owner becomes visible. The caller still owns the issued proof
/// and must publish the explanatory terminal CQE.
pub(crate) struct PendingStreamAdmissionError {
    pub(crate) error: AxError,
    pub(crate) issued: IssuedRequest,
    pub(crate) file: IoUringFileLease,
    pub(crate) buffer: IoUringBufferLease,
    pub(crate) context: IoOperationContext,
    pub(crate) capability: UserMemoryCapability,
}

fn allocate_ring_id() -> AxResult<RingId> {
    let raw = NEXT_RING_ID
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::OutOfRange)?;
    RingId::new(raw).map_err(map_core_error)
}

fn checked_page_round(bytes: u32) -> AxResult<usize> {
    let bytes = usize::try_from(bytes).map_err(|_| AxError::InvalidInput)?;
    bytes
        .checked_add(PAGE_BYTES - 1)
        .map(|value| value & !(PAGE_BYTES - 1))
        .filter(|value| *value != 0)
        .ok_or(AxError::InvalidInput)
}

fn map_core_error(error: IoUringError) -> AxError {
    match error {
        IoUringError::AllocationFailed => AxError::NoMemory,
        IoUringError::CompletionQueueFull
        | IoUringError::RequestCapacityExceeded
        | IoUringError::FileLeaseCapacityExceeded
        | IoUringError::BufferLeaseCapacityExceeded
        | IoUringError::Busy => AxError::ResourceBusy,
        IoUringError::Closing | IoUringError::Draining | IoUringError::Closed => {
            AxError::BadFileDescriptor
        }
        IoUringError::InvalidFileSlot
        | IoUringError::FileSlotEmpty
        | IoUringError::UnknownFileLease
        | IoUringError::FileTableNotPublished
        | IoUringError::InvalidBufferSlot
        | IoUringError::BufferSlotEmpty
        | IoUringError::UnknownBufferLease
        | IoUringError::BufferTableNotPublished => AxError::BadFileDescriptor,
        IoUringError::CancellationTargetNotFound => AxError::NotFound,
        IoUringError::UnsupportedOpcode
        | IoUringError::UnsupportedSubmissionFlags
        | IoUringError::UnsupportedOperationFlags
        | IoUringError::CurrentPositionUnsupported
        | IoUringError::UnsupportedRegistration => AxError::OperationNotSupported,
        IoUringError::Overflow | IoUringError::GenerationExhausted => AxError::OutOfRange,
        _ => AxError::InvalidInput,
    }
}

fn map_buffer_lease_error(error: IoUringError) -> AxError {
    match error {
        IoUringError::InvalidBufferRange
        | IoUringError::InvalidBufferSlot
        | IoUringError::BufferSlotEmpty => AxError::BadAddress,
        IoUringError::BufferLeaseCapacityExceeded => AxError::ResourceBusy,
        error => map_core_error(error),
    }
}

fn reservation_is_backpressure(error: IoUringError) -> bool {
    matches!(
        error,
        IoUringError::CompletionQueueFull | IoUringError::RequestCapacityExceeded
    )
}

fn poll_events_from_linux(events: u32) -> IoEvents {
    let mut generic = POLL_ALWAYS_REPORTED;
    for (linux, event) in [
        (POLLIN, IoEvents::READABLE),
        (POLLPRI, IoEvents::PRIORITY),
        (POLLOUT, IoEvents::WRITABLE),
        (POLLERR, IoEvents::ERROR),
        (POLLHUP, IoEvents::HANGUP),
        (POLLNVAL, IoEvents::INVALID),
        (POLLRDNORM, IoEvents::READ_NORMAL),
        (POLLRDBAND, IoEvents::READ_BAND),
        (POLLWRNORM, IoEvents::WRITE_NORMAL),
        (POLLWRBAND, IoEvents::WRITE_BAND),
        (POLLMSG, IoEvents::MESSAGE),
        (POLLREMOVE, IoEvents::REMOVED),
        (POLLRDHUP, IoEvents::READ_HANGUP),
    ] {
        if events & linux != 0 {
            generic |= event;
        }
    }
    generic
}

fn poll_events_to_linux(events: IoEvents) -> u32 {
    let mut linux = 0;
    for (event, bit) in [
        (IoEvents::READABLE, POLLIN),
        (IoEvents::PRIORITY, POLLPRI),
        (IoEvents::WRITABLE, POLLOUT),
        (IoEvents::ERROR, POLLERR),
        (IoEvents::HANGUP, POLLHUP),
        (IoEvents::INVALID, POLLNVAL),
        (IoEvents::READ_NORMAL, POLLRDNORM),
        (IoEvents::READ_BAND, POLLRDBAND),
        (IoEvents::WRITE_NORMAL, POLLWRNORM),
        (IoEvents::WRITE_BAND, POLLWRBAND),
        (IoEvents::MESSAGE, POLLMSG),
        (IoEvents::REMOVED, POLLREMOVE),
        (IoEvents::READ_HANGUP, POLLRDHUP),
    ] {
        if events.contains(event) {
            linux |= bit;
        }
    }
    linux
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalClosePhase {
    Begin,
    Polls,
    FixedFiles,
    Buffers,
    Completions,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FinalCloseProgress {
    phase: FinalClosePhase,
    cursor: usize,
}

impl FinalCloseProgress {
    const fn new() -> Self {
        Self {
            phase: FinalClosePhase::Begin,
            cursor: 0,
        }
    }

    fn enter(&mut self, phase: FinalClosePhase) {
        self.phase = phase;
        self.cursor = 0;
    }

    fn take_slots(&mut self, capacity: usize) -> core::ops::Range<usize> {
        let start = self.cursor.min(capacity);
        let end = start.saturating_add(FINAL_CLOSE_STEP_BUDGET).min(capacity);
        self.cursor = end;
        start..end
    }
}

struct RingState {
    requests: RequestRegistry,
    sq_head: u32,
    sq_dropped: u32,
    admission_in_progress: bool,
    fixed_files: Option<RegisteredFiles>,
    registered_buffers: Option<RegisteredBuffers>,
    next_file_table_id: u64,
    next_buffer_table_id: u64,
    polls: Vec<Option<Arc<PollControl>>>,
    pending_publications: Vec<Option<CompletionToken>>,
    /// Preallocated physical worker slots.  `physical_work_count` charges
    /// both queued and currently-draining items, so QD cannot be exceeded
    /// while a worker temporarily owns an item outside this array.
    physical_work: Vec<Option<PhysicalIoWork>>,
    /// Typed fail-stop custody for an already-published work item when an
    /// internal publication invariant fails.  It is deliberately separate
    /// from the reusable worker slots: retaining the owner here never makes
    /// a published effect look like an ordinary drop or fallback.
    physical_custody: Vec<Option<PhysicalIoWork>>,
    physical_slot_reserved: Vec<bool>,
    physical_work_count: usize,
    /// Fixed-capacity task-context owners for zero-offset FIFO READ_FIXED
    /// operations which admitted but observed `WouldBlock`.
    pending_stream: Vec<Option<PendingStreamWork>>,
    pending_stream_count: usize,
    final_close: FinalCloseProgress,
}

type SubmissionWorkParts = (
    PreparedRequest,
    Result<ParsedSubmission, IoUringError>,
    Option<IoUringFileLease>,
    Option<IoUringBufferLease>,
    Option<IoOperationContext>,
    Option<PreparedPhysicalIoAdmission>,
    Option<AxError>,
    UserMemoryCapability,
);

/// One accepted SQ entry after terminal credit and SQ-head publication.
pub(crate) struct SubmissionWork {
    prepared: PreparedRequest,
    parsed: Result<ParsedSubmission, IoUringError>,
    file: Option<IoUringFileLease>,
    buffer: Option<IoUringBufferLease>,
    /// Captured while the SQE is admitted.  Generic operations execute on the
    /// submitting task; a future worker may consume only an explicitly
    /// worker-safe physical plan and must never recreate this from `current`.
    context: Option<IoOperationContext>,
    /// A successful worker admission owns the file and fixed-buffer leases.
    /// Policy failures are kept separately so execution can publish the
    /// already-admitted CQE without repeating fanotify or RLIMIT side effects.
    physical: Option<PreparedPhysicalIoAdmission>,
    admission_error: Option<AxError>,
    capability: UserMemoryCapability,
}

impl SubmissionWork {
    pub(crate) fn into_parts(self) -> SubmissionWorkParts {
        (
            self.prepared,
            self.parsed,
            self.file,
            self.buffer,
            self.context,
            self.physical,
            self.admission_error,
            self.capability,
        )
    }
}

pub(crate) enum SubmissionStep<'a> {
    Empty,
    CompletionQueueFull,
    AdmissionBusy,
    Dropped,
    Admission(SubmissionAdmission<'a>),
}

/// Reversible SQ admission which owns terminal credit but no published head.
pub(crate) struct SubmissionAdmission<'a> {
    ring: &'a IoUring,
    reservation: Option<RequestReservation>,
    parsed: Result<ParsedSubmission, IoUringError>,
}

impl SubmissionAdmission<'_> {
    pub(crate) const fn parsed(&self) -> Result<ParsedSubmission, IoUringError> {
        self.parsed
    }

    pub(crate) fn commit(
        mut self,
        file: Option<IoUringFileLease>,
        buffer: Option<IoUringBufferLease>,
        context: Option<IoOperationContext>,
        physical: Option<PreparedPhysicalIoAdmission>,
        admission_error: Option<AxError>,
        capability: UserMemoryCapability,
    ) -> AxResult<SubmissionWork> {
        let _submission = self.ring.submission_serial.lock();
        let mut state = self.ring.state.lock();
        if !state.admission_in_progress {
            return Err(AxError::BadState);
        }
        let reservation = self.reservation.take().ok_or(AxError::BadState)?;
        let prepared = state.requests.commit(reservation).map_err(map_core_error)?;
        state.sq_head = state.sq_head.wrapping_add(1);
        state.admission_in_progress = false;
        self.ring.sq_head.store_release(state.sq_head);
        drop(state);
        Ok(SubmissionWork {
            prepared,
            parsed: self.parsed,
            file,
            buffer,
            context,
            physical,
            admission_error,
            capability,
        })
    }

    pub(crate) fn commit_poll(
        self,
        lease: IoUringFileLease,
        linux_events: u32,
        capability: UserMemoryCapability,
    ) -> AxResult<()> {
        let ring = self.ring;
        ring.commit_poll_admission(self, lease, linux_events, capability)
    }
}

impl Drop for SubmissionAdmission<'_> {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        let _submission = self.ring.submission_serial.lock();
        let mut state = self.ring.state.lock();
        if let Err(error) = state.requests.rollback(reservation) {
            error!("io_uring admission rollback lost request ownership: {error:?}");
        }
        state.admission_in_progress = false;
    }
}

pub(crate) struct IoUring {
    id: RingId,
    layout: RingLayout,
    rings: Arc<SharedPages>,
    sqes: Arc<SharedPages>,
    ring_region: FixedSharedMmapRegion,
    cq_ring_region: FixedSharedMmapRegion,
    sqe_region: FixedSharedMmapRegion,
    sq_head: SharedAtomicU32,
    sq_tail: SharedAtomicU32,
    cq_head: SharedAtomicU32,
    cq_tail: SharedAtomicU32,
    sq_dropped: SharedAtomicU32,
    completion_wait: PollSet<RING_WAITER_SLOTS>,
    self_weak: Once<Weak<IoUring>>,
    submission_serial: Mutex<()>,
    completion_serial: Mutex<()>,
    registration_serial: Mutex<()>,
    deferred_next: AtomicPtr<IoUring>,
    deferred_queued: AtomicBool,
    final_close_requested: AtomicBool,
    poll_hint_pending: AtomicBool,
    physical_work_pending: AtomicBool,
    close_waiting_on_physical: AtomicBool,
    poll_hint_bits: Vec<AtomicUsize>,
    pending_publication_count: AtomicUsize,
    state: Mutex<RingState>,
    registered_buffer_budget: Arc<RegisteredBufferPinBudget>,
    _request_charge: RequestSlotCharge,
}

impl IoUring {
    pub(crate) fn try_new(layout: RingLayout) -> AxResult<Arc<Self>> {
        let id = allocate_ring_id()?;
        let request_slots =
            usize::try_from(layout.sq_entries()).map_err(|_| AxError::InvalidInput)?;
        let request_charge = RequestSlotCharge::try_new(request_slots)?;
        let ring_bytes = checked_page_round(layout.ring_bytes())?;
        let sqe_bytes = checked_page_round(layout.sqe_bytes())?;
        let rings = Arc::try_new(SharedPages::new_fixed(ring_bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;
        let sqes = Arc::try_new(SharedPages::new_fixed(sqe_bytes, PageSize::Size4K)?)
            .map_err(|_| AxError::NoMemory)?;

        initialize_ring_header(&rings, layout)?;
        let sq_offsets = layout.sq_offsets();
        let cq_offsets = layout.cq_offsets();
        let sq_head = rings.atomic_u32(sq_offsets.head() as usize)?;
        let sq_tail = rings.atomic_u32(sq_offsets.tail() as usize)?;
        let cq_head = rings.atomic_u32(cq_offsets.head() as usize)?;
        let cq_tail = rings.atomic_u32(cq_offsets.tail() as usize)?;
        let sq_dropped = rings.atomic_u32(sq_offsets.dropped() as usize)?;
        let ring_region = FixedSharedMmapRegion::try_new_detached(
            thekernel_linux_io_uring::IORING_OFF_SQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let cq_ring_region = FixedSharedMmapRegion::try_new_detached(
            thekernel_linux_io_uring::IORING_OFF_CQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let sqe_region = FixedSharedMmapRegion::try_new_detached(
            thekernel_linux_io_uring::IORING_OFF_SQES,
            Arc::clone(&sqes),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let requests = RequestRegistry::new(id, layout.sq_entries(), layout.cq_entries())
            .map_err(map_core_error)?;
        let mut polls = Vec::new();
        polls
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        polls.resize_with(request_slots, || None);
        let mut pending_publications = Vec::new();
        pending_publications
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        pending_publications.resize_with(request_slots, || None);
        let mut physical_work = Vec::new();
        physical_work
            .try_reserve_exact(IO_URING_PHYSICAL_MAX_QD)
            .map_err(|_| AxError::NoMemory)?;
        physical_work.resize_with(IO_URING_PHYSICAL_MAX_QD, || None);
        let mut physical_custody = Vec::new();
        physical_custody
            .try_reserve_exact(IO_URING_PHYSICAL_MAX_QD * 2)
            .map_err(|_| AxError::NoMemory)?;
        physical_custody.resize_with(IO_URING_PHYSICAL_MAX_QD * 2, || None);
        let mut physical_slot_reserved = Vec::new();
        physical_slot_reserved
            .try_reserve_exact(IO_URING_PHYSICAL_MAX_QD)
            .map_err(|_| AxError::NoMemory)?;
        physical_slot_reserved.resize(IO_URING_PHYSICAL_MAX_QD, false);
        let mut pending_stream = Vec::new();
        pending_stream
            .try_reserve_exact(IO_URING_PENDING_STREAM_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        pending_stream.resize_with(IO_URING_PENDING_STREAM_CAPACITY, || None);
        let hint_words = request_slots.div_ceil(usize::BITS as usize);
        let mut poll_hint_bits = Vec::new();
        poll_hint_bits
            .try_reserve_exact(hint_words)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..hint_words {
            poll_hint_bits.push(AtomicUsize::new(0));
        }
        let registered_buffer_budget =
            Arc::try_new(RegisteredBufferPinBudget::new()).map_err(|_| AxError::NoMemory)?;

        let ring = Arc::try_new(Self {
            id,
            layout,
            rings,
            sqes,
            ring_region,
            cq_ring_region,
            sqe_region,
            sq_head,
            sq_tail,
            cq_head,
            cq_tail,
            sq_dropped,
            completion_wait: PollSet::new(),
            self_weak: Once::new(),
            submission_serial: Mutex::new(()),
            completion_serial: Mutex::new(()),
            registration_serial: Mutex::new(()),
            deferred_next: AtomicPtr::new(ptr::null_mut()),
            deferred_queued: AtomicBool::new(false),
            final_close_requested: AtomicBool::new(false),
            poll_hint_pending: AtomicBool::new(false),
            physical_work_pending: AtomicBool::new(false),
            close_waiting_on_physical: AtomicBool::new(false),
            poll_hint_bits,
            pending_publication_count: AtomicUsize::new(0),
            state: Mutex::new(RingState {
                requests,
                sq_head: 0,
                sq_dropped: 0,
                admission_in_progress: false,
                fixed_files: None,
                registered_buffers: None,
                next_file_table_id: 1,
                next_buffer_table_id: 1,
                polls,
                pending_publications,
                physical_work,
                physical_custody,
                physical_slot_reserved,
                physical_work_count: 0,
                pending_stream,
                pending_stream_count: 0,
                final_close: FinalCloseProgress::new(),
            }),
            registered_buffer_budget,
            _request_charge: request_charge,
        })
        .map_err(|_| AxError::NoMemory)?;
        ring.self_weak.call_once(|| Arc::downgrade(&ring));
        Ok(ring)
    }

    pub(crate) const fn layout(&self) -> RingLayout {
        self.layout
    }

    pub(crate) fn try_finalizer_resource(self: &Arc<Self>) -> AxResult<DescriptionResource> {
        Box::try_new(IoUringFinalizer {
            ring: Some(Arc::clone(self)),
        })
        .map(|resource| resource as DescriptionResource)
        .map_err(|_| AxError::NoMemory)
    }

    fn write_completion(&self, publication: &CompletionPublication) -> AxResult<()> {
        let completion = publication.completion();
        let offset = self
            .layout
            .cq_offsets()
            .cqes()
            .checked_add(
                publication
                    .slot()
                    .checked_mul(thekernel_linux_io_uring::CQE_BYTES)
                    .ok_or(AxError::BadState)?,
            )
            .ok_or(AxError::BadState)? as usize;
        let mut bytes = [0_u8; thekernel_linux_io_uring::CQE_BYTES as usize];
        bytes[0..8].copy_from_slice(&completion.user_data().to_ne_bytes());
        bytes[8..12].copy_from_slice(&completion.result().to_ne_bytes());
        bytes[12..16].copy_from_slice(&completion.flags().to_ne_bytes());
        self.rings.write_bytes(offset, &bytes)?;
        self.cq_tail.store_release(publication.new_tail());
        Ok(())
    }

    fn has_completions(&self) -> bool {
        let head = self.cq_head.load_acquire();
        let tail = self.cq_tail.load_acquire();
        let pending = tail.wrapping_sub(head);
        pending != 0 && pending <= self.layout.cq_entries()
    }
}

impl IoUring {
    pub(crate) fn observe_completion_head(&self) -> AxResult<u32> {
        self.flush_pending_publications()?;
        let _publication = self.completion_serial.lock();
        let head = self.cq_head.load_acquire();
        self.state
            .lock()
            .requests
            .observe_completion_head(head)
            .map_err(map_core_error)
    }

    pub(crate) fn available_completions(&self) -> AxResult<u32> {
        let state = self.state.lock();
        Ok(state
            .requests
            .completion_tail()
            .wrapping_sub(state.requests.completion_head()))
    }

    pub(crate) fn prepare_submission(&self) -> AxResult<SubmissionStep<'_>> {
        let _submission = self.submission_serial.lock();
        let head = {
            let state = self.state.lock();
            if state.admission_in_progress {
                return Ok(SubmissionStep::AdmissionBusy);
            }
            state.sq_head
        };
        let tail = self.sq_tail.load_acquire();
        if self
            .layout
            .pending_submissions(head, tail)
            .map_err(map_core_error)?
            == 0
        {
            return Ok(SubmissionStep::Empty);
        }

        let slot = self.layout.submission_slot(head);
        let sqe_index = if let Some(array_offset) = self.layout.sq_offsets().array() {
            let offset = array_offset
                .checked_add(slot.checked_mul(4).ok_or(AxError::BadState)?)
                .ok_or(AxError::BadState)? as usize;
            let mut bytes = [0_u8; 4];
            self.rings.read_bytes(offset, &mut bytes)?;
            u32::from_ne_bytes(bytes)
        } else {
            slot
        };
        let sqe_index = match self.layout.validate_sqe_index(sqe_index) {
            Ok(index) => index,
            Err(_) => {
                let mut state = self.state.lock();
                state.sq_head = state.sq_head.wrapping_add(1);
                state.sq_dropped = state.sq_dropped.wrapping_add(1);
                self.sq_dropped.store_release(state.sq_dropped);
                self.sq_head.store_release(state.sq_head);
                return Ok(SubmissionStep::Dropped);
            }
        };

        let offset = usize::try_from(sqe_index)
            .ok()
            .and_then(|index| index.checked_mul(thekernel_linux_io_uring::SQE_BYTES as usize))
            .ok_or(AxError::BadState)?;
        let mut bytes = [0_u8; thekernel_linux_io_uring::SQE_BYTES as usize];
        self.sqes.read_bytes(offset, &mut bytes)?;
        let copied = CopiedSubmission::new(bytes);
        let descriptor = copied.descriptor();
        let parsed = copied.parse();

        let mut state = self.state.lock();
        let reservation = match state.requests.reserve(descriptor) {
            Ok(reservation) => reservation,
            Err(error) if reservation_is_backpressure(error) => {
                return Ok(SubmissionStep::CompletionQueueFull);
            }
            Err(error) => return Err(map_core_error(error)),
        };
        state.admission_in_progress = true;
        Ok(SubmissionStep::Admission(SubmissionAdmission {
            ring: self,
            reservation: Some(reservation),
            parsed,
        }))
    }

    pub(crate) fn issue_request(
        &self,
        prepared: PreparedRequest,
    ) -> Result<IssuedRequest, RequestIssueError> {
        self.state.lock().requests.issue(prepared)
    }

    /// Reserves one of the preallocated physical completion slots.  This is
    /// the only capacity check on the physical path and runs before an effect
    /// is published to the device.
    pub(crate) fn reserve_physical_worker_slot(&self) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        self.reserve_physical_worker_slot_for_device(physical_completion_default_identity())
    }

    pub(crate) fn reserve_physical_worker_slot_for_device(
        &self,
        device_identity: usize,
    ) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        let admission_gate = PhysicalCompletionAdmissionGuard::begin_for(device_identity)?;
        let owner = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        let mut state = self.state.lock();
        if state.physical_work_count >= IO_URING_PHYSICAL_MAX_QD {
            return Err(AxError::ResourceBusy);
        }
        let slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                (entry.is_none() && !state.physical_slot_reserved[slot]).then_some(slot)
            })
            .ok_or(AxError::ResourceBusy)?;
        // Mark the slot before releasing the state lock.  A reservation is
        // still a live QD owner even though its work item has not been
        // published into `physical_work` yet; without this bit two submitter
        // tasks could reserve the same empty entry concurrently.
        let next_count = state
            .physical_work_count
            .checked_add(1)
            .ok_or(AxError::BadState)?;
        state.physical_slot_reserved[slot] = true;
        state.physical_work_count = next_count;
        Ok(PhysicalIoWorkerReservation {
            ring: self,
            owner,
            slot,
            device_identity,
            routes: None,
            admission_gate,
            owns_slot_charge: true,
            pending_claim: None,
            committed: false,
        })
    }

    /// Claims an already charged PendingPublication slot for one retry.  The
    /// lifecycle guard is acquired before the pending metadata is claimed, so
    /// device reset/close either waits for this retry or observes the owner as
    /// an unclaimed pending request; it can never retire a half-published
    /// owner by stale `(ring, slot)` identity alone.
    fn reserve_pending_physical_worker_slot_for_retry(
        &self,
        device_identity: usize,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> AxResult<PhysicalIoWorkerReservation<'_>> {
        let admission_gate = PhysicalCompletionAdmissionGuard::begin_for(device_identity)?;
        let owner = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        if physical_completion_generation_for(device_identity) != Some(generation) {
            return Err(AxError::BadState);
        }
        let mut state = self.state.lock();
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        let Some(pending) = router.pending.iter_mut().flatten().find(|pending| {
            pending.device_identity == device_identity
                && pending.generation == generation
                && pending.request == request
                && pending.slot == slot
                && Arc::ptr_eq(&pending.ring, &owner)
                && !pending.claimed
        }) else {
            return Err(AxError::BadState);
        };
        if !state
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(|work| {
                work.pending_publication
                    && work.request_id() == Some(request)
                    && work.device_identity() == device_identity
                    && work.device_generation() == generation
            })
            || state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(true)
        {
            return Err(AxError::BadState);
        }
        pending.claimed = true;
        state.physical_slot_reserved[slot] = true;
        drop(router);
        drop(state);
        Ok(PhysicalIoWorkerReservation {
            ring: self,
            owner,
            slot,
            device_identity,
            routes: None,
            admission_gate,
            owns_slot_charge: false,
            pending_claim: Some((request, generation)),
            committed: false,
        })
    }

    fn take_pending_physical_worker_for_retry(
        &self,
        device_identity: usize,
        request: RequestId,
        slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        if !state
            .physical_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false)
            || !state
                .physical_work
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|work| {
                    work.is_pending_publication()
                        && work.device_identity() == device_identity
                        && work.device_generation() == generation
                        && work.request_id() == Some(request)
                })
        {
            return None;
        }
        state.physical_work.get_mut(slot).and_then(Option::take)
    }

    fn release_reserved_physical_worker_slot(&self, slot: usize) {
        let mut state = self.state.lock();
        if state.physical_work.get(slot).is_some_and(Option::is_none)
            && state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(false)
        {
            state.physical_slot_reserved[slot] = false;
            state.physical_work_count = state.physical_work_count.saturating_sub(1);
            let wake_close = self.final_close_requested.load(Ordering::Acquire)
                && state.physical_work_count == 0;
            if wake_close {
                self.close_waiting_on_physical
                    .store(false, Ordering::Release);
            }
            drop(state);
            if wake_close && let Some(ring) = self.self_weak.get().and_then(Weak::upgrade) {
                ring.enqueue_deferred();
            }
        }
    }

    fn release_physical_worker_slot(&self, slot: usize) {
        let mut state = self.state.lock();
        // `PhysicalIoWork` is always dropped after extraction.  The fence bit
        // remains set until this destructor takes the charge back, so a
        // submitter cannot reserve the empty slot between extraction and the
        // owner drop.  Reservation drops use the sibling helper above.
        let slot_is_empty = state.physical_work.get(slot).is_some_and(Option::is_none);
        let slot_fenced = state
            .physical_slot_reserved
            .get(slot)
            .copied()
            .unwrap_or(false);
        if slot_is_empty && slot_fenced {
            state.physical_slot_reserved[slot] = false;
            state.physical_work_count = state.physical_work_count.saturating_sub(1);
        }
        let pending = state.physical_work_count != 0;
        self.physical_work_pending.store(pending, Ordering::Release);
        if self.final_close_requested.load(Ordering::Acquire) && !pending {
            self.close_waiting_on_physical
                .store(false, Ordering::Release);
            drop(state);
            if let Some(ring) = self.self_weak.get().and_then(Weak::upgrade) {
                ring.enqueue_deferred();
            }
        }
    }

    /// Takes one queued item without releasing its QD charge.  The returned
    /// owner keeps that charge until it is dropped after exact settlement.
    pub(crate) fn take_physical_worker_work(&self) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                (entry.is_some() && !state.physical_slot_reserved[slot]).then_some(slot)
            });
        let item = slot.and_then(|slot| self.take_physical_worker_work_at_slot(&mut state, slot));
        if state.physical_work.iter().all(Option::is_none) {
            self.physical_work_pending.store(false, Ordering::Release);
        }
        item
    }

    /// Keeps a published owner in bounded typed custody after an internal
    /// hand-off invariant fails.  The slot fence is intentionally retained;
    /// only a later typed reset/retirement path may make that slot reusable.
    fn park_physical_worker_custody(&self, work: PhysicalIoWork) {
        let mut state = self.state.lock();
        let Some(entry) = state
            .physical_custody
            .iter_mut()
            .find(|entry| entry.is_none())
        else {
            // The custody array is twice the maximum live QD, while every
            // work owner consumes one QD charge. Exhaustion therefore means
            // the ring metadata invariant was already corrupted; aborting is
            // safer than dropping a published DMA owner.
            panic!("io_uring physical custody capacity exhausted");
        };
        *entry = Some(work);
    }

    fn take_physical_worker_work_at_slot(
        &self,
        state: &mut RingState,
        slot: usize,
    ) -> Option<PhysicalIoWork> {
        if !state.physical_work.get(slot).is_some_and(Option::is_some)
            || state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(true)
        {
            return None;
        }
        // The extracted owner remains QD-charged, but the empty table entry
        // is fenced until retention/reinsert or the owner's terminal drop.
        state.physical_slot_reserved[slot] = true;
        let item = state.physical_work.get_mut(slot).and_then(Option::take);
        if item.is_some() && state.physical_work.iter().all(Option::is_none) {
            self.physical_work_pending.store(false, Ordering::Release);
        }
        item
    }

    fn take_physical_worker_work_for_handle(&self, handle: u64) -> Option<PhysicalIoWork> {
        self.take_physical_worker_work_for_device(physical_completion_default_identity(), handle)
    }

    fn take_physical_worker_work_for_device(
        &self,
        device_identity: usize,
        handle: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state.physical_work.iter().position(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity && work.owns_handle(handle)
            })
        })?;
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    fn take_physical_worker_work_for_finalization(&self) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let slot = state.physical_work.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(PhysicalIoWork::needs_finalization_retry)
        })?;
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    fn take_physical_worker_work_for_finalization_at_slot(
        &self,
        slot: usize,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        if !state
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(PhysicalIoWork::needs_finalization_retry)
        {
            return None;
        }
        self.take_physical_worker_work_at_slot(&mut state, slot)
    }

    fn has_physical_finalization_retry(&self) -> bool {
        self.state.lock().physical_work.iter().any(|entry| {
            entry
                .as_ref()
                .is_some_and(PhysicalIoWork::needs_finalization_retry)
        })
    }

    fn has_physical_finalization_retry_at_slot(&self, slot: usize) -> bool {
        self.state
            .lock()
            .physical_work
            .get(slot)
            .and_then(Option::as_ref)
            .is_some_and(PhysicalIoWork::needs_finalization_retry)
    }

    fn retain_physical_worker_work(&self, work: PhysicalIoWork) -> AxResult<()> {
        let slot = work.slot();
        let mut state = self.state.lock();
        let slot_available = state.physical_work.get(slot).is_some_and(Option::is_none)
            && state
                .physical_slot_reserved
                .get(slot)
                .copied()
                .unwrap_or(false);
        if state.physical_work.get(slot).is_none() {
            drop(state);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        if !slot_available {
            drop(state);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        state.physical_slot_reserved[slot] = false;
        state.physical_work[slot] = Some(work);
        self.physical_work_pending.store(true, Ordering::Release);
        Ok(())
    }

    /// Removes one exact published owner for the reset supervisor.  The
    /// owner may still be in its reusable worker slot or already parked in
    /// typed custody after a failed hand-off; both cases keep the slot fence
    /// until the owner is dropped after reset evidence.
    fn take_physical_worker_for_reset(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        self.take_physical_worker_for_reset_for_device(
            physical_completion_default_identity(),
            request,
            worker_slot,
            generation,
        )
    }

    fn take_physical_worker_for_reset_for_device(
        &self,
        device_identity: usize,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> Option<PhysicalIoWork> {
        let mut state = self.state.lock();
        let work_slot = state
            .physical_work
            .iter()
            .enumerate()
            .find_map(|(slot, entry)| {
                entry
                    .as_ref()
                    .is_some_and(|work| {
                        slot == worker_slot
                            && work.device_identity() == device_identity
                            && work.matches_reset_identity_with_generation(
                                request,
                                worker_slot,
                                generation,
                            )
                    })
                    .then_some(slot)
            });
        if let Some(work_slot) = work_slot {
            return self.take_physical_worker_work_at_slot(&mut state, work_slot);
        }
        let custody = state.physical_custody.iter().position(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        })?;
        state.physical_custody[custody].take()
    }

    fn has_physical_worker_request(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> bool {
        self.has_physical_worker_request_for_device(
            physical_completion_default_identity(),
            request,
            worker_slot,
            generation,
        )
    }

    fn has_physical_worker_request_for_device(
        &self,
        device_identity: usize,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
    ) -> bool {
        let state = self.state.lock();
        state.physical_work.iter().any(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        }) || state.physical_custody.iter().any(|entry| {
            entry.as_ref().is_some_and(|work| {
                work.device_identity() == device_identity
                    && work.matches_reset_identity_with_generation(request, worker_slot, generation)
            })
        })
    }

    /// Finishes one already extracted ring owner after the lower reset has
    /// proved quiescence.  The reset transaction extracts every owner and
    /// validates every route before committing; this final step is therefore
    /// deliberately infallible and cannot leave a partially released owner
    /// set behind a fallible CQE/ring operation.
    fn finish_physical_worker_after_reset(
        &self,
        mut work: PhysicalIoWork,
        proof: PhysicalIoResetProof,
    ) {
        let admission = work.take_admission();
        let issued = work.take_issued();
        run_physical_reset_terminal_order(
            || {
                if let Some(admission) = admission {
                    // The lower reset proof makes it safe to release the
                    // effect's range/cache/inode custody, along with the
                    // sibling file, buffer and policy leases.
                    admission.retire_after_reset(proof);
                }
            },
            || {
                // Dropping the now-empty work releases the exact fixed slot
                // and QD charge before any user-visible completion exists.
                drop(work);
            },
            || {
                // The reset owner may race a close/cancel terminal claimant.
                // The request proof is consumed either way; a failed
                // publication is handled by the normal close path.
                if let Some(issued) = issued {
                    let _ = self.complete_issued(
                        issued,
                        TerminalCause::Completed,
                        -LinuxError::EIO.code(),
                        0,
                    );
                }
            },
        );
    }

    /// Retires one exact ring owner after the lower reset has proved
    /// quiescence.  Kept as a small typed helper for callers that already own
    /// the reset identity; the global reset path uses the batch transaction
    /// above so route/work release cannot partially commit.
    fn retire_physical_worker_after_reset(
        &self,
        request: RequestId,
        worker_slot: usize,
        generation: u64,
        proof: PhysicalIoResetProof,
    ) -> AxResult<bool> {
        let Some(work) = self.take_physical_worker_for_reset(request, worker_slot, generation)
        else {
            return Ok(false);
        };
        self.finish_physical_worker_after_reset(work, proof);
        Ok(true)
    }

    fn publish_terminal_physical_work(
        self: &Arc<Self>,
        work: PhysicalIoWork,
        result: i32,
        route_handle: Option<u64>,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let device_identity = work.device_identity();
        let Some(operation) = work.admission().map(PreparedPhysicalIoAdmission::operation) else {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                route_handle,
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        if work.issued().is_none() {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                route_handle,
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        }
        // Retire the complete admission and empty work-slot owner before
        // releasing the route or publishing the IssuedRequest. No physical
        // owner, route, or QD charge may remain when the CQE becomes visible.
        let payload = PhysicalIoTerminalPayload::from_valid_work(work);
        let (completion_ring, issued) = match payload.retire() {
            Ok(retired) => retired,
            Err(error) => {
                // Physical retirement is already proven. Even a malformed
                // upper payload must not leave a route pointing at a dropped
                // Work owner.
                release_physical_completion_routes_for_device(
                    device_identity,
                    self,
                    request,
                    route_handle,
                );
                return Err(error);
            }
        };
        release_physical_completion_routes_for_device(device_identity, self, request, route_handle);
        let completed_bytes = result.max(0) as usize;
        record_io_uring_physical_completed(completed_bytes);
        match operation {
            PreparedPhysicalIoOperation::Read => {
                record_io_uring_dma_direct_read_hit(completed_bytes)
            }
            PreparedPhysicalIoOperation::Write => {
                record_io_uring_dma_direct_write_hit(completed_bytes)
            }
        }
        completion_ring.complete_issued(issued, TerminalCause::Completed, result, 0)?;
        Ok(PhysicalIoCompletionDisposition::Settled)
    }

    /// Retries only a previously settled filesystem finalization. A bounded
    /// caller invokes this from the existing task-context physical worker;
    /// no lower completion is replayed and no physical request is reissued.
    fn retry_physical_finalization(
        self: &Arc<Self>,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(work) = self.take_physical_worker_work_for_finalization() else {
            return Ok(None);
        };
        let device_identity = work.device_identity();
        let result = self.retry_physical_finalization_work(work);
        if result.is_err() {
            mark_physical_completion_device_reset_pending(device_identity);
        }
        result
    }

    fn retry_physical_finalization_at_slot(
        self: &Arc<Self>,
        slot: usize,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(work) = self.take_physical_worker_work_for_finalization_at_slot(slot) else {
            return Ok(None);
        };
        let device_identity = work.device_identity();
        let result = self.retry_physical_finalization_work(work);
        if result.is_err() {
            // Finalization failure belongs to this work's exact lower queue;
            // do not fence a sibling device merely because both share the
            // one task-context completion owner.
            mark_physical_completion_device_reset_pending(device_identity);
        }
        result
    }

    fn retry_physical_finalization_work(
        self: &Arc<Self>,
        mut work: PhysicalIoWork,
    ) -> AxResult<Option<PhysicalIoCompletionDisposition>> {
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let device_identity = work.device_identity();
        let Some(admission) = work.admission_mut() else {
            quarantine_physical_completion_routes_for_device(device_identity, self, request, None);
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let outcome = admission.effect_mut().retry_finalization();
        match outcome {
            PhysicalIoSettleOutcome::RetryFinalization => {
                self.retain_physical_worker_work(work)?;
                Ok(Some(PhysicalIoCompletionDisposition::Retained))
            }
            PhysicalIoSettleOutcome::Settled { result } => {
                let result = physical_io_completion_result(result);
                self.publish_terminal_physical_work(work, result, None)
                    .map(Some)
            }
            PhysicalIoSettleOutcome::Retain { .. } => {
                quarantine_physical_completion_routes_for_device(
                    device_identity,
                    self,
                    request,
                    None,
                );
                self.park_physical_worker_custody(work);
                Err(AxError::BadState)
            }
        }
    }

    /// Applies one exact task-context device completion.  The block wait
    /// owner supplies these records after `wait_any_physical_completion`; IRQ
    /// code never calls this method.  A retained result puts the work owner
    /// back into its fixed slot, while a settled result is the only path that
    /// consumes the issued token and publishes a CQE.
    fn consume_physical_completion(
        self: &Arc<Self>,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        self.consume_physical_completion_for_device(
            physical_completion_default_identity(),
            completion,
        )
    }

    fn consume_physical_completion_for_device(
        self: &Arc<Self>,
        device_identity: usize,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let slot = {
            let state = self.state.lock();
            state.physical_work.iter().position(|entry| {
                entry.as_ref().is_some_and(|work| {
                    work.device_identity() == device_identity && work.owns_handle(completion.handle)
                })
            })
        };
        let Some(slot) = slot else {
            // A duplicate or a malformed driver ordering can expose a route
            // whose Work owner is already being consumed. Preserve the
            // observation in non-replayable custody; never turn it into a
            // synthetic EIO or silently discard the device record.
            quarantine_physical_completion_for_device(device_identity, completion, false)?;
            return Ok(PhysicalIoCompletionDisposition::Unknown);
        };
        self.consume_physical_completion_for_device_at_slot(device_identity, slot, completion)
    }

    fn consume_physical_completion_for_device_at_slot(
        self: &Arc<Self>,
        device_identity: usize,
        slot: usize,
        completion: PhysicalIoCompletion,
    ) -> AxResult<PhysicalIoCompletionDisposition> {
        let work = {
            let mut state = self.state.lock();
            let matches = state
                .physical_work
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|work| {
                    work.device_identity() == device_identity && work.owns_handle(completion.handle)
                });
            matches.then(|| self.take_physical_worker_work_at_slot(&mut state, slot))
        }
        .flatten();
        let Some(mut work) = work else {
            // The route lookup supplied an exact fixed slot, but a competing
            // completion/finalizer may already own it. Revalidate the handle
            // under RingState and retain this observation instead of scanning
            // the other 31 slots or aliasing a recycled owner.
            quarantine_physical_completion_for_device(device_identity, completion, false)?;
            return Ok(PhysicalIoCompletionDisposition::Unknown);
        };
        let Some(request) = work.request_id() else {
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let Some(admission) = work.admission_mut() else {
            quarantine_physical_completion_routes_for_device(
                device_identity,
                self,
                request,
                Some(completion.handle),
            );
            self.park_physical_worker_custody(work);
            return Err(AxError::BadState);
        };
        let settlement = admission
            .effect_mut()
            .settle(core::slice::from_ref(&completion));
        // `settle` consumes exactly one lower completion.  A normal partial
        // batch reports `MissingCompletion` until the remaining children
        // arrive; the final child can report either `Settled` or a bounded
        // finalization retry.  Count only those outcomes, never protocol
        // failures that merely retain the owner in quarantine custody.
        if matches!(
            &settlement,
            PhysicalIoSettleOutcome::Retain {
                reason: PhysicalIoPendingReason::MissingCompletion { .. }
            } | PhysicalIoSettleOutcome::Settled { .. }
                | PhysicalIoSettleOutcome::RetryFinalization
        ) {
            record_io_uring_physical_child_completed();
        }
        match settlement {
            PhysicalIoSettleOutcome::Retain { reason } => {
                // A multi-extent effect normally returns Retain while it is
                // waiting for the remaining exact handles. The completion
                // has already been consumed into the vendor effect and must
                // not fill the bounded quarantine slab. Only protocol
                // failures retain the observation as diagnostic custody.
                let quarantine = retained_completion_needs_quarantine(reason);
                if quarantine {
                    quarantine_physical_completion_routes_for_device(
                        device_identity,
                        self,
                        request,
                        Some(completion.handle),
                    );
                }
                self.retain_physical_worker_work(work)?;
                if quarantine {
                    quarantine_physical_completion_for_device(device_identity, completion, false)?;
                }
                Ok(PhysicalIoCompletionDisposition::Retained)
            }
            PhysicalIoSettleOutcome::Settled { result } => {
                let result = physical_io_completion_result(result);
                self.publish_terminal_physical_work(work, result, Some(completion.handle))
            }
            PhysicalIoSettleOutcome::RetryFinalization => {
                // Every device handle has already retired. Keep the exact
                // work/issued/effect owner and retry only filesystem
                // finalization from the task-context continuation.
                self.retain_physical_worker_work(work)?;
                Ok(PhysicalIoCompletionDisposition::Retained)
            }
        }
    }

    pub(crate) fn physical_worker_len(&self) -> usize {
        self.state.lock().physical_work_count
    }

    pub(crate) fn close_waiting_on_physical(&self) -> bool {
        self.close_waiting_on_physical.load(Ordering::Acquire)
    }

    fn finish_request(
        &self,
        id: RequestId,
        cause: TerminalCause,
        result: i32,
        flags: u32,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let permit = state
            .requests
            .claim_terminal(id, cause)
            .map_err(map_core_error)?;
        let token = state
            .requests
            .finish_terminal(permit, result, flags)
            .map_err(map_core_error)?;
        self.queue_completion_locked(&mut state, token)
    }

    fn queue_completion_locked(
        &self,
        state: &mut RingState,
        token: CompletionToken,
    ) -> AxResult<()> {
        let slot = usize::try_from(token.id().slot()).map_err(|_| AxError::BadState)?;
        let capacity = state.pending_publications.len();
        let pending = state
            .pending_publications
            .get_mut(slot)
            .ok_or(AxError::BadState)?;
        if pending.is_some() {
            return Err(AxError::BadState);
        }
        let previous = self
            .pending_publication_count
            .fetch_add(1, Ordering::AcqRel);
        if previous >= capacity {
            self.pending_publication_count
                .fetch_sub(1, Ordering::AcqRel);
            return Err(AxError::BadState);
        }
        *pending = Some(token);
        Ok(())
    }

    fn take_completion_locked(
        &self,
        state: &mut RingState,
        slot: usize,
    ) -> AxResult<Option<CompletionToken>> {
        let pending = state
            .pending_publications
            .get_mut(slot)
            .ok_or(AxError::BadState)?;
        let Some(token) = pending.take() else {
            return Ok(None);
        };
        if self
            .pending_publication_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .is_err()
        {
            *pending = Some(token);
            return Err(AxError::BadState);
        }
        Ok(Some(token))
    }

    fn publish_pending_slot(&self, slot: usize) -> AxResult<bool> {
        let published = {
            let _publication = self.completion_serial.lock();
            if self.final_close_requested.load(Ordering::Acquire) {
                let phase = self.state.lock().final_close.phase;
                // Close may begin while an uncancellable pending stream is
                // still running. Let its terminal CQE publish during the
                // Polls/FixedFiles/Buffers phases so the registered-buffer
                // lease can retire; only the final drain may discard CQEs.
                if matches!(
                    phase,
                    FinalClosePhase::Completions | FinalClosePhase::Finished
                ) {
                    return Ok(false);
                }
            }
            let token = {
                let mut state = self.state.lock();
                self.take_completion_locked(&mut state, slot)?
            };
            let Some(token) = token else {
                return Ok(false);
            };
            let publication = {
                let mut state = self.state.lock();
                match state.requests.publish(&token) {
                    Ok(publication) => publication,
                    Err(error) => {
                        self.queue_completion_locked(&mut state, token)?;
                        return Err(map_core_error(error));
                    }
                }
            };
            if let Err(error) = self.write_completion(&publication) {
                let retry = self
                    .state
                    .lock()
                    .requests
                    .rollback_publication(publication)
                    .map_err(map_core_error)?;
                let mut state = self.state.lock();
                self.queue_completion_locked(&mut state, retry)?;
                return Err(error);
            }
            self.state
                .lock()
                .requests
                .commit_publication(publication)
                .map_err(map_core_error)?;
            true
        };
        if published {
            self.completion_wait.wake();
        }
        Ok(published)
    }

    fn flush_pending_publications(&self) -> AxResult<()> {
        if self.pending_publication_count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        for slot in 0..usize::try_from(self.layout.sq_entries()).map_err(|_| AxError::BadState)? {
            self.publish_pending_slot(slot)?;
            if self.pending_publication_count.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn complete_request(
        &self,
        id: RequestId,
        cause: TerminalCause,
        result: i32,
        flags: u32,
    ) -> AxResult<()> {
        self.finish_request(id, cause, result, flags)?;
        self.publish_pending_slot(id.slot() as usize).map(|_| ())
    }

    /// Consumes the sole issued-request proof at the completion boundary.
    /// Physical workers must use this API instead of passing a copied
    /// [`RequestId`], so a stale/duplicate worker cannot manufacture a CQE
    /// after another terminal owner has won the request.
    pub(crate) fn complete_issued(
        &self,
        issued: IssuedRequest,
        cause: TerminalCause,
        result: i32,
        flags: u32,
    ) -> AxResult<()> {
        let id = issued.id();
        self.finish_request(id, cause, result, flags)?;
        self.publish_pending_slot(id.slot() as usize).map(|_| ())
    }

    fn is_ring_description(description: &Arc<FileDescription>) -> bool {
        description.file_handle().downcast::<IoUring>().is_ok()
    }

    pub(crate) fn retain_descriptor(
        &self,
        description: Arc<FileDescription>,
    ) -> AxResult<IoUringFileLease> {
        if Self::is_ring_description(&description) {
            return Err(AxError::BadFileDescriptor);
        }
        Ok(IoUringFileLease::Descriptor(description))
    }

    pub(crate) fn acquire_registered_file(&self, slot: FileSlot) -> AxResult<IoUringFileLease> {
        let lease = self
            .state
            .lock()
            .fixed_files
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .table
            .acquire(slot)
            .map_err(map_core_error)?;
        let ring = self.self_weak.get().ok_or(AxError::BadState)?.clone();
        Ok(IoUringFileLease::Registered {
            ring,
            lease: Some(lease),
        })
    }

    pub(crate) fn acquire_registered_buffer(
        &self,
        slot: BufferSlot,
        address: u64,
        length: u32,
    ) -> AxResult<IoUringBufferLease> {
        // Acquire the ring owner before taking a table lease. A failed weak
        // upgrade must not strand the table's lease counter during teardown.
        let ring = self
            .self_weak
            .get()
            .ok_or(AxError::BadState)?
            .upgrade()
            .ok_or(AxError::BadState)?;
        let lease = self
            .state
            .lock()
            .registered_buffers
            .as_mut()
            .ok_or(AxError::BadFileDescriptor)?
            .table
            .acquire(slot, address, length)
            .map_err(map_buffer_lease_error)?;
        Ok(IoUringBufferLease {
            ring,
            lease: Some(lease),
        })
    }

    fn release_registered_file(&self, lease: RegisteredFileLease<FileDescription>) {
        let (retired, closed) = {
            let mut state = self.state.lock();
            let Some(files) = state.fixed_files.as_mut() else {
                drop(state);
                drop(lease);
                return;
            };
            let retired = match files.table.release(lease) {
                Ok(LeaseRelease::Active) => None,
                Ok(LeaseRelease::Retired(retired)) => Some(retired),
                Err(error) => {
                    let kind = error.error();
                    core::mem::forget(error.into_lease());
                    error!("io_uring registered-file release lost ownership: {kind:?}");
                    return;
                }
            };
            let should_close = files
                .table
                .progress()
                .is_ok_and(|progress| progress.empty());
            if should_close {
                if let Err(error) = files.table.finish_retire() {
                    error!("io_uring fixed-file retirement did not finish: {error:?}");
                    (retired, None)
                } else {
                    (retired, state.fixed_files.take())
                }
            } else {
                (retired, None)
            }
        };
        drop(retired);
        drop(closed);
    }

    fn release_registered_buffer(&self, lease: RegisteredBufferLease<RegisteredBuffer>) {
        let (retired, closed) = {
            let mut state = self.state.lock();
            let Some(buffers) = state.registered_buffers.as_mut() else {
                drop(state);
                drop(lease);
                return;
            };
            let retired = match buffers.table.release(lease) {
                Ok(BufferLeaseRelease::Active) => None,
                Ok(BufferLeaseRelease::Retired(retired)) => Some(retired),
                Err(error) => {
                    let kind = error.error();
                    core::mem::forget(error.into_lease());
                    error!("io_uring registered-buffer release lost ownership: {kind:?}");
                    return;
                }
            };
            let should_close = buffers
                .table
                .progress()
                .is_ok_and(|progress| progress.empty());
            if should_close {
                if let Err(error) = buffers.table.finish_retire() {
                    error!("io_uring registered-buffer retirement did not finish: {error:?}");
                    (retired, None)
                } else {
                    (retired, state.registered_buffers.take())
                }
            } else {
                (retired, None)
            }
        };
        drop(retired);
        drop(closed);
    }

    pub(crate) fn register_files(&self, files: Vec<Option<Arc<FileDescription>>>) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        if files.is_empty() {
            return Err(AxError::InvalidInput);
        }
        if files.iter().flatten().any(Self::is_ring_description) {
            return Err(AxError::BadFileDescriptor);
        }
        let charge = FixedFileSlotCharge::try_new(files.len())?;
        let table_id = {
            let mut state = self.state.lock();
            if state.fixed_files.is_some() {
                return Err(AxError::ResourceBusy);
            }
            let raw = state.next_file_table_id;
            state.next_file_table_id = raw.checked_add(1).ok_or(AxError::OutOfRange)?;
            FileTableId::new(raw).map_err(map_core_error)?
        };
        let capacity = u32::try_from(files.len()).map_err(|_| AxError::InvalidInput)?;
        let mut table =
            RegisteredFileTable::new(self.id, table_id, capacity, self.layout.sq_entries())
                .map_err(map_core_error)?;
        for (slot, file) in files.into_iter().enumerate() {
            if let Some(file) = file
                && let Err(error) = table.install(
                    FileSlot::new(u32::try_from(slot).map_err(|_| AxError::InvalidInput)?),
                    file,
                )
            {
                let kind = error.error();
                drop(error.into_owner());
                return Err(map_core_error(kind));
            }
        }
        table.publish().map_err(map_core_error)?;
        let mut state = self.state.lock();
        if state.fixed_files.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.fixed_files = Some(RegisteredFiles {
            table,
            _charge: charge,
        });
        Ok(())
    }

    pub(crate) fn unregister_files(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        {
            let mut state = self.state.lock();
            let files = state
                .fixed_files
                .as_mut()
                .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
            files.table.begin_retire().map_err(map_core_error)?;
        }
        self.drain_registered_files_after_retire()
    }

    fn drain_registered_files_after_retire(&self) -> AxResult<()> {
        loop {
            let retired = {
                let mut state = self.state.lock();
                let Some(files) = state.fixed_files.as_mut() else {
                    break;
                };
                let Some(token) = files.table.next_retirable().map_err(map_core_error)? else {
                    break;
                };
                files.table.retire(token).map_err(map_core_error)?
            };
            drop(retired);
        }
        let closed = {
            let mut state = self.state.lock();
            if let Some(files) = state.fixed_files.as_mut() {
                if files.table.progress().map_err(map_core_error)?.empty() {
                    files.table.finish_retire().map_err(map_core_error)?;
                    state.fixed_files.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(closed);
        Ok(())
    }

    pub(crate) fn register_buffers(
        &self,
        capability: &UserMemoryCapability,
        buffers: Vec<(usize, usize)>,
    ) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        if buffers.is_empty() {
            return Err(AxError::InvalidInput);
        }
        let capacity = u32::try_from(buffers.len()).map_err(|_| AxError::InvalidInput)?;
        // Validate every descriptor and its page-cover arithmetic before
        // publishing or pinning any owner. The syscall adapter has already
        // checked user write access; this second pass keeps the ring API
        // transactional for future callers too.
        for &(address, length) in &buffers {
            if length == 0 {
                return Err(AxError::InvalidInput);
            }
            let end = address.checked_add(length).ok_or(AxError::BadAddress)?;
            let page_start = address & !(PAGE_BYTES - 1);
            let page_end = end
                .checked_add(PAGE_BYTES - 1)
                .map(|value| value & !(PAGE_BYTES - 1))
                .ok_or(AxError::BadAddress)?;
            if page_end <= page_start || page_end - page_start < PAGE_BYTES {
                return Err(AxError::InvalidInput);
            }
        }
        let charge = RegisteredBufferSlotCharge::try_new(buffers.len())?;
        let table_id = {
            let mut state = self.state.lock();
            if state.final_close.phase != FinalClosePhase::Begin {
                return Err(AxError::BadFileDescriptor);
            }
            if state.registered_buffers.is_some() {
                return Err(AxError::ResourceBusy);
            }
            let raw = state.next_buffer_table_id;
            state.next_buffer_table_id = raw.checked_add(1).ok_or(AxError::OutOfRange)?;
            BufferTableId::new(raw).map_err(map_core_error)?
        };
        let mut table =
            RegisteredBufferTable::new(self.id, table_id, capacity, self.layout.sq_entries())
                .map_err(map_core_error)?;
        for (slot, (address, length)) in buffers.into_iter().enumerate() {
            let end = address.checked_add(length).ok_or(AxError::BadAddress)?;
            let page_start = address & !(PAGE_BYTES - 1);
            let page_end = end
                .checked_add(PAGE_BYTES - 1)
                .map(|value| value & !(PAGE_BYTES - 1))
                .ok_or(AxError::BadAddress)?;
            let page_len = page_end - page_start;
            let page_count = page_len / PAGE_BYTES;
            let pin_charge = self.registered_buffer_budget.try_charge(page_count)?;
            let pin = match try_pin_user_segments_to_user_longterm_with(
                capability,
                page_start as *mut u8,
                page_len,
            ) {
                Some(pin) => pin,
                None => {
                    drop(pin_charge);
                    return Err(AxError::ResourceBusy);
                }
            };
            let mut segment_ends = Vec::new();
            segment_ends
                .try_reserve_exact(pin.segments().len())
                .map_err(|_| AxError::NoMemory)?;
            let mut segment_end = 0usize;
            for segment in pin.segments() {
                segment_end = segment_end
                    .checked_add(segment.len)
                    .ok_or(AxError::BadAddress)?;
                segment_ends.push(segment_end);
            }
            if segment_end != page_len {
                return Err(AxError::BadState);
            }
            let owner = Arc::try_new(RegisteredBuffer {
                address,
                length,
                pin_start: page_start,
                pin_len: page_len,
                segment_ends,
                pin_segments_disjoint: physical_segments_are_disjoint(pin.segments()),
                capability: capability.clone(),
                _pin_owner: PinBeforeCharge::new(pin, pin_charge),
            })
            .map_err(|_| AxError::NoMemory)?;
            if let Err(error) = table.install(
                BufferSlot::new(u32::try_from(slot).map_err(|_| AxError::InvalidInput)?),
                address as u64,
                length as u64,
                owner,
            ) {
                let kind = error.error();
                drop(error.into_owner());
                return Err(map_core_error(kind));
            }
        }
        table.publish().map_err(map_core_error)?;
        let mut state = self.state.lock();
        if state.final_close.phase != FinalClosePhase::Begin {
            return Err(AxError::BadFileDescriptor);
        }
        if state.registered_buffers.is_some() {
            return Err(AxError::ResourceBusy);
        }
        state.registered_buffers = Some(RegisteredBuffers {
            table,
            _charge: charge,
        });
        Ok(())
    }

    pub(crate) fn unregister_buffers(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        {
            let mut state = self.state.lock();
            let buffers = state
                .registered_buffers
                .as_mut()
                .ok_or_else(|| AxError::from(LinuxError::ENXIO))?;
            buffers.table.begin_retire().map_err(map_core_error)?;
        }
        loop {
            let retired = {
                let mut state = self.state.lock();
                let Some(buffers) = state.registered_buffers.as_mut() else {
                    break;
                };
                let Some(token) = buffers.table.next_retirable().map_err(map_core_error)? else {
                    break;
                };
                buffers.table.retire(token).map_err(map_core_error)?
            };
            drop(retired);
        }
        let closed = {
            let mut state = self.state.lock();
            if let Some(buffers) = state.registered_buffers.as_mut() {
                if buffers.table.progress().map_err(map_core_error)?.empty() {
                    buffers.table.finish_retire().map_err(map_core_error)?;
                    state.registered_buffers.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        drop(closed);
        Ok(())
    }

    fn request_final_close(self: &Arc<Self>) {
        self.final_close_requested.store(true, Ordering::Release);
        self.enqueue_deferred();
    }

    fn begin_final_close_step(&self) -> AxResult<bool> {
        let mut state = self.state.lock();
        state.requests.begin_close().map_err(map_core_error)?;
        state.final_close.enter(FinalClosePhase::Polls);
        Ok(false)
    }

    fn close_polls_step(&self) -> AxResult<bool> {
        let (slots, capacity) = {
            let mut state = self.state.lock();
            let capacity = state.polls.len();
            (state.final_close.take_slots(capacity), capacity)
        };

        for slot in slots {
            let control = {
                let mut state = self.state.lock();
                let request = state
                    .polls
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|control| control.request);
                let Some(request) = request else {
                    continue;
                };
                match state
                    .requests
                    .claim_terminal(request, TerminalCause::Closing)
                {
                    Ok(permit) => {
                        let token = state
                            .requests
                            .finish_terminal(permit, -LinuxError::ECANCELED.code(), 0)
                            .map_err(map_core_error)?;
                        self.queue_completion_locked(&mut state, token)?;
                        state.polls[slot].take().ok_or(AxError::BadState)?
                    }
                    Err(
                        IoUringError::TerminalAlreadyClaimed
                        | IoUringError::UnknownRequest
                        | IoUringError::RequestUncancellable,
                    ) => state.polls[slot].take().ok_or(AxError::BadState)?,
                    Err(error) => return Err(map_core_error(error)),
                }
            };
            drop(control.deactivate());
            drop(control);
        }

        let mut state = self.state.lock();
        if state.final_close.cursor >= capacity {
            state.final_close.enter(FinalClosePhase::FixedFiles);
        }
        Ok(false)
    }

    fn close_fixed_files_step(&self) -> AxResult<bool> {
        for _ in 0..FINAL_CLOSE_STEP_BUDGET {
            let (retired, closed, stop) = {
                let _registration = self.registration_serial.lock();
                let mut state = self.state.lock();
                let Some(files) = state.fixed_files.as_mut() else {
                    state.final_close.enter(FinalClosePhase::Buffers);
                    return Ok(false);
                };
                files.table.begin_retire().map_err(map_core_error)?;
                match files.table.next_retirable().map_err(map_core_error)? {
                    Some(token) => (
                        Some(files.table.retire(token).map_err(map_core_error)?),
                        None,
                        false,
                    ),
                    None => {
                        if files.table.progress().map_err(map_core_error)?.empty() {
                            files.table.finish_retire().map_err(map_core_error)?;
                            let closed = state.fixed_files.take();
                            state.final_close.enter(FinalClosePhase::Buffers);
                            (None, closed, true)
                        } else {
                            (None, None, true)
                        }
                    }
                }
            };
            drop(retired);
            drop(closed);
            if stop {
                break;
            }
        }
        Ok(false)
    }

    fn close_buffers_step(&self) -> AxResult<bool> {
        for _ in 0..FINAL_CLOSE_STEP_BUDGET {
            let (retired, closed, stop) = {
                let _registration = self.registration_serial.lock();
                let mut state = self.state.lock();
                let Some(buffers) = state.registered_buffers.as_mut() else {
                    state.final_close.enter(FinalClosePhase::Completions);
                    return Ok(false);
                };
                buffers.table.begin_retire().map_err(map_core_error)?;
                match buffers.table.next_retirable().map_err(map_core_error)? {
                    Some(token) => (
                        Some(buffers.table.retire(token).map_err(map_core_error)?),
                        None,
                        false,
                    ),
                    None => {
                        if buffers.table.progress().map_err(map_core_error)?.empty() {
                            buffers.table.finish_retire().map_err(map_core_error)?;
                            let closed = state.registered_buffers.take();
                            state.final_close.enter(FinalClosePhase::Completions);
                            (None, closed, true)
                        } else {
                            (None, None, true)
                        }
                    }
                }
            };
            drop(retired);
            drop(closed);
            if stop {
                break;
            }
        }
        Ok(false)
    }

    fn close_completions_step(&self) -> AxResult<bool> {
        let publication = self.completion_serial.lock();
        let (slots, capacity) = {
            let mut state = self.state.lock();
            // Published physical effects retain the file/buffer leases and
            // their device completion authority.  Final close parks here
            // until the last work owner drops; it must not discard requests
            // or spin-requeue while DMA may still be active.
            if state.physical_work_count != 0 {
                self.close_waiting_on_physical
                    .store(true, Ordering::Release);
                return Ok(false);
            }
            match state.requests.begin_draining() {
                Ok(_) => {}
                Err(IoUringError::Busy) => return Ok(false),
                Err(error) => return Err(map_core_error(error)),
            }
            let capacity = state.pending_publications.len();
            (state.final_close.take_slots(capacity), capacity)
        };

        let mut state = self.state.lock();
        for slot in slots {
            if let Some(token) = self.take_completion_locked(&mut state, slot)? {
                state
                    .requests
                    .discard_completion(token)
                    .map_err(map_core_error)?;
            }
        }
        if state.final_close.cursor < capacity {
            return Ok(false);
        }
        if self.pending_publication_count.load(Ordering::Acquire) != 0 {
            return Err(AxError::BadState);
        }
        state.requests.discard_published().map_err(map_core_error)?;
        state.requests.finish_close().map_err(map_core_error)?;
        state.final_close.enter(FinalClosePhase::Finished);
        drop(state);
        drop(publication);

        for word in &self.poll_hint_bits {
            word.store(0, Ordering::Release);
        }
        self.poll_hint_pending.store(false, Ordering::Release);
        self.completion_wait.close();
        Ok(true)
    }

    fn close_in_policy_worker(&self) -> AxResult<bool> {
        let phase = self.state.lock().final_close.phase;
        match phase {
            FinalClosePhase::Begin => self.begin_final_close_step(),
            FinalClosePhase::Polls => self.close_polls_step(),
            FinalClosePhase::FixedFiles => self.close_fixed_files_step(),
            FinalClosePhase::Buffers => self.close_buffers_step(),
            FinalClosePhase::Completions => self.close_completions_step(),
            FinalClosePhase::Finished => Ok(true),
        }
    }

    fn pending_stream_count(&self) -> usize {
        self.state.lock().pending_stream_count
    }

    /// Installs one already-admitted pending stream owner into the fixed
    /// ring-local table. Returning the owner on failure keeps the issued
    /// proof and both leases available for an explanatory terminal CQE.
    #[allow(clippy::result_large_err)]
    fn install_pending_stream(
        &self,
        mut work: PendingStreamWork,
    ) -> Result<(), (AxError, PendingStreamWork)> {
        let mut state = self.state.lock();
        let Some(slot) = state.pending_stream.iter().position(Option::is_none) else {
            return Err((AxError::ResourceBusy, work));
        };
        if state.pending_stream_count >= IO_URING_PENDING_STREAM_CAPACITY {
            return Err((AxError::ResourceBusy, work));
        }
        work.slot = slot;
        state.pending_stream[slot] = Some(work);
        state.pending_stream_count += 1;
        Ok(())
    }

    /// Transfers an admitted fixed-buffer FIFO read into the bounded pending
    /// owner. Readiness registration is prepared before publication; any
    /// allocation/capacity/registration failure returns every exact lease so
    /// the submitter can complete the issued request with a visible errno.
    #[allow(clippy::result_large_err)]
    pub(crate) fn admit_pending_stream(
        &self,
        issued: IssuedRequest,
        file: IoUringFileLease,
        buffer: IoUringBufferLease,
        request: ReadWriteRequest,
        context: IoOperationContext,
        capability: UserMemoryCapability,
    ) -> Result<(), PendingStreamAdmissionError> {
        let owner = match self.self_weak.get().and_then(Weak::upgrade) {
            Some(owner) => owner,
            None => {
                return Err(PendingStreamAdmissionError {
                    error: AxError::BadState,
                    issued,
                    file,
                    buffer,
                    context,
                    capability,
                });
            }
        };
        let control = match PollControl::try_new(
            Arc::downgrade(&owner),
            issued.id(),
            file,
            pending_stream_events(),
        ) {
            Ok(control) => control,
            Err((error, file)) => {
                return Err(PendingStreamAdmissionError {
                    error,
                    issued,
                    file,
                    buffer,
                    context,
                    capability,
                });
            }
        };
        let ready = match control.check_arm_check() {
            Ok(ready) => ready,
            Err(error) => {
                let file = control
                    .deactivate()
                    .expect("pending stream control lost file lease");
                return Err(PendingStreamAdmissionError {
                    error,
                    issued,
                    file,
                    buffer,
                    context,
                    capability,
                });
            }
        };
        let request_id = issued.id();
        let work = PendingStreamWork {
            slot: 0,
            issued: Some(issued),
            request,
            control: Arc::clone(&control),
            buffer,
            context,
            capability,
        };
        if let Err((error, work)) = self.install_pending_stream(work) {
            let PendingStreamWork {
                issued,
                control,
                buffer,
                context,
                capability,
                ..
            } = work;
            let file = control
                .deactivate()
                .expect("pending stream capacity failure lost file lease");
            return Err(PendingStreamAdmissionError {
                error,
                issued: issued.expect("pending stream capacity failure lost issued request"),
                file,
                buffer,
                context,
                capability,
            });
        }
        if (!ready.is_empty() || control.has_source_wake())
            && let Some(ring) = self.self_weak.get().and_then(Weak::upgrade)
        {
            ring.publish_poll_hint(request_id);
        }
        Ok(())
    }

    fn take_pending_stream(&self, request: RequestId) -> Option<PendingStreamWork> {
        let mut state = self.state.lock();
        let slot = state.pending_stream.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|work| work.request_id() == request)
        })?;
        let work = state.pending_stream[slot].take();
        if work.is_some() {
            state.pending_stream_count = state.pending_stream_count.saturating_sub(1);
        }
        work
    }

    #[allow(clippy::result_large_err)]
    fn reinsert_pending_stream(&self, work: PendingStreamWork) -> Result<(), PendingStreamWork> {
        let slot = work.slot;
        let mut state = self.state.lock();
        if state.pending_stream.get(slot).is_none_or(Option::is_some) {
            return Err(work);
        }
        if state.pending_stream_count >= IO_URING_PENDING_STREAM_CAPACITY {
            return Err(work);
        }
        state.pending_stream[slot] = Some(work);
        state.pending_stream_count += 1;
        Ok(())
    }

    fn complete_pending_stream_work(self: &Arc<Self>, mut work: PendingStreamWork, result: i32) {
        let issued = work
            .issued
            .take()
            .expect("pending stream completion lost issued request");
        let lease = work.control.deactivate();
        if let Err(error) = self.complete_issued(issued, TerminalCause::Completed, result, 0) {
            error!("io_uring pending stream completion failed: {error:?}");
        }
        // The CQE publication boundary is above the exact registered-buffer
        // lease drop. Unregister can therefore detach its table while this
        // owner still pins the slot, but it cannot release the pin before the
        // terminal CQE is visible.
        drop(lease);
        drop(work);
        if self.final_close_requested.load(Ordering::Acquire) {
            self.enqueue_deferred();
        }
    }

    fn complete_pending_stream_error(self: &Arc<Self>, work: PendingStreamWork, error: AxError) {
        self.complete_pending_stream_work(work, -LinuxError::from(error).code());
    }

    /// Retries one exact pending stream owner in task context. IRQ/readiness
    /// callbacks only set the source-wake bit and enqueue this ring; all user
    /// memory and pipe consumption happens here under a fixed worker budget.
    fn retry_pending_stream(self: &Arc<Self>, control: Arc<PollControl>) {
        let request = control.request;
        let Some(work) = self.take_pending_stream(request) else {
            return;
        };
        let result = work.control.description().and_then(|description| {
            crate::syscall::io_uring_pending_read_fixed(
                &work.capability,
                &description,
                &work.buffer,
                &work.context,
            )
        });
        match result {
            Ok(result) => self.complete_pending_stream_work(work, result as i32),
            Err(AxError::WouldBlock) => {
                let ready = work.control.check_arm_check();
                match ready {
                    Ok(ready) if !ready.is_empty() || work.control.has_source_wake() => {
                        match self.reinsert_pending_stream(work) {
                            Ok(()) => self.publish_poll_hint(request),
                            Err(work) => {
                                error!("io_uring pending stream owner lost while rearming");
                                self.complete_pending_stream_error(work, AxError::BadState);
                            }
                        }
                    }
                    Ok(_) => {
                        if let Err(work) = self.reinsert_pending_stream(work) {
                            self.complete_pending_stream_error(work, AxError::BadState);
                        }
                    }
                    Err(error) => self.complete_pending_stream_error(work, error),
                }
            }
            Err(error) => self.complete_pending_stream_error(work, error),
        }
    }

    fn commit_poll_admission(
        &self,
        mut admission: SubmissionAdmission<'_>,
        lease: IoUringFileLease,
        linux_events: u32,
        capability: UserMemoryCapability,
    ) -> AxResult<()> {
        let id = admission
            .reservation
            .as_ref()
            .ok_or(AxError::BadState)?
            .id();
        let ring = self.self_weak.get().ok_or(AxError::BadState)?.clone();
        let events = poll_events_from_linux(linux_events);
        let control = match PollControl::try_new(ring, id, lease, events) {
            Ok(control) => control,
            Err((error, lease)) => {
                drop(lease);
                let work = admission.commit(None, None, None, None, None, capability.clone())?;
                let (prepared, ..) = work.into_parts();
                return self.complete_request(
                    prepared.id(),
                    TerminalCause::PreparationFailed,
                    -LinuxError::from(error).code(),
                    0,
                );
            }
        };
        let ready = match control.check_arm_check() {
            Ok(ready) => ready,
            Err(error) => {
                drop(control.deactivate());
                let work = admission.commit(None, None, None, None, None, capability)?;
                let (prepared, ..) = work.into_parts();
                return self.complete_request(
                    prepared.id(),
                    TerminalCause::PreparationFailed,
                    -LinuxError::from(error).code(),
                    0,
                );
            }
        };
        {
            let _submission = self.submission_serial.lock();
            let mut state = self.state.lock();
            if !state.admission_in_progress {
                drop(state);
                drop(control.deactivate());
                return Err(AxError::BadState);
            }
            let reservation = admission.reservation.take().ok_or(AxError::BadState)?;
            let prepared = state.requests.commit(reservation).map_err(map_core_error)?;
            let slot = usize::try_from(id.slot()).map_err(|_| AxError::BadState)?;
            if state.polls.get(slot).ok_or(AxError::BadState)?.is_some() {
                drop(state);
                drop(control.deactivate());
                return Err(AxError::BadState);
            }
            state.polls[slot] = Some(Arc::clone(&control));
            if let Err(error) = state.requests.issue(prepared) {
                state.polls[slot] = None;
                state.admission_in_progress = false;
                drop(state);
                drop(control.deactivate());
                return Err(map_core_error(error.error()));
            }
            state.sq_head = state.sq_head.wrapping_add(1);
            state.admission_in_progress = false;
            self.sq_head.store_release(state.sq_head);
        }
        if !ready.is_empty() {
            self.finish_poll_control(
                &control,
                TerminalCause::Completed,
                poll_events_to_linux(ready) as i32,
            )?;
        }
        if control.has_source_wake()
            && let Some(ring) = self.self_weak.get().and_then(Weak::upgrade)
        {
            ring.publish_poll_hint(id);
        }
        Ok(())
    }

    fn finish_poll_control(
        &self,
        expected: &Arc<PollControl>,
        cause: TerminalCause,
        result: i32,
    ) -> AxResult<bool> {
        let (id, control) = {
            let mut state = self.state.lock();
            let id = expected.request;
            let slot = usize::try_from(id.slot()).map_err(|_| AxError::BadState)?;
            let current = state.polls.get(slot).and_then(Option::as_ref);
            if current.is_none_or(|control| !Arc::ptr_eq(control, expected)) {
                return Ok(false);
            }
            let permit = match state.requests.claim_terminal(id, cause) {
                Ok(permit) => permit,
                Err(
                    IoUringError::TerminalAlreadyClaimed
                    | IoUringError::UnknownRequest
                    | IoUringError::RequestUncancellable,
                ) => return Ok(false),
                Err(error) => return Err(map_core_error(error)),
            };
            let token = state
                .requests
                .finish_terminal(permit, result, 0)
                .map_err(map_core_error)?;
            self.queue_completion_locked(&mut state, token)?;
            let control = state.polls[slot].take().ok_or(AxError::BadState)?;
            (id, control)
        };
        let lease = control.deactivate();
        let publication = self.publish_pending_slot(id.slot() as usize);
        drop(lease);
        drop(control);
        publication?;
        Ok(true)
    }

    pub(crate) fn cancel_request(
        &self,
        cancel: IssuedRequest,
        target_user_data: u64,
    ) -> AxResult<()> {
        let cancel_id = cancel.id();
        let (target_id, control) = {
            let mut state = self.state.lock();
            match state
                .requests
                .claim_cancel(CancelSelector::UserData(target_user_data), Some(cancel_id))
            {
                Ok(target) => {
                    let target_id = target.id();
                    let target_token = state
                        .requests
                        .finish_terminal(target, -LinuxError::ECANCELED.code(), 0)
                        .map_err(map_core_error)?;
                    self.queue_completion_locked(&mut state, target_token)?;
                    let cancel_permit = state
                        .requests
                        .claim_terminal(cancel_id, TerminalCause::Completed)
                        .map_err(map_core_error)?;
                    let cancel_token = state
                        .requests
                        .finish_terminal(cancel_permit, 0, 0)
                        .map_err(map_core_error)?;
                    self.queue_completion_locked(&mut state, cancel_token)?;
                    let control = usize::try_from(target_id.slot())
                        .ok()
                        .and_then(|slot| state.polls.get_mut(slot))
                        .and_then(Option::take);
                    (Some(target_id), control)
                }
                Err(IoUringError::CancellationTargetNotFound) => {
                    let cancel_permit = state
                        .requests
                        .claim_terminal(cancel_id, TerminalCause::Completed)
                        .map_err(map_core_error)?;
                    let cancel_token = state
                        .requests
                        .finish_terminal(cancel_permit, -LinuxError::ENOENT.code(), 0)
                        .map_err(map_core_error)?;
                    self.queue_completion_locked(&mut state, cancel_token)?;
                    (None, None)
                }
                Err(error) => return Err(map_core_error(error)),
            }
        };
        let lease = control.as_ref().and_then(|control| control.deactivate());
        let target_result = match target_id {
            Some(id) => self.publish_pending_slot(id.slot() as usize).map(|_| ()),
            None => Ok(()),
        };
        let cancel_result = self
            .publish_pending_slot(cancel_id.slot() as usize)
            .map(|_| ());
        drop(lease);
        drop(control);
        target_result.and(cancel_result)
    }

    fn publish_poll_hint(self: &Arc<Self>, request: RequestId) {
        let slot = request.slot() as usize;
        let bits = usize::BITS as usize;
        let Some(word) = self.poll_hint_bits.get(slot / bits) else {
            return;
        };
        word.fetch_or(1_usize << (slot % bits), Ordering::Release);
        self.poll_hint_pending.store(true, Ordering::Release);
        self.enqueue_deferred();
    }

    fn enqueue_deferred(self: &Arc<Self>) {
        if self
            .deferred_queued
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let node = Arc::into_raw(Arc::clone(self)).cast_mut();
        let mut head = DEFERRED_IO_URING_WORK.load(Ordering::Acquire);
        loop {
            self.deferred_next.store(head, Ordering::Relaxed);
            match DEFERRED_IO_URING_WORK.compare_exchange_weak(
                head,
                node,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn drain_poll_hints(self: &Arc<Self>) {
        if !self.poll_hint_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        let word_bits = usize::BITS as usize;
        let mut pending_budget = IO_URING_PENDING_STREAM_BUDGET;
        for (word_index, word) in self.poll_hint_bits.iter().enumerate() {
            let mut hinted = word.swap(0, Ordering::AcqRel);
            while hinted != 0 {
                let bit = hinted.trailing_zeros() as usize;
                hinted &= hinted - 1;
                let slot = word_index * word_bits + bit;
                let (control, pending) = {
                    let state = self.state.lock();
                    if let Some(control) = state
                        .polls
                        .get(slot)
                        .and_then(|control| control.as_ref().map(Arc::clone))
                    {
                        (Some(control), false)
                    } else {
                        let pending = state.pending_stream.iter().find_map(|entry| {
                            entry
                                .as_ref()
                                .filter(|work| work.request_id().slot() as usize == slot)
                                .map(|work| Arc::clone(&work.control))
                        });
                        (pending, true)
                    }
                };
                let Some(control) = control else {
                    continue;
                };
                if pending {
                    if pending_budget == 0 {
                        if let Some(word) = self.poll_hint_bits.get(slot / word_bits) {
                            word.fetch_or(1_usize << (slot % word_bits), Ordering::Release);
                        }
                        self.poll_hint_pending.store(true, Ordering::Release);
                        continue;
                    }
                    pending_budget -= 1;
                    // A pending hint is an explicit retry request (including
                    // the initial ready-after-arm race), so it does not need
                    // the poll operation's source-wake gate. Clear the stale
                    // source bit before the one-shot attempt.
                    control.take_source_wake();
                    control.registration_fired();
                    self.retry_pending_stream(control);
                    continue;
                }
                if !control.take_source_wake() {
                    continue;
                }
                control.registration_fired();
                let ready = control.check_arm_check();
                match ready {
                    Ok(ready) if ready.is_empty() => {}
                    Ok(ready) => {
                        let result = poll_events_to_linux(ready) as i32;
                        if let Err(error) =
                            self.finish_poll_control(&control, TerminalCause::Completed, result)
                        {
                            error!("io_uring poll completion failed: {error:?}");
                        }
                    }
                    Err(error) => {
                        let result = -LinuxError::from(error).code();
                        if let Err(error) = self.finish_poll_control(
                            &control,
                            TerminalCause::PreparationFailed,
                            result,
                        ) {
                            error!("io_uring poll failure completion failed: {error:?}");
                        }
                    }
                }
            }
        }
    }
}

impl FileLike for IoUring {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[io_uring]".into())
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        match self
            .layout
            .mapping_region(request.offset())
            .map_err(map_core_error)?
        {
            MappingRegion::Rings
                if request.offset() == thekernel_linux_io_uring::IORING_OFF_CQ_RING =>
            {
                self.cq_ring_region.prepare(request)
            }
            MappingRegion::Rings => self.ring_region.prepare(request),
            MappingRegion::SubmissionEntries => self.sqe_region.prepare(request),
        }
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
        Ok(())
    }
}

impl Pollable for IoUring {
    fn poll(&self) -> IoEvents {
        if self.has_completions() {
            IoEvents::READABLE
        } else {
            IoEvents::empty()
        }
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            PollRegistration::single(&self.completion_wait, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}

pub(crate) fn has_deferred_io_uring_work() -> bool {
    !DEFERRED_IO_URING_WORK.load(Ordering::Acquire).is_null()
}

pub(crate) fn drain_deferred_io_uring_work() {
    let mut node = DEFERRED_IO_URING_WORK.swap(ptr::null_mut(), Ordering::AcqRel);
    while !node.is_null() {
        // SAFETY: enqueue_deferred transferred exactly one strong reference
        // for this intrusive publication, and deferred_queued prevents the
        // embedded node from appearing twice before this reference is taken.
        let ring = unsafe { Arc::from_raw(node) };
        let next = ring.deferred_next.swap(ptr::null_mut(), Ordering::AcqRel);
        ring.deferred_queued.store(false, Ordering::Release);
        ring.drain_poll_hints();
        let final_close_pending = if ring.final_close_requested.load(Ordering::Acquire) {
            match ring.close_in_policy_worker() {
                Ok(true) => {
                    ring.final_close_requested.store(false, Ordering::Release);
                    false
                }
                Ok(false) => true,
                Err(error) => {
                    error!("io_uring final close will retry after failure: {error:?}");
                    true
                }
            }
        } else {
            false
        };
        // An active physical reservation/Work owner can block an earlier
        // close phase (fixed-file or buffer retirement) before
        // `close_completions_step` gets a chance to set its parked bit. Do
        // not requeue that close in a polling loop; the last lease drop wakes
        // this deferred node from task context.
        if (final_close_pending
            && ring.physical_worker_len() == 0
            && !ring.close_waiting_on_physical()
            && ring.pending_stream_count() == 0)
            || ring.poll_hint_pending.load(Ordering::Acquire)
        {
            ring.enqueue_deferred();
        }
        node = next;
    }
    // All rings published before their deferred nodes were drained have now
    // been admitted. Wake the dedicated device-global owner only after this
    // bounded policy pass; it never waits from an individual ring, and this
    // worker must remain free for fanotify/inotify/RCU/final-close policy.
    wake_physical_completion_worker();
}

/// Runs one bounded task-context pass for the device-global physical
/// completion owner.  The caller-provided `wait` closure is the sole bridge
/// to the block driver's `wait_any_physical_completion`; no ring may wait on
/// the shared device queue independently because a completion for another
/// ring is a valid result of that wait.  A continuation is returned to the
/// scheduler immediately and is never synthesized by polling or by waiting
/// for another interrupt.
pub(crate) fn run_physical_completion_pass(
    output: &mut [PhysicalIoCompletion],
    budget: usize,
    wait: impl FnOnce(&mut [PhysicalIoCompletion]) -> AxResult<(usize, bool)>,
) -> AxResult<PhysicalIoCompletionPass> {
    run_physical_completion_pass_for_device(
        physical_completion_default_identity(),
        output,
        budget,
        wait,
    )
}

pub(crate) fn run_physical_completion_pass_for_device(
    device_identity: usize,
    output: &mut [PhysicalIoCompletion],
    budget: usize,
    wait: impl FnOnce(&mut [PhysicalIoCompletion]) -> AxResult<(usize, bool)>,
) -> AxResult<PhysicalIoCompletionPass> {
    if physical_completion_route_count_for_device(device_identity) == 0
        || budget == 0
        || output.is_empty()
    {
        return Ok(PhysicalIoCompletionPass {
            drained: 0,
            continuation: false,
        });
    }
    let output_len = output.len().min(budget);
    let output = &mut output[..output_len];
    // A used-ring completion can race the submitter between vendor
    // publication and the atomic Reserved -> Owner commit. Replay those
    // bounded records in this task context before entering the lower wait;
    // this keeps the device-global owner from losing a completion to an
    // intentionally unpublished route.
    let replayed = take_replayable_physical_completions_for_device(device_identity, output);
    if replayed != 0 {
        for completion in output.iter().copied().take(replayed) {
            route_physical_completion_for_device(device_identity, completion)?;
        }
        return Ok(PhysicalIoCompletionPass {
            drained: replayed,
            continuation: physical_completion_route_count_for_device(device_identity) != 0,
        });
    }
    if physical_completion_has_quarantined_route_for_device(device_identity) {
        return Err(AxError::BadState);
    }
    let (drained, continuation) = wait(output)?;
    if drained > output.len() {
        return Err(AxError::BadState);
    }
    for completion in output.iter().copied().take(drained) {
        route_physical_completion_for_device(device_identity, completion)?;
    }
    Ok(PhysicalIoCompletionPass {
        drained,
        continuation,
    })
}

fn initialize_ring_header(pages: &SharedPages, layout: RingLayout) -> AxResult<()> {
    let sq = layout.sq_offsets();
    let cq = layout.cq_offsets();
    for (offset, value) in [
        (sq.head(), 0),
        (sq.tail(), 0),
        (sq.ring_mask(), layout.sq_mask()),
        (sq.ring_entries(), layout.sq_entries()),
        (sq.flags(), 0),
        (sq.dropped(), 0),
        (cq.head(), 0),
        (cq.tail(), 0),
        (cq.ring_mask(), layout.cq_mask()),
        (cq.ring_entries(), layout.cq_entries()),
        (cq.overflow(), 0),
        (cq.flags(), 0),
    ] {
        pages.write_bytes(offset as usize, &value.to_ne_bytes())?;
    }
    if let Some(array) = sq.array() {
        for index in 0..layout.sq_entries() {
            let offset = array
                .checked_add(index.checked_mul(4).ok_or(AxError::InvalidInput)?)
                .ok_or(AxError::InvalidInput)?;
            pages.write_bytes(offset as usize, &index.to_ne_bytes())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod adapter_state_tests {
    use alloc::vec;

    use thekernel_linux_io_uring::{
        FeatureFlags, RequestDescriptor, RequestOperation, SetupFlags, SetupRequest,
    };

    use super::*;

    // The completion router is intentionally device-global. Serialize tests
    // that install synthetic routes so parallel unit tests cannot make one
    // test observe another test's bounded QD custody.
    static PHYSICAL_COMPLETION_TEST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

    #[test]
    fn successful_physical_write_reports_logical_bytes_to_cqe() {
        assert_eq!(physical_io_completion_result(Ok(4096)), 4096);
    }

    #[test]
    fn physical_progress_snapshot_does_not_clear_new_callback_edge() {
        let mut progress = PhysicalCompletionProgressState::default();
        advance_physical_completion_progress(&mut progress).unwrap();
        let observed = Some(progress.generation);

        // A lower IRQ arrives after the worker snapshot but before its clear.
        advance_physical_completion_progress(&mut progress).unwrap();
        clear_physical_completion_progress_if_unchanged(&mut progress, observed);
        assert!(progress.pending);
        assert_eq!(progress.generation, 2);
        assert!(!progress.overflowed);

        let current_generation = progress.generation;
        clear_physical_completion_progress_if_unchanged(&mut progress, Some(current_generation));
        assert!(!progress.pending);
    }

    #[test]
    fn physical_progress_upper_commit_republishes_after_early_edge_clear() {
        let mut progress = PhysicalCompletionProgressState::default();

        // Model a lower completion that was consumed by an early worker pass
        // before the upper route/work publication became visible.
        advance_physical_completion_progress(&mut progress).unwrap();
        let early_generation = progress.generation;
        clear_physical_completion_progress_if_unchanged(&mut progress, Some(early_generation));
        assert!(!progress.pending);

        // PhysicalIoWorkerReservation::commit uses the same state transition
        // after publishing both owners, so the worker predicate is live even
        // though the transport generation never changed.
        advance_physical_completion_progress(&mut progress).unwrap();
        assert!(progress.pending);
        assert_eq!(progress.generation, early_generation + 1);
    }

    #[test]
    fn physical_progress_wrong_identity_cannot_pollute_sibling_state() {
        let mut device_a = PhysicalCompletionProgressState::default();
        let device_b = PhysicalCompletionProgressState::default();

        advance_physical_completion_progress(&mut device_a).unwrap();
        assert_eq!(device_a.generation, 1);
        assert!(device_a.pending);
        assert_eq!(device_b, PhysicalCompletionProgressState::default());
    }

    #[test]
    fn physical_progress_overflow_is_explicit_and_non_wrapping() {
        let mut progress = PhysicalCompletionProgressState {
            generation: u64::MAX,
            ..PhysicalCompletionProgressState::default()
        };
        assert_eq!(
            advance_physical_completion_progress(&mut progress),
            Err(AxError::BadState)
        );
        assert!(progress.pending);
        assert_eq!(progress.generation, u64::MAX);
        assert!(progress.overflowed);
        clear_physical_completion_progress_if_unchanged(&mut progress, Some(u64::MAX));
        assert!(progress.pending);

        // Once fenced, a repeated callback cannot re-arm a reset/wake edge
        // or change the generation.  Removal/reinstall is the only route to
        // a fresh sequence namespace.
        assert_eq!(
            advance_physical_completion_progress(&mut progress),
            Err(AxError::BadState)
        );
        assert_eq!(progress.generation, u64::MAX);
        assert!(progress.overflowed && progress.pending);
    }

    #[test]
    fn physical_terminal_sequence_overflow_is_checked_and_stable() {
        assert_eq!(
            advance_physical_completion_terminal_sequence(u64::MAX),
            Err(AxError::BadState)
        );
        assert_eq!(
            advance_physical_completion_terminal_sequence(u64::MAX - 1),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn physical_callback_context_is_never_reused_for_incarnations() {
        let first = allocate_physical_completion_callback_context().unwrap();
        let second = allocate_physical_completion_callback_context().unwrap();
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_ne!(second, 0);
    }

    struct FixedFileTestObject;

    impl Pollable for FixedFileTestObject {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<PollRegistration<'a>, PollRegistrationError> {
            PollRegistration::empty()
        }
    }

    impl FileLike for FixedFileTestObject {
        fn stat(&self) -> AxResult<Kstat> {
            Ok(Kstat::default())
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("io-uring-fixed-file-test"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    struct DropProbe {
        name: &'static str,
        order: Arc<SpinNoIrq<Vec<&'static str>>>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.order.lock().push(self.name);
        }
    }

    fn reserve_test_request_id(ring: &IoUring) -> RequestId {
        let mut state = ring.state.lock();
        let reservation = state
            .requests
            .reserve(RequestDescriptor::new(0, RequestOperation::Read))
            .unwrap();
        let id = reservation.id();
        state.requests.rollback(reservation).unwrap();
        id
    }

    fn issue_test_request(ring: &IoUring) -> IssuedRequest {
        let mut state = ring.state.lock();
        let reservation = state
            .requests
            .reserve(RequestDescriptor::new(0, RequestOperation::Read))
            .unwrap();
        let prepared = state.requests.commit(reservation).unwrap();
        state.requests.issue(prepared).unwrap()
    }

    #[test]
    fn request_and_completion_capacity_are_retryable_backpressure() {
        assert!(reservation_is_backpressure(
            IoUringError::CompletionQueueFull
        ));
        assert!(reservation_is_backpressure(
            IoUringError::RequestCapacityExceeded
        ));
        assert!(!reservation_is_backpressure(
            IoUringError::InvalidRequestState
        ));
    }

    #[test]
    fn pending_only_backpressure_requires_delayed_worker_retry() {
        // A completion pass drained no lower record and the bounded publish
        // retry observed Backpressure. With no published route left, no real
        // completion can provide another wake edge; clearing work here would
        // strand the issued PendingPublication owner indefinitely.
        assert_eq!(
            physical_publication_retry_disposition(false, true, false),
            PhysicalPublicationRetryDisposition::PendingOnly
        );

        // A published sibling owns a future completion edge, while a
        // successful republish needs an immediate bounded follow-up pass.
        assert_eq!(
            physical_publication_retry_disposition(false, true, true),
            PhysicalPublicationRetryDisposition::WaitingForCompletion
        );
        assert_eq!(
            physical_publication_retry_disposition(true, true, true),
            PhysicalPublicationRetryDisposition::Republished
        );
    }

    #[test]
    fn final_close_cursor_limits_each_slot_batch() {
        let mut progress = FinalCloseProgress::new();
        progress.enter(FinalClosePhase::Polls);
        assert_eq!(progress.take_slots(130), 0..FINAL_CLOSE_STEP_BUDGET);
        assert_eq!(
            progress.take_slots(130),
            FINAL_CLOSE_STEP_BUDGET..FINAL_CLOSE_STEP_BUDGET * 2
        );
        assert_eq!(progress.take_slots(130), 128..130);
        assert_eq!(progress.take_slots(130), 130..130);

        progress.enter(FinalClosePhase::Completions);
        assert_eq!(progress.take_slots(1), 0..1);
    }

    #[test]
    fn physical_reset_terminal_order_retires_before_cqe() {
        let order = Arc::new(SpinNoIrq::new(Vec::new()));
        run_physical_reset_terminal_order(
            || order.lock().push("retire"),
            || order.lock().push("release-work"),
            || order.lock().push("publish-cqe"),
        );
        assert_eq!(&*order.lock(), &["retire", "release-work", "publish-cqe"]);
    }

    #[test]
    fn physical_finalization_round_robin_advances_ring_and_slot() {
        let mut pending = [[false; IO_URING_PHYSICAL_MAX_QD]; IO_URING_PHYSICAL_MAX_QD];
        pending[0][0] = true;
        pending[0][1] = true;
        pending[1][0] = true;
        pending[1][1] = true;
        let mut ring_cursor = 0;
        let mut slot_cursor = 0;
        let mut selected = [(0, 0); PHYSICAL_FINALIZATION_RETRY_BUDGET];

        let selected_len = select_physical_finalization_round_robin(
            2,
            &mut ring_cursor,
            &mut slot_cursor,
            &mut selected,
            |ring, slot| pending[ring][slot],
        );
        assert_eq!(selected_len, PHYSICAL_FINALIZATION_RETRY_BUDGET);
        assert_eq!(&selected[..selected_len], &[(0, 0), (1, 1), (0, 1)]);

        let selected_len = select_physical_finalization_round_robin(
            2,
            &mut ring_cursor,
            &mut slot_cursor,
            &mut selected,
            |ring, slot| pending[ring][slot],
        );
        assert_eq!(selected_len, PHYSICAL_FINALIZATION_RETRY_BUDGET);
        assert_eq!(&selected[..selected_len], &[(1, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn physical_finalization_sleep_error_is_bounded_fail_stop() {
        assert_eq!(
            physical_finalization_sleep_error_action(),
            PhysicalFinalizationSleepErrorAction::FailStop
        );
    }

    #[test]
    fn registered_buffer_pin_budget_checks_cover_and_refunds() {
        let budget = Arc::new(RegisteredBufferPinBudget::new());
        assert!(
            budget
                .try_charge(IO_URING_RING_REGISTERED_BUFFER_PAGES + 1)
                .is_err()
        );
        assert!(matches!(
            budget.try_charge(usize::MAX / PAGE_BYTES + 1),
            Err(AxError::NoMemory)
        ));

        let charge = budget.try_charge(2).expect("small pin charge");
        assert_eq!(budget.pages.load(Ordering::Acquire), 2);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 2 * PAGE_BYTES);
        drop(charge);
        assert_eq!(budget.pages.load(Ordering::Acquire), 0);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn physical_segment_index_uses_prefix_boundaries() {
        let ends = [4, 12, 20];
        assert_eq!(locate_physical_segment(&ends, 0).unwrap(), (0, 0));
        assert_eq!(locate_physical_segment(&ends, 3).unwrap(), (0, 3));
        assert_eq!(locate_physical_segment(&ends, 4).unwrap(), (1, 0));
        assert_eq!(locate_physical_segment(&ends, 11).unwrap(), (1, 7));
        assert_eq!(locate_physical_segment(&ends, 12).unwrap(), (2, 0));
        assert_eq!(locate_physical_segment(&ends, 20).unwrap(), (3, 0));
    }

    #[test]
    fn physical_byte_stream_equivalence_accepts_exact_and_resegmented_sg() {
        fn equivalent(upper: &[PhysicalIoSegment], lower: &[PhysicalIoSegment]) -> AxResult<()> {
            physical_byte_streams_equivalent(
                upper,
                lower,
                |segment| (segment.paddr, segment.len),
                |segment| (segment.paddr, segment.len),
            )
        }

        let exact = [
            PhysicalIoSegment::new(0x1000, 0x1000),
            PhysicalIoSegment::new(0x4000, 0x1000),
        ];
        assert!(equivalent(&exact, &exact).is_ok());

        // A lower extent boundary may split one registered-buffer segment.
        let registered = [PhysicalIoSegment::new(0x8000, 0x4000)];
        let extent_slices = [
            PhysicalIoSegment::new(0x8000, 0x1000),
            PhysicalIoSegment::new(0x9000, 0x3000),
        ];
        assert!(equivalent(&registered, &extent_slices).is_ok());

        // Both sides may have independent boundaries, including a physical
        // discontinuity that is represented by matching SG entries on each
        // side.
        let upper = [
            PhysicalIoSegment::new(0x1000, 3),
            PhysicalIoSegment::new(0x4000, 4),
        ];
        let lower = [
            PhysicalIoSegment::new(0x1000, 1),
            PhysicalIoSegment::new(0x1001, 2),
            PhysicalIoSegment::new(0x4000, 2),
            PhysicalIoSegment::new(0x4002, 2),
        ];
        assert!(equivalent(&upper, &lower).is_ok());
    }

    #[test]
    fn physical_byte_stream_equivalence_rejects_truncated_gap_reorder_zero_overflow() {
        fn equivalent(upper: &[PhysicalIoSegment], lower: &[PhysicalIoSegment]) -> AxResult<()> {
            physical_byte_streams_equivalent(
                upper,
                lower,
                |segment| (segment.paddr, segment.len),
                |segment| (segment.paddr, segment.len),
            )
        }

        let valid = [PhysicalIoSegment::new(0x1000, 4)];

        // The lower stream is truncated even though its prefix matches.
        assert!(equivalent(&valid, &[PhysicalIoSegment::new(0x1000, 3)]).is_err());
        // The second descriptor starts after a missing physical byte.
        assert!(
            equivalent(
                &valid,
                &[
                    PhysicalIoSegment::new(0x1000, 2),
                    PhysicalIoSegment::new(0x1003, 2),
                ],
            )
            .is_err()
        );
        // Reordering physically distinct ranges cannot be hidden by a new
        // descriptor boundary.
        let ordered = [
            PhysicalIoSegment::new(0x1000, 2),
            PhysicalIoSegment::new(0x2000, 2),
        ];
        let reordered = [
            PhysicalIoSegment::new(0x2000, 2),
            PhysicalIoSegment::new(0x1000, 2),
        ];
        assert!(equivalent(&ordered, &reordered).is_err());
        // Empty descriptors are never valid stream elements.
        assert!(
            equivalent(
                &[
                    PhysicalIoSegment::new(0x1000, 0),
                    PhysicalIoSegment::new(0x1000, 4)
                ],
                &valid,
            )
            .is_err()
        );
        // The physical end must be representable on every descriptor.
        assert!(
            equivalent(
                &[PhysicalIoSegment::new(usize::MAX, 1)],
                &[PhysicalIoSegment::new(usize::MAX, 1)],
            )
            .is_err()
        );
    }

    #[test]
    fn prepared_physical_sg_is_derived_from_the_registered_range() {
        let segments = [
            UserIoPinSegment {
                paddr: 0x1000,
                len: 0x1000,
            },
            UserIoPinSegment {
                paddr: 0x2000,
                len: 0x1000,
            },
            UserIoPinSegment {
                paddr: 0x5000,
                len: 0x1000,
            },
        ];
        let mut physical = [PhysicalIoSegment::new(0, 0); IO_URING_PHYSICAL_MAX_SEGMENTS];
        let count =
            clip_registered_physical_segments(&segments, 0x400, 0x2c00, &mut physical).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            &physical[..count],
            &[
                PhysicalIoSegment::new(0x1400, 0x1c00),
                PhysicalIoSegment::new(0x5000, 0x1000),
            ]
        );
    }

    #[test]
    fn fabricated_prepared_plan_segment_count_is_rejected() {
        let plan = PreparedPhysicalIoPlan::new(
            PreparedPhysicalIoOperation::Read,
            0,
            0x1000,
            0x1000,
            0x1000,
            [PhysicalIoSegment::new(0x2000, 0x1000); IO_URING_PHYSICAL_MAX_SEGMENTS],
            5,
        );
        assert!(matches!(plan.physical_segments(), Err(AxError::BadState)));
    }

    #[test]
    fn registered_buffer_lower_pin_drops_before_admission_charge() {
        let order = Arc::new(SpinNoIrq::new(Vec::new()));
        drop(PinBeforeCharge::new(
            DropProbe {
                name: "pin",
                order: Arc::clone(&order),
            },
            DropProbe {
                name: "charge",
                order: Arc::clone(&order),
            },
        ));
        assert_eq!(&*order.lock(), &["pin", "charge"]);
    }

    #[test]
    fn unregister_drain_tolerates_last_lease_taking_the_table() {
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let description = FileDescription::new(Arc::new(FixedFileTestObject)).unwrap();
        ring.register_files(vec![Some(description)]).unwrap();
        let lease = ring.acquire_registered_file(FileSlot::new(0)).unwrap();

        {
            let mut state = ring.state.lock();
            state
                .fixed_files
                .as_mut()
                .unwrap()
                .table
                .begin_retire()
                .unwrap();
        }
        drop(lease);

        assert!(ring.state.lock().fixed_files.is_none());
        ring.drain_registered_files_after_retire().unwrap();
    }

    #[test]
    fn physical_worker_reservation_is_fixed_capacity_and_reversible() {
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();

        let mut reservations = Vec::new();
        for _ in 0..IO_URING_PHYSICAL_MAX_QD {
            reservations.push(ring.reserve_physical_worker_slot().unwrap());
        }
        assert_eq!(ring.physical_worker_len(), IO_URING_PHYSICAL_MAX_QD);
        assert!(matches!(
            ring.reserve_physical_worker_slot(),
            Err(AxError::ResourceBusy)
        ));

        drop(reservations.pop());
        assert_eq!(ring.physical_worker_len(), IO_URING_PHYSICAL_MAX_QD - 1);
        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_eq!(ring.physical_worker_len(), IO_URING_PHYSICAL_MAX_QD);
        drop(reservation);
        drop(reservations);
        assert_eq!(ring.physical_worker_len(), 0);
    }

    #[test]
    fn physical_reserved_slot_drop_wakes_parked_close() {
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        ring.close_waiting_on_physical
            .store(true, Ordering::Release);
        ring.final_close_requested.store(true, Ordering::Release);
        ring.state.lock().final_close.phase = FinalClosePhase::Completions;

        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_eq!(ring.physical_worker_len(), 1);
        drop(reservation);

        assert_eq!(ring.physical_worker_len(), 0);
        assert!(!ring.close_waiting_on_physical());
        // Consume the one deferred wake published by the lease drop so the
        // global intrusive queue cannot leak this test's ring into a later
        // adapter-state test.
        drain_deferred_io_uring_work();
    }

    #[test]
    fn extracted_physical_slot_is_fenced_until_reinsert_or_drop() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let slot = 0;
        {
            let mut state = ring.state.lock();
            state.physical_work[slot] = Some(PhysicalIoWork {
                ring: Arc::clone(&ring),
                slot,
                issued: None,
                admission: None,
                pending_publication: false,
                test_handle: Some(0xFECE),
            });
            state.physical_work_count = 1;
        }

        let work = ring
            .take_physical_worker_work_for_handle(0xFECE)
            .expect("test work must be extracted");
        assert!(ring.state.lock().physical_slot_reserved[slot]);

        // A concurrent submitter may charge another free slot, but it cannot
        // reuse the extracted slot while the owner is between take/retain.
        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_ne!(reservation.slot(), slot);
        drop(reservation);
        assert_eq!(ring.physical_worker_len(), 1);

        ring.retain_physical_worker_work(work).unwrap();
        assert!(!ring.state.lock().physical_slot_reserved[slot]);
        assert_eq!(ring.physical_worker_len(), 1);

        let work = ring
            .take_physical_worker_work_for_handle(0xFECE)
            .expect("retained work must be extractable");
        assert!(ring.state.lock().physical_slot_reserved[slot]);
        drop(work);
        assert_eq!(ring.physical_worker_len(), 0);
        assert!(!ring.state.lock().physical_slot_reserved[slot]);
    }

    #[test]
    fn physical_admission_gate_rejects_stop_and_worker_loss() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);

        // Model an installed/live production owner without requiring a real
        // block device.  The ring reservation must hold the lifecycle count
        // until its route/publication hand-off is complete.
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = true;
            state.in_flight = 0;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);

        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_eq!(
            stop_physical_completion_device(),
            Err(AxError::ResourceBusy)
        );
        assert!(matches!(
            ring.reserve_physical_worker_slot(),
            Err(AxError::BadState)
        ));

        // A worker failure closes the same generation gate.  The already
        // reserved operation is still prepublication, so it may fall back;
        // no caller can invoke the publish closure after the worker has gone.
        note_physical_completion_worker_stopped();
        assert_eq!(
            reservation.with_physical_publish(|| ()),
            Err(AxError::BadState)
        );
        assert!(matches!(
            ring.reserve_physical_worker_slot(),
            Err(AxError::BadState)
        ));
        drop(reservation);

        // Once the in-flight reservation is gone, teardown may close the
        // device. Restore globals so this state-machine test is isolated from
        // the other adapter tests.
        assert!(stop_physical_completion_device().is_ok());
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn reset_busy_with_inflight_admission_keeps_reset_custody_pending() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = true;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);

        // No lower device is installed in this pure lifecycle test. The
        // in-flight admission must still win the reset gate before the
        // device lookup, preserving the later publication/route commit.
        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_eq!(
            reset_physical_completion_device(),
            Err(AxError::ResourceBusy)
        );
        assert!(!PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert!(!PHYSICAL_COMPLETION_ADMISSION_STATE.lock().open);
        assert_eq!(
            PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire),
            generation,
            "busy reset must not fence the generation before the guard commits"
        );

        drop(reservation);
        assert_eq!(
            PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire),
            generation,
            "busy reset must leave the generation unchanged until lower reset starts"
        );
        assert!(PHYSICAL_COMPLETION_RESET_PENDING.load(Ordering::Acquire));

        let _ = stop_physical_completion_device();
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn external_terminal_notification_fences_and_reopens_upper_generation() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);
        let route = PhysicalCompletionRouteReservation::new(1).unwrap();
        route.activate_test(&ring, request, 0, 0xE17E);
        {
            let mut state = ring.state.lock();
            state.physical_work[0] = Some(PhysicalIoWork {
                ring: Arc::clone(&ring),
                slot: 0,
                issued: None,
                admission: None,
                pending_publication: false,
                test_handle: Some(0xE17E),
            });
            state.physical_work_count = 1;
        }
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = true;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert_eq!(ring.physical_worker_len(), 1);

        let terminal_generation = generation + 7;
        physical_completion_terminal_notifier(
            physical_completion_terminal_context(),
            BlockCompletionAvailability::Live {
                generation: terminal_generation,
            },
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire),
            PHYSICAL_COMPLETION_TERMINAL_QUIESCED
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
            terminal_generation
        );
        assert!(!PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert_eq!(ring.physical_worker_len(), 1);

        // The notifier itself never retires upper custody. The worker-side
        // terminal pass consumes the lower proof, fences generation, and then
        // makes the reusable queue live again.
        service_physical_completion_reset().unwrap();
        assert_eq!(
            PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire),
            terminal_generation
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire),
            PHYSICAL_COMPLETION_TERMINAL_NONE
        );
        assert!(!PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_ADMISSION_STATE.lock().open);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
        assert_eq!(ring.physical_worker_len(), 0);

        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn stale_terminal_event_cannot_consume_new_generation() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        clear_physical_completion_terminal_event();
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);

        physical_completion_terminal_notifier(
            physical_completion_terminal_context(),
            BlockCompletionAvailability::Live {
                generation: generation + 1,
            },
        );
        let first = physical_completion_terminal_event().expect("first terminal event");
        physical_completion_terminal_notifier(
            physical_completion_terminal_context(),
            BlockCompletionAvailability::Live {
                generation: generation + 2,
            },
        );

        assert_eq!(
            finish_physical_completion_external_terminal_event(first, BlockResetOutcome::Quiesced,),
            Err(AxError::ResourceBusy)
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire),
            PHYSICAL_COMPLETION_TERMINAL_QUIESCED
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
            generation + 2
        );
        assert!(!PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert!(!PHYSICAL_COMPLETION_ADMISSION_STATE.lock().open);

        let second = physical_completion_terminal_event().expect("second terminal event");
        finish_physical_completion_external_terminal_event(second, BlockResetOutcome::Quiesced)
            .unwrap();
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire),
            PHYSICAL_COMPLETION_TERMINAL_NONE
        );
        assert!(!PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_ADMISSION_STATE.lock().open);

        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        clear_physical_completion_terminal_event();
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn self_reset_snapshot_cannot_clear_new_quarantine_event() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        clear_physical_completion_terminal_event();
        PHYSICAL_COMPLETION_RESET_PENDING.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);

        // Model the local lower reset's Quiesced proof before a second owner
        // publishes a newer Quarantined terminal event.
        let first = physical_completion_terminal_event_for_reset(
            BlockResetOutcome::Quiesced,
            generation + 1,
        );
        physical_completion_terminal_notifier(
            physical_completion_terminal_context(),
            BlockCompletionAvailability::Quarantined {
                generation: generation + 2,
            },
        );
        assert_eq!(
            finish_physical_completion_external_terminal_event(first, BlockResetOutcome::Quiesced),
            Err(AxError::ResourceBusy)
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_STATE.load(Ordering::Acquire),
            PHYSICAL_COMPLETION_TERMINAL_QUARANTINED
        );
        assert_eq!(
            PHYSICAL_COMPLETION_TERMINAL_GENERATION.load(Ordering::Acquire),
            generation + 2
        );
        assert!(!PHYSICAL_COMPLETION_DEVICE_ACTIVE.load(Ordering::Acquire));
        assert!(PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));
        assert!(!PHYSICAL_COMPLETION_ADMISSION_STATE.lock().open);

        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
        clear_physical_completion_terminal_event();
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn reset_marker_rearms_pending_after_drain_clear() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_QUIESCED, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(true, Ordering::Release);

        assert!(clear_physical_completion_work_pending_with_recheck());
        assert!(PHYSICAL_COMPLETION_WORK_PENDING.load(Ordering::Acquire));

        PHYSICAL_COMPLETION_TERMINAL_STATE
            .store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
        PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
    }

    #[test]
    fn worker_stop_defers_generation_bump_until_publish_commit_guard_drops() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = true;
            state.open = true;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(true, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);

        let reservation = ring.reserve_physical_worker_slot().unwrap();
        assert_eq!(reservation.with_physical_publish(|| ()), Ok(()));
        note_physical_completion_worker_stopped();
        assert_eq!(
            PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire),
            generation,
            "worker stop must not invalidate publish-to-commit custody"
        );
        drop(reservation);
        assert_eq!(
            PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire),
            generation + 1
        );

        assert_eq!(stop_physical_completion_device(), Ok(()));
        {
            let mut state = PHYSICAL_COMPLETION_ADMISSION_STATE.lock();
            state.configured = false;
            state.open = false;
            state.in_flight = 0;
            state.generation_bump_pending = false;
        }
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STARTED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_WORKER_STOPPED.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
    }

    #[test]
    fn physical_completion_routes_are_global_and_preserve_ring_identity() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring_a = IoUring::try_new(layout).unwrap();
        let ring_b = IoUring::try_new(layout).unwrap();
        let request_a = reserve_test_request_id(&ring_a);
        let request_b = reserve_test_request_id(&ring_b);

        let route_a = PhysicalCompletionRouteReservation::new(1).unwrap();
        route_a.activate_test(&ring_a, request_a, 3, 0xA11CE);
        let route_b = PhysicalCompletionRouteReservation::new(1).unwrap();
        route_b.activate_test(&ring_b, request_b, 7, 0xB0B);

        // The shared device drain is allowed to return B before A.  Lookup is
        // by exact handle and retains the owning ring/slot pair, so no ring
        // can quarantine another ring's completion as an unknown local item.
        let (owner_b, slot_b) = lookup_physical_completion_route(0xB0B).unwrap();
        assert!(Arc::ptr_eq(&owner_b, &ring_b));
        assert_eq!(slot_b, 7);
        let (owner_a, slot_a) = lookup_physical_completion_route(0xA11CE).unwrap();
        assert!(Arc::ptr_eq(&owner_a, &ring_a));
        assert_eq!(slot_a, 3);

        assert!(release_physical_completion_routes(
            &ring_b,
            request_b,
            Some(0xB0B)
        ));
        assert!(release_physical_completion_routes(
            &ring_a,
            request_a,
            Some(0xA11CE)
        ));
        assert!(lookup_physical_completion_route(0xA11CE).is_none());
        assert!(lookup_physical_completion_route(0xB0B).is_none());
    }

    #[test]
    fn stale_route_cleanup_cannot_clear_reused_worker_slot_generation() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let old_request = reserve_test_request_id(&ring);
        let new_request = reserve_test_request_id(&ring);
        assert_eq!(old_request.slot(), new_request.slot());
        assert_ne!(old_request.generation(), new_request.generation());

        let old_route = PhysicalCompletionRouteReservation::new(1).unwrap();
        old_route.activate_test(&ring, old_request, 3, 0x1111);
        let new_route = PhysicalCompletionRouteReservation::new(1).unwrap();
        new_route.activate_test(&ring, new_request, 3, 0x2222);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 2);

        assert!(release_physical_completion_routes(
            &ring,
            old_request,
            Some(0x1111)
        ));
        assert!(lookup_physical_completion_route(0x1111).is_none());
        assert!(lookup_physical_completion_route(0x2222).is_some());
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert!(!release_physical_completion_routes(
            &ring,
            old_request,
            Some(0x1111)
        ));
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert!(release_physical_completion_routes(
            &ring,
            new_request,
            Some(0x2222)
        ));
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn reserved_completion_replays_after_route_commit() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        #[cfg(feature = "test-io-control")]
        let (stats_were_enabled, quarantine_before) = {
            let enabled = IO_URING_DMA_DIRECT_STATS_ENABLED.swap(true, Ordering::AcqRel);
            (
                enabled,
                IO_URING_PHYSICAL_QUARANTINE.load(Ordering::Acquire),
            )
        };
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);
        let completion = PhysicalIoCompletion {
            handle: 0xC0DE,
            cookie: 0xCAFE,
            bytes: 4096,
            success: true,
        };

        // Simulate a used-ring record observed while the submitter still owns
        // the Reserved route. The record must remain bounded custody rather
        // than being lost as an unknown completion.
        quarantine_physical_completion(completion, true).unwrap();
        let route = PhysicalCompletionRouteReservation::new(1).unwrap();
        route.activate_test(&ring, request, 11, completion.handle);

        let mut replay = [PhysicalIoCompletion {
            handle: 0,
            cookie: 0,
            bytes: 0,
            success: false,
        }; IO_URING_PHYSICAL_MAX_QD];
        assert_eq!(take_replayable_physical_completions(&mut replay), 1);
        assert_eq!(replay[0], completion);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().quarantine_len, 0);
        #[cfg(feature = "test-io-control")]
        {
            assert_eq!(
                IO_URING_PHYSICAL_QUARANTINE.load(Ordering::Acquire),
                quarantine_before
            );
            IO_URING_DMA_DIRECT_STATS_ENABLED.store(stats_were_enabled, Ordering::Release);
        }
        assert!(release_physical_completion_routes(
            &ring,
            request,
            Some(completion.handle)
        ));
    }

    #[test]
    fn replay_cookie_mismatch_quarantines_reused_handle() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        #[cfg(feature = "test-io-control")]
        let (stats_were_enabled, quarantine_before) = {
            let enabled = IO_URING_DMA_DIRECT_STATS_ENABLED.swap(true, Ordering::AcqRel);
            (
                enabled,
                IO_URING_PHYSICAL_QUARANTINE.load(Ordering::Acquire),
            )
        };
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);
        let completion = PhysicalIoCompletion {
            handle: 0xD00D,
            cookie: 0xDEAD,
            bytes: 4096,
            success: true,
        };
        quarantine_physical_completion(completion, true).unwrap();
        let route = PhysicalCompletionRouteReservation::new(1).unwrap();
        route.activate_test_with_cookie(&ring, request, 13, completion.handle, Some(0xBEEF));

        let mut output = [PhysicalIoCompletion {
            handle: 0,
            cookie: 0,
            bytes: 0,
            success: false,
        }];
        assert_eq!(take_replayable_physical_completions(&mut output), 0);
        assert!(physical_completion_has_quarantined_route());
        assert!(
            !PHYSICAL_COMPLETION_ROUTER
                .lock()
                .quarantine
                .iter()
                .flatten()
                .next()
                .is_some_and(|record| record.replayable)
        );
        #[cfg(feature = "test-io-control")]
        {
            assert_eq!(
                IO_URING_PHYSICAL_QUARANTINE.load(Ordering::Acquire),
                quarantine_before + 1
            );
            IO_URING_DMA_DIRECT_STATS_ENABLED.store(stats_were_enabled, Ordering::Release);
        }
        assert!(release_physical_completion_routes(
            &ring,
            request,
            Some(completion.handle)
        ));
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        router.quarantine.fill(None);
        router.quarantine_len = 0;
    }

    #[test]
    fn late_completion_from_old_generation_is_quarantined() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);
        let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
        let route = PhysicalCompletionRouteReservation::new(1).unwrap();
        route.activate_test_with_cookie(&ring, request, 17, 0xFACE, Some(0xC0DE));

        // Reset/transport replacement advances the accepted upper generation
        // before a late IRQ can be handed to the route table. The old owner
        // remains custody, but its record must not settle a replacement
        // request that may reuse the raw handle.
        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation + 1, Ordering::Release);
        let disposition = route_physical_completion(PhysicalIoCompletion {
            handle: 0xFACE,
            cookie: 0xC0DE,
            bytes: 4096,
            success: true,
        })
        .unwrap();
        assert_eq!(disposition, PhysicalIoCompletionDisposition::Unknown);
        assert!(physical_completion_has_quarantined_route());
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().quarantine_len, 1);

        PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
        assert!(release_physical_completion_routes(
            &ring,
            request,
            Some(0xFACE)
        ));
        let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
        router.quarantine.fill(None);
        router.quarantine_len = 0;
    }

    #[test]
    fn partial_extent_retirement_does_not_fill_quarantine() {
        assert!(!retained_completion_needs_quarantine(
            PhysicalIoPendingReason::MissingCompletion {
                observed: 1,
                expected: 2,
            }
        ));
        assert!(retained_completion_needs_quarantine(
            PhysicalIoPendingReason::CookieMismatch
        ));
        assert!(retained_completion_needs_quarantine(
            PhysicalIoPendingReason::DuplicateCompletion
        ));
    }

    #[test]
    fn terminal_partial_route_releases_unaccepted_suffix() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);

        let routes =
            PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS).unwrap();
        let handles: [Option<u64>; 7] = [
            Some(0xA11CE),
            Some(0xA11CF),
            Some(0xA11D0),
            Some(0xA11D1),
            Some(0xA11D2),
            Some(0xA11D3),
            Some(0xA11D4),
        ];
        assert!(!routes.activate_test_with_handles(&ring, request, 9, &handles));
        assert_eq!(physical_completion_route_count(), handles.len());
        assert_eq!(physical_completion_custody_count(), 1);
        assert!(!physical_completion_has_quarantined_route());
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);

        // The accepted prefix remains routable and owns the operation's one
        // work charge; only the never-published suffix was rolled back.
        for handle in handles.iter().flatten() {
            assert!(lookup_physical_completion_route(*handle).is_some());
        }
        assert!(release_physical_completion_routes(
            &ring,
            request,
            Some(handles[3].expect("test handle"))
        ));
        assert_eq!(physical_completion_route_count(), 0);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn physical_route_reservation_is_global_qd_limited() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let mut reservations = Vec::new();
        for _ in 0..IO_URING_PHYSICAL_MAX_QD {
            reservations.push(
                PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS).unwrap(),
            );
        }
        assert!(matches!(
            PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS),
            Err(AxError::ResourceBusy)
        ));
        assert!(matches!(
            PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS + 1),
            Err(AxError::BadState)
        ));
        drop(reservations);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn malformed_publication_keeps_reset_custody_route() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);

        let routes = PhysicalCompletionRouteReservation::new(2).unwrap();
        routes.activate(&ring, request, 5, None);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert_eq!(physical_completion_route_count(), 0);
        assert!(physical_completion_has_quarantined_route());

        // No usable handle is exposed to the wait owner, but reset/teardown
        // can still find and release the exact ring/slot custody.
        assert!(release_physical_completion_routes(&ring, request, None));
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn malformed_prefix_quarantines_the_complete_group() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);

        let routes = PhysicalCompletionRouteReservation::new(4).unwrap();
        // A missing child in an accepted prefix is malformed.  The valid
        // siblings must remain reset custody too; none may be exposed as an
        // exact wait route while lower ownership is ambiguous.
        assert!(routes.activate_test_with_handles(
            &ring,
            request,
            6,
            &[Some(0xA100), None, Some(0xA102)],
        ));
        assert_eq!(physical_completion_route_count(), 0);
        assert_eq!(physical_completion_custody_count(), 1);
        assert!(physical_completion_has_quarantined_route());
        assert!(lookup_physical_completion_route(0xA100).is_none());
        assert!(lookup_physical_completion_route(0xA102).is_none());
        assert!(release_physical_completion_routes(&ring, request, None));
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn completion_group_children_lookup_out_of_order() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let request = reserve_test_request_id(&ring);
        let routes = PhysicalCompletionRouteReservation::new(4).unwrap();
        let handles = [Some(0xB100), Some(0xB101), Some(0xB102), Some(0xB103)];
        assert!(!routes.activate_test_with_handles(&ring, request, 12, &handles));

        // Device completion order is independent from child index.  Each
        // lookup must still return the one group owner and worker slot.
        for handle in handles.iter().flatten().rev() {
            let (owner, slot) = lookup_physical_completion_route(*handle).unwrap();
            assert!(Arc::ptr_eq(&owner, &ring));
            assert_eq!(slot, 12);
        }
        assert_eq!(physical_completion_route_count(), handles.len());
        assert!(release_physical_completion_routes(
            &ring,
            request,
            Some(handles[2].expect("test handle")),
        ));
        assert_eq!(physical_completion_route_count(), 0);
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    }

    #[test]
    fn reset_retirement_releases_quarantine_and_unblocks_final_close() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        let _context = crate::test_support::scheduler_test_context();
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let issued = issue_test_request(&ring);
        let request = issued.id();
        let route = PhysicalCompletionRouteReservation::new(1).unwrap();
        route.activate_test(&ring, request, 0, 0xBAD0);
        quarantine_physical_completion_routes(&ring, request, Some(0xBAD0));

        {
            let mut state = ring.state.lock();
            state.physical_work[0] = Some(PhysicalIoWork {
                ring: Arc::clone(&ring),
                slot: 0,
                issued: Some(issued),
                admission: None,
                pending_publication: false,
                test_handle: Some(0xBAD0),
            });
            state.physical_work_count = 1;
        }
        ring.final_close_requested.store(true, Ordering::Release);
        ring.state.lock().final_close.phase = FinalClosePhase::Completions;

        assert!(physical_completion_has_quarantined_route());
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
        assert_eq!(ring.physical_worker_len(), 1);
        // VirtIO returns `Retired` after proving queue quiescence and
        // dismantling the transport.  It must take the same upper retirement
        // path as `Quiesced`; only re-enable is skipped by the production
        // reset wrapper.
        retire_physical_completion_after_reset(axdriver::prelude::BlockResetOutcome::Retired)
            .unwrap();
        assert!(!physical_completion_has_quarantined_route());
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
        assert_eq!(ring.physical_worker_len(), 0);

        // Reset retirement queues a typed EIO terminal but does not require a
        // CQE publication while final close is already in progress.  The
        // normal close-completions step can now discard it and finish.
        assert!(ring.close_completions_step().unwrap());
    }

    #[test]
    fn lower_physical_completion_conversion_is_exact_and_fail_closed() {
        let completion = BlockCompletion {
            handle: BlockRequestHandle { raw: 0x1234 },
            owner: BlockCompletionOwner::Physical,
            cookie: 0x5678,
            status: BlockCompletionStatus::Success,
            bytes: 4096,
        };
        assert_eq!(
            convert_block_completion(completion),
            Ok(PhysicalIoCompletion {
                handle: 0x1234,
                cookie: 0x5678,
                bytes: 4096,
                success: true,
            })
        );

        let failed = BlockCompletion {
            status: BlockCompletionStatus::DeviceError(7),
            ..completion
        };
        assert_eq!(
            convert_block_completion(failed),
            Ok(PhysicalIoCompletion {
                success: false,
                ..PhysicalIoCompletion {
                    handle: 0x1234,
                    cookie: 0x5678,
                    bytes: 4096,
                    success: true,
                }
            })
        );

        for malformed in [
            BlockCompletion {
                owner: BlockCompletionOwner::Ordinary,
                ..completion
            },
            BlockCompletion {
                handle: BlockRequestHandle { raw: 0 },
                ..completion
            },
            BlockCompletion {
                cookie: 0,
                ..completion
            },
            BlockCompletion {
                status: BlockCompletionStatus::Quarantined,
                ..completion
            },
        ] {
            assert_eq!(convert_block_completion(malformed), Err(AxError::BadState));
        }
    }

    #[test]
    fn malformed_lower_batch_retains_prior_exact_records() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        {
            let mut router = PHYSICAL_COMPLETION_ROUTER.lock();
            assert_eq!(router.quarantine_len, 0);
            router.quarantine.fill(None);
        }
        let records = [
            BlockCompletion {
                handle: BlockRequestHandle { raw: 0xCAFE },
                owner: BlockCompletionOwner::Physical,
                cookie: 0xBEEF,
                status: BlockCompletionStatus::Success,
                bytes: 4096,
            },
            BlockCompletion {
                handle: BlockRequestHandle { raw: 0xBAD },
                owner: BlockCompletionOwner::Ordinary,
                cookie: 0xFACE,
                status: BlockCompletionStatus::Success,
                bytes: 0,
            },
        ];
        assert_eq!(
            quarantine_drained_block_completions(&records, records.len()),
            AxError::BadState
        );
        assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().quarantine_len, 1);
        PHYSICAL_COMPLETION_ROUTER.lock().quarantine.fill(None);
        PHYSICAL_COMPLETION_ROUTER.lock().quarantine_len = 0;
    }

    #[test]
    fn production_completion_owner_stop_and_reset_are_fail_closed_when_uninstalled() {
        let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
        note_physical_completion_worker_stopped();
        assert!(stop_physical_completion_device().is_ok());
        assert!(!physical_completion_device_ready());
        assert_eq!(
            reset_physical_completion_device(),
            Err(AxError::OperationNotSupported)
        );
        note_physical_completion_worker_started();
    }
}
