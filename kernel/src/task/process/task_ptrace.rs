//! Task-scoped tracing; shared job control and freezing stay on ProcessData.
use super::{super::Thread, *};

impl Thread {
    /// Acquires the fixed ptrace publication order used by attach/traceme and
    /// process exit: lifecycle first, then the sleepable action gate.
    pub(crate) fn lock_ptrace_publication(&self) -> PtracePublicationGuard<'_> {
        let lifecycle = self.proc_data.process_lifecycle.lock();
        let task_parent = lock_task_parent_publication();
        let actions = self.ptrace_actions.lock();
        PtracePublicationGuard {
            owner: &self.proc_data,
            target: self,
            tracer_owner: None,
            _actions: actions,
            task_parent,
            _second_lifecycle: None,
            _first_lifecycle: lifecycle,
        }
    }

    /// Pins both the tracee and exact prospective tracer process against task
    /// exit/reparenting. Distinct ProcessData lifecycle locks use immutable
    /// object-address order, followed by the tracee action gate.
    pub(crate) fn lock_ptrace_traceme_publication<'a>(
        &'a self,
        tracer: &'a ProcessData,
    ) -> AxResult<PtracePublicationGuard<'a>> {
        if core::ptr::eq(&*self.proc_data, tracer) {
            return Err(AxError::OperationNotPermitted);
        }
        let (first_owner, second_owner) = if ptrace_lifecycle_first(&self.proc_data, tracer) {
            (&*self.proc_data, tracer)
        } else {
            (tracer, &*self.proc_data)
        };
        let first_lifecycle = first_owner.process_lifecycle.lock();
        let second_lifecycle = second_owner.process_lifecycle.lock();
        let task_parent = lock_task_parent_publication();
        let actions = self.ptrace_actions.lock();
        Ok(PtracePublicationGuard {
            owner: &self.proc_data,
            target: self,
            tracer_owner: Some(tracer),
            _actions: actions,
            task_parent,
            _second_lifecycle: Some(second_lifecycle),
            _first_lifecycle: first_lifecycle,
        })
    }

    pub(crate) fn lock_ptrace_actions(&self) -> PtraceActionGuard<'_> {
        let guard = self.ptrace_actions.lock();
        #[cfg(test)]
        let probe = PostCommitLockProbe::new(PostCommitLockKind::PtraceAction);
        PtraceActionGuard {
            _guard: guard,
            #[cfg(test)]
            _probe: probe,
        }
    }

    pub fn ptrace_tracer(&self) -> Option<Pid> {
        self.ptrace_ctl
            .lock()
            .active_session()
            .map(|session| session.tracer)
    }

    pub(crate) fn ptrace_active_session(&self) -> Option<PtraceSession> {
        self.ptrace_ctl.lock().active_session()
    }

    /// Atomically snapshots both the exact ptrace generation and Linux's
    /// immutable relationship-time `ptracer_cred`. Exec and other privilege
    /// consumers must not combine `ptrace_tracer()` with a later PID
    /// credential lookup.
    pub(crate) fn ptrace_relationship_snapshot(&self) -> Option<PtraceRelationshipSnapshot> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let relationship = ptrace_ctl.active_relationship();
        drop(ptrace_ctl);
        relationship
    }

    /// Samples the inherited relationship together with its option word and
    /// seize mode. Clone must never splice a relationship from one generation
    /// to options observed after a detach/reattach.
    pub(crate) fn ptrace_clone_snapshot(
        &self,
        kernel_tid: Pid,
    ) -> Option<(PtraceRelationshipSnapshot, u32, bool)> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let mut options = ptrace_ctl.options;
        // A sibling must not inherit another thread's seccomp suspension into
        // its child. Linux copies current->ptrace, not the group's options.
        if !ptrace_seccomp_suspended_for_tid(&ptrace_ctl, &self.ptrace_suspended_tracee, kernel_tid)
        {
            options &= !tk_linux_process::ptrace_options::SUSPEND_SECCOMP;
        }
        Some((
            ptrace_ctl.active_relationship()?,
            options,
            ptrace_ctl.seized,
        ))
    }

    pub(crate) fn ptrace_session_if_traced_by(
        &self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
    ) -> Option<PtraceSession> {
        self.ptrace_ctl
            .lock()
            .active_session_if_owned_by(tracer, tracer_kernel_tid)
    }

    pub(crate) fn ptrace_session_if_traced_by_process(&self, tracer: Pid) -> Option<PtraceSession> {
        self.ptrace_ctl
            .lock()
            .active_session()
            .filter(|session| session.tracer == tracer)
    }

    /// Returns the caller-owned relationship only when its exact generation
    /// also owns the current ptrace stop.
    pub(crate) fn ptrace_inactive_session_if_traced_by(
        &self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
    ) -> Option<PtraceSession> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let session = ptrace_ctl.active_session_if_owned_by(tracer, tracer_kernel_tid)?;
        let job_ctl = self.ptrace_job_ctl.lock();
        (!ptrace_ctl.listening && job_ctl.is_ptrace_inactive_for(session)).then_some(session)
    }

    pub(crate) fn ptrace_set_options(&self, session: PtraceSession, options: u32) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.ptrace_job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session)
        {
            return false;
        }
        ptrace_ctl.options = options;
        true
    }

    /// Reports whether the tracer of one exact relationship asked for
    /// `PTRACE_O_EXITKILL`.
    ///
    /// Linux stores the option as `PT_EXITKILL` in the *tracee's* `ptrace` word
    /// and `exit_ptrace()` tests it while the relationship is still published:
    ///
    /// ```c
    /// 	list_for_each_entry_safe(p, n, &tracer->ptraced, ptrace_entry) {
    /// 		if (unlikely(p->ptrace & PT_EXITKILL))
    /// 			send_sig_info(SIGKILL, SEND_SIG_PRIV, p);
    /// ```
    ///
    /// Sampling under the same guard that retires the relationship keeps a
    /// detach/reattach by the same numeric tracer from making the new
    /// relationship inherit the old request.
    pub(crate) fn ptrace_exitkill_requested(&self, session: PtraceSession) -> bool {
        let ptrace_ctl = self.ptrace_ctl.lock();
        ptrace_ctl.active_session() == Some(session)
            && ptrace_ctl.options & tk_linux_process::ptrace_options::EXITKILL != 0
    }

    /// Reports whether the active relationship's tracer suspended this exact
    /// task's seccomp policy with `PTRACE_O_SUSPEND_SECCOMP`.
    ///
    /// Linux keeps the option in the *tracee's* `ptrace` word as
    /// `PT_SUSPEND_SECCOMP`, and `__secure_computing()` tests it before it
    /// looks at the seccomp mode at all (kernel/seccomp.c):
    ///
    /// ```c
    /// 	if (IS_ENABLED(CONFIG_CHECKPOINT_RESTORE) &&
    /// 	    unlikely(current->ptrace & PT_SUSPEND_SECCOMP))
    /// 		return 0;
    /// ```
    ///
    /// The option word is task-local. The exact kernel TID and generation
    /// tag still make an obsolete suspension inert after retirement. A
    /// detach resumes enforcement because `clear_session()` zeroes `options`
    /// and the retirement paths clear the tag with it.
    pub(crate) fn ptrace_seccomp_suspended_for(&self, kernel_tid: Pid) -> bool {
        ptrace_seccomp_suspended_for_tid(
            &self.ptrace_ctl.lock(),
            &self.ptrace_suspended_tracee,
            kernel_tid,
        )
    }

    pub(crate) fn ptrace_event_message(&self, session: PtraceSession) -> Option<usize> {
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.ptrace_job_ctl.lock();
        (ptrace_ctl.active_session() == Some(session) && job_ctl.is_ptrace_inactive_for(session))
            .then_some(ptrace_ctl.event_message)
    }

    pub(crate) fn ptrace_set_event_message(
        &self,
        session: PtraceSession,
        event_message: usize,
    ) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        ptrace_ctl.event_message = event_message;
        true
    }

    /// Pins the image only if the exact relationship still owns an inactive
    /// ptrace stop at the image/session/job-control linearization point.
    pub(crate) fn ptrace_inactive_image_if_session(
        &self,
        session: PtraceSession,
    ) -> Option<Arc<Mutex<AddrSpace>>> {
        ptrace_inactive_image_snapshot_if_session(
            &self.ptrace_ctl,
            &self.ptrace_job_ctl,
            &self.proc_data.image_binding,
            session,
        )
    }

    /// Publishes both directions of one ptrace relationship after revalidating
    /// the exact hook-authorized tasks, actor credential, and target image.
    /// The actor credential guard is acquired before the publication gate;
    /// `relationship_credential` is either that live credential or the
    /// immutable relationship-time credential retained by CLONE_PTRACE. The
    /// fixed order inside is exec gate, image, access security, exact target
    /// credential, ptrace control, then tracer reverse links.
    pub(crate) fn publish_ptrace_relationship(
        &self,
        publication: &PtracePublicationGuard<'_>,
        target: &Thread,
        ptracer: &Thread,
        authorized_ptracer: &CredentialSnapshotGuard<'_>,
        origin: PtraceRelationshipOrigin,
        relationship_credential: &Arc<Cred>,
        seized: bool,
        initial_options: u32,
        authorized: &ProcessImageAccessSnapshot,
        mut reverse_link: PreparedPtraceReverseLink<'_>,
    ) -> AxResult<PtraceSession> {
        if !core::ptr::eq(publication.owner, &*self.proc_data)
            || !core::ptr::eq(publication.target, self)
        {
            return Err(AxError::BadState);
        }
        let tracer = ptracer.proc_data.proc.pid();
        let tracer_kernel_tid = ptracer.kernel_tid();
        if let Some(tracer_owner) = publication.tracer_owner
            && (!core::ptr::eq(tracer_owner, &*ptracer.proc_data)
                || tracer_owner.proc.pid() != tracer
                || !tracer_owner
                    .proc
                    .thread_ids()
                    .any(|tid| tid == tracer_kernel_tid))
        {
            return Err(AxError::NoSuchProcess);
        }
        let ptracer_slot = ptracer.credential_slot();
        if ptracer.exit.load(Ordering::Acquire)
            || !ptracer
                .proc_data
                .proc
                .thread_ids()
                .any(|tid| tid == tracer_kernel_tid)
            || !core::ptr::eq(authorized_ptracer.slot(), &*ptracer_slot)
        {
            return Err(AxError::NoSuchProcess);
        }
        let relationship_owner = match origin {
            PtraceRelationshipOrigin::Attach => ptracer,
            PtraceRelationshipOrigin::Traceme => target,
            PtraceRelationshipOrigin::Inherited => target,
        };
        if origin != PtraceRelationshipOrigin::Inherited {
            let relationship_slot = relationship_owner.credential_slot();
            if !Arc::ptr_eq(relationship_credential, &relationship_slot.current()) {
                return Err(AxError::BadState);
            }
        }
        if let Some(node) = reverse_link.node.as_mut() {
            // Core process identity survives nonleader exec/TID rebinding and
            // final scheduler task destruction. It is lookup metadata only.
            node.process = self.proc_data.proc.pid();
        }
        if reverse_link.tracer != tracer
            || reverse_link.tracer_kernel_tid != tracer_kernel_tid
            || reverse_link
                .node
                .as_ref()
                .is_none_or(|node| node.tracee != self.kernel_tid())
        {
            return Err(AxError::BadState);
        }
        if !core::ptr::eq(target, self)
            || target.exit.load(Ordering::Acquire)
            || !self
                .proc_data
                .proc
                .thread_ids()
                .any(|tid| tid == target.kernel_tid())
        {
            return Err(AxError::NoSuchProcess);
        }
        let exec_ctl = self.proc_data.exec_ctl.lock();
        if exec_ctl.group_exit || target.exit.load(Ordering::Acquire) {
            return Err(AxError::NoSuchProcess);
        }
        if exec_ctl.owner.is_some() {
            return Err(AxError::OperationNotPermitted);
        }
        let image = self.proc_data.image_binding.read();
        if !Arc::ptr_eq(&image.aspace, &authorized.aspace)
            || !Arc::ptr_eq(&image.access_state, &authorized.access_state)
            || !authorized.exact_target_matches(target)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let security = image.access_state.security.lock();
        if security.dumpability != authorized.dumpability
            || !Arc::ptr_eq(&image.access_state.owner_user_ns, &authorized.owner_user_ns)
        {
            return Err(AxError::OperationNotPermitted);
        }
        let current_credential = target.credential_slot().current();
        if !Arc::ptr_eq(&current_credential, &authorized.credential)
            || target.exit.load(Ordering::Acquire)
        {
            return Err(AxError::OperationNotPermitted);
        }
        if origin == PtraceRelationshipOrigin::Traceme
            && !Arc::ptr_eq(relationship_credential, &authorized.credential)
        {
            return Err(AxError::OperationNotPermitted);
        }
        if origin == PtraceRelationshipOrigin::Attach
            && !Arc::ptr_eq(relationship_credential, authorized_ptracer.credential())
        {
            return Err(AxError::BadState);
        }
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        let old_generation = ptrace_ctl.generation;
        let Some(session) = ptrace_ctl.try_begin(
            tracer,
            tracer_kernel_tid,
            seized,
            initial_options,
            origin,
            relationship_credential,
        ) else {
            return Err(if ptrace_ctl.active_session().is_some() {
                AxError::OperationNotPermitted
            } else {
                AxError::OutOfRange
            });
        };
        // The option word and the exact task it describes publish under one
        // ptrace-control critical section, so `__secure_computing()` can never
        // read a `SUSPEND_SECCOMP` word whose traced tid is not this one.
        record_ptrace_suspended_tracee(&self.ptrace_suspended_tracee, session, target.kernel_tid());
        if let Err((error, reverse_link)) = reverse_link.publish(session) {
            let retired_relationship = ptrace_ctl
                .rollback_begin(session, old_generation)
                .expect("new ptrace relationship owns rollback session");
            *self.ptrace_suspended_tracee.lock() = None;
            drop(ptrace_ctl);
            drop(current_credential);
            drop(security);
            drop(image);
            drop(exec_ctl);
            // The preallocated node and reservation token are destroyed only
            // after every publication spin/image guard has been released.
            // The relationship credential follows the same destruction-safe
            // boundary because its free hooks may not run under those gates.
            // The typed `relationship_credential` guard still owns the same
            // Arc until the caller releases the outer publication guard, so
            // this rollback drop also cannot be the final free callback.
            drop(reverse_link);
            drop(retired_relationship);
            return Err(error);
        }
        drop(ptrace_ctl);
        drop(current_credential);
        drop(security);
        drop(image);
        drop(exec_ctl);
        Ok(session)
    }

    /// Stops at a signal-delivery boundary while transferring exact queue
    /// ownership into ptrace state. On failure the caller gets the untouched
    /// record back and may publish it normally.
    // Returning the record by value is the rollback contract stated above;
    // boxing it would add an allocation to signal delivery.
    #[allow(clippy::result_large_err)]
    pub(crate) fn try_ptrace_signal_stop(
        &self,
        record: PtraceSignalRecord,
    ) -> Result<(), PtraceSignalRecord> {
        let mut pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let mut job_ctl = self.ptrace_job_ctl.lock();
        let Some(session) = ptrace_ctl.active_session() else {
            return Err(record);
        };
        if job_ctl.state != StopState::Running || pending.is_some() {
            return Err(record);
        }

        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = record.info().signo() as u8;
        job_ctl.ptrace_event = 0;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.stop_notified = false;
        job_ctl.continued = false;
        *pending = Some(record);
        Ok(())
    }

    pub(crate) fn ptrace_signal_info(&self, session: PtraceSession) -> Option<SignalInfo> {
        let pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.ptrace_job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session)
            || ptrace_ctl.listening
            || !job_ctl.is_ptrace_inactive_for(session)
        {
            return None;
        }
        if let Some(record) = pending.as_ref() {
            return Some(*record.info());
        }
        let signo = Signo::from_repr(job_ctl.stop_signal)?;
        let event = job_ctl.ptrace_event;
        if event == 0 {
            return None;
        }
        drop(job_ctl);
        drop(ptrace_ctl);
        drop(pending);
        Some(SignalInfo::new_user(
            signo,
            (event as i32) << 8 | signo as i32,
            self.pid_ns().visible_pid(self.proc_data.proc.pid()),
            self.user_ns()
                .from_kuid_munged(self.current_cred().ids().ruid),
        ))
    }

    pub(crate) fn replace_ptrace_signal_info(
        &self,
        session: PtraceSession,
        info: SignalInfo,
    ) -> AxResult<()> {
        let mut pending = self.ptrace_signal.lock();
        let ptrace_ctl = self.ptrace_ctl.lock();
        let job_ctl = self.ptrace_job_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session)
        {
            return Err(AxError::NoSuchProcess);
        }
        let record = pending.as_mut().ok_or(AxError::InvalidInput)?;
        if record.info().signo() != info.try_signo().ok_or(AxError::InvalidInput)? {
            return Err(AxError::InvalidInput);
        }
        record
            .replace_info(info)
            .map(|_| ())
            .ok_or(AxError::InvalidInput)
    }

    /// Resumes a ptrace stop and atomically takes its retained signal record.
    /// If `detach` is true, tracer ownership is cleared under the same gate so
    /// no new delivery stop can appear between resume and detach.
    pub(crate) fn resume_ptrace(
        &self,
        session: PtraceSession,
        detach: bool,
        commit_resume: impl FnOnce(),
    ) -> Option<(
        ContinueResult,
        Option<PtraceSignalRecord>,
        Option<PtraceRelationshipSnapshot>,
    )> {
        self.resume_ptrace_inner(session, detach, true, commit_resume)
    }

    fn resume_ptrace_inner(
        &self,
        session: PtraceSession,
        detach: bool,
        require_inactive: bool,
        commit_resume: impl FnOnce(),
    ) -> Option<(
        ContinueResult,
        Option<PtraceSignalRecord>,
        Option<PtraceRelationshipSnapshot>,
    )> {
        let mut pending = self.ptrace_signal.lock();
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return None;
        }

        let mut job_ctl = self.ptrace_job_ctl.lock();
        if require_inactive && !job_ctl.is_ptrace_inactive_for(session) {
            return None;
        }
        // A stopped owner can be woken by an unrelated interrupt before the
        // explicit stop-event notification. Commit resume mode while the job
        // gate still excludes any observation of Running. This callback must
        // only update task-local state: no allocation, usercopy or sleep.
        commit_resume();
        let retired_relationship = detach.then(|| {
            let retired = ptrace_ctl
                .clear_session(session)
                .expect("validated ptrace session must clear");
            *self.ptrace_suspended_tracee.lock() = None;
            retired
        });

        let result = match job_ctl.state {
            StopState::Running => ContinueResult::None,
            StopState::Stopping if !require_inactive && job_ctl.stop_kind == StopKind::Ptrace => {
                job_ctl.state = StopState::Running;
                job_ctl.ptrace_session = None;
                ContinueResult::CanceledStopping
            }
            StopState::Stopping => ContinueResult::None,
            StopState::Stopped => {
                if job_ctl.is_ptrace_inactive_for(session) {
                    job_ctl.state = StopState::Running;
                    job_ctl.ptrace_session = None;
                    ContinueResult::ResumedStopped
                } else {
                    ContinueResult::None
                }
            }
        };
        let record = if result == ContinueResult::None && !require_inactive {
            None
        } else {
            pending.take()
        };
        drop(job_ctl);
        drop(ptrace_ctl);
        drop(pending);
        // The caller carries `retired_relationship` past any sleepable outer
        // ptrace-action guard before dropping it. Credential security free
        // hooks may run when this was the final owner.
        Some((result, record, retired_relationship))
    }

    /// Publishes one stop only for the exact relationship which requested it.
    /// A stale attach/exec completion cannot stop a later reattachment that
    /// happens to use the same numeric tracer PID.
    pub(crate) fn ptrace_stop(&self, session: PtraceSession, signo: u8) -> bool {
        let ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        let mut job_ctl = self.ptrace_job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session) {
            return false;
        }
        if job_ctl.stop_kind == StopKind::Ptrace && job_ctl.state != StopState::Running {
            return false;
        }
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.ptrace_event = 0;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.stop_notified = false;
        job_ctl.continued = false;
        true
    }

    /// Publishes a clone/fork/vfork event for an already traced parent.  The
    /// child PID is retained in the ptrace control word for GETEVENTMSG while
    /// wait status derives its event high byte from the job-control record.
    pub(crate) fn ptrace_event_stop(
        &self,
        session: PtraceSession,
        event: u8,
        message: usize,
    ) -> bool {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) {
            return false;
        }
        let mut job_ctl = self.ptrace_job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session)
            || (job_ctl.stop_kind == StopKind::Ptrace && job_ctl.state != StopState::Running)
        {
            return false;
        }
        ptrace_ctl.event_message = message;
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = Signo::SIGTRAP as u8;
        job_ctl.ptrace_event = event;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.stop_notified = false;
        job_ctl.continued = false;
        true
    }

    /// Applies `PTRACE_INTERRUPT` to an exact seized relationship. Unlike
    /// ordinary actions this is allowed while the tracee is running.
    pub(crate) fn ptrace_interrupt(&self, session: PtraceSession, signo: u8) -> Option<bool> {
        let mut ptrace_ctl = self.ptrace_ctl.lock();
        if ptrace_ctl.active_session() != Some(session) || !ptrace_ctl.seized {
            return None;
        }
        let mut job_ctl = self.ptrace_job_ctl.lock();
        if job_ctl.is_ptrace_inactive_for(session) && !ptrace_ctl.listening {
            // Retain the promised trap across the current stop and resume.
            ptrace_ctl.interrupt_pending = true;
            return Some(false);
        }
        ptrace_ctl.listening = false;
        ptrace_ctl.interrupt_pending = false;
        job_ctl.state = StopState::Stopped;
        job_ctl.stop_signal = signo;
        job_ctl.ptrace_event = 128;
        job_ctl.stop_kind = StopKind::Ptrace;
        job_ctl.ptrace_session = Some(session);
        job_ctl.stop_reported = false;
        job_ctl.stop_notified = false;
        job_ctl.continued = false;
        Some(true)
    }

    /// Publishes the wake only after the caller has resolved the retained
    /// ptrace signal record. This prevents a tracee from returning to user mode
    /// before a requested reinjection has become pending.
    pub(crate) fn finish_ptrace_resume(&self, result: ContinueResult) {
        if result != ContinueResult::None {
            self.ptrace_stop_event.wake();
        }
    }

    pub(crate) fn end_ptrace(&self, session: PtraceSession) -> Option<PtraceRelationshipSnapshot> {
        let (result, record, retired_relationship) =
            self.resume_ptrace_inner(session, true, false, || {})?;
        if let Some(record) = record {
            super::super::timer::acknowledge_posix_timer_signal(&self.proc_data, record.info());
            drop(record);
        }
        self.finish_ptrace_resume(result);
        Some(retired_relationship.expect("end_ptrace always detaches the validated relationship"))
    }

    pub(crate) fn clear_ptrace(&self) -> Option<PtraceRelationshipSnapshot> {
        let (relationship, record) = {
            let mut pending = self.ptrace_signal.lock();
            let mut ptrace_ctl = self.ptrace_ctl.lock();
            let relationship = ptrace_ctl.clear_active();
            *self.ptrace_suspended_tracee.lock() = None;
            (relationship, pending.take())
        };
        if let Some(record) = record {
            super::super::timer::acknowledge_posix_timer_signal(&self.proc_data, record.info());
            drop(record);
        }
        // The process-exit caller keeps this owner until its outer
        // ptrace-action guard is gone.
        relationship
    }
}

impl Thread {
    pub(crate) fn should_wait_for_ptrace_stop(&self) -> bool {
        self.ptrace_job_ctl.lock().state != StopState::Running
    }
    pub(crate) fn peek_ptrace_stop_status(&self, filter: StopFilter) -> Option<StopReport> {
        let report = self.ptrace_job_ctl.lock().stop_report_for(filter)?;
        self.ptrace_stop_ready().then_some(report)
    }
    pub(crate) fn claim_ptrace_stop_status(&self, report: StopReport) -> Option<StopReport> {
        let mut job = self.ptrace_job_ctl.lock();
        if job.stop_reported || job.current_stop_report()? != report {
            return None;
        }
        job.stop_reported = true;
        Some(report)
    }
}
