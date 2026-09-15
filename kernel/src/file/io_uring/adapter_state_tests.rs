use alloc::vec;

use tk_linux_io_uring::{
    FeatureFlags, RequestDescriptor, RequestOperation, SetupFlags, SetupRequest,
};

use super::*;

// The completion router is intentionally device-global. Serialize tests
// that install synthetic routes so parallel unit tests cannot make one
// test observe another test's bounded QD custody.
static PHYSICAL_COMPLETION_TEST_LOCK: spin::Mutex<()> = spin::Mutex::new(());

#[test]
fn poll_irq_callbacks_queue_weak_tokens_and_preserve_budget_continuation() {
    let _context = crate::test_support::scheduler_test_context();
    let _serial = PHYSICAL_COMPLETION_TEST_LOCK.lock();
    let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let request = issue_test_request(&ring).id();
    let weak = Arc::downgrade(&ring);
    let mut callbacks = Vec::new();
    for _ in 0..POLL_CALLBACK_BUDGET + 1 {
        callbacks.push(Arc::new(PollCallbackState {
            ring: weak.clone(),
            request,
            enabled: AtomicBool::new(true),
            source_woke: AtomicBool::new(false),
            deferred_next: AtomicPtr::new(ptr::null_mut()),
            deferred_queued: AtomicBool::new(false),
        }));
    }
    {
        let irq_gate = SpinNoIrq::new(());
        let _irq = irq_gate.lock();
        for callback in &callbacks {
            callback.publish_source_wake();
            callback.publish_source_wake(); // one node per token
        }
        assert_eq!(Arc::strong_count(&ring), 1);
        assert!(!ring.poll_hint_pending.load(Ordering::Acquire));
    }
    // The callback queue must not postpone task-context destruction or
    // upgrade a dead ring when its delayed notifications are dispatched.
    drop(ring);
    assert!(weak.upgrade().is_none());
    drain_deferred_poll_callbacks();
    assert_eq!(
        callbacks
            .iter()
            .filter(|callback| callback.deferred_queued.load(Ordering::Acquire))
            .count(),
        1
    );
    assert!(has_deferred_io_uring_work());
    drain_deferred_poll_callbacks();
    assert!(
        callbacks
            .iter()
            .all(|callback| !callback.deferred_queued.load(Ordering::Acquire))
    );
}

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

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"io-uring-fixed-file-test",
        )))
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

#[cfg(feature = "io-submit-batch")]
struct BatchWakeCount(core::sync::atomic::AtomicUsize);

#[cfg(feature = "io-submit-batch")]
impl alloc::task::Wake for BatchWakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(feature = "io-submit-batch")]
#[test]
fn submission_batch_keeps_cq_visible_and_unrelated_completion_notifies() {
    let _context = crate::test_support::scheduler_test_context();
    let layout = SetupRequest::new(4, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let wakes = Arc::new(BatchWakeCount(core::sync::atomic::AtomicUsize::new(0)));
    let waker = core::task::Waker::from(wakes.clone());
    let _registration = ring.completion_wait.register(&waker).unwrap();
    let mut batch = SubmissionCompletionBatch::new(&ring);
    let first = issue_test_request(&ring);
    let id = first.id();
    drop(first);
    ring.complete_request_batched(id, TerminalCause::Completed, 11, 0, &mut batch)
        .unwrap();
    assert_eq!(ring.cq_tail.load_acquire(), 1);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    // Models an independent async producer: it has no batch token.
    let second = issue_test_request(&ring);
    ring.complete_issued(second, TerminalCause::Completed, 22, 0)
        .unwrap();
    assert_eq!(ring.cq_tail.load_acquire(), 2);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    let _registration = ring.completion_wait.register(&waker).unwrap();
    // An error cannot discard the first completion's deferred wake.
    assert!(
        ring.complete_request_batched(id, TerminalCause::Completed, 0, 0, &mut batch)
            .is_err()
    );
    drop(batch);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 2);
}

