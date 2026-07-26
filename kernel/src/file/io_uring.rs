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
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use axerrno::{AxError, AxResult, LinuxError};
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
    CancelSelector, CompletionPublication, CompletionToken, CopiedSubmission, FileSlot,
    FileTableId, IoUringError, IssuedRequest, LeaseRelease, MappingRegion, ParsedSubmission,
    PreparedRequest, RegisteredFileLease, RegisteredFileTable, RequestId, RequestIssueError,
    RequestRegistry, RequestReservation, RingId, RingLayout, TerminalCause,
};

use super::{
    DescriptionResource, FileDescription, FileHandle, FileLike, FileMmapRequest,
    FixedSharedMmapRegion, Kstat, PreparedFileMmap, SharedPages, anon_inode_stat,
};
use crate::mm::SharedAtomicU32;

const RING_WAITER_SLOTS: usize = 64;
const PAGE_BYTES: usize = PageSize::Size4K as usize;
const IO_URING_GLOBAL_REQUEST_SLOTS: usize = 65_536;
const IO_URING_GLOBAL_FIXED_FILE_SLOTS: usize = 65_536;
const FINAL_CLOSE_STEP_BUDGET: usize = 64;
const POLL_ALWAYS_REPORTED: IoEvents = IoEvents::ALWAYS;

static NEXT_RING_ID: AtomicU64 = AtomicU64::new(1);
static IO_URING_REQUEST_SLOTS: AtomicUsize = AtomicUsize::new(0);
static IO_URING_FIXED_FILE_SLOTS: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_IO_URING_WORK: AtomicPtr<IoUring> = AtomicPtr::new(ptr::null_mut());

struct RequestSlotCharge(usize);

impl RequestSlotCharge {
    fn try_new(slots: usize) -> AxResult<Self> {
        IO_URING_REQUEST_SLOTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
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

struct RegisteredFiles {
    table: RegisteredFileTable<FileDescription>,
    _charge: FixedFileSlotCharge,
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

fn allocate_ring_id() -> AxResult<RingId> {
    let raw = NEXT_RING_ID
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
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
        | IoUringError::Busy => AxError::ResourceBusy,
        IoUringError::Closing | IoUringError::Draining | IoUringError::Closed => {
            AxError::BadFileDescriptor
        }
        IoUringError::InvalidFileSlot
        | IoUringError::FileSlotEmpty
        | IoUringError::UnknownFileLease
        | IoUringError::FileTableNotPublished => AxError::BadFileDescriptor,
        IoUringError::CancellationTargetNotFound => AxError::NotFound,
        IoUringError::RegisteredBuffersUnsupported
        | IoUringError::UnsupportedOpcode
        | IoUringError::UnsupportedSubmissionFlags
        | IoUringError::UnsupportedOperationFlags
        | IoUringError::CurrentPositionUnsupported
        | IoUringError::UnsupportedRegistration => AxError::OperationNotSupported,
        IoUringError::Overflow | IoUringError::GenerationExhausted => AxError::OutOfRange,
        _ => AxError::InvalidInput,
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
    next_file_table_id: u64,
    polls: Vec<Option<Arc<PollControl>>>,
    pending_publications: Vec<Option<CompletionToken>>,
    final_close: FinalCloseProgress,
}

/// One accepted SQ entry after terminal credit and SQ-head publication.
pub(crate) struct SubmissionWork {
    prepared: PreparedRequest,
    parsed: Result<ParsedSubmission, IoUringError>,
    file: Option<IoUringFileLease>,
}

impl SubmissionWork {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PreparedRequest,
        Result<ParsedSubmission, IoUringError>,
        Option<IoUringFileLease>,
    ) {
        (self.prepared, self.parsed, self.file)
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

    pub(crate) fn commit(mut self, file: Option<IoUringFileLease>) -> AxResult<SubmissionWork> {
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
        })
    }

    pub(crate) fn commit_poll(self, lease: IoUringFileLease, linux_events: u32) -> AxResult<()> {
        let ring = self.ring;
        ring.commit_poll_admission(self, lease, linux_events)
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
    poll_hint_bits: Vec<AtomicUsize>,
    pending_publication_count: AtomicUsize,
    state: Mutex<RingState>,
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
        let ring_region = FixedSharedMmapRegion::try_new(
            thekernel_linux_io_uring::IORING_OFF_SQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let cq_ring_region = FixedSharedMmapRegion::try_new(
            thekernel_linux_io_uring::IORING_OFF_CQ_RING,
            Arc::clone(&rings),
            super::FileMmapProtection::READ | super::FileMmapProtection::WRITE,
        )?;
        let sqe_region = FixedSharedMmapRegion::try_new(
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
        let hint_words = request_slots.div_ceil(usize::BITS as usize);
        let mut poll_hint_bits = Vec::new();
        poll_hint_bits
            .try_reserve_exact(hint_words)
            .map_err(|_| AxError::NoMemory)?;
        for _ in 0..hint_words {
            poll_hint_bits.push(AtomicUsize::new(0));
        }

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
            poll_hint_bits,
            pending_publication_count: AtomicUsize::new(0),
            state: Mutex::new(RingState {
                requests,
                sq_head: 0,
                sq_dropped: 0,
                admission_in_progress: false,
                fixed_files: None,
                next_file_table_id: 1,
                polls,
                pending_publications,
                final_close: FinalCloseProgress::new(),
            }),
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
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
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
                return Ok(false);
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
        loop {
            let retired = {
                let mut state = self.state.lock();
                let files = state.fixed_files.as_mut().ok_or(AxError::BadState)?;
                let Some(token) = files.table.next_retirable().map_err(map_core_error)? else {
                    break;
                };
                files.table.retire(token).map_err(map_core_error)?
            };
            drop(retired);
        }
        let closed = {
            let mut state = self.state.lock();
            let files = state.fixed_files.as_mut().ok_or(AxError::BadState)?;
            if files.table.progress().map_err(map_core_error)?.empty() {
                files.table.finish_retire().map_err(map_core_error)?;
                state.fixed_files.take()
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
                    state.final_close.enter(FinalClosePhase::Completions);
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
            FinalClosePhase::Completions => self.close_completions_step(),
            FinalClosePhase::Finished => Ok(true),
        }
    }

    fn commit_poll_admission(
        &self,
        mut admission: SubmissionAdmission<'_>,
        lease: IoUringFileLease,
        linux_events: u32,
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
                let work = admission.commit(None)?;
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
                let work = admission.commit(None)?;
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
        for (word_index, word) in self.poll_hint_bits.iter().enumerate() {
            let mut hinted = word.swap(0, Ordering::AcqRel);
            while hinted != 0 {
                let bit = hinted.trailing_zeros() as usize;
                hinted &= hinted - 1;
                let slot = word_index * word_bits + bit;
                let control = self
                    .state
                    .lock()
                    .polls
                    .get(slot)
                    .and_then(|control| control.as_ref().map(Arc::clone));
                let Some(control) = control else {
                    continue;
                };
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
        if final_close_pending || ring.poll_hint_pending.load(Ordering::Acquire) {
            ring.enqueue_deferred();
        }
        node = next;
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
mod adapter_state_tests {
    use super::*;

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
}
