//! Concrete TheKernel adapter for Linux `io_uring` ring ownership.

use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::BTreeMap,
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
use axfs_ng_vfs::{FsPathBuf, PhysicalIoSegment, SubmittedFileIo};
use axhal::paging::PageSize;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    POLLERR, POLLHUP, POLLIN, POLLMSG, POLLNVAL, POLLOUT, POLLPRI, POLLRDBAND, POLLRDHUP,
    POLLRDNORM, POLLREMOVE, POLLWRBAND, POLLWRNORM, open_how,
};
use ouroboros::self_referencing;
use spin::Once;
use tk_linux_io_uring::{
    BufferLeaseRelease, BufferSlot, BufferTableId, CancelSelector, CompletionPublication,
    CompletionToken, CopiedSubmission, FileSlot, FileTableId, IoUringError, IssuedRequest,
    LeaseRelease, MappingRegion, ParsedSubmission, PreparedRequest, ProviderCancelOutcome,
    ReadWriteRequest, RegisteredBufferLease, RegisteredBufferTable, RegisteredFileLease,
    RegisteredFileTable, RequestDescriptor, RequestId, RequestIssueError, RequestRegistry,
    RequestReservation, RingId, RingLayout, SetupFlags, TerminalCause,
};
use tk_linux_process_adapter::Pid;
use tk_linux_signal::SignalSet;

use super::{
    DescriptionResource, FileDescription, FileHandle, FileLike, FileMmapProtection,
    FileMmapRequest, FixedSharedMmapRegion, IoOperationContext, Kstat, PreparedFileMmap,
    SharedPages, UringCmdCompletion, anon_inode_stat, event::EventFd, fanotify::FanotifyEventActor,
    fd_table::FdTable, memfd::MemfdMutationGuard, permission::VfsSecurityContext,
    privilege_metadata::ContentWritePrivilegeGuard,
};
use crate::mm::{
    MutationAdmission, PinnedUserSegments, PinnedUserSegmentsMut, SharedAtomicU32,
    UserIoPinProvenance, UserIoPinSegment, UserMemoryCapability, physical_segments_are_disjoint,
    try_pin_user_segments_to_user_longterm_with,
};

/// The physical effect path is deliberately bounded independently from the
/// SQ/CQ geometry.  A ring can advertise a larger SQ, but only this many
/// device-owned effects may wait for completion at once.
pub(crate) const RING_WAITER_SLOTS: usize = 64;
const PAGE_BYTES: usize = PageSize::Size4K as usize;
const IO_URING_GLOBAL_REQUEST_SLOTS: usize = 65_536;
const IO_URING_GLOBAL_FIXED_FILE_SLOTS: usize = 65_536;
const IO_URING_GLOBAL_REGISTERED_BUFFER_SLOTS: usize = 65_536;

const IO_URING_PHYSICAL_MAX_QD: usize = 32;
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

// Internal modules share the ring state, but keep each ownership protocol
// together. Crate consumers continue to use this module's typed entry points.
mod physical_device;
use physical_device::*;
mod physical_routes;
use physical_routes::*;
mod physical_worker;
use physical_worker::*;
pub(crate) use physical_worker::{
    drain_physical_completion_work, has_physical_completion_work,
    install_default_physical_completion_device, note_physical_completion_worker_started,
    note_physical_completion_worker_stopped, physical_completion_device_ready_for,
    physical_completion_worker_is_stopped, wake_physical_completion_worker,
};
mod resources;
use resources::*;
pub(crate) use resources::{IoUringBufferLease, IoUringFileLease};
mod physical_io;
#[cfg(test)]
use physical_io::*;
pub(crate) use physical_io::{
    PhysicalIoCompletionDisposition, PhysicalIoCompletionPass, PhysicalIoWork,
    PreparedPhysicalIoAdmission, PreparedPhysicalIoOperation, PreparedPhysicalIoPlan,
};
mod polling;
use polling::*;
pub(crate) use polling::{
    PendingStreamAdmissionError, SocketMultishotWork, drain_deferred_io_uring_work,
    has_deferred_io_uring_work,
};
mod completion;
mod diagnostics;
mod lifecycle;
mod owned_io;
pub(crate) use diagnostics::{
    IoUringDmaFallbackReason, record_io_uring_dma_direct_read_fallback,
    record_io_uring_dma_direct_read_hit, record_io_uring_dma_direct_write_fallback,
    record_io_uring_dma_direct_write_hit, record_io_uring_physical_child_completed,
    record_io_uring_physical_completed, record_io_uring_physical_quarantine,
    record_io_uring_physical_submitted,
};

