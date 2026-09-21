//! Device completion, reset, and route-custody regressions.

use super::*;

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

    let mut rollback = PhysicalCompletionResetWorks::new();
    rollback.works[0] = Some(PhysicalCompletionResetWork {
        owner: PhysicalCompletionResetOwner {
            device_identity: 0,
            ring: ring.clone(),
            request: reserve_test_request_id(&ring),
            slot,
            generation: 0,
        },
        work,
    });
    rollback.len = 1;
    restore_physical_completion_reset_works(&mut rollback);
    assert_eq!(rollback.len, 0);
    assert!(rollback.works.iter().all(Option::is_none));
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
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
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
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
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
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
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
    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
    PHYSICAL_COMPLETION_WORK_PENDING.store(false, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
}

#[test]
fn quarantined_terminal_keeps_published_owner_until_quiesced_proof() {
    let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
    let _context = crate::test_support::scheduler_test_context();
    // This fixture uses a synthetic PhysicalIoWork without a lower
    // admission, whose test-only device generation is zero. Match its
    // route generation explicitly instead of inheriting another test's
    // global transport generation.
    let previous_generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.swap(0, Ordering::AcqRel);
    let worker_started = PHYSICAL_COMPLETION_WORKER_STARTED.swap(true, Ordering::AcqRel);
    let worker_stopped = PHYSICAL_COMPLETION_WORKER_STOPPED.swap(false, Ordering::AcqRel);
    let device_active = PHYSICAL_COMPLETION_DEVICE_ACTIVE.swap(false, Ordering::AcqRel);
    let reset_pending = PHYSICAL_COMPLETION_RESET_PENDING.swap(false, Ordering::AcqRel);
    let work_pending = PHYSICAL_COMPLETION_WORK_PENDING.swap(false, Ordering::AcqRel);
    let callback_context = register_terminal_callback_test_slot(0);
    let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let request = reserve_test_request_id(&ring);
    let handle = 0xC057_0D1A;
    let route = PhysicalCompletionRouteReservation::new(1).unwrap();
    route.activate_test(&ring, request, 0, handle);
    {
        let mut state = ring.state.lock();
        state.physical_work[0] = Some(PhysicalIoWork {
            ring: Arc::clone(&ring),
            slot: 0,
            issued: None,
            admission: None,
            pending_publication: false,
            test_handle: Some(handle),
        });
        state.physical_work_count = 1;
    }
    // Exercise the production per-device callback mailbox and consumer.
    // The test slot intentionally has no lower handle: a real callback
    // has already supplied this terminal state, so no driver operation
    // is needed to validate upper acknowledgement and custody.
    physical_completion_terminal_notifier(
        callback_context,
        BlockCompletionAvailability::Quarantined { generation: 1 },
    );
    assert_eq!(
        service_physical_completion_reset_for_device(0),
        Err(AxError::BadState)
    );
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
    assert_eq!(physical_completion_route_count(), 1);
    assert!(lookup_physical_completion_route(handle).is_some());
    assert_eq!(ring.physical_worker_len(), 1);

    assert!(physical_completion_terminal_event_for_device(0).is_none());
    {
        let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let slot = registry.slots[0].as_ref().unwrap();
        assert_eq!(slot.terminal_sequence, slot.terminal_consumed_sequence);
    }

    // A later typed Quiesced callback retires that exact generation once.
    physical_completion_terminal_notifier(
        callback_context,
        BlockCompletionAvailability::Live { generation: 2 },
    );
    service_physical_completion_reset_for_device(0).unwrap();
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    assert_eq!(physical_completion_route_count(), 0);
    assert!(lookup_physical_completion_route(handle).is_none());
    assert_eq!(ring.physical_worker_len(), 0);

    remove_terminal_callback_test_slot(callback_context);
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(previous_generation, Ordering::Release);
    PHYSICAL_COMPLETION_WORKER_STARTED.store(worker_started, Ordering::Release);
    PHYSICAL_COMPLETION_WORKER_STOPPED.store(worker_stopped, Ordering::Release);
    PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(device_active, Ordering::Release);
    PHYSICAL_COMPLETION_RESET_PENDING.store(reset_pending, Ordering::Release);
    PHYSICAL_COMPLETION_WORK_PENDING.store(work_pending, Ordering::Release);
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
    let first =
        physical_completion_terminal_event_for_reset(BlockResetOutcome::Quiesced, generation + 1);
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

    PHYSICAL_COMPLETION_TERMINAL_STATE.store(PHYSICAL_COMPLETION_TERMINAL_NONE, Ordering::Release);
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
fn terminal_route_mismatch_retains_work_and_issued_proof_without_cqe() {
    let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
    let _context = crate::test_support::scheduler_test_context();
    let layout = SetupRequest::new(2, 0, SetupFlags::NO_SQARRAY)
        .resolve(FeatureFlags::EMPTY)
        .unwrap();
    let ring = IoUring::try_new(layout).unwrap();
    let issued = issue_test_request(&ring);
    let request = issued.id();
    let route = PhysicalCompletionRouteReservation::new(1).unwrap();
    route.activate_test(&ring, request, 0, 0xA100);
    {
        let mut state = ring.state.lock();
        state.physical_work_count = 1;
        state.physical_slot_reserved[0] = true;
    }
    let work = PhysicalIoWork {
        ring: Arc::clone(&ring),
        slot: 0,
        issued: Some(issued),
        admission: None,
        pending_publication: false,
        test_handle: Some(0xA100),
    };
    assert!(matches!(
        ring.release_terminal_physical_routes(work, Some(0xA101)),
        Err(AxError::BadState)
    ));
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
    let work = {
        let mut state = ring.state.lock();
        assert_eq!(state.physical_work_count, 1);
        assert!(state.physical_slot_reserved[0]);
        assert_eq!(state.requests.progress().unwrap().completion_pending(), 0);
        let work = state
            .physical_custody
            .iter_mut()
            .find_map(Option::take)
            .unwrap();
        assert!(state.requests.issued_is_live(work.issued().unwrap()));
        work
    };
    // The exact handle can retire the quarantined route, preserving the
    // issued proof until the caller performs the terminal transition.
    let mut work = ring
        .release_terminal_physical_routes(work, Some(0xA100))
        .unwrap();
    let issued = work.take_issued().unwrap();
    drop(work);
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    assert_eq!(ring.state.lock().physical_work_count, 0);
    ring.complete_issued(issued, TerminalCause::Completed, 4096, 0)
        .unwrap();
    assert_eq!(ring.cq_tail.load_acquire(), 1);
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

    let routes = PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS).unwrap();
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
fn physical_route_reservation_is_per_device_qd_limited() {
    let _router = PHYSICAL_COMPLETION_TEST_LOCK.lock();
    let mut reservations = Vec::new();
    for _ in 0..IO_URING_PHYSICAL_MAX_QD {
        reservations
            .push(PhysicalCompletionRouteReservation::new(IO_URING_PHYSICAL_MAX_EXTENTS).unwrap());
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
fn physical_router_quarantine_capacity_is_isolated_per_device() {
    let failed_device = 0xDEAD;
    let healthy_device = 0xBEEF;
    let mut router = PhysicalCompletionRouter::new();

    assert_eq!(
        router.groups.len(),
        PHYSICAL_COMPLETION_MAX_DEVICES * IO_URING_PHYSICAL_MAX_QD
    );
    for index in 0..IO_URING_PHYSICAL_MAX_QD {
        let group = &mut router.groups[index];
        *group = Some(PhysicalCompletionRouteGroup::reserved(failed_device, 7, 1));
        router.quarantine[index] = Some(QuarantinedPhysicalCompletion {
            completion: PhysicalIoCompletion {
                handle: index as u64 + 1,
                cookie: index as u64 + 1,
                bytes: 4096,
                success: false,
            },
            device_identity: failed_device,
            replayable: false,
        });
    }

    assert!(!physical_completion_device_group_capacity_available(
        &router,
        failed_device
    ));
    assert!(!physical_completion_device_quarantine_capacity_available(
        &router,
        failed_device
    ));
    // The failed device has consumed all of its QD entries, but a healthy
    // sibling still has its own deterministic QD namespace. This is the
    // property that prevents permanent custody on one device from
    // globally closing physical-DMA admission.
    assert!(physical_completion_device_group_capacity_available(
        &router,
        healthy_device
    ));
    assert!(physical_completion_device_quarantine_capacity_available(
        &router,
        healthy_device
    ));
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
    // This test models a lower owner without a full admission object, so
    // its synthetic work generation is zero. Establish the matching
    // synthetic route generation explicitly; other tests legitimately
    // advance the global generation before this fixture runs.
    let previous_generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.swap(0, Ordering::AcqRel);
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
    // Exercise the real close lifecycle before parking at the completion
    // phase. Merely setting the final-close flag is not enough: request
    // retirement may enqueue an EIO token, and `begin_draining` accepts
    // it only after `begin_close` has established the Closing state.
    ring.request_final_close();
    assert!(!ring.begin_final_close_step().unwrap());
    ring.state.lock().final_close.phase = FinalClosePhase::Completions;

    assert!(physical_completion_has_quarantined_route());
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 1);
    assert_eq!(ring.physical_worker_len(), 1);
    // VirtIO returns `Retired` after proving queue quiescence and
    // dismantling the transport.  It must take the same upper retirement
    // path as `Quiesced`; only re-enable is skipped by the production
    // reset wrapper.
    retire_physical_completion_after_reset(axdriver::prelude::BlockResetOutcome::Retired).unwrap();
    assert!(!physical_completion_has_quarantined_route());
    assert_eq!(PHYSICAL_COMPLETION_ROUTER.lock().work_count, 0);
    assert_eq!(ring.physical_worker_len(), 0);

    // Reset retirement queues a typed EIO terminal but does not require a
    // CQE publication while final close is already in progress.  The
    // normal close-completions step can now discard it and finish.
    assert!(ring.close_completions_step().unwrap());
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(previous_generation, Ordering::Release);
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
    let generation = PHYSICAL_COMPLETION_DEVICE_GENERATION.load(Ordering::Acquire);
    note_physical_completion_worker_stopped();
    assert!(stop_physical_completion_device().is_ok());
    assert!(!physical_completion_device_ready());
    assert_eq!(
        reset_physical_completion_device(),
        Err(AxError::OperationNotSupported)
    );
    note_physical_completion_worker_started();
    PHYSICAL_COMPLETION_DEVICE_GENERATION.store(generation, Ordering::Release);
}