#[cfg(feature = "io-submit-batch")]
#[test]
fn submission_batch_flushes_at_bounded_size() {
    let _context = crate::test_support::scheduler_test_context();
    let layout = SetupRequest::new(32, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let wakes = Arc::new(BatchWakeCount(core::sync::atomic::AtomicUsize::new(0)));
    let waker = core::task::Waker::from(wakes.clone());
    let _registration = ring.completion_wait.register(&waker).unwrap();
    let mut batch = SubmissionCompletionBatch::new(&ring);
    for _ in 0..32 {
        let request = issue_test_request(&ring);
        let id = request.id();
        drop(request);
        ring.complete_request_batched(id, TerminalCause::Completed, 0, 0, &mut batch)
            .unwrap();
    }
    assert_eq!(ring.cq_tail.load_acquire(), 32);
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert_eq!(batch.pending, 0);
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

struct BufferRetirementWake {
    ring: Arc<IoUring>,
    retired: AtomicBool,
}

impl Wake for BufferRetirementWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // This is the first synchronous CQ notification, not a check
        // after the completing executor has returned and dropped locals.
        self.retired.fetch_or(
            self.ring.cq_tail.load_acquire() == 1
                && self.ring.state.lock().registered_buffers.is_none(),
            Ordering::Release,
        );
    }
}

struct CancelledFileIoControl;

impl axfs_ng_vfs::SubmittedFileIoControl for CancelledFileIoControl {
    fn cancel(self: Box<Self>) -> axfs_ng_vfs::FileIoCancelOutcome {
        axfs_ng_vfs::FileIoCancelOutcome::Cancelled
    }
}

// A synchronous scheduling edge at the start of lease destruction models
// another CPU draining pending CQEs before that destruction can acquire the
// ring lock. Target one ring so unrelated parallel tests remain unaffected.
static BUFFER_RETIREMENT_PUBLISHER: spin::Mutex<Option<Weak<IoUring>>> = spin::Mutex::new(None);

pub(super) fn publish_at_buffer_retirement(ring: &Arc<IoUring>) {
    let selected = BUFFER_RETIREMENT_PUBLISHER
        .lock()
        .as_ref()
        .is_some_and(|selected| selected.ptr_eq(&Arc::downgrade(ring)));
    if selected {
        ring.flush_pending_publications_now().unwrap();
        assert_eq!(
            ring.cq_tail.load_acquire(),
            0,
            "CQE visible before buffer retirement"
        );
        assert_eq!(ring.pending_publication_count.load(Ordering::Acquire), 0);
    }
}

struct BufferRetirementPublisher;

impl Drop for BufferRetirementPublisher {
    fn drop(&mut self) {
        *BUFFER_RETIREMENT_PUBLISHER.lock() = None;
    }
}

#[test]
fn fixed_buffer_retires_before_terminal_cq_notification() {
    check_fixed_buffer_retirement(false);
}

#[test]
fn fixed_buffer_retires_before_independent_pending_cq_drain() {
    check_fixed_buffer_retirement(true);
}