/// Explicit execution identity retained by an SQPOLL ring.  A polling worker
/// is a kernel task and must never inherit its own files, address space, or
/// credentials for a userspace SQE.  The creator installs this immutable
/// snapshot before the worker is started; individual SQEs still acquire a
/// fresh descriptor lease from the retained `files` table, so close/exec
/// races retain Linux's issue-time lookup behavior.
#[derive(Clone)]
pub(crate) struct IoUringSubmissionActor {
    world: crate::task::WorldId,
    files: Arc<FdTable>,
    memory: UserMemoryCapability,
    security: VfsSecurityContext,
    fanotify_actor: FanotifyEventActor,
    process_id: Pid,
    path_snapshot: crate::task::NamespaceCredentialFsSnapshot,
    nofile_limit: usize,
}

impl IoUringSubmissionActor {
    pub(crate) fn new(
        world: crate::task::WorldId,
        files: Arc<FdTable>,
        memory: UserMemoryCapability,
        security: VfsSecurityContext,
        fanotify_actor: FanotifyEventActor,
        process_id: Pid,
        path_snapshot: crate::task::NamespaceCredentialFsSnapshot,
        nofile_limit: usize,
    ) -> Self {
        Self {
            world,
            files,
            memory,
            security,
            fanotify_actor,
            process_id,
            path_snapshot,
            nofile_limit,
        }
    }

    pub(crate) const fn world(&self) -> crate::task::WorldId {
        self.world
    }

    pub(crate) const fn files(&self) -> &Arc<FdTable> {
        &self.files
    }
    pub(crate) const fn memory(&self) -> &UserMemoryCapability {
        &self.memory
    }
    pub(crate) const fn security(&self) -> &VfsSecurityContext {
        &self.security
    }
    pub(crate) const fn fanotify_actor(&self) -> FanotifyEventActor {
        self.fanotify_actor
    }
    pub(crate) const fn process_id(&self) -> Pid {
        self.process_id
    }
    pub(crate) const fn path_snapshot(&self) -> &crate::task::NamespaceCredentialFsSnapshot {
        &self.path_snapshot
    }
    pub(crate) const fn nofile_limit(&self) -> usize {
        self.nofile_limit
    }
}

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
static DEFERRED_POLL_CALLBACKS: AtomicPtr<PollCallbackState> = AtomicPtr::new(ptr::null_mut());
// Only the policy worker consumes this detached continuation. New IRQ
// publications remain on DEFERRED_POLL_CALLBACKS until the next bounded batch.
static DEFERRED_POLL_CONTINUATION: AtomicPtr<PollCallbackState> = AtomicPtr::new(ptr::null_mut());
const POLL_CALLBACK_BUDGET: usize = 64;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalClosePhase {
    Begin,
    Polls,
    OwnedFileIo,
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
    registered_wait_region: Option<RegisteredWaitRegion>,
    next_file_table_id: u64,
    next_buffer_table_id: u64,
    polls: Vec<Option<Arc<PollControl>>>,
    completion_eventfd: Option<FileHandle<EventFd>>,
    /// `PROVIDE_BUFFERS` groups.  A slot moves Ready -> Leased -> Ready;
    /// remove/close may retire only Ready slots, so a multishot completion
    /// never exposes a buffer which has already been recycled.
    provided_buffers: BTreeMap<u16, ProvidedBufferGroup>,
    pending_publications: Vec<Option<CompletionToken>>,
    pending_nonterminal_publications: Vec<Option<CompletionPublication>>,
    iopoll_uring_cmd: Vec<Option<UringCmdOwner>>,
    owned_file_io: Vec<Option<OwnedFileIoOwner>>,
    /// Accepted work held behind an IOSQE_IO_LINK predecessor.  This is sized
    /// with SQ depth at ring creation, so dependency admission never allocates
    /// after consuming an SQE.
    parked_submissions: Vec<Option<ParkedSubmission>>,
    /// The request whose IOSQE_IO_LINK/HARDLINK flag binds the next SQE.
    link_tail: Option<(RequestId, tk_linux_io_uring::SubmissionLink)>,
    /// Terminal result retained until its slot can be reused; dependency
    /// release is deliberately independent of CQ publication/task-work.
    terminal_results: Vec<Option<(RequestId, i32)>>,
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
    socket_multishot: Vec<Option<SocketMultishotWork>>,
    final_close: FinalCloseProgress,
}

