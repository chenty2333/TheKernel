//! Terminal claims and serialized CQ publication.

use super::*;

impl IoUring {
    pub(super) fn finish_request(
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

    pub(super) fn queue_completion_locked(
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

    pub(super) fn take_completion_locked(
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

    pub(super) fn publish_pending_slot(&self, slot: usize) -> AxResult<bool> {
        if self.defer_taskrun && !self.final_close_requested.load(Ordering::Acquire) {
            self.taskrun_pending.store(true, Ordering::Release);
            return Ok(false);
        }
        self.publish_pending_slot_now(slot)
    }

    /// Publishes one already-terminal request without consulting
    /// `DEFER_TASKRUN`.  This is intentionally private to an explicit
    /// task-work/teardown drain: completion producers must use
    /// [`Self::publish_pending_slot`] so a concurrent producer can never
    /// observe a temporary, process-wide "defer disabled" state.
    pub(super) fn publish_pending_slot_now(&self, slot: usize) -> AxResult<bool> {
        self.publish_pending_slot_with_notification(
            slot,
            #[cfg(feature = "io-submit-batch")]
            None,
        )
    }

    pub(super) fn publish_pending_slot_with_notification(
        &self,
        slot: usize,
        #[cfg(feature = "io-submit-batch")] mut batch: Option<&mut SubmissionCompletionBatch<'_>>,
    ) -> AxResult<bool> {
        let published = {
            #[cfg(feature = "io-submit-batch")]
            let _publication = match self.completion_serial.try_lock() {
                Some(guard) => guard,
                None => {
                    if let Some(batch) = batch.as_deref_mut() {
                        batch.flush();
                    }
                    self.completion_serial.lock()
                }
            };
            #[cfg(not(feature = "io-submit-batch"))]
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
            #[cfg(feature = "io-submit-batch")]
            if let Some(batch) = batch {
                debug_assert!(core::ptr::eq(self, batch.ring));
                batch.record();
                return Ok(true);
            }
            self.completion_wait.wake();
        }
        Ok(published)
    }

    /// Attempts normal publication for callers that are not an explicit
    /// task-work edge.  With DEFER_TASKRUN this only records pending work.
    pub(super) fn flush_pending_publications(&self) -> AxResult<()> {
        if self.pending_publication_count.load(Ordering::Acquire) == 0
            && self
                .pending_nonterminal_publication_count
                .load(Ordering::Acquire)
                == 0
        {
            return Ok(());
        }
        for slot in 0..usize::try_from(self.layout.sq_entries()).map_err(|_| AxError::BadState)? {
            self.publish_pending_slot(slot)?;
            if self.pending_publication_count.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        self.flush_pending_nonterminal_publications_now(false)?;
        Ok(())
    }

    /// Drains the queue at an explicit task-work or close edge, bypassing
    /// deferred publication for this invocation only.
    pub(super) fn flush_pending_publications_now(&self) -> AxResult<()> {
        if self.pending_publication_count.load(Ordering::Acquire) == 0
            && self
                .pending_nonterminal_publication_count
                .load(Ordering::Acquire)
                == 0
        {
            return Ok(());
        }
        for slot in 0..usize::try_from(self.layout.sq_entries()).map_err(|_| AxError::BadState)? {
            self.publish_pending_slot_now(slot)?;
            if self.pending_publication_count.load(Ordering::Acquire) == 0 {
                break;
            }
        }
        self.flush_pending_nonterminal_publications_now(true)?;
        Ok(())
    }

    pub(super) fn flush_pending_nonterminal_publications_now(
        &self,
        bypass_defer: bool,
    ) -> AxResult<()> {
        if !bypass_defer
            && self.defer_taskrun
            && !self.final_close_requested.load(Ordering::Acquire)
        {
            self.taskrun_pending.store(true, Ordering::Release);
            return Ok(());
        }
        for slot in 0..usize::try_from(self.layout.sq_entries()).map_err(|_| AxError::BadState)? {
            let publication = {
                let mut state = self.state.lock();
                state
                    .pending_nonterminal_publications
                    .get_mut(slot)
                    .ok_or(AxError::BadState)?
                    .take()
            };
            let Some(publication) = publication else {
                continue;
            };
            self.pending_nonterminal_publication_count
                .fetch_sub(1, Ordering::AcqRel);
            if let Err(error) = self.publish_nonterminal_publication_now(publication) {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Drains deferred completion task-work at an explicit enter/GETEVENTS
    /// boundary. Close bypasses DEFER so terminal teardown cannot strand an
    /// owned CQE behind a task which has already exited.
    pub(crate) fn run_task_work(&self) -> AxResult<()> {
        if (self.coop_taskrun && self.taskrun_pending.load(Ordering::Acquire))
            || self.taskrun_pending.swap(false, Ordering::AcqRel)
            || self.final_close_requested.load(Ordering::Acquire)
        {
            // Do not mutate `defer_taskrun`: it is immutable setup state and
            // another completion producer may run concurrently.  The drain
            // selects the private immediate-publication path instead.
            self.flush_pending_publications_now()?;
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
        let linked = self.release_linked_after_terminal(id, result)?;
        self.publish_pending_slot(id.slot() as usize)?;
        if let Some(dispatch) = linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(())
    }

    /// Explicit synchronous completion owner; linked dispatch is flushed
    /// before execution because the released operation may block.
    #[cfg(feature = "io-submit-batch")]
    pub(crate) fn complete_request_batched(
        &self,
        id: RequestId,
        cause: TerminalCause,
        result: i32,
        flags: u32,
        batch: &mut SubmissionCompletionBatch<'_>,
    ) -> AxResult<()> {
        assert!(core::ptr::eq(self, batch.ring));
        self.finish_request(id, cause, result, flags)?;
        let linked = self.release_linked_after_terminal(id, result)?;
        if self.defer_taskrun && !self.final_close_requested.load(Ordering::Acquire) {
            self.taskrun_pending.store(true, Ordering::Release);
        } else {
            self.publish_pending_slot_with_notification(id.slot() as usize, Some(batch))?;
        }
        if let Some(dispatch) = linked {
            batch.flush();
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(())
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
        let linked = self.release_linked_after_terminal(id, result)?;
        self.publish_pending_slot(id.slot() as usize)?;
        if let Some(dispatch) = linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(())
    }

    /// Claims terminal ownership before an adjacent resource hand-off (such
    /// as a supplied buffer becoming userspace-owned). The hook runs after
    /// cancellation can no longer win, but before the terminal CQE is made
    /// pending/published.
    pub(crate) fn complete_issued_with_claim_hook(
        &self,
        issued: IssuedRequest,
        cause: TerminalCause,
        result: i32,
        flags: u32,
        hook: impl FnOnce(),
    ) -> AxResult<()> {
        let id = issued.id();
        let permit = self
            .state
            .lock()
            .requests
            .claim_terminal(id, cause)
            .map_err(map_core_error)?;
        hook();
        let token = {
            let mut state = self.state.lock();
            let token = state
                .requests
                .finish_terminal(permit, result, flags)
                .map_err(map_core_error)?;
            let slot = token.id().slot() as usize;
            self.queue_completion_locked(&mut state, token)?;
            slot
        };
        let linked = self.release_linked_after_terminal(id, result)?;
        self.publish_pending_slot(token)?;
        if let Some(dispatch) = linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(())
    }
}