fn check_fixed_buffer_retirement(interleave_publication: bool) {
    use axhal::paging::MappingFlags;
    use memory_addr::VirtAddr;

    let _context = crate::test_support::scheduler_test_context();
    for path in [
        "owned",
        "stream",
        "stream-rearm-error",
        "cancel",
        "close",
        "stream-cancel",
        "stream-close",
        "stream-worker-rearm",
        "stream-worker-close",
    ] {
        let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let base = VirtAddr::from(0x1000);
        let mut space = crate::mm::AddrSpace::new_empty(base, PAGE_BYTES).unwrap();
        space
            .map(
                base,
                PAGE_BYTES,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                crate::mm::Backend::new_alloc(base, PageSize::Size4K),
            )
            .unwrap();
        let capability = UserMemoryCapability::new(Arc::new(Mutex::new(space)));
        ring.register_buffers(ring.world, &capability, vec![(0x1000, PAGE_BYTES)])
            .unwrap();
        let buffer = ring
            .acquire_registered_buffer(ring.world, BufferSlot::new(0), 0x1000, 32)
            .unwrap();
        ring.unregister_buffers().unwrap();
        assert!(ring.state.lock().registered_buffers.is_some());
        let _publisher = BufferRetirementPublisher;
        if interleave_publication {
            *BUFFER_RETIREMENT_PUBLISHER.lock() = Some(Arc::downgrade(&ring));
        }
        let wake = Arc::new(BufferRetirementWake {
            ring: ring.clone(),
            retired: AtomicBool::new(false),
        });
        let waker = Waker::from(wake.clone());
        let _registration = ring.completion_wait.register(&waker).unwrap();
        let issued = if path == "cancel" || path == "close" {
            let mut state = ring.state.lock();
            let reservation = state
                .requests
                .reserve(RequestDescriptor::new(7, RequestOperation::Read))
                .unwrap();
            let prepared = state.requests.commit(reservation).unwrap();
            state
                .requests
                .issue_with_cancellation_mode(
                    prepared,
                    Some(tk_linux_io_uring::CancellationMode::ProviderControlled),
                )
                .unwrap()
        } else {
            issue_test_request(&ring)
        };
        if path == "owned" {
            ring.complete_owned_immediate(issued, 32, Some(buffer))
                .unwrap();
        } else if path == "cancel" || path == "close" {
            let id = issued.id();
            ring.state.lock().owned_file_io[id.slot() as usize] = Some(OwnedFileIoOwner {
                id,
                state: OwnedFileIoControlState::Submitted {
                    generation: id.generation(),
                    bridge: OwnedFileIoBridge { issued },
                    control: SubmittedFileIo::new(Box::new(CancelledFileIoControl)),
                },
                buffer: Some(buffer),
            });
            if path == "cancel" {
                ring.cancel_request(issue_test_request(&ring), 7).unwrap();
            } else {
                ring.state
                    .lock()
                    .final_close
                    .enter(FinalClosePhase::OwnedFileIo);
                ring.close_owned_file_io_step().unwrap();
                // Reopen only this synthetic close phase for the final
                // registration assertion; production close is terminal.
                ring.state.lock().final_close.phase = FinalClosePhase::Begin;
            }
        } else {
            let description = FileDescription::new(Arc::new(FixedFileTestObject)).unwrap();
            let context = description.capture_io_operation_context(
                VfsSecurityContext::new(
                    crate::task::Cred::try_root(
                        crate::task::UserNamespace::try_new_root().unwrap(),
                    )
                    .unwrap(),
                ),
                FanotifyEventActor::default(),
            );
            let control = PollControl::try_new(
                Arc::downgrade(&ring),
                issued.id(),
                IoUringFileLease::Descriptor(description),
                pending_stream_events(),
                false,
            )
            .unwrap_or_else(|_| panic!("control allocation failed"));
            let mut bytes = [0; tk_linux_io_uring::SQE_BYTES as usize];
            bytes[0] = 4; // IORING_OP_READ_FIXED
            let tk_linux_io_uring::SubmissionOperation::Read(request) =
                ParsedSubmission::parse(bytes).unwrap().operation()
            else {
                unreachable!()
            };
            if path == "stream" || path == "stream-rearm-error" {
                ring.state
                    .lock()
                    .requests
                    .begin_side_effect(&issued)
                    .unwrap();
            }
            let work = PendingStreamWork {
                slot: issued.id().slot() as usize,
                issued: Some(issued),
                request,
                control,
                buffer,
                context,
                capability: capability.clone(),
            };
            match path {
                "stream" => ring.complete_pending_stream_work(work, 32),
                "stream-cancel"
                | "stream-close"
                | "stream-worker-rearm"
                | "stream-worker-close" => {
                    ring.state.lock().pending_stream[0] = Some(work);
                    ring.state.lock().pending_stream_count = 1;
                    if path.starts_with("stream-worker-") {
                        let id = ring.state.lock().pending_stream[0]
                            .as_ref()
                            .unwrap()
                            .request_id();
                        let work = ring.claim_pending_stream(id).unwrap();
                        assert!(
                            matches!(
                                ring.state
                                    .lock()
                                    .requests
                                    .claim_cancel(CancelSelector::UserData(0), None),
                                Err(IoUringError::CancellationTargetNotFound)
                            ),
                            "private stream worker must remain uncancellable"
                        );
                        if path == "stream-worker-close" {
                            ring.final_close_requested.store(true, Ordering::Release);
                            let work = ring.reinsert_pending_stream(work).err().unwrap();
                            assert!(matches!(
                                ring.state
                                    .lock()
                                    .requests
                                    .claim_cancel(CancelSelector::UserData(0), None),
                                Err(IoUringError::CancellationTargetNotFound)
                            ));
                            // The flag above tests the real reinsert guard;
                            // clear it before completion to avoid enqueuing
                            // synthetic teardown work in the global worker.
                            ring.final_close_requested.store(false, Ordering::Release);
                            ring.complete_pending_stream_error(work, AxError::BadState);
                        } else {
                            assert!(ring.reinsert_pending_stream(work).is_ok());
                            ring.cancel_request(issue_test_request(&ring), 0).unwrap();
                        }
                    } else if path == "stream-cancel" {
                        ring.cancel_request(issue_test_request(&ring), 0).unwrap();
                    } else {
                        ring.state.lock().final_close.enter(FinalClosePhase::Polls);
                        ring.close_polls_step().unwrap();
                        ring.state.lock().final_close.phase = FinalClosePhase::Begin;
                        ring.flush_pending_publications_now().unwrap();
                    }
                }
                _ => ring.complete_pending_stream_error(work, AxError::NoMemory),
            }
        }
        assert!(
            wake.retired.load(Ordering::Acquire),
            "{path}: CQE preceded lease retirement"
        );
        ring.register_buffers(ring.world, &capability, vec![(0x1000, PAGE_BYTES)])
            .unwrap();
        ring.unregister_buffers().unwrap();
    }
}