struct ParkedSubmission {
    work: SubmissionWork,
    predecessor: Option<RequestId>,
    hardlink: bool,
    drain: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum UringCmdHandoffState {
    Prepared,
    Submitting,
    Active,
    CancelPending,
}

struct UringCmdOwner {
    id: RequestId,
    file: FileHandle<dyn FileLike>,
    iopoll: bool,
    disabled: AtomicBool,
    state: UringCmdHandoffState,
}

struct OwnedFileIoBridge {
    issued: IssuedRequest,
}

/// Exact provider custody for one reusable request slot.  `generation` is
/// repeated here deliberately: a stale provider callback must never affect a
/// later owner of the same slot.
enum OwnedFileIoControlState {
    Empty,
    Publishing {
        generation: u64,
        bridge: OwnedFileIoBridge,
    },
    Submitted {
        generation: u64,
        bridge: OwnedFileIoBridge,
        control: SubmittedFileIo,
    },
    CancelPending {
        generation: u64,
        bridge: OwnedFileIoBridge,
    },
    /// A provider completion raced with its consuming cancel control.  The
    /// cancel path owns the registry's nonterminal-shot fence and completes
    /// this result only after it has restored or terminalized that fence.
    CompletionPending {
        generation: u64,
        bridge: OwnedFileIoBridge,
        result: i32,
    },
    InFlight {
        generation: u64,
        bridge: OwnedFileIoBridge,
    },
    Terminal,
}

struct OwnedFileIoOwner {
    id: RequestId,
    state: OwnedFileIoControlState,
    /// Fixed and supplied buffers remain leased until the one terminal
    /// provider completion is claimed.  In particular, BUFFER_SELECT must
    /// never recycle its chosen slot before the CQE hands it to userspace.
    buffer: Option<IoUringBufferLease>,
}

struct OwnedFileIoTerminal {
    issued: IssuedRequest,
    result: i32,
    buffer: Option<IoUringBufferLease>,
}

struct ProvidedBuffer {
    id: u16,
    address: usize,
    length: usize,
    capability: UserMemoryCapability,
    leased: bool,
    consumed: bool,
    retiring: bool,
}

struct ProvidedBufferGroup {
    slots: Vec<ProvidedBuffer>,
}

/// Fully copied OPENAT2 execution input.  The pathname and `open_how` never
/// outlive usercopy as borrowed addresses; filesystem/credential/files-table
/// authority is likewise the submitting task's immutable snapshot.
pub(crate) struct IoUringOpenAt2Work {
    pub(crate) path: FsPathBuf,
    pub(crate) how: open_how,
    pub(crate) flags: i32,
    /// Linked/drained work can run after `close(2)` has removed the base
    /// directory from the submitter's table. Retain the issue-time OFD so
    /// that delayed pathname resolution has stable dirfd authority.
    pub(crate) dirfd: Option<Arc<FileDescription>>,
    pub(crate) actor: IoUringSubmissionActor,
}

type SubmissionWorkParts = (
    PreparedRequest,
    Result<ParsedSubmission, IoUringError>,
    Option<IoUringFileLease>,
    Option<IoUringBufferLease>,
    Option<IoOperationContext>,
    Option<PreparedPhysicalIoAdmission>,
    Option<AxError>,
    Option<IoUringOpenAt2Work>,
    UserMemoryCapability,
    Option<axfs::PreparedOwnedFileIo>,
);

#[cfg(feature = "io-submit-batch")]
pub(crate) trait SubmissionNotificationFlush {
    fn flush_notifications(&mut self);
}

/// A stack-owned notification batch. Only completions explicitly passed this
/// token can defer waiter notification; asynchronous producers never consult
/// task-local or ring-global batching state. CQEs and eventfd counts remain
/// visible immediately.
#[cfg(feature = "io-submit-batch")]
pub(crate) struct SubmissionCompletionBatch<'a> {
    ring: &'a IoUring,
    pending: u32,
}

#[cfg(feature = "io-submit-batch")]
impl<'a> SubmissionCompletionBatch<'a> {
    pub(crate) fn new(ring: &'a IoUring) -> Self {
        Self { ring, pending: 0 }
    }
    pub(crate) fn flush(&mut self) {
        if core::mem::take(&mut self.pending) != 0 {
            self.ring.completion_wait.wake();
        }
    }
    fn record(&mut self) {
        self.pending += 1;
        if self.pending == 32 {
            self.flush();
        }
    }
}

#[cfg(feature = "io-submit-batch")]
impl SubmissionNotificationFlush for SubmissionCompletionBatch<'_> {
    fn flush_notifications(&mut self) {
        self.flush();
    }
}

#[cfg(feature = "io-submit-batch")]
impl Drop for SubmissionCompletionBatch<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

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
    openat2: Option<IoUringOpenAt2Work>,
    capability: UserMemoryCapability,
    /// Provider-approved generic file I/O prepared while the SQE admission
    /// still owns the submitter's MM/OFD/actor snapshot.  It is intentionally
    /// separate from physical I/O: this token may represent cached/direct
    /// provider work with long-term user pins.
    owned: Option<axfs::PreparedOwnedFileIo>,
}

impl SubmissionWork {
    pub(crate) const fn id(&self) -> RequestId {
        self.prepared.id()
    }

