//! Readiness controls, pending streams, and multishot execution.

use super::*;

#[self_referencing]
pub(super) struct OwnedPollRegistration {
    pub(super) file: FileHandle<dyn FileLike>,
    #[borrows(file)]
    #[covariant]
    pub(super) registration: PollRegistration<'this>,
}

#[derive(Default)]
pub(super) struct PollRegistrationState {
    pub(super) arming: bool,
    pub(super) woke_during_arm: bool,
    pub(super) registration: Option<OwnedPollRegistration>,
}

pub(super) struct PollCallbackState {
    pub(super) ring: Weak<IoUring>,
    pub(super) request: RequestId,
    pub(super) enabled: AtomicBool,
    pub(super) source_woke: AtomicBool,
    pub(super) deferred_next: AtomicPtr<PollCallbackState>,
    pub(super) deferred_queued: AtomicBool,
}

impl PollCallbackState {
    pub(super) fn publish_source_wake(self: &Arc<Self>) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        self.source_woke.store(true, Ordering::Release);
        // A source can call us on the timer IRQ stack. Upgrading the ring
        // here would allow its final strong reference (and SharedPages) to
        // drop in IRQ context after a concurrent policy-worker close. Queue
        // only this callback token; its ring ownership remains weak.
        if self.deferred_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        let node = Arc::into_raw(Arc::clone(self)).cast_mut();
        let mut head = DEFERRED_POLL_CALLBACKS.load(Ordering::Acquire);
        loop {
            self.deferred_next.store(head, Ordering::Relaxed);
            match DEFERRED_POLL_CALLBACKS.compare_exchange_weak(
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
}

pub(super) struct PollWake(Arc<PollCallbackState>);

impl Wake for PollWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.publish_source_wake();
    }
}

pub(super) struct PollControl {
    pub(super) request: RequestId,
    pub(super) events: IoEvents,
    pub(super) multishot: bool,
    pub(super) active: AtomicBool,
    pub(super) callback: Arc<PollCallbackState>,
    pub(super) waker: Once<Waker>,
    pub(super) registration: SpinNoIrq<PollRegistrationState>,
    pub(super) lease: SpinNoIrq<Option<IoUringFileLease>>,
}

