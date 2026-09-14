//! Final-close progression and request cancellation.

use super::*;

impl IoUring {
    pub(super) fn request_final_close(self: &Arc<Self>) {
        self.final_close_requested.store(true, Ordering::Release);
        // The SQ worker holds an Arc while it is asleep.  Close is the
        // terminal wake edge; do not wait for its periodic idle deadline
        // before beginning descriptor/buffer retirement.
        self.sqpoll_stop.store(true, Ordering::Release);
        self.sqpoll_wake.wake();
        self.request_physical_reset_for_final_close();
        self.enqueue_deferred();
    }

    /// Closing a ring with a published physical effect is an explicit
    /// management abort request, not evidence that DMA has stopped. Fence
    /// only the exact devices used by this ring and hand the actual reset to
    /// the dedicated task-context completion worker. That worker invokes the
    /// lower bounded reset API and may release these owners only after its
    /// `Quiesced` or `Retired` terminal proof arrives.
    ///
    /// A normally slow request never reaches this path: there is no timer
    /// that reinterprets elapsed I/O time as quiescence or resets an active
    /// device. A close provides the explicit cancellation/management edge
    /// for an otherwise silent device; a lower `Quarantined` result remains
    /// fail-closed and keeps this ring parked.
    pub(super) fn request_physical_reset_for_final_close(&self) {
        let mut identities = [0usize; PHYSICAL_COMPLETION_MAX_DEVICES];
        let mut len = 0;
        {
            let state = self.state.lock();
            for work in state
                .physical_work
                .iter()
                .chain(state.physical_custody.iter())
                .flatten()
            {
                let identity = work.device_identity();
                if identity == 0 || identities[..len].contains(&identity) {
                    continue;
                }
                if len == identities.len() {
                    // The registry itself bounds the number of physical
                    // devices. A ring carrying an unrepresentable identity
                    // set is already corrupted; preserve custody rather than
                    // guessing which lower queue to reset.
                    break;
                }
                identities[len] = identity;
                len += 1;
            }
        }
        for identity in identities[..len].iter().copied() {
            mark_physical_completion_device_reset_pending(identity);
        }
    }