    pub(crate) fn dependencies(&self) -> Option<tk_linux_io_uring::SubmissionDependencies> {
        self.parsed.ok().map(ParsedSubmission::dependencies)
    }
    pub(crate) fn into_parts(self) -> SubmissionWorkParts {
        (
            self.prepared,
            self.parsed,
            self.file,
            self.buffer,
            self.context,
            self.physical,
            self.admission_error,
            self.openat2,
            self.capability,
            self.owned,
        )
    }

    pub(crate) fn set_owned(&mut self, owned: axfs::PreparedOwnedFileIo) -> AxResult<()> {
        if self.owned.is_some() {
            return Err(AxError::BadState);
        }
        self.owned = Some(owned);
        Ok(())
    }
}

pub(crate) enum SubmissionStep<'a> {
    Empty,
    CompletionQueueFull,
    AdmissionBusy,
    Dropped,
    Admission(SubmissionAdmission<'a>),
}

/// Result of dependency admission/release.  `Cancelled` still owns a fully
/// admitted request and must be terminally completed by the syscall executor.
pub(crate) enum DependencyDispatch {
    Execute(SubmissionWork),
    Cancelled(SubmissionWork),
    Parked,
}

/// Reversible SQ admission which owns terminal credit but no published head.
pub(crate) struct SubmissionAdmission<'a> {
    ring: &'a IoUring,
    #[cfg(feature = "io-submit-batch")]
    notification_flush: Option<&'a mut dyn SubmissionNotificationFlush>,
    reservation: Option<RequestReservation>,
    parsed: Result<ParsedSubmission, IoUringError>,
}

impl SubmissionAdmission<'_> {
    #[cfg(feature = "io-submit-batch")]
    pub(crate) fn flush_notifications(&mut self) {
        if let Some(flush) = self.notification_flush.as_deref_mut() {
            flush.flush_notifications();
        }
    }

    pub(crate) const fn parsed(&self) -> Result<ParsedSubmission, IoUringError> {
        self.parsed
    }

    /// Generation-scoped identity reserved for this still-unpublished SQE.
    /// Admission-time owned-I/O preparation uses it to bind its completion
    /// bridge before any long-term pin can become provider-visible.
    pub(crate) fn id(&self) -> AxResult<RequestId> {
        self.reservation
            .as_ref()
            .map(RequestReservation::id)
            .ok_or(AxError::BadState)
    }

    pub(crate) fn commit(
        self,
        file: Option<IoUringFileLease>,
        buffer: Option<IoUringBufferLease>,
        context: Option<IoOperationContext>,
        physical: Option<PreparedPhysicalIoAdmission>,
        admission_error: Option<AxError>,
        capability: UserMemoryCapability,
    ) -> AxResult<SubmissionWork> {
        self.commit_with_openat2(
            file,
            buffer,
            context,
            physical,
            admission_error,
            None,
            capability,
        )
    }

    pub(crate) fn commit_with_openat2(
        mut self,
        file: Option<IoUringFileLease>,
        buffer: Option<IoUringBufferLease>,
        context: Option<IoOperationContext>,
        physical: Option<PreparedPhysicalIoAdmission>,
        admission_error: Option<AxError>,
        openat2: Option<IoUringOpenAt2Work>,
        capability: UserMemoryCapability,
    ) -> AxResult<SubmissionWork> {
        let ring = self.ring;
        #[cfg(feature = "io-submit-batch")]
        let _submission = match ring.submission_serial.try_lock() {
            Some(guard) => guard,
            None => {
                self.flush_notifications();
                ring.submission_serial.lock()
            }
        };
        #[cfg(not(feature = "io-submit-batch"))]
        let _submission = ring.submission_serial.lock();
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
            openat2,
            capability,
            owned: None,
        })
    }

    pub(crate) fn commit_poll(
        self,
        lease: IoUringFileLease,
        linux_events: u32,
        multishot: bool,
        capability: UserMemoryCapability,
    ) -> AxResult<()> {
        let ring = self.ring;
        ring.commit_poll_admission(self, lease, linux_events, multishot, capability)
    }
}

impl Drop for SubmissionAdmission<'_> {
    fn drop(&mut self) {
        let Some(reservation) = self.reservation.take() else {
            return;
        };
        let ring = self.ring;
        #[cfg(feature = "io-submit-batch")]
        let _submission = match ring.submission_serial.try_lock() {
            Some(guard) => guard,
            None => {
                self.flush_notifications();
                ring.submission_serial.lock()
            }
        };
        #[cfg(not(feature = "io-submit-batch"))]
        let _submission = ring.submission_serial.lock();
        let mut state = self.ring.state.lock();
        if let Err(error) = state.requests.rollback(reservation) {
            error!("io_uring admission rollback lost request ownership: {error:?}");
        }
        state.admission_in_progress = false;
    }
}

