//! Device installation, reset service, and completion worker scheduling.

use super::*;

/// Converts one bounded lower-device drain into the exact completion records
/// consumed by the physical effect router. The lower wait is device-global,
/// so ordinary completions are never removed by this callback; the lower
/// mailbox's physical-owner filter guarantees that every record here belongs
/// to a published physical request.
pub(super) fn shared_block_completion_waiter_for_device(
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
pub(super) fn install_additional_physical_completion_device(
    mut device: SharedBlockDevice,
) -> AxResult<()> {
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
                    .and_then(|slot| slot.device.clone());
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
            && slot.device.as_ref().is_some_and(|device| {
                matches!(
                    device.completion_availability(),
                    BlockCompletionAvailability::Live { generation }
                        if generation == slot.generation
                )
            })
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

pub(super) fn physical_completion_in_flight() -> usize {
    PHYSICAL_COMPLETION_ADMISSION_STATE.lock().in_flight
}

/// Schedules a later reset/terminal-custody pass.  The one-millisecond task
/// sleep is deliberately outside the lower/device lock and gives an admitted
/// submitter time to finish its publication/route commit; a direct wake here
/// would turn a persistent `ResourceBusy` into a busy loop.
pub(super) fn defer_physical_completion_reset_retry() {
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
pub(super) fn retire_physical_completion_after_external_terminal() -> AxResult<()> {
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
pub(super) fn finish_physical_completion_external_terminal_event(
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
pub(super) fn service_physical_completion_reset() -> AxResult<()> {
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

pub(super) fn physical_completion_in_flight_for_device(device_identity: usize) -> usize {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .map_or(0, |slot| slot.in_flight)
}

pub(super) fn finish_physical_completion_external_terminal_event_for_device(
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

pub(super) fn stabilize_physical_completion_overflow(device_identity: usize) {
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

/// Ends one explicit reset attempt that failed before the lower layer
/// produced a typed physical-retirement proof. This is deliberately not a
/// retirement path: every route, effect, pin, cache lease, and device credit
/// remains owned by the fenced slot. Clearing only `reset_pending` prevents a
/// reset-unsupported or silent device from turning the shared worker into an
/// infinite self-wake loop. A future management request may set the bit
/// again, and a later lower terminal callback is still processed normally.
pub(super) fn stabilize_physical_completion_reset_failure(device_identity: usize) {
    {
        let mut registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
        let Some(slot) = registry
            .slots
            .iter_mut()
            .flatten()
            .find(|slot| slot.identity == device_identity)
        else {
            return;
        };
        slot.active = false;
        slot.admission_open = false;
        slot.reset_pending = false;
    }
    if device_identity == physical_completion_default_identity() {
        PHYSICAL_COMPLETION_DEVICE_ACTIVE.store(false, Ordering::Release);
        PHYSICAL_COMPLETION_RESET_PENDING.store(false, Ordering::Release);
    }
}

/// Applies an exact per-device lower terminal proof to upper physical-I/O
/// custody.  Both the callback-first and reset-return paths use this helper:
/// a quarantine marker is deliberately not a proof, while Quiesced/Retired
/// first fence the exact generation and then retire every matching route/work
/// owner as one transaction.  Slot/mailbox acknowledgement remains with the
/// caller because it is tied to the particular lower notification source.
pub(super) fn retire_physical_completion_terminal_owners_for_device(
    device_identity: usize,
    event: PhysicalCompletionTerminalEvent,
) -> AxResult<BlockResetOutcome> {
    if event.state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
        return Err(AxError::BadState);
    }
    if physical_completion_in_flight_for_device(device_identity) != 0 {
        defer_physical_completion_reset_retry();
        return Err(AxError::ResourceBusy);
    }
    let outcome = physical_completion_terminal_outcome(event.state).ok_or(AxError::BadState)?;
    physical_completion_generation_store_for_device(device_identity, event.generation);
    retire_physical_completion_after_reset_for_device(device_identity, outcome)?;
    Ok(outcome)
}

/// Services one registry slot without consulting root-device globals.  The
/// lower SharedBlockDevice callback supplies the exact terminal generation;
/// only after that proof are this device's routes and ring owners retired.
pub(super) fn service_physical_completion_reset_for_device(device_identity: usize) -> AxResult<()> {
    if let Some(event) = physical_completion_terminal_event_for_device(device_identity) {
        if event.state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
            // Quarantine is an explicit lower terminal state but not a
            // physical retirement proof. Consume this notification without
            // releasing any route, pin, file lease, or device credit: the
            // slot remains admission-closed and its owners remain custody.
            // Leaving the same non-proof terminal event pending would make
            // the shared worker self-wake forever, needlessly penalizing
            // healthy devices. A later lower Quiesced/Retired notification
            // is a new sequence and is still the only event that can retire
            // this device's owners.
            clear_physical_completion_terminal_event_for_device(device_identity);
            if physical_completion_device_progress_overflowed(device_identity)
                || physical_completion_device_terminal_sequence_overflowed(device_identity)
            {
                stabilize_physical_completion_overflow(device_identity);
            }
            return Err(AxError::BadState);
        }
        let outcome =
            retire_physical_completion_terminal_owners_for_device(device_identity, event)?;
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
            // An admitted submitter may still be between lower publication
            // and route commit. Give that bounded hand-off time to install
            // exact custody before attempting the lower reset again.
            drop(registry);
            defer_physical_completion_reset_retry();
            return Err(AxError::ResourceBusy);
        }
        slot.active = false;
        slot.device.clone().ok_or(AxError::OperationNotSupported)?
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
        } else {
            stabilize_physical_completion_reset_failure(device_identity);
        }
        return Err(error);
    }
    let Some(event) = physical_completion_terminal_event_for_device(device_identity) else {
        // Reset without the exact lower terminal callback is not enough to
        // retire an upper route.  An overflowed marker has now spent its one
        // reset attempt and enters stable custody until an external exact
        // proof or slot removal/reinstall; ordinary markers likewise become
        // stable fail-closed custody until a new explicit reset request or
        // later recovery edge supplies a proof.
        if physical_completion_device_progress_overflowed(device_identity)
            || physical_completion_device_terminal_sequence_overflowed(device_identity)
        {
            stabilize_physical_completion_overflow(device_identity);
        } else {
            stabilize_physical_completion_reset_failure(device_identity);
        }
        return Err(AxError::BadState);
    };
    if event.state == PHYSICAL_COMPLETION_TERMINAL_QUARANTINED {
        // See the early terminal-event path above. The reset call did not
        // prove physical quiescence, so retain all upper custody but consume
        // the one notification to avoid an unbounded worker wake/retry loop.
        clear_physical_completion_terminal_event_for_device(device_identity);
        if physical_completion_device_progress_overflowed(device_identity)
            || physical_completion_device_terminal_sequence_overflowed(device_identity)
        {
            stabilize_physical_completion_overflow(device_identity);
        }
        return Err(AxError::BadState);
    }
    let outcome = retire_physical_completion_terminal_owners_for_device(device_identity, event)?;
    finish_physical_completion_external_terminal_event_for_device(device_identity, event, outcome)
}

pub(super) fn physical_completion_generation_store_for_device(
    device_identity: usize,
    generation: u64,
) {
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

pub(super) fn physical_completion_work_count() -> usize {
    let router = PHYSICAL_COMPLETION_ROUTER.lock();
    router.work_count.saturating_add(router.pending_count)
}

pub(super) fn physical_completion_work_count_for_device(device_identity: usize) -> usize {
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

pub(super) fn mark_physical_completion_device_reset_pending(device_identity: usize) {
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

pub(super) fn physical_completion_device_reset_pending(device_identity: usize) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.reset_pending)
}

pub(super) fn physical_completion_device_progress_generation(
    device_identity: usize,
) -> Option<u64> {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .map(|slot| slot.progress_generation)
}

pub(super) fn physical_completion_device_progress_overflowed(device_identity: usize) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.progress_overflowed)
}

pub(super) fn physical_completion_device_terminal_sequence_overflowed(
    device_identity: usize,
) -> bool {
    let registry = PHYSICAL_COMPLETION_DEVICE_REGISTRY.lock();
    registry
        .slots
        .iter()
        .flatten()
        .find(|slot| slot.identity == device_identity)
        .is_some_and(|slot| slot.terminal_sequence_overflowed)
}

pub(super) fn clear_physical_completion_device_progress_if_unchanged(
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

pub(super) fn physical_completion_device_pending_locked(
    slot: &PhysicalCompletionDeviceSlot,
) -> bool {
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
            && slot.device.as_ref().is_some_and(|device| {
                device.physical_completion_broker_installed()
                    && matches!(
                        device.completion_availability(),
                        BlockCompletionAvailability::Live { generation }
                            if generation == slot.generation
                    )
            })
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
pub(super) fn physical_completion_reset_or_terminal_pending() -> bool {
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
pub(super) fn clear_physical_completion_work_pending_with_recheck() -> bool {
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
pub(super) fn select_physical_finalization_round_robin(
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
pub(super) enum PhysicalFinalizationSleepErrorAction {
    FailStop,
}

pub(super) const fn physical_finalization_sleep_error_action()
-> PhysicalFinalizationSleepErrorAction {
    PhysicalFinalizationSleepErrorAction::FailStop
}

pub(super) fn fail_stop_physical_finalization_worker(error: AxError) {
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

pub(super) fn complete_pending_physical_prepublication_error(
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

pub(super) fn handoff_pending_physical_publication(
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
pub(super) fn retry_pending_physical_publications_for_device(
    device_identity: usize,
) -> AxResult<PhysicalPublicationRetryDisposition> {
    let start = PHYSICAL_PUBLICATION_RETRY_CURSOR.fetch_add(1, Ordering::AcqRel)
        % PHYSICAL_COMPLETION_ROUTER_CAPACITY;
    let mut attempts = 0usize;
    let mut published = false;

    for offset in 0..PHYSICAL_COMPLETION_ROUTER_CAPACITY {
        if attempts == PHYSICAL_PUBLICATION_RETRY_BUDGET {
            break;
        }
        let index = (start + offset) % PHYSICAL_COMPLETION_ROUTER_CAPACITY;
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

pub(super) fn retry_pending_physical_finalizations() -> AxResult<bool> {
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