    pub(super) fn begin_final_close_step(&self) -> AxResult<bool> {
        let mut state = self.state.lock();
        state.requests.begin_close().map_err(map_core_error)?;
        // Linked/DRAIN followers have an accepted terminal credit but remain
        // `Prepared` until their predecessor releases them.  Final close must
        // detach those owners under this same request-table lock: otherwise a
        // closed ring can retain a copied OPENAT2 (and its files/MM snapshot)
        // indefinitely behind an uncancellable predecessor.
        state.link_tail = None;
        for slot in 0..state.parked_submissions.len() {
            let Some(parked) = state.parked_submissions[slot].take() else {
                continue;
            };
            let request = parked.work.id();
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
                }
                // A terminal callback may have won immediately before the
                // final-close lock. The detached work is no longer eligible
                // for dispatch, and that callback owns its sole credit.
                Err(IoUringError::TerminalAlreadyClaimed | IoUringError::UnknownRequest) => {}
                Err(error) => return Err(map_core_error(error)),
            }
        }
        state.final_close.enter(FinalClosePhase::Polls);
        Ok(false)
    }

    pub(super) fn close_polls_step(&self) -> AxResult<bool> {
        let (slots, capacity) = {
            let mut state = self.state.lock();
            let capacity = state.polls.len();
            (state.final_close.take_slots(capacity), capacity)
        };

        for slot in slots.clone() {
            let (permit, control, pending_stream_work, socket_work) = {
                let mut state = self.state.lock();
                let request = state
                    .polls
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|control| control.request)
                    .or_else(|| {
                        state.socket_multishot.iter().find_map(|entry| {
                            entry
                                .as_ref()
                                .filter(|work| work.request_id().slot() as usize == slot)
                                .map(SocketMultishotWork::request_id)
                        })
                    })
                    .or_else(|| {
                        state.pending_stream.iter().find_map(|entry| {
                            entry
                                .as_ref()
                                .filter(|work| work.request_id().slot() as usize == slot)
                                .map(PendingStreamWork::request_id)
                        })
                    });
                let Some(request) = request else {
                    continue;
                };
                match state
                    .requests
                    .claim_terminal(request, TerminalCause::Closing)
                {
                    Ok(permit) => {
                        let control = state.polls[slot].take();
                        let pending_stream_work = state
                            .pending_stream
                            .iter()
                            .position(|entry| {
                                entry
                                    .as_ref()
                                    .is_some_and(|work| work.request_id() == request)
                            })
                            .and_then(|index| {
                                let work = state.pending_stream[index].take();
                                if work.is_some() {
                                    state.pending_stream_count =
                                        state.pending_stream_count.saturating_sub(1);
                                }
                                work
                            });
                        let socket_work = state
                            .socket_multishot
                            .iter()
                            .position(|entry| {
                                entry
                                    .as_ref()
                                    .is_some_and(|work| work.request_id() == request)
                            })
                            .and_then(|index| state.socket_multishot[index].take());
                        (Some(permit), control, pending_stream_work, socket_work)
                    }
                    Err(
                        IoUringError::TerminalAlreadyClaimed
                        | IoUringError::UnknownRequest
                        | IoUringError::RequestUncancellable,
                    ) => {
                        let control = state.polls[slot].take();
                        let pending_stream_work = state
                            .pending_stream
                            .iter()
                            .position(|entry| {
                                entry
                                    .as_ref()
                                    .is_some_and(|work| work.request_id() == request)
                            })
                            .and_then(|index| {
                                let work = state.pending_stream[index].take();
                                if work.is_some() {
                                    state.pending_stream_count =
                                        state.pending_stream_count.saturating_sub(1);
                                }
                                work
                            });
                        let socket_work = state
                            .socket_multishot
                            .iter()
                            .position(|entry| {
                                entry
                                    .as_ref()
                                    .is_some_and(|work| work.request_id() == request)
                            })
                            .and_then(|index| state.socket_multishot[index].take());
                        (None, control, pending_stream_work, socket_work)
                    }
                    Err(error) => return Err(map_core_error(error)),
                }
            };
            let lease = control.as_ref().and_then(|control| control.deactivate());
            let pending_stream_lease = pending_stream_work
                .as_ref()
                .and_then(|work| work.control.deactivate());
            let socket_lease = socket_work
                .as_ref()
                .and_then(|work| work.control.deactivate());
            drop(lease);
            drop(pending_stream_lease);
            drop(pending_stream_work);
            drop(socket_lease);
            drop(control);
            drop(socket_work);
            // A concurrent CQ drainer must not observe this request until
            // every detached buffer/file owner has retired outside the lock.
            if let Some(permit) = permit {
                let mut state = self.state.lock();
                let token = state
                    .requests
                    .finish_terminal(permit, -LinuxError::ECANCELED.code(), 0)
                    .map_err(map_core_error)?;
                self.queue_completion_locked(&mut state, token)?;
            }
        }

        // Poll controls own a readiness registration and were detached above.
        // Other cancellable executors (currently relative timeout workers)
        // have no file lease/control object, but still must be terminally
        // claimed here: otherwise final close would wait for their original
        // deadline even though cancellation was already requested.
        let mut state = self.state.lock();
        for slot in slots.clone() {
            let token = match state
                .requests
                .claim_cancellable_at_slot(slot as u32, TerminalCause::Closing)
                .map_err(map_core_error)?
            {
                Some(permit) => Some(
                    state
                        .requests
                        .finish_terminal(permit, -LinuxError::ECANCELED.code(), 0)
                        .map_err(map_core_error)?,
                ),
                None => None,
            };
            if let Some(token) = token {
                self.queue_completion_locked(&mut state, token)?;
            }
        }

        if state.final_close.cursor >= capacity {
            state.final_close.enter(FinalClosePhase::OwnedFileIo);
        }
        drop(state);
        // Provider-owned URING_CMD completions are not poll controls, but
        // they retain an issued token and exact OFD.  Detach each generation
        // before notifying the provider so close cannot leave an opaque queue
        // holding the ring alive after its terminal credit was claimed.
        for slot in slots {
            let provider = {
                let mut state = self.state.lock();
                let terminal = state
                    .iopoll_uring_cmd
                    .get(slot)
                    .and_then(Option::as_ref)
                    .and_then(|owner| {
                        state.requests.request(owner.id).ok().map(|(_, request)| {
                            !matches!(request, thekernel_linux_io_uring::RequestState::Issued(_))
                        })
                    })
                    .unwrap_or(false);
                if terminal {
                    state.iopoll_uring_cmd.get_mut(slot).and_then(Option::take)
                } else {
                    None
                }
            };
            if let Some(owner) = provider {
                let request = owner.id;
                let provider = owner.file;
                provider.cancel_uring_cmd(request);
            }
        }
        Ok(false)
    }

    /// Cancels provider-owned generic I/O after poll controls have been
    /// detached but before registered files/buffers can release the OFD and
    /// long-term pins retained by a provider.  Provider controls are always
    /// invoked outside `RingState`; a worker which already owns an operation
    /// remains in `InFlight` until its sole completion bridge fires.
    pub(super) fn close_owned_file_io_step(&self) -> AxResult<bool> {
        let (slots, capacity) = {
            let mut state = self.state.lock();
            let capacity = state.owned_file_io.len();
            (state.final_close.take_slots(capacity), capacity)
        };
        for slot in slots {
            let control = {
                let mut state = self.state.lock();
                let Some(id) = state
                    .owned_file_io
                    .get(slot)
                    .and_then(Option::as_ref)
                    .map(|owner| owner.id)
                else {
                    continue;
                };
                if !matches!(state.owned_file_io.get(slot).and_then(Option::as_ref).map(|owner| &owner.state),
                    Some(OwnedFileIoControlState::Submitted { generation, .. }) if *generation == id.generation())
                {
                    continue;
                }
                state
                    .requests
                    .begin_provider_cancel(id)
                    .map_err(map_core_error)?;
                let owner = state
                    .owned_file_io
                    .get_mut(slot)
                    .and_then(Option::as_mut)
                    .ok_or(AxError::BadState)?;
                let previous =
                    core::mem::replace(&mut owner.state, OwnedFileIoControlState::Terminal);
                let OwnedFileIoControlState::Submitted {
                    generation,
                    bridge,
                    control,
                } = previous
                else {
                    return Err(AxError::BadState);
                };
                owner.state = OwnedFileIoControlState::CancelPending { generation, bridge };
                Some((id, control))
            };
            let Some((id, control)) = control else {
                continue;
            };
            let outcome = control.cancel();
            let (callback, cancelled) = {
                // Declare custody first so error returns also unlock before
                // its buffer lease re-enters the ring during Drop.
                let mut owner;
                let mut state = self.state.lock();
                let slot = id.slot() as usize;
                owner = state
                    .owned_file_io
                    .get_mut(slot)
                    .and_then(Option::take)
                    .filter(|owner| owner.id == id)
                    .ok_or(AxError::BadState)?;
                let previous =
                    core::mem::replace(&mut owner.state, OwnedFileIoControlState::Terminal);
                let pending = match previous {
                    OwnedFileIoControlState::CompletionPending {
                        generation,
                        bridge,
                        result,
                    } if generation == id.generation() => Some((bridge, result)),
                    state @ OwnedFileIoControlState::CancelPending { .. } => {
                        owner.state = state;
                        None
                    }
                    state => {
                        owner.state = state;
                        None
                    }
                };
                match outcome {
                    axfs_ng_vfs::FileIoCancelOutcome::Cancelled => {
                        let permit = state
                            .requests
                            .finish_provider_cancel(id, ProviderCancelOutcome::Cancelled)
                            .map_err(map_core_error)?
                            .ok_or(AxError::BadState)?;
                        drop(state);
                        drop(owner);
                        let mut state = self.state.lock();
                        let token = state
                            .requests
                            .finish_terminal(permit, -LinuxError::ECANCELED.code(), 0)
                            .map_err(map_core_error)?;
                        self.queue_completion_locked(&mut state, token)?;
                        (None, true)
                    }
                    axfs_ng_vfs::FileIoCancelOutcome::InFlight
                    | axfs_ng_vfs::FileIoCancelOutcome::Terminal => {
                        state
                            .requests
                            .finish_provider_cancel(id, ProviderCancelOutcome::InFlight)
                            .map_err(map_core_error)?;
                        if let Some((bridge, result)) = pending {
                            (
                                Some(OwnedFileIoTerminal {
                                    issued: bridge.issued,
                                    result,
                                    buffer: owner.buffer,
                                }),
                                false,
                            )
                        } else {
                            let previous = core::mem::replace(
                                &mut owner.state,
                                OwnedFileIoControlState::Terminal,
                            );
                            let OwnedFileIoControlState::CancelPending { generation, bridge } =
                                previous
                            else {
                                return Err(AxError::BadState);
                            };
                            owner.state = OwnedFileIoControlState::InFlight { generation, bridge };
                            state.owned_file_io[slot] = Some(owner);
                            (None, false)
                        }
                    }
                }
            };
            if cancelled {
                let linked =
                    self.release_linked_after_terminal(id, -LinuxError::ECANCELED.code())?;
                self.publish_pending_slot(id.slot() as usize)?;
                if let Some(dispatch) = linked {
                    let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
                }
            }
            if let Some(terminal) = callback {
                self.complete_owned_terminal(terminal)?;
            }
        }
        let mut state = self.state.lock();
        if state.final_close.cursor >= capacity {
            if state.owned_file_io.iter().all(Option::is_none) {
                state.final_close.enter(FinalClosePhase::FixedFiles);
            } else {
                state.final_close.cursor = 0;
            }
        }
        Ok(false)
    }

    pub(super) fn close_fixed_files_step(&self) -> AxResult<bool> {
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

    pub(super) fn close_buffers_step(&self) -> AxResult<bool> {
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

    pub(super) fn close_completions_step(&self) -> AxResult<bool> {
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
        let eventfd = state.completion_eventfd.take();
        state.final_close.enter(FinalClosePhase::Finished);
        drop(state);
        drop(publication);
        drop(eventfd);

        for word in &self.poll_hint_bits {
            word.store(0, Ordering::Release);
        }
        self.poll_hint_pending.store(false, Ordering::Release);
        self.completion_wait.close();
        Ok(true)
    }

    pub(super) fn close_in_policy_worker(&self) -> AxResult<bool> {
        let phase = self.state.lock().final_close.phase;
        match phase {
            FinalClosePhase::Begin => self.begin_final_close_step(),
            FinalClosePhase::Polls => self.close_polls_step(),
            FinalClosePhase::OwnedFileIo => self.close_owned_file_io_step(),
            FinalClosePhase::FixedFiles => self.close_fixed_files_step(),
            FinalClosePhase::Buffers => self.close_buffers_step(),
            FinalClosePhase::Completions => self.close_completions_step(),
            FinalClosePhase::Finished => Ok(true),
        }
    }
}

impl IoUring {
    pub(crate) fn cancel_request(
        &self,
        cancel: IssuedRequest,
        target_user_data: u64,
    ) -> AxResult<()> {
        self.cancel_request_selected(cancel, CancelSelector::UserData(target_user_data))
    }

    /// `IORING_OP_TIMEOUT_REMOVE` is intentionally narrower than
    /// `ASYNC_CANCEL`: duplicate user-data values must not let it detach an
    /// unrelated poll or future cancellable provider request.
    pub(crate) fn cancel_timeout_request(
        &self,
        cancel: IssuedRequest,
        target_user_data: u64,
    ) -> AxResult<()> {
        self.cancel_request_selected(cancel, CancelSelector::TimeoutUserData(target_user_data))
    }

    pub(super) fn cancel_request_selected(
        &self,
        cancel: IssuedRequest,
        selector: CancelSelector,
    ) -> AxResult<()> {
        // Provider-owned generic file I/O has a consuming control object.
        // Fence that exact slot/generation before the legacy generic cancel
        // path can claim an unrelated request with the same user_data.
        let owned = {
            let mut state = self.state.lock();
            let selected = {
                let RingState {
                    requests,
                    owned_file_io,
                    ..
                } = &mut *state;
                requests.select_provider_cancel_candidate(
                    selector,
                    Some(cancel.id()),
                    |id| {
                        owned_file_io
                            .get(id.slot() as usize)
                            .and_then(Option::as_ref)
                            .is_some_and(|owner| {
                                owner.id == id
                                    && matches!(&owner.state, OwnedFileIoControlState::Submitted { generation, .. } if *generation == id.generation())
                            })
                    },
                )
            };
            match selected {
                Ok(id) => {
                    let owner = state
                        .owned_file_io
                        .get_mut(id.slot() as usize)
                        .and_then(Option::as_mut)
                        .filter(|owner| owner.id == id)
                        .ok_or(AxError::BadState)?;
                    let previous =
                        core::mem::replace(&mut owner.state, OwnedFileIoControlState::Terminal);
                    let OwnedFileIoControlState::Submitted {
                        generation,
                        bridge,
                        control,
                    } = previous
                    else {
                        return Err(AxError::BadState);
                    };
                    owner.state = OwnedFileIoControlState::CancelPending { generation, bridge };
                    Some((id, control))
                }
                Err(IoUringError::CancellationTargetNotFound) => None,
                Err(error) => return Err(map_core_error(error)),
            }
        };
        if let Some((target_id, control)) = owned {
            // `cancel` may synchronously invoke the weak completion bridge.
            // That bridge records CompletionPending under the same ring lock;
            // only this path resolves the registry's ShotInFlight fence.
            let outcome = control.cancel();
            let (callback, target_cancelled, cancel_result) = {
                let mut owner;
                let mut state = self.state.lock();
                let slot = target_id.slot() as usize;
                owner = state
                    .owned_file_io
                    .get_mut(slot)
                    .and_then(Option::take)
                    .filter(|owner| owner.id == target_id)
                    .ok_or(AxError::BadState)?;
                let pending =
                    match core::mem::replace(&mut owner.state, OwnedFileIoControlState::Terminal) {
                        OwnedFileIoControlState::CompletionPending {
                            generation,
                            bridge,
                            result,
                        } if generation == target_id.generation() => Some((bridge, result)),
                        state @ OwnedFileIoControlState::CancelPending { .. }
                        | state @ OwnedFileIoControlState::InFlight { .. } => {
                            owner.state = state;
                            None
                        }
                        state => {
                            owner.state = state;
                            None
                        }
                    };
                match outcome {
                    axfs_ng_vfs::FileIoCancelOutcome::Cancelled => {
                        let permit = state
                            .requests
                            .finish_provider_cancel(target_id, ProviderCancelOutcome::Cancelled)
                            .map_err(map_core_error)?;
                        let permit = permit.ok_or(AxError::BadState)?;
                        drop(state);
                        drop(owner);
                        let mut state = self.state.lock();
                        let token = state
                            .requests
                            .finish_terminal(permit, -LinuxError::ECANCELED.code(), 0)
                            .map_err(map_core_error)?;
                        self.queue_completion_locked(&mut state, token)?;
                        (None, true, 0)
                    }
                    axfs_ng_vfs::FileIoCancelOutcome::InFlight => {
                        if let Some((bridge, result)) = pending {
                            state
                                .requests
                                .finish_provider_cancel(target_id, ProviderCancelOutcome::InFlight)
                                .map_err(map_core_error)?;
                            (
                                Some(OwnedFileIoTerminal {
                                    issued: bridge.issued,
                                    result,
                                    buffer: owner.buffer,
                                }),
                                false,
                                -LinuxError::EALREADY.code(),
                            )
                        } else {
                            state
                                .requests
                                .finish_provider_cancel(target_id, ProviderCancelOutcome::InFlight)
                                .map_err(map_core_error)?;
                            let previous = core::mem::replace(
                                &mut owner.state,
                                OwnedFileIoControlState::Terminal,
                            );
                            let OwnedFileIoControlState::CancelPending { generation, bridge } =
                                previous
                            else {
                                return Err(AxError::BadState);
                            };
                            owner.state = OwnedFileIoControlState::InFlight { generation, bridge };
                            state.owned_file_io[slot] = Some(owner);
                            (None, false, -LinuxError::EALREADY.code())
                        }
                    }
                    axfs_ng_vfs::FileIoCancelOutcome::Terminal => {
                        if let Some((bridge, result)) = pending {
                            state
                                .requests
                                .finish_provider_cancel(target_id, ProviderCancelOutcome::InFlight)
                                .map_err(map_core_error)?;
                            (
                                Some(OwnedFileIoTerminal {
                                    issued: bridge.issued,
                                    result,
                                    buffer: owner.buffer,
                                }),
                                false,
                                -LinuxError::ENOENT.code(),
                            )
                        } else {
                            // The provider reported a terminal transition but
                            // did not deliver its bridge.  Keep the exact
                            // owner fenced rather than manufacturing a second
                            // CQE or allowing the slot to be reused.
                            state
                                .requests
                                .finish_provider_cancel(target_id, ProviderCancelOutcome::InFlight)
                                .map_err(map_core_error)?;
                            let previous = core::mem::replace(
                                &mut owner.state,
                                OwnedFileIoControlState::Terminal,
                            );
                            let OwnedFileIoControlState::CancelPending { generation, bridge } =
                                previous
                            else {
                                return Err(AxError::BadState);
                            };
                            owner.state = OwnedFileIoControlState::InFlight { generation, bridge };
                            state.owned_file_io[slot] = Some(owner);
                            (None, false, -LinuxError::ENOENT.code())
                        }
                    }
                }
            };
            if target_cancelled {
                let linked =
                    self.release_linked_after_terminal(target_id, -LinuxError::ECANCELED.code())?;
                self.publish_pending_slot(target_id.slot() as usize)?;
                if let Some(dispatch) = linked {
                    let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
                }
            }
            if let Some(terminal) = callback {
                self.complete_owned_terminal(terminal)?;
            }
            self.complete_issued(cancel, TerminalCause::Completed, cancel_result, 0)?;
            return Ok(());
        }
        let cancel_id = cancel.id();
        let (
            target_id,
            permits,
            cancel_completion,
            control,
            pending_stream_work,
            socket_work,
            parked_work,
            iopoll_provider,
        ) = {
            let mut state = self.state.lock();
            match state.requests.claim_cancel(selector, Some(cancel_id)) {
                Ok(target) => {
                    let target_id = target.id();
                    let cancel_permit = state
                        .requests
                        .claim_terminal(cancel_id, TerminalCause::Completed)
                        .map_err(map_core_error)?;
                    let control = usize::try_from(target_id.slot())
                        .ok()
                        .and_then(|slot| state.polls.get_mut(slot))
                        .and_then(Option::take);
                    // Pending fixed-buffer streams own both a poll
                    // registration and the supplied-buffer lease outside the
                    // ordinary poll table.  Claim and remove that exact owner
                    // under the same lock that consumes the request's one
                    // terminal credit, so a later readiness edge cannot
                    // reinsert it after ASYNC_CANCEL has won.
                    let pending_stream_work = state
                        .pending_stream
                        .iter()
                        .position(|entry| {
                            entry
                                .as_ref()
                                .is_some_and(|work| work.request_id() == target_id)
                        })
                        .and_then(|index| {
                            let work = state.pending_stream[index].take();
                            if work.is_some() {
                                state.pending_stream_count =
                                    state.pending_stream_count.saturating_sub(1);
                            }
                            work
                        });
                    let socket_work = state
                        .socket_multishot
                        .iter()
                        .position(|entry| {
                            entry
                                .as_ref()
                                .is_some_and(|work| work.request_id() == target_id)
                        })
                        .and_then(|index| state.socket_multishot[index].take());
                    // A linked follower can still be Prepared.  Remove its
                    // parked owner under the same state lock which claims its
                    // terminal credit, so a predecessor completion cannot
                    // resurrect execution after cancellation wins.
                    let parked_work = state
                        .parked_submissions
                        .iter()
                        .position(|entry| {
                            entry
                                .as_ref()
                                .is_some_and(|work| work.work.id() == target_id)
                        })
                        .and_then(|index| state.parked_submissions[index].take());
                    let iopoll_provider = state
                        .iopoll_uring_cmd
                        .get_mut(target_id.slot() as usize)
                        .and_then(|entry| {
                            match entry.as_mut().filter(|owner| owner.id == target_id) {
                                Some(owner)
                                    if matches!(
                                        owner.state,
                                        UringCmdHandoffState::Prepared
                                            | UringCmdHandoffState::Submitting
                                    ) =>
                                {
                                    owner.state = UringCmdHandoffState::CancelPending;
                                    None
                                }
                                Some(_) => entry
                                    .take()
                                    .map(|owner| (owner.id, owner.file, owner.iopoll)),
                                None => None,
                            }
                        });
                    (
                        Some(target_id),
                        (Some(target), cancel_permit),
                        0,
                        control,
                        pending_stream_work,
                        socket_work,
                        parked_work,
                        iopoll_provider,
                    )
                }
                Err(IoUringError::CancellationTargetNotFound) => {
                    let cancel_permit = state
                        .requests
                        .claim_terminal(cancel_id, TerminalCause::Completed)
                        .map_err(map_core_error)?;
                    (
                        None,
                        (None, cancel_permit),
                        -LinuxError::ENOENT.code(),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                Err(error) => return Err(map_core_error(error)),
            }
        };
        let lease = control.as_ref().and_then(|control| control.deactivate());
        let pending_stream_lease = pending_stream_work
            .as_ref()
            .and_then(|work| work.control.deactivate());
        let socket_lease = socket_work
            .as_ref()
            .and_then(|work| work.control.deactivate());
        // Holding terminal claims fences cancellation/readiness, while
        // keeping both CQEs invisible to independent pending-CQ drainers.
        drop(lease);
        drop(pending_stream_lease);
        drop(pending_stream_work);
        drop(socket_lease);
        drop(socket_work);
        drop(control);
        drop(parked_work);
        if let Some((request, provider, _iopoll)) = iopoll_provider {
            provider.cancel_uring_cmd(request);
            drop(provider);
        }
        {
            let mut state = self.state.lock();
            if let Some(target) = permits.0 {
                let token = state
                    .requests
                    .finish_terminal(target, -LinuxError::ECANCELED.code(), 0)
                    .map_err(map_core_error)?;
                self.queue_completion_locked(&mut state, token)?;
            }
            let token = state
                .requests
                .finish_terminal(permits.1, cancel_completion, 0)
                .map_err(map_core_error)?;
            self.queue_completion_locked(&mut state, token)?;
        }
        let target_linked = match target_id {
            Some(id) => self.release_linked_after_terminal(id, -LinuxError::ECANCELED.code())?,
            None => None,
        };
        let cancel_linked = self.release_linked_after_terminal(cancel_id, cancel_completion)?;
        let target_result = match target_id {
            Some(id) => self.publish_pending_slot(id.slot() as usize).map(|_| ()),
            None => Ok(()),
        };
        let cancel_result = self
            .publish_pending_slot(cancel_id.slot() as usize)
            .map(|_| ());
        target_result.and(cancel_result)?;
        if let Some(dispatch) = target_linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        if let Some(dispatch) = cancel_linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(())
    }
}