pub(crate) struct IoUring {
    world: crate::task::WorldId,
    id: RingId,
    layout: RingLayout,
    rings: Arc<SharedPages>,
    sqes: Arc<SharedPages>,
    ring_region: FixedSharedMmapRegion,
    cq_ring_region: FixedSharedMmapRegion,
    sqe_region: FixedSharedMmapRegion,
    // These handles are created by try_new and retained only by the file
    // object/final-close policy path. Neither ownership path runs in hard IRQ
    // context, so their final drop may release SharedPages normally.
    sq_head: SharedAtomicU32,
    sq_tail: SharedAtomicU32,
    cq_head: SharedAtomicU32,
    cq_tail: SharedAtomicU32,
    sq_dropped: SharedAtomicU32,
    /// Task identity captured at setup for `IORING_SETUP_SINGLE_ISSUER`.
    single_issuer: Mutex<Option<u64>>,
    sqpoll: bool,
    iopoll: bool,
    disabled: AtomicBool,
    /// Captured setup actor for the SQ polling task.  It is intentionally
    /// separate from `RingState`: final close can stop the worker without
    /// holding request-table ownership across a sleep.
    sqpoll_actor: Mutex<Option<IoUringSubmissionActor>>,
    sqpoll_stop: AtomicBool,
    sqpoll_failed: AtomicBool,
    sqpoll_started: AtomicBool,
    sqpoll_worker_task: AtomicU64,
    sqpoll_wake: PollSet<1>,
    defer_taskrun: bool,
    coop_taskrun: bool,
    taskrun_pending: AtomicBool,
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
    pending_nonterminal_publication_count: AtomicUsize,
    state: Mutex<RingState>,
    registered_buffer_budget: Arc<RegisteredBufferPinBudget>,
    _request_charge: RequestSlotCharge,
}

// `self_weak` is initialized before the ring is published and is immutable
// afterwards.  It is a self-referential ownership aid, so `spin::Once` cannot
// derive these auto traits without a circular proof through `Weak<IoUring>`.
// Every mutable ring field is independently atomic or behind a lock.
unsafe impl Send for IoUring {}
unsafe impl Sync for IoUring {}

impl IoUring {
    #[cfg(test)]
    fn try_new(layout: RingLayout) -> AxResult<Arc<Self>> {
        Self::try_new_in_world(layout, crate::task::WorldId::BOOT)
    }