impl PollControl {
    pub(super) fn try_new(
        ring: Weak<IoUring>,
        request: RequestId,
        lease: IoUringFileLease,
        events: IoEvents,
        multishot: bool,
    ) -> Result<Arc<Self>, (AxError, IoUringFileLease)> {
        let callback = match Arc::try_new(PollCallbackState {
            ring,
            request,
            enabled: AtomicBool::new(true),
            source_woke: AtomicBool::new(false),
            deferred_next: AtomicPtr::new(ptr::null_mut()),
            deferred_queued: AtomicBool::new(false),
        }) {
            Ok(callback) => callback,
            Err(_) => return Err((AxError::NoMemory, lease)),
        };
        let control = match Arc::try_new(Self {
            request,
            events,
            multishot,
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

    pub(super) fn file_handle(&self) -> AxResult<FileHandle<dyn FileLike>> {
        self.lease
            .lock()
            .as_ref()
            .ok_or(AxError::BadState)?
            .description()
            .map(|description| description.file_handle())
    }

    pub(super) fn description(&self) -> AxResult<Arc<FileDescription>> {
        self.lease
            .lock()
            .as_ref()
            .ok_or(AxError::BadState)
            .and_then(|lease| lease.description().cloned())
    }

    pub(super) fn begin_registration(&self) -> bool {
        let mut state = self.registration.lock();
        if !self.active.load(Ordering::Acquire) || state.arming || state.registration.is_some() {
            return false;
        }
        state.arming = true;
        state.woke_during_arm = false;
        true
    }

    pub(super) fn finish_registration(&self, registration: OwnedPollRegistration) {
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

    pub(super) fn abort_registration(&self) {
        let mut state = self.registration.lock();
        state.arming = false;
        state.woke_during_arm = false;
    }

    pub(super) fn registration_fired(&self) {
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

    pub(super) fn ensure_armed(&self) -> AxResult<()> {
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

    pub(super) fn check_arm_check(&self) -> AxResult<IoEvents> {
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

    pub(super) fn take_source_wake(&self) -> bool {
        self.callback.source_woke.swap(false, Ordering::AcqRel)
    }

    pub(super) fn has_source_wake(&self) -> bool {
        self.callback.source_woke.load(Ordering::Acquire)
    }

    pub(super) fn deactivate(&self) -> Option<IoUringFileLease> {
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
/// ring fd is closed. It is removed after the issued request has won its
/// terminal transition, before publishing the CQE permits buffer reuse.
pub(super) struct PendingStreamWork {
    pub(super) slot: usize,
    pub(super) issued: Option<IssuedRequest>,
    pub(super) request: ReadWriteRequest,
    pub(super) control: Arc<PollControl>,
    pub(super) buffer: IoUringBufferLease,
    pub(super) context: IoOperationContext,
    pub(super) capability: UserMemoryCapability,
}

/// Socket-only multishot owner.  Unlike `PendingStreamWork`, it never owns a
/// supplied buffer between shots: recv acquires one when readiness fires and
/// releases it only after that shot's CQE publication.  The retained OFD and
/// issued proof make cancellation/final-close a single terminal transition.
pub(crate) struct SocketMultishotWork {
    pub(super) slot: usize,
    pub(super) issued: Option<IssuedRequest>,
    pub(super) control: Arc<PollControl>,
    pub(super) context: IoOperationContext,
    pub(super) recv_group: Option<u16>,
    pub(super) accepting: bool,
    pub(super) accept_flags: u32,
    pub(super) recv_request: Option<ReadWriteRequest>,
}

impl SocketMultishotWork {
    pub(super) fn request_id(&self) -> RequestId {
        self.issued
            .as_ref()
            .expect("socket multishot lost issued owner")
            .id()
    }

    pub(super) fn finalise(mut self, ring: &IoUring, result: i32) {
        let issued = self
            .issued
            .take()
            .expect("socket multishot lost terminal owner");
        let lease = self.control.deactivate();
        let _ = ring.state.lock().requests.abort_nonterminal_shot(&issued);
        if let Err(error) = ring.complete_issued(issued, TerminalCause::Completed, result, 0) {
            error!("io_uring socket multishot final CQE failed: {error:?}");
        }
        drop(lease);
    }
}

impl IoUring {
    /// Weak ownership for an external provider completion bridge.  The
    /// provider must never keep a closed ring alive merely because it still
    /// retains an I/O request and its long-term pin.
    pub(crate) fn weak_owner(&self) -> Weak<IoUring> {
        self.self_weak
            .get()
            .expect("live io_uring missing self weak")
            .clone()
    }

    pub(crate) fn uring_cmd_completion(
        &self,
        issued: IssuedRequest,
        file: Option<IoUringFileLease>,
        iopoll: bool,
    ) -> UringCmdCompletion {
        let ring = self
            .self_weak
            .get()
            .and_then(Weak::upgrade)
            .expect("live io_uring issued a URING_CMD without self ownership");
        UringCmdCompletion::new(ring, issued, file, iopoll)
    }

    pub(crate) fn register_uring_cmd(
        &self,
        issued: &IssuedRequest,
        file: FileHandle<dyn FileLike>,
        iopoll: bool,
    ) -> AxResult<()> {
        let mut state = self.state.lock();
        let (_, request) = state
            .requests
            .request(issued.id())
            .map_err(map_core_error)?;
        if !matches!(request, thekernel_linux_io_uring::RequestState::Issued(_)) {
            return Err(AxError::BadState);
        }
        let slot = issued.id().slot() as usize;
        let entry = state
            .iopoll_uring_cmd
            .get_mut(slot)
            .ok_or(AxError::BadState)?;
        if entry.is_some() {
            return Err(AxError::BadState);
        }
        *entry = Some(UringCmdOwner {
            id: issued.id(),
            file,
            iopoll,
            disabled: AtomicBool::new(false),
            state: UringCmdHandoffState::Prepared,
        });
        Ok(())
    }

    pub(crate) fn complete_uring_cmd(
        &self,
        issued: IssuedRequest,
        cause: TerminalCause,
        result: i32,
        flags: u32,
        _iopoll: bool,
    ) -> AxResult<()> {
        {
            let mut state = self.state.lock();
            let slot = issued.id().slot() as usize;
            if state
                .iopoll_uring_cmd
                .get(slot)
                .is_some_and(|entry| entry.as_ref().is_some_and(|owner| owner.id == issued.id()))
            {
                state.iopoll_uring_cmd[slot] = None;
            }
        }
        self.complete_issued(issued, cause, result, flags)
    }

    pub(crate) fn unregister_iopoll_uring_cmd(&self, id: RequestId) {
        let mut state = self.state.lock();
        let slot = id.slot() as usize;
        if state
            .iopoll_uring_cmd
            .get(slot)
            .is_some_and(|entry| entry.as_ref().is_some_and(|owner| owner.id == id))
        {
            state.iopoll_uring_cmd[slot] = None;
        }
    }

    /// Begins provider handoff. Cancellation may mark this entry pending but
    /// cannot remove it until the provider has observed the command.
    pub(crate) fn begin_uring_cmd_handoff(&self, id: RequestId) -> AxResult<bool> {
        let mut state = self.state.lock();
        let (_, request) = state.requests.request(id).map_err(map_core_error)?;
        if !matches!(request, thekernel_linux_io_uring::RequestState::Issued(_)) {
            return Ok(false);
        }
        let owner = state
            .iopoll_uring_cmd
            .get_mut(id.slot() as usize)
            .and_then(Option::as_mut)
            .filter(|owner| owner.id == id)
            .ok_or(AxError::BadState)?;
        if owner.state != UringCmdHandoffState::Prepared {
            return Ok(false);
        }
        owner.state = UringCmdHandoffState::Submitting;
        Ok(true)
    }

    pub(crate) fn finish_uring_cmd_handoff(
        &self,
        id: RequestId,
    ) -> AxResult<Option<FileHandle<dyn FileLike>>> {
        let mut state = self.state.lock();
        let owner = state
            .iopoll_uring_cmd
            .get_mut(id.slot() as usize)
            .and_then(Option::as_mut)
            .filter(|owner| owner.id == id)
            .ok_or(AxError::BadState)?;
        if owner.state == UringCmdHandoffState::CancelPending {
            return Ok(Some(owner.file.clone()));
        }
        owner.state = UringCmdHandoffState::Active;
        Ok(None)
    }

    pub(crate) fn harvest_iopoll_uring_cmd(&self) -> AxResult<()> {
        // Do not allocate a transient provider list in the GETEVENTS path.
        // Clone one retained OFD at a time, release ring state, then let the
        // provider complete/cancel its exact generation without lock nesting.
        let capacity = self.state.lock().iopoll_uring_cmd.len();
        for slot in 0..capacity {
            let provider = self
                .state
                .lock()
                .iopoll_uring_cmd
                .get(slot)
                .and_then(Option::as_ref)
                .and_then(|owner| {
                    (owner.iopoll && owner.state == UringCmdHandoffState::Active)
                        .then_some((owner.id, owner.file.clone()))
                });
            if let Some((_id, provider)) = provider {
                provider.harvest_uring_cmd()?;
            }
        }
        Ok(())
    }

    /// Applies the generic link edge before execution crosses the issue
    /// boundary.  A follower is retained in its own preallocated slot and
    /// only released by the predecessor's terminal transition.
    pub(crate) fn submit_with_dependencies(
        &self,
        work: SubmissionWork,
    ) -> AxResult<DependencyDispatch> {
        use thekernel_linux_io_uring::SubmissionLink;

        let id = work.id();
        let dependencies = work.dependencies().unwrap_or_default();
        let mut state = self.state.lock();
        let previous = state.link_tail.take();
        if !matches!(dependencies.link(), SubmissionLink::None) {
            state.link_tail = Some((id, dependencies.link()));
        }
        let (predecessor, hardlink) = match previous {
            Some((predecessor, link)) => match state
                .terminal_results
                .get(predecessor.slot() as usize)
                .copied()
                .flatten()
            {
                Some((completed, result))
                    if completed == predecessor
                        && result < 0
                        && matches!(link, SubmissionLink::Soft) =>
                {
                    return Ok(DependencyDispatch::Cancelled(work));
                }
                // A different generation in this reusable slot proves the
                // predecessor was terminal and reaped; it cannot remain an
                // outstanding link owner.
                Some((completed, _)) if completed == predecessor => (None, false),
                Some(_) => (None, false),
                None => (Some(predecessor), matches!(link, SubmissionLink::Hard)),
            },
            None => (None, false),
        };
        if predecessor.is_none()
            && (!dependencies.drain()
                || state
                    .requests
                    .prior_requests_terminal(id)
                    .map_err(map_core_error)?)
        {
            return Ok(DependencyDispatch::Execute(work));
        }
        let slot = id.slot() as usize;
        let parked = state
            .parked_submissions
            .get_mut(slot)
            .ok_or(AxError::BadState)?;
        if parked.is_some() {
            return Err(AxError::BadState);
        }
        *parked = Some(ParkedSubmission {
            work,
            predecessor,
            hardlink,
            drain: dependencies.drain(),
        });
        Ok(DependencyDispatch::Parked)
    }

    pub(super) fn release_linked_after_terminal(
        &self,
        predecessor: RequestId,
        result: i32,
    ) -> AxResult<Option<DependencyDispatch>> {
        let mut state = self.state.lock();
        let result_slot = predecessor.slot() as usize;
        *state
            .terminal_results
            .get_mut(result_slot)
            .ok_or(AxError::BadState)? = Some((predecessor, result));
        let Some(slot) = state.parked_submissions.iter().position(|entry| {
            entry.as_ref().is_some_and(|parked| {
                (parked.predecessor == Some(predecessor)
                    && (!parked.drain
                        || state
                            .requests
                            .prior_requests_terminal(parked.work.id())
                            .unwrap_or(false)))
                    || (parked.predecessor.is_none()
                        && parked.drain
                        && state
                            .requests
                            .prior_requests_terminal(parked.work.id())
                            .unwrap_or(false))
            })
        }) else {
            return Ok(None);
        };
        let parked = state.parked_submissions[slot]
            .take()
            .ok_or(AxError::BadState)?;
        // DRAIN is only an execution barrier.  Only the actual soft-link
        // predecessor propagates its failure into -ECANCELED.
        Ok(Some(
            if parked.predecessor == Some(predecessor) && result < 0 && !parked.hardlink {
                DependencyDispatch::Cancelled(parked.work)
            } else {
                DependencyDispatch::Execute(parked.work)
            },
        ))
    }

    /// Publishes a visible multishot notification without consuming the
    /// retained issued request.  The core registry keeps its terminal credit
    /// and request identity live until the owner emits its sole final CQE.
    pub(super) fn publish_nonterminal_issued(
        &self,
        issued: &IssuedRequest,
        result: i32,
        flags: u32,
    ) -> AxResult<()> {
        let publication = {
            let mut state = self.state.lock();
            state
                .requests
                .publish_nonterminal(issued, result, flags)
                .map_err(map_core_error)?
        };
        if self.defer_taskrun && !self.final_close_requested.load(Ordering::Acquire) {
            let slot = issued.id().slot() as usize;
            let mut state = self.state.lock();
            let pending = state
                .pending_nonterminal_publications
                .get_mut(slot)
                .ok_or(AxError::BadState)?;
            if pending.is_some() {
                return Err(AxError::BadState);
            }
            *pending = Some(publication);
            self.pending_nonterminal_publication_count
                .fetch_add(1, Ordering::AcqRel);
            self.taskrun_pending.store(true, Ordering::Release);
            return Ok(());
        }
        self.publish_nonterminal_publication_now(publication)
    }

    pub(super) fn publish_nonterminal_publication_now(
        &self,
        publication: CompletionPublication,
    ) -> AxResult<()> {
        let published = {
            let _publication = self.completion_serial.lock();
            if let Err(error) = self.write_completion(&publication) {
                // A nonterminal plan never changes request state, so failed
                // user-ring I/O only clears the registry's in-flight gate.
                let mut state = self.state.lock();
                if state
                    .requests
                    .rollback_nonterminal_publication(publication)
                    .is_err()
                {
                    return Err(AxError::BadState);
                }
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
        Ok(())
    }

    /// Takes ownership of an issued socket multishot after the original file
    /// lease and security snapshot have been admitted.  Failures are terminal
    /// by construction: the caller must not retain a second path to `issued`.
    pub(crate) fn admit_socket_multishot(
        &self,
        issued: IssuedRequest,
        file: IoUringFileLease,
        context: IoOperationContext,
        accepting: bool,
        accept_flags: u32,
        recv_request: Option<ReadWriteRequest>,
        recv_group: Option<u16>,
        _capability: UserMemoryCapability,
    ) -> AxResult<()> {
        let owner = match self.self_weak.get().and_then(Weak::upgrade) {
            Some(owner) => owner,
            None => {
                return self.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::EIO.code(),
                    0,
                );
            }
        };
        let id = issued.id();
        let events = if accepting {
            IoEvents::READABLE
        } else {
            IoEvents::READABLE
        };
        let control = match PollControl::try_new(Arc::downgrade(&owner), id, file, events, true) {
            Ok(control) => control,
            Err((error, _file)) => {
                return self.complete_issued(
                    issued,
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
                return self.complete_issued(
                    issued,
                    TerminalCause::PreparationFailed,
                    -LinuxError::from(error).code(),
                    0,
                );
            }
        };
        let work = SocketMultishotWork {
            slot: 0,
            issued: Some(issued),
            control: Arc::clone(&control),
            context,
            recv_group,
            accepting,
            accept_flags,
            recv_request,
        };
        if let Err(work) = self.install_socket_multishot(work) {
            let issued = work
                .issued
                .expect("socket multishot installation lost request");
            drop(work.control.deactivate());
            return self.complete_issued(
                issued,
                TerminalCause::PreparationFailed,
                -LinuxError::EBUSY.code(),
                0,
            );
        }
        if !ready.is_empty() || control.has_source_wake() {
            owner.publish_poll_hint(id);
        }
        Ok(())
    }
    pub(crate) fn install_socket_multishot(
        &self,
        mut work: SocketMultishotWork,
    ) -> Result<(), SocketMultishotWork> {
        let mut state = self.state.lock();
        let Some(slot) = state.socket_multishot.iter().position(Option::is_none) else {
            return Err(work);
        };
        work.slot = slot;
        state.socket_multishot[slot] = Some(work);
        Ok(())
    }

    pub(super) fn claim_socket_multishot(&self, request: RequestId) -> Option<SocketMultishotWork> {
        let mut state = self.state.lock();
        let slot = state.socket_multishot.iter().position(|work| {
            work.as_ref()
                .is_some_and(|work| work.request_id() == request)
        })?;
        state.socket_multishot[slot].take()
    }

    pub(super) fn reinsert_socket_multishot(
        &self,
        work: SocketMultishotWork,
    ) -> Result<(), SocketMultishotWork> {
        let slot = work.slot;
        let mut state = self.state.lock();
        if !work.issued.as_ref().is_some_and(|issued| {
            state.requests.issued_is_live(issued)
                || state.requests.issued_shot_publication_pending(issued)
        }) {
            return Err(work);
        }
        if state.socket_multishot.get(slot).is_none_or(Option::is_some) {
            return Err(work);
        }
        state.socket_multishot[slot] = Some(work);
        Ok(())
    }
}

impl PendingStreamWork {
    pub(super) fn request_id(&self) -> RequestId {
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

pub(super) fn poll_events_from_linux(events: u32) -> IoEvents {
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

pub(super) fn poll_events_to_linux(events: IoEvents) -> u32 {
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

impl IoUring {
    pub(super) fn pending_stream_count(&self) -> usize {
        self.state.lock().pending_stream_count
    }

    /// Installs one already-admitted pending stream owner into the fixed
    /// ring-local table. Returning the owner on failure keeps the issued
    /// proof and both leases available for an explanatory terminal CQE.
    #[allow(clippy::result_large_err)]
    pub(super) fn install_pending_stream(
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
            false,
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

    pub(super) fn take_pending_stream(&self, request: RequestId) -> Option<PendingStreamWork> {
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
    pub(super) fn reinsert_pending_stream(
        &self,
        work: PendingStreamWork,
    ) -> Result<(), PendingStreamWork> {
        let slot = work.slot;
        let mut state = self.state.lock();
        // A readiness worker temporarily owns the table slot while it tries
        // one read.  ASYNC_CANCEL/final-close may have consumed the request's
        // terminal credit during that interval; such work must be dropped,
        // never rearmed for a second I/O attempt.
        if !work
            .issued
            .as_ref()
            .is_some_and(|issued| state.requests.issued_is_live(issued))
        {
            return Err(work);
        }
        // The close cursor can have passed this slot while the worker held
        // it in ShotInFlight.  Never reintroduce a readable-owner after close
        // begins; the caller terminalizes this restored Issued request below.
        if self.final_close_requested.load(Ordering::Acquire) {
            return Err(work);
        }
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

    pub(super) fn complete_pending_stream_work(
        self: &Arc<Self>,
        mut work: PendingStreamWork,
        result: i32,
    ) {
        let issued = work
            .issued
            .take()
            .expect("pending stream completion lost issued request");
        let lease = work.control.deactivate();
        // `retry_pending_stream` enters ShotInFlight before its first
        // externally visible read.  Finish from that state atomically rather
        // than reopening cancellation between the read and its final CQE.
        let completed = (|| {
            let permit = self
                .state
                .lock()
                .requests
                .claim_terminal_after_nonterminal_shot(&issued, TerminalCause::Completed)
                .map_err(map_core_error)?;
            // The read has stopped and cancellation cannot win. Release the
            // exact owners outside the ring lock, before making a CQE pending.
            drop(lease);
            drop(work);
            let mut state = self.state.lock();
            let token = state
                .requests
                .finish_terminal(permit, result, 0)
                .map_err(map_core_error)?;
            self.queue_completion_locked(&mut state, token)
        })();
        if let Err(error) = completed {
            error!("io_uring pending stream completion failed: {error:?}");
        } else {
            let id = issued.id();
            match self.release_linked_after_terminal(id, result) {
                Ok(linked) => {
                    if let Err(error) = self.publish_pending_slot(id.slot() as usize) {
                        error!("io_uring pending stream CQE publication failed: {error:?}");
                    }
                    if let Some(dispatch) = linked {
                        if let Err(error) =
                            crate::syscall::dispatch_dependency_submission(self, dispatch)
                        {
                            error!("io_uring pending stream dependency release failed: {error:?}");
                        }
                    }
                }
                Err(error) => {
                    error!("io_uring pending stream dependency accounting failed: {error:?}")
                }
            }
        }
        if self.final_close_requested.load(Ordering::Acquire) {
            self.enqueue_deferred();
        }
    }

    pub(super) fn complete_pending_stream_error(
        self: &Arc<Self>,
        work: PendingStreamWork,
        error: AxError,
    ) {
        self.complete_pending_stream_work(work, -LinuxError::from(error).code());
    }

    /// Fails an owner after it has been returned from ShotInFlight to Issued
    /// for a readiness rearm.  This path must not use the shot finalizer: it
    /// intentionally exposes a normal cancellable request again before the
    /// rearm decision.
    pub(super) fn fail_pending_stream_rearm(
        self: &Arc<Self>,
        mut work: PendingStreamWork,
        error: AxError,
    ) {
        let issued = work
            .issued
            .take()
            .expect("pending stream rearm lost issued request");
        let lease = work.control.deactivate();
        drop(lease);
        drop(work);
        let _ = self.complete_issued(
            issued,
            TerminalCause::PreparationFailed,
            -LinuxError::from(error).code(),
            0,
        );
    }

    /// Retries one exact pending stream owner in task context. IRQ/readiness
    /// callbacks only set the source-wake bit and enqueue this ring; all user
    /// memory and pipe consumption happens here under a fixed worker budget.
    pub(super) fn retry_pending_stream(self: &Arc<Self>, control: Arc<PollControl>) {
        let request = control.request;
        let Some(work) = self.take_pending_stream(request) else {
            return;
        };
        let begun = work
            .issued
            .as_ref()
            .map(|issued| self.state.lock().requests.begin_side_effect(issued))
            .unwrap_or(Err(IoUringError::UnknownRequest));
        if begun.is_err() {
            // Cancellation/close (or another execution claimant) has
            // already won. Do not touch the FIFO or supplied buffer from a
            // stale readiness hint.
            drop(work.control.deactivate());
            drop(work);
            return;
        }
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
                let aborted = work
                    .issued
                    .as_ref()
                    .map(|issued| self.state.lock().requests.abort_nonterminal_shot(issued))
                    .unwrap_or(Err(IoUringError::UnknownRequest));
                if aborted.is_err() {
                    drop(work.control.deactivate());
                    drop(work);
                    return;
                }
                let ready = work.control.check_arm_check();
                match ready {
                    Ok(ready) if !ready.is_empty() || work.control.has_source_wake() => {
                        match self.reinsert_pending_stream(work) {
                            Ok(()) => self.publish_poll_hint(request),
                            Err(work) => {
                                error!("io_uring pending stream owner lost while rearming");
                                self.fail_pending_stream_rearm(work, AxError::BadState);
                            }
                        }
                    }
                    Ok(_) => {
                        if let Err(work) = self.reinsert_pending_stream(work) {
                            self.fail_pending_stream_rearm(work, AxError::BadState);
                        }
                    }
                    Err(error) => self.fail_pending_stream_rearm(work, error),
                }
            }
            Err(error) => self.complete_pending_stream_error(work, error),
        }
    }

    /// Executes exactly one socket multishot readiness edge.  The owner is
    /// removed from its fixed slot while a shot runs, preventing cancellation,
    /// close, and a second wake from observing a duplicate issued proof.
    pub(super) fn retry_socket_multishot(self: &Arc<Self>, control: Arc<PollControl>) {
        const IORING_CQE_F_BUFFER: u32 = 1;
        const IORING_CQE_F_MORE: u32 = 1 << 1;
        const IORING_CQE_F_SOCK_NONEMPTY: u32 = 1 << 2;
        let request = control.request;
        let Some(work) = self.claim_socket_multishot(request) else {
            return;
        };
        let begun = work
            .issued
            .as_ref()
            .map(|issued| self.state.lock().requests.begin_nonterminal_shot(issued))
            .unwrap_or(Err(IoUringError::UnknownRequest));
        if begun.is_err() {
            // A full CQ reservation is backpressure, not a terminal socket
            // result. Preserve this exact owner until a reap makes space;
            // cancellation/close still wins through the normal terminal path.
            if !self.final_close_requested.load(Ordering::Acquire) {
                self.rearm_socket_multishot(work);
                return;
            }
            drop(work.control.deactivate());
            return;
        }
        let shot = if work.accepting {
            work.control.description().and_then(|description| {
                let socket = crate::file::PinnedSocketDescription::from_description(description)?;
                crate::syscall::accept_pinned(
                    &socket,
                    work.context.security().actor_arc(),
                    work.accept_flags,
                )
                .map(|fd| (fd, 0))
            })
        } else {
            let result = (|| {
                let group = work.recv_group.ok_or(AxError::BadState)?;
                let buffer = self.acquire_provided_buffer(group)?;
                let (address, length) = buffer.range()?;
                let request = work.recv_request.ok_or(AxError::BadState)?;
                let capability = buffer.capability()?;
                let description = work.control.description()?;
                let socket = crate::file::PinnedSocketDescription::from_description(description)?;
                let result = crate::syscall::io_uring_recv_pinned(
                    &capability,
                    &socket,
                    address as *mut u8,
                    usize::try_from(length).map_err(|_| AxError::BadAddress)?,
                    request.recv_flags(),
                )?;
                let mut flags = buffer
                    .provided_id()
                    .map(|id| IORING_CQE_F_BUFFER | (u32::from(id) << 16))
                    .unwrap_or(0);
                let socket_nonempty = work
                    .control
                    .check_arm_check()
                    .is_ok_and(|ready| !ready.is_empty());
                if socket_nonempty && !result.eof {
                    flags |= IORING_CQE_F_SOCK_NONEMPTY;
                }
                // The lease returns to its group only after the CQE is
                // visible; a completion therefore never identifies a buffer
                // another live shot can concurrently select.
                Ok::<_, AxError>((result.bytes, flags, result.eof, buffer))
            })();
            match result {
                Ok((result, flags, eof, mut buffer)) => {
                    if eof {
                        // EOF is the socket receive terminal condition; do
                        // not leave an always-readable peer-closed socket
                        // generating an infinite stream of MORE CQEs.
                        drop(buffer);
                        work.finalise(self, 0);
                        return;
                    }
                    let issued = work.issued.as_ref().expect("socket multishot lost request");
                    // Mark the provided slot consumed before releasing the
                    // CQ tail. A failed CQ write restores the leased slot so
                    // the Drop path returns it to Ready without a ghost CQE.
                    buffer.consume_provided();
                    let published =
                        self.publish_nonterminal_issued(issued, result, flags | IORING_CQE_F_MORE);
                    if published.is_err() {
                        buffer.rollback_consumed_provided();
                    }
                    drop(buffer);
                    published.map(|_| (0, 0))
                }
                Err(error) => Err(error),
            }
        };
        match shot {
            Ok((result, flags)) if work.accepting => {
                let issued = work.issued.as_ref().expect("socket multishot lost request");
                match self.publish_nonterminal_issued(
                    issued,
                    i32::try_from(result).unwrap_or(-LinuxError::EOVERFLOW.code()),
                    flags | IORING_CQE_F_MORE,
                ) {
                    Ok(()) => self.rearm_socket_multishot(work),
                    Err(error) => work.finalise(self, -LinuxError::from(error).code()),
                }
            }
            Ok(_) => self.rearm_socket_multishot(work),
            Err(AxError::WouldBlock) => {
                if let Some(issued) = work.issued.as_ref() {
                    let _ = self.state.lock().requests.abort_nonterminal_shot(issued);
                }
                self.rearm_socket_multishot(work)
            }
            Err(error) => work.finalise(self, -LinuxError::from(error).code()),
        }
    }

    pub(super) fn rearm_socket_multishot(self: &Arc<Self>, work: SocketMultishotWork) {
        // Final close may have started while this owner was outside its fixed
        // slot in ShotInFlight.  Never put it back after that intent: emit
        // the one terminal cancellation CQE instead.
        if self.final_close_requested.load(Ordering::Acquire) {
            work.finalise(self, -LinuxError::ECANCELED.code());
            return;
        }
        let request = work.request_id();
        match work.control.check_arm_check() {
            Ok(ready) => match self.reinsert_socket_multishot(work) {
                Ok(()) if !ready.is_empty() => self.publish_poll_hint(request),
                Ok(()) => {}
                Err(work) => work.finalise(self, -LinuxError::EIO.code()),
            },
            Err(error) => work.finalise(self, -LinuxError::from(error).code()),
        }
    }

    pub(super) fn commit_poll_admission(
        &self,
        mut admission: SubmissionAdmission<'_>,
        lease: IoUringFileLease,
        linux_events: u32,
        multishot: bool,
        capability: UserMemoryCapability,
    ) -> AxResult<()> {
        let id = admission
            .reservation
            .as_ref()
            .ok_or(AxError::BadState)?
            .id();
        let ring = self.self_weak.get().ok_or(AxError::BadState)?.clone();
        let events = poll_events_from_linux(linux_events);
        let control = match PollControl::try_new(ring, id, lease, events, multishot) {
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
            let result = poll_events_to_linux(ready) as i32;
            if control.multishot {
                self.publish_multishot_poll(&control, result)?;
            } else {
                self.finish_poll_control(&control, TerminalCause::Completed, result)?;
            }
        }
        if control.has_source_wake()
            && let Some(ring) = self.self_weak.get().and_then(Weak::upgrade)
        {
            ring.publish_poll_hint(id);
        }
        Ok(())
    }

    pub(super) fn finish_poll_control(
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
        let linked = self.release_linked_after_terminal(id, result)?;
        let publication = self.publish_pending_slot(id.slot() as usize);
        drop(lease);
        drop(control);
        publication?;
        if let Some(dispatch) = linked {
            let _ = crate::syscall::dispatch_dependency_submission(self, dispatch)?;
        }
        Ok(true)
    }

    /// Publishes one `IORING_CQE_F_MORE` notification while retaining the
    /// original poll request and its readiness registration.  Each visible
    /// CQE receives a fresh, bounded request slot/credit; the long-lived poll
    /// itself never reuses an already published terminal slot.
    pub(super) fn publish_multishot_poll(
        &self,
        expected: &Arc<PollControl>,
        result: i32,
    ) -> AxResult<bool> {
        const IORING_CQE_F_MORE: u32 = 1 << 1;
        let token = {
            let mut state = self.state.lock();
            let slot = usize::try_from(expected.request.slot()).map_err(|_| AxError::BadState)?;
            if state
                .polls
                .get(slot)
                .and_then(Option::as_ref)
                .is_none_or(|control| !Arc::ptr_eq(control, expected))
            {
                return Ok(false);
            }
            let (original, _) = state
                .requests
                .request(expected.request)
                .map_err(map_core_error)?;
            let descriptor = RequestDescriptor::new(
                original.user_data(),
                thekernel_linux_io_uring::RequestOperation::PollAdd,
            );
            let reservation = state.requests.reserve(descriptor).map_err(map_core_error)?;
            let id = reservation.id();
            let prepared = state.requests.commit(reservation).map_err(map_core_error)?;
            let permit = state
                .requests
                .claim_terminal(prepared.id(), TerminalCause::Completed)
                .map_err(map_core_error)?;
            let token = state
                .requests
                .finish_terminal(permit, result, IORING_CQE_F_MORE)
                .map_err(map_core_error)?;
            self.queue_completion_locked(&mut state, token)?;
            id
        };
        self.publish_pending_slot(token.slot() as usize)?;
        Ok(true)
    }
}

pub(crate) fn has_deferred_io_uring_work() -> bool {
    !DEFERRED_IO_URING_WORK.load(Ordering::Acquire).is_null()
        || !DEFERRED_POLL_CALLBACKS.load(Ordering::Acquire).is_null()
        || !DEFERRED_POLL_CONTINUATION.load(Ordering::Acquire).is_null()
}

pub(super) fn drain_deferred_poll_callbacks() {
    let mut node = DEFERRED_POLL_CONTINUATION.swap(ptr::null_mut(), Ordering::AcqRel);
    if node.is_null() {
        node = DEFERRED_POLL_CALLBACKS.swap(ptr::null_mut(), Ordering::AcqRel);
    }
    for _ in 0..POLL_CALLBACK_BUDGET {
        if node.is_null() {
            break;
        }
        // SAFETY: publication transfers one Arc reference; the queued bit
        // prevents reuse until we have detached this node's next pointer.
        let callback = unsafe { Arc::from_raw(node) };
        let next = callback
            .deferred_next
            .swap(ptr::null_mut(), Ordering::AcqRel);
        callback.deferred_queued.store(false, Ordering::Release);
        if callback.enabled.load(Ordering::Acquire)
            && let Some(ring) = callback.ring.upgrade()
        {
            let state = ring.state.lock();
            if state.requests.request(callback.request).is_ok()
                && callback.enabled.load(Ordering::Acquire)
            {
                ring.publish_poll_hint(callback.request);
            }
        }
        node = next;
    }
    // Keep the unvisited nodes' queue-owned references and queued bits intact.
    // has_deferred_io_uring_work schedules another pass; ring/close work gets
    // its turn now even if sources keep publishing more callbacks.
    DEFERRED_POLL_CONTINUATION.store(node, Ordering::Release);
}

pub(crate) fn drain_deferred_io_uring_work() {
    drain_deferred_poll_callbacks();
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