#[test]
fn owned_publishing_cancel_waits_for_real_completion_and_reuses_slot() {
    let _context = crate::test_support::scheduler_test_context();
    for result in [4096, -LinuxError::EIO.code()] {
        let layout = SetupRequest::new(4, 0, SetupFlags::NO_SQARRAY)
            .resolve(FeatureFlags::EMPTY)
            .unwrap();
        let ring = IoUring::try_new(layout).unwrap();
        let id = {
            let mut state = ring.state.lock();
            let reservation = state
                .requests
                .reserve(RequestDescriptor::new(7, RequestOperation::Read))
                .unwrap();
            let prepared = state.requests.commit(reservation).unwrap();
            let issued = state
                .requests
                .issue_with_cancellation_mode(
                    prepared,
                    Some(tk_linux_io_uring::CancellationMode::ProviderControlled),
                )
                .unwrap();
            let id = issued.id();
            state.owned_file_io[id.slot() as usize] = Some(OwnedFileIoOwner {
                id,
                state: OwnedFileIoControlState::Publishing {
                    generation: id.generation(),
                    bridge: OwnedFileIoBridge { issued },
                },
                buffer: None,
            });
            id
        };
        // Pause submit at its unlocked Publishing edge. Exercise the
        // actual adapter's provider-selector -> generic-cancel fallback.
        let cancel = issue_test_request(&ring);
        ring.cancel_request(cancel, 7).unwrap();
        assert_eq!(ring.cq_tail.load_acquire(), 1); // cancel CQE only
        {
            let state = ring.state.lock();
            assert!(state.owned_file_io[id.slot() as usize].is_some());
            assert_eq!(
                state.requests.request(id).unwrap().1,
                tk_linux_io_uring::RequestState::Issued(
                    tk_linux_io_uring::CancellationMode::ProviderControlled
                )
            );
        }
        // Both a callback made synchronously inside submit and submit's
        // failure-retirement path reach this same real completion bridge.
        ring.complete_owned_file_io(id, result);
        assert!(ring.state.lock().owned_file_io[id.slot() as usize].is_none());
        assert_eq!(ring.cq_tail.load_acquire(), 2);
        let replacement = issue_test_request(&ring);
        assert_eq!(replacement.id().slot(), id.slot());
        assert_ne!(replacement.id().generation(), id.generation());
        ring.complete_owned_file_io(id, result);
        assert_eq!(ring.cq_tail.load_acquire(), 2);
        ring.complete_issued(replacement, TerminalCause::Completed, 0, 0)
            .unwrap();
        assert_eq!(ring.cq_tail.load_acquire(), 3);
    }
}