    pub(crate) fn try_new_in_world(
        layout: RingLayout,
        world: crate::task::WorldId,
    ) -> AxResult<Arc<Self>> {
        let profile = world.profile();
        if !profile.enables(tk_linux_profile::Capability::AsyncFileIo) {
            return Err(AxError::OperationNotSupported);
        }
        if layout.sq_entries() > profile.limits().io_uring_entries {
            return Err(AxError::InvalidInput);
        }
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
            tk_linux_io_uring::IORING_OFF_SQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let cq_ring_region = FixedSharedMmapRegion::try_new_detached(
            tk_linux_io_uring::IORING_OFF_CQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let sqe_region = FixedSharedMmapRegion::try_new_detached(
            tk_linux_io_uring::IORING_OFF_SQES,
            Arc::clone(&sqes),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let mut requests = RequestRegistry::new(id, layout.sq_entries(), layout.cq_entries())
            .map_err(map_core_error)?;
        requests.set_observer(Some(crate::pseudofs::trace::record_io_uring));
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
        let mut pending_nonterminal_publications = Vec::new();
        pending_nonterminal_publications
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        pending_nonterminal_publications.resize_with(request_slots, || None);
        let mut iopoll_uring_cmd = Vec::new();
        iopoll_uring_cmd
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        iopoll_uring_cmd.resize_with(request_slots, || None);
        let mut owned_file_io = Vec::new();
        owned_file_io
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        owned_file_io.resize_with(request_slots, || None);
        let mut parked_submissions = Vec::new();
        parked_submissions
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        parked_submissions.resize_with(request_slots, || None);
        let mut terminal_results = Vec::new();
        terminal_results
            .try_reserve_exact(request_slots)
            .map_err(|_| AxError::NoMemory)?;
        terminal_results.resize(request_slots, None);
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
        let mut socket_multishot = Vec::new();
        socket_multishot
            .try_reserve_exact(IO_URING_PENDING_STREAM_CAPACITY)
            .map_err(|_| AxError::NoMemory)?;
        socket_multishot.resize_with(IO_URING_PENDING_STREAM_CAPACITY, || None);
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
            world,
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
            single_issuer: Mutex::new(
                (layout.setup_flags().contains(SetupFlags::SINGLE_ISSUER)
                    && !layout.setup_flags().contains(SetupFlags::R_DISABLED))
                .then(|| axtask::current().id().as_u64()),
            ),
            sqpoll: layout.setup_flags().contains(SetupFlags::SQPOLL),
            iopoll: layout.setup_flags().contains(SetupFlags::IOPOLL),
            disabled: AtomicBool::new(layout.setup_flags().contains(SetupFlags::R_DISABLED)),
            sqpoll_actor: Mutex::new(None),
            sqpoll_stop: AtomicBool::new(false),
            sqpoll_failed: AtomicBool::new(false),
            sqpoll_started: AtomicBool::new(false),
            sqpoll_worker_task: AtomicU64::new(0),
            sqpoll_wake: PollSet::new(),
            defer_taskrun: layout.setup_flags().contains(SetupFlags::DEFER_TASKRUN),
            coop_taskrun: layout.setup_flags().contains(SetupFlags::COOP_TASKRUN),
            taskrun_pending: AtomicBool::new(false),
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
            pending_nonterminal_publication_count: AtomicUsize::new(0),
            state: Mutex::new(RingState {
                requests,
                sq_head: 0,
                sq_dropped: 0,
                admission_in_progress: false,
                fixed_files: None,
                registered_buffers: None,
                registered_wait_region: None,
                next_file_table_id: 1,
                next_buffer_table_id: 1,
                polls,
                completion_eventfd: None,
                provided_buffers: BTreeMap::new(),
                pending_publications,
                pending_nonterminal_publications,
                iopoll_uring_cmd,
                owned_file_io,
                parked_submissions,
                link_tail: None,
                terminal_results,
                physical_work,
                physical_custody,
                physical_slot_reserved,
                physical_work_count: 0,
                pending_stream,
                pending_stream_count: 0,
                socket_multishot,
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

    pub(crate) const fn ring_id(&self) -> RingId {
        self.id
    }

    /// Rejects foreign actors before queue or registered-resource access.
    pub(crate) fn admit_world(&self, actor: crate::task::WorldId) -> AxResult<()> {
        if self.world.admits(actor) {
            Ok(())
        } else {
            Err(AxError::PermissionDenied)
        }
    }

    /// Installs the setup-task actor before an SQPOLL worker can consume the
    /// shared SQ.  Replacing it would silently retarget an existing ring to a
    /// different process, therefore configuration is strictly one-shot.
    pub(crate) fn install_sqpoll_actor(&self, actor: IoUringSubmissionActor) -> AxResult<()> {
        self.admit_world(actor.world)?;
        if !self.sqpoll || self.final_close_requested.load(Ordering::Acquire) {
            return Err(AxError::BadState);
        }
        let mut slot = self.sqpoll_actor.lock();
        if slot.is_some() {
            return Err(AxError::ResourceBusy);
        }
        *slot = Some(actor);
        Ok(())
    }

    pub(crate) fn sqpoll_actor(&self) -> AxResult<IoUringSubmissionActor> {
        if !self.sqpoll
            || self.sqpoll_stop.load(Ordering::Acquire)
            || self.sqpoll_failed.load(Ordering::Acquire)
        {
            return Err(AxError::BadState);
        }
        self.sqpoll_actor.lock().clone().ok_or(AxError::BadState)
    }

    pub(crate) const fn sqpoll_enabled(&self) -> bool {
        self.sqpoll
    }
    pub(crate) const fn iopoll_enabled(&self) -> bool {
        self.iopoll
    }
    pub(crate) fn disabled(&self) -> bool {
        self.disabled.load(Ordering::Acquire)
    }
    pub(crate) fn enable(&self) -> AxResult<()> {
        let _registration = self.registration_serial.lock();
        // Keep R_DISABLED's release transition as the single externally
        // visible commit point.  An entering issuer observes it with Acquire,
        // so it cannot see an enabled SINGLE_ISSUER ring before this caller's
        // identity is fully published.
        if !self.disabled.load(Ordering::Acquire) {
            return Err(AxError::from(LinuxError::EBADFD));
        }
        if self
            .layout
            .setup_flags()
            .contains(SetupFlags::SINGLE_ISSUER)
        {
            *self.single_issuer.lock() = Some(axtask::current().id().as_u64());
        }
        self.disabled.store(false, Ordering::Release);
        Ok(())
    }

    /// Wakes the SQ worker after a submitter enters the ring.  Userspace may
    /// also advance SQ tail without entering, so the worker retains its idle
    /// poll deadline as the liveness backstop.
    pub(crate) fn wake_sqpoll(&self) {
        if self.sqpoll {
            self.sqpoll_wake.wake();
        }
    }

    fn set_sqpoll_need_wakeup(&self, enabled: bool) -> AxResult<()> {
        const IORING_SQ_NEED_WAKEUP: u32 = 1;
        let value = if enabled { IORING_SQ_NEED_WAKEUP } else { 0 };
        self.rings.write_bytes(
            self.layout.sq_offsets().flags() as usize,
            &value.to_ne_bytes(),
        )
    }

    /// Sleeps an idle SQPOLL owner after its polling interval.  A direct tail
    /// update observed before registration is consumed by the predicate;
    /// after the interval Linux requires an enter wake, which uses this same
    /// PollSet. Close/failure wakes the identical wait edge.
    pub(crate) fn wait_sqpoll_wakeup(&self) -> AxResult<()> {
        self.set_sqpoll_need_wakeup(true)?;
        let result = crate::readiness::block_on_poll_set(&self.sqpoll_wake, || {
            if self.sqpoll_should_stop() {
                return Err(AxError::BadState);
            }
            let head = self.state.lock().sq_head;
            let tail = self.sq_tail.load_acquire();
            if self
                .layout
                .pending_submissions(head, tail)
                .map_err(map_core_error)?
                != 0
            {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        });
        self.set_sqpoll_need_wakeup(false)?;
        result
    }

    pub(crate) fn sqpoll_has_submissions(&self) -> AxResult<bool> {
        let head = self.state.lock().sq_head;
        let tail = self.sq_tail.load_acquire();
        self.layout
            .pending_submissions(head, tail)
            .map(|pending| pending != 0)
            .map_err(map_core_error)
    }

    pub(crate) fn sqpoll_should_stop(&self) -> bool {
        self.sqpoll_stop.load(Ordering::Acquire) || self.sqpoll_failed.load(Ordering::Acquire)
    }

    pub(crate) fn sqpoll_failed(&self) -> bool {
        self.sqpoll_failed.load(Ordering::Acquire)
    }

    /// Publishes terminal SQ worker startup/runtime failure.  This is not a
    /// recoverable fallback to caller-driven submit: continuing on the wrong
    /// CPU would violate the ring's requested execution contract.
    pub(crate) fn fail_sqpoll(&self) {
        self.sqpoll_failed.store(true, Ordering::Release);
        self.sqpoll_stop.store(true, Ordering::Release);
        self.sqpoll_wake.wake();
    }

    pub(crate) fn mark_sqpoll_started(&self) -> AxResult<()> {
        if !self.sqpoll
            || self.sqpoll_stop.load(Ordering::Acquire)
            || self.sqpoll_started.swap(true, Ordering::AcqRel)
        {
            return Err(AxError::BadState);
        }
        self.sqpoll_worker_task
            .store(axtask::current().id().as_u64(), Ordering::Release);
        Ok(())
    }

    pub(crate) fn mark_sqpoll_stopped(&self) {
        self.sqpoll_worker_task.store(0, Ordering::Release);
        self.sqpoll_started.store(false, Ordering::Release);
    }

    /// Acquires the ring owner for an executor that outlives syscall return.
    /// All such executors retain the exact ring generation rather than
    /// rediscovering it through a numeric file descriptor.
    pub(crate) fn arc_owner(&self) -> AxResult<Arc<Self>> {
        self.self_weak
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::BadState)
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
                    .checked_mul(tk_linux_io_uring::CQE_BYTES)
                    .ok_or(AxError::BadState)?,
            )
            .ok_or(AxError::BadState)? as usize;
        let mut bytes = [0_u8; tk_linux_io_uring::CQE_BYTES as usize];
        bytes[0..8].copy_from_slice(&completion.user_data().to_ne_bytes());
        bytes[8..12].copy_from_slice(&completion.result().to_ne_bytes());
        bytes[12..16].copy_from_slice(&completion.flags().to_ne_bytes());
        self.rings.write_bytes(offset, &bytes)?;
        self.cq_tail.store_release(publication.new_tail());
        let eventfd = self.state.lock().completion_eventfd.clone();
        if let Some(eventfd) = eventfd {
            // CQ publication is already visible. Eventfd saturation must not
            // retroactively roll that CQE back; Linux treats this as a
            // coalesced wakeup and readers can still observe the CQ tail.
            let _ = eventfd.signal(1);
        }
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
        let consumed = self
            .state
            .lock()
            .requests
            .observe_completion_head(head)
            .map_err(map_core_error)?;
        if consumed != 0 {
            let bits = usize::BITS as usize;
            let state = self.state.lock();
            for work in state.socket_multishot.iter().flatten() {
                let slot = work.request_id().slot() as usize;
                if let Some(word) = self.poll_hint_bits.get(slot / bits) {
                    word.fetch_or(1_usize << (slot % bits), Ordering::Release);
                }
            }
            drop(state);
            self.poll_hint_pending.store(true, Ordering::Release);
            if let Some(owner) = self.self_weak.get().and_then(Weak::upgrade) {
                owner.enqueue_deferred();
            }
        }
        Ok(consumed)
    }

    pub(crate) fn available_completions(&self) -> AxResult<u32> {
        let state = self.state.lock();
        Ok(state
            .requests
            .completion_tail()
            .wrapping_sub(state.requests.completion_head()))
    }

    pub(crate) fn prepare_submission(
        &self,
        world: crate::task::WorldId,
    ) -> AxResult<SubmissionStep<'_>> {
        self.prepare_submission_with_notifications(
            world,
            #[cfg(feature = "io-submit-batch")]
            None,
        )
    }

    #[cfg(feature = "io-submit-batch")]
    pub(crate) fn prepare_submission_batched<'a>(
        &'a self,
        world: crate::task::WorldId,
        batch: &'a mut dyn SubmissionNotificationFlush,
    ) -> AxResult<SubmissionStep<'a>> {
        self.prepare_submission_with_notifications(world, Some(batch))
    }

    fn prepare_submission_with_notifications<'a>(
        &'a self,
        world: crate::task::WorldId,
        #[cfg(feature = "io-submit-batch")] mut notification_flush: Option<
            &'a mut dyn SubmissionNotificationFlush,
        >,
    ) -> AxResult<SubmissionStep<'a>> {
        self.admit_world(world)?;
        if let Some(issuer) = *self.single_issuer.lock()
            && issuer != axtask::current().id().as_u64()
            && (!self.sqpoll
                || self.sqpoll_worker_task.load(Ordering::Acquire)
                    != axtask::current().id().as_u64())
        {
            return Err(AxError::PermissionDenied);
        }
        #[cfg(feature = "io-submit-batch")]
        let _submission = match self.submission_serial.try_lock() {
            Some(guard) => guard,
            None => {
                if let Some(flush) = notification_flush.as_deref_mut() {
                    flush.flush_notifications();
                }
                self.submission_serial.lock()
            }
        };
        #[cfg(not(feature = "io-submit-batch"))]
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
            .and_then(|index| index.checked_mul(tk_linux_io_uring::SQE_BYTES as usize))
            .ok_or(AxError::BadState)?;
        let mut bytes = [0_u8; tk_linux_io_uring::SQE_BYTES as usize];
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
            #[cfg(feature = "io-submit-batch")]
            notification_flush,
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