fn register_terminal_callback_test_slot(generation: u64) -> usize {
    let callback_context = allocate_physical_completion_callback_context().unwrap();
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    assert!(registry.slots.iter().all(Option::is_none));
    registry.slots[0] = Some(PhysicalCompletionDeviceSlot {
        identity: 0,
        callback_context,
        device: None,
        generation,
        configured: true,
        active: true,
        admission_open: true,
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
    callback_context
}

fn remove_terminal_callback_test_slot(callback_context: usize) {
    let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    let index = registry
        .slots
        .iter()
        .position(|slot| {
            slot.as_ref()
                .is_some_and(|slot| slot.identity == 0 && slot.callback_context == callback_context)
        })
        .expect("terminal callback test slot");
    registry.slots[index] = None;
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
    let count = clip_registered_physical_segments(&segments, 0x400, 0x2c00, &mut physical).unwrap();
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
        IO_URING_PHYSICAL_MAX_SEGMENTS + 1,
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
fn foreign_world_is_rejected_before_submission_or_buffer_lookup() {
    let _context = crate::test_support::scheduler_test_context();
    let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let world = crate::task::WorldId::FOREIGN_TEST;
    // A zero-filled SQE is a valid NOP. Publish it before trying the
    // foreign actor so rejection proves pending work was not consumed.
    ring.sq_tail.store_release(1);
    let head = ring.state.lock().sq_head;
    assert_eq!(ring.admit_world(world), Err(AxError::PermissionDenied));
    assert!(matches!(
        ring.prepare_submission(world),
        Err(AxError::PermissionDenied)
    ));
    assert_eq!(ring.state.lock().sq_head, head);
    // Permission failure must precede descriptor lookup even with no
    // registered table; a foreign actor cannot probe ring resources.
    assert!(matches!(
        ring.acquire_registered_buffer(world, BufferSlot::new(0), 0x1000, 32),
        Err(AxError::PermissionDenied)
    ));
    assert!(matches!(
        ring.prepare_submission(crate::task::WorldId::BOOT),
        Ok(SubmissionStep::Admission(_))
    ));
}

#[test]
fn registered_world_owner_waits_for_cancel_and_completion_leases() {
    let world = Arc::new(crate::task::WorldId::BOOT);
    let weak = Arc::downgrade(&world);
    let mut table = RegisteredBufferTable::new(
        RingId::new(1).unwrap(),
        BufferTableId::new(1).unwrap(),
        1,
        2,
    )
    .unwrap();
    table
        .install(BufferSlot::new(0), 0x1000, 4096, world)
        .unwrap();
    table.publish().unwrap();
    let cancelled = table.acquire(BufferSlot::new(0), 0x1000, 32).unwrap();
    let completed = table.acquire(BufferSlot::new(0), 0x1020, 32).unwrap();
    table.begin_retire().unwrap();
    assert!(table.acquire(BufferSlot::new(0), 0x1000, 32).is_err());
    assert!(table.finish_retire().is_err());
    drop(table.release(cancelled).unwrap());
    assert_eq!(**completed.owner(), crate::task::WorldId::BOOT);
    assert!(weak.upgrade().is_some());
    assert!(table.finish_retire().is_err());
    drop(table.release(completed).unwrap());
    table.finish_retire().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn retired_buffer_lease_cannot_recover_world_authority() {
    let _context = crate::test_support::scheduler_test_context();
    let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    // The lease's owner is consumed by retirement. Retaining a ring Arc
    // must not make an already consumed buffer usable again.
    let lease = IoUringBufferLease {
        ring: ring.clone(),
        owner: IoUringBufferLeaseOwner::Registered(None),
        provided_return_on_drop: true,
    };
    assert_eq!(lease.validate_world(ring.world), Err(AxError::BadState));
    assert_eq!(lease.range(), Err(AxError::BadState));
    assert!(matches!(lease.capability(), Err(AxError::BadState)));
    #[cfg(feature = "io-submit-batch")]
    assert!(matches!(
        lease.capability_and_range(),
        Err(AxError::BadState)
    ));
    ring.request_final_close();
    assert_eq!(ring.world, crate::task::WorldId::BOOT);
    assert_eq!(lease.validate_world(ring.world), Err(AxError::BadState));
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

mod physical_completion;