    pub(crate) fn issue_request_with_cancellation_mode(
        &self,
        prepared: PreparedRequest,
        mode: tk_linux_io_uring::CancellationMode,
    ) -> Result<IssuedRequest, RequestIssueError> {
        self.state
            .lock()
            .requests
            .issue_with_cancellation_mode(prepared, Some(mode))
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
                let (control, pending, socket_multishot) = {
                    let state = self.state.lock();
                    if let Some(control) = state
                        .polls
                        .get(slot)
                        .and_then(|control| control.as_ref().map(Arc::clone))
                    {
                        (Some(control), false, false)
                    } else {
                        let pending = state.pending_stream.iter().find_map(|entry| {
                            entry
                                .as_ref()
                                .filter(|work| work.request_id().slot() as usize == slot)
                                .map(|work| Arc::clone(&work.control))
                        });
                        let pending = pending.or_else(|| {
                            state.socket_multishot.iter().find_map(|entry| {
                                entry
                                    .as_ref()
                                    .filter(|work| work.request_id().slot() as usize == slot)
                                    .map(|work| Arc::clone(&work.control))
                            })
                        });
                        let socket_multishot = state.socket_multishot.iter().any(|entry| {
                            entry
                                .as_ref()
                                .is_some_and(|work| work.request_id().slot() as usize == slot)
                        });
                        (pending, true, socket_multishot)
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
                    if socket_multishot {
                        self.retry_socket_multishot(control);
                    } else {
                        self.retry_pending_stream(control);
                    }
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
                        let completed = if control.multishot {
                            self.publish_multishot_poll(&control, result).map(|_| ())
                        } else {
                            self.finish_poll_control(&control, TerminalCause::Completed, result)
                                .map(|_| ())
                        };
                        if let Err(error) = completed {
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

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[io_uring]",
        )))
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        if let Some(region) = self.state.lock().registered_wait_region.as_ref()
            && let RegisteredWaitBacking::Kernel { region, .. } = &region.backing
        {
            if let Some(mapping) = region.prepare(request)? {
                return Ok(Some(mapping));
            }
        }
        match self
            .layout
            .mapping_region(request.offset())
            .map_err(map_core_error)?
        {
            MappingRegion::Rings
                if request.offset() == tk_linux_io_uring::IORING_OFF_CQ_RING =>
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
mod adapter_state_tests;

#[cfg(feature = "test-io-control")]
pub(crate) use diagnostics::{
    io_uring_dma_direct_stats_snapshot, reset_io_uring_dma_direct_stats,
    set_io_uring_dma_direct_stats_enabled,
};
