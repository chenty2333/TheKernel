use crate::{
    RegistrationLifecycle, RestartDecision, RseqArea, RseqDescriptor, RseqEpoch, RseqError,
    RseqRegisterPlan, RseqRegistration, RseqRegistrationRequest, RseqRegistrationState,
    RseqUnregisterPlan, decide_restart,
};

/// Scheduler/signal observations that can require rseq IP fixup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RseqEventMask(u32);

impl RseqEventMask {
    /// No restart-triggering event is pending.
    pub const EMPTY: Self = Self(0);
    /// A preemption observation occurred.
    pub const PREEMPT: Self = Self(1 << 0);
    /// Signal delivery occurred.
    pub const SIGNAL: Self = Self(1 << 1);
    /// CPU migration occurred.
    pub const MIGRATE: Self = Self(1 << 2);
    /// Complete Linux v6.6 event vocabulary.
    pub const ALL: Self = Self(Self::PREEMPT.0 | Self::SIGNAL.0 | Self::MIGRATE.0);

    /// Decodes a raw event mask, rejecting bits outside the profile.
    pub const fn from_bits(bits: u32) -> Result<Self, RseqError> {
        if bits & !Self::ALL.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(RseqError::InvalidEventFlags)
        }
    }

    /// Returns raw policy bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether no restart-triggering event is pending.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every bit in `other` is pending.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Adds event observations without wrapping or allocation.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Removes event observations after a successful restart-gate finalize.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl Default for RseqEventMask {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Non-wrapping revision fencing observations and lifecycle transitions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RseqRevision(u64);

impl RseqRevision {
    /// Builds a revision for an adapter-owned fixture.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw revision number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances without ABA reuse.
    pub const fn next(self) -> Result<Self, RseqError> {
        match self.0.checked_add(1) {
            Some(raw) => Ok(Self(raw)),
            None => Err(RseqError::RevisionExhausted),
        }
    }
}

/// Fork behavior corresponding to Linux's `rseq_fork()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkMode {
    /// `CLONE_VM`: child is a new thread and starts without rseq state.
    CloneVm,
    /// Private-VM fork: child inherits registration and pending events.
    PrivateVm,
}

impl ForkMode {
    /// Spelling useful to callers that name the flag rather than the mode.
    pub const CLONE_VM: Self = Self::CloneVm;
}

#[derive(Debug, Eq, PartialEq)]
struct ResumeReservation {
    revision: RseqRevision,
    events: RseqEventMask,
}

#[derive(Debug, Eq, PartialEq)]
struct ExecReservation {
    revision: RseqRevision,
    epoch: RseqEpoch,
}

#[derive(Debug, Eq, PartialEq)]
struct ForkReservation {
    revision: RseqRevision,
    mode: ForkMode,
}

/// Thread-scoped registration, pending-event, and resume policy.
///
/// External side effects use an explicit reservation protocol:
///
/// 1. `prepare_register`, `prepare_unregister`, `begin_resume`,
///    `prepare_fork`, or `prepare_exec` reserves every fallible counter
///    transition;
/// 2. the adapter performs usercopy/register/IP side effects while holding its
///    required gate; and
/// 3. the corresponding finalize method is infallible for that token, while
///    `cancel_*` handles an adapter-side failure.
///
/// In particular, an event revision published between a side effect and its
/// finalize cannot strand the side effect behind a stale-plan error.  Events
/// raised during a resume reservation remain pending for the next gate.  A
/// fork reservation is stricter: event publication and all lifecycle
/// reservations are rejected until the fork is committed or canceled, so its
/// child snapshot cannot race with parent state changes.
#[derive(Debug, Eq, PartialEq)]
pub struct ThreadRseq {
    registration: RseqRegistrationState,
    pending_events: RseqEventMask,
    revision: RseqRevision,
    resume_reservation: Option<ResumeReservation>,
    exec_reservation: Option<ExecReservation>,
    fork_reservation: Option<ForkReservation>,
}

impl ThreadRseq {
    /// Creates an unregistered thread state with no pending events.
    pub const fn new() -> Self {
        Self {
            registration: RseqRegistrationState::new(),
            pending_events: RseqEventMask::EMPTY,
            revision: RseqRevision::new(0),
            resume_reservation: None,
            exec_reservation: None,
            fork_reservation: None,
        }
    }

    /// Creates an empty state at a specified resume revision.
    pub const fn with_revision(revision: RseqRevision) -> Self {
        Self {
            registration: RseqRegistrationState::new(),
            pending_events: RseqEventMask::EMPTY,
            revision,
            resume_reservation: None,
            exec_reservation: None,
            fork_reservation: None,
        }
    }

    /// Current registration lifecycle.
    pub const fn lifecycle(&self) -> RegistrationLifecycle {
        self.registration.lifecycle()
    }

    /// Current active registration, if any.
    pub const fn registration(&self) -> Option<RseqRegistration> {
        self.registration.registration()
    }

    /// Current registration epoch.
    pub const fn epoch(&self) -> RseqEpoch {
        self.registration.epoch()
    }

    /// Current non-wrapping resume revision.
    pub const fn revision(&self) -> RseqRevision {
        self.revision
    }

    /// Events observed but not yet consumed by a successful restart-gate
    /// finalize.
    pub const fn pending_events(&self) -> RseqEventMask {
        self.pending_events
    }

    /// Whether a side-effect transaction is in flight.
    pub const fn has_pending_operation(&self) -> bool {
        self.registration.has_pending_operation()
            || self.resume_reservation.is_some()
            || self.exec_reservation.is_some()
            || self.fork_reservation.is_some()
    }

    fn ensure_lifecycle_idle(&self) -> Result<(), RseqError> {
        if self.registration.has_pending_operation()
            || self.resume_reservation.is_some()
            || self.exec_reservation.is_some()
            || self.fork_reservation.is_some()
        {
            Err(RseqError::OperationInProgress)
        } else {
            Ok(())
        }
    }

    /// Reserves registration state before the adapter performs user access.
    /// The returned plan must be finalized or canceled; finalize cannot fail
    /// because epoch exhaustion was checked before the side effect.
    pub fn prepare_register(
        &mut self,
        request: RseqRegistrationRequest,
    ) -> Result<ThreadRegisterPlan, RseqError> {
        self.ensure_lifecycle_idle()?;
        if self.registration.has_pending_operation() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        let plan = self.registration.prepare_register(request)?;
        self.revision = revision;
        Ok(ThreadRegisterPlan { revision, plan })
    }

    /// Finalizes a successful adapter-side registration.  This method is
    /// intentionally non-fallible after the side effect.
    pub fn commit_register(&mut self, plan: ThreadRegisterPlan) -> RseqRegistration {
        self.registration.commit_register(plan.plan)
    }

    /// Cancels a failed adapter-side registration attempt.
    pub fn cancel_register(&mut self, plan: ThreadRegisterPlan) {
        self.registration.cancel_register(plan.plan);
    }

    /// Reserves exact unregister state before adapter-side teardown.
    pub fn prepare_unregister(
        &mut self,
        request: RseqRegistrationRequest,
    ) -> Result<ThreadUnregisterPlan, RseqError> {
        self.ensure_lifecycle_idle()?;
        if self.registration.has_pending_operation() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        let plan = self.registration.prepare_unregister(request)?;
        self.revision = revision;
        Ok(ThreadUnregisterPlan { revision, plan })
    }

    /// Finalizes a successful adapter-side unregister without a stale-plan
    /// branch.
    pub fn commit_unregister(&mut self, plan: ThreadUnregisterPlan) -> RseqRegistration {
        self.registration.commit_unregister(plan.plan)
    }

    /// Cancels a failed adapter-side unregister attempt.
    pub fn cancel_unregister(&mut self, plan: ThreadUnregisterPlan) {
        self.registration.cancel_unregister(plan.plan);
    }

    /// Publishes pending preemption/signal/migration observations.
    pub fn raise_events(&mut self, events: RseqEventMask) -> Result<(), RseqError> {
        // A fork snapshot is a point-in-time child contract.  Do not let an
        // event race between prepare_fork and its external commit; callers
        // must cancel the failed fork or commit the reserved snapshot first.
        if self.fork_reservation.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        self.pending_events = self.pending_events.union(events);
        self.revision = revision;
        Ok(())
    }

    /// Alias for [`Self::raise_events`].
    pub fn notify(&mut self, events: RseqEventMask) -> Result<(), RseqError> {
        self.raise_events(events)
    }

    /// Begins a restart decision.  A successful gate snapshots and consumes
    /// all pending events, regardless of whether it returns `NoActive`,
    /// `ClearOnly`, or `Abort`.  The snapshot is reserved before the adapter
    /// performs its side effect; events raised after this point remain in
    /// `pending_events` for the next gate.
    pub fn begin_resume(
        &mut self,
        area: RseqArea,
        descriptor: Option<RseqDescriptor>,
        instruction_pointer: u64,
        registration_signature: u32,
        abort_signature: u32,
    ) -> Result<ResumePlan, RseqError> {
        self.ensure_lifecycle_idle()?;
        if self.resume_reservation.is_some() {
            return Err(RseqError::OperationInProgress);
        }
        let decision = decide_restart(
            area,
            descriptor,
            instruction_pointer,
            self.pending_events,
            registration_signature,
            abort_signature,
        )?;
        let events = self.pending_events;
        if events.is_empty() {
            return Ok(ResumePlan {
                revision: self.revision,
                events,
                decision,
            });
        }

        let revision = self.revision.next()?;
        self.pending_events = RseqEventMask::EMPTY;
        self.revision = revision;
        self.resume_reservation = Some(ResumeReservation { revision, events });
        Ok(ResumePlan {
            revision,
            events,
            decision,
        })
    }

    /// Finalizes a successful adapter-side restart-gate operation.  No event
    /// revision check can fail after the external side effect.
    pub fn commit_resume(&mut self, plan: ResumePlan) -> RestartDecision {
        if !plan.events.is_empty() {
            let reservation = self.resume_reservation.take();
            assert_eq!(
                reservation,
                Some(ResumeReservation {
                    revision: plan.revision,
                    events: plan.events,
                }),
                "rseq resume finalize token does not belong to this state"
            );
        }
        plan.decision
    }

    /// Restores reserved events after an adapter-side restart-gate side effect
    /// failed.  Events raised while the side effect was in flight are kept.
    pub fn cancel_resume(&mut self, plan: ResumePlan) {
        if !plan.events.is_empty() {
            let reservation = self.resume_reservation.take();
            assert_eq!(
                reservation,
                Some(ResumeReservation {
                    revision: plan.revision,
                    events: plan.events,
                }),
                "rseq resume cancel token does not belong to this state"
            );
            self.pending_events = self.pending_events.union(plan.events);
        }
    }

    /// Reserves the epoch/revision transition needed by an upcoming exec.
    /// Calling this before invoking the external exec path ensures that
    /// successful `on_exec_success` cannot fail due counter exhaustion.
    pub fn prepare_exec(&mut self) -> Result<ExecPlan, RseqError> {
        self.ensure_lifecycle_idle()?;
        if self.registration.has_pending_operation() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        let epoch = self.registration.epoch().next()?;
        self.revision = revision;
        self.exec_reservation = Some(ExecReservation { revision, epoch });
        Ok(ExecPlan { revision, epoch })
    }

    /// Clears registration and events only after the adapter has successfully
    /// committed exec.  The finalize path is non-fallible.
    pub fn on_exec_success(&mut self, plan: ExecPlan) -> Option<RseqRegistration> {
        let reservation = self.exec_reservation.take();
        assert_eq!(
            reservation,
            Some(ExecReservation {
                revision: plan.revision,
                epoch: plan.epoch,
            }),
            "rseq exec-success token does not belong to this state"
        );
        let old = self.registration.reset_after_exec(plan.epoch);
        self.pending_events = RseqEventMask::EMPTY;
        old
    }

    /// Leaves registration/events intact when the external exec attempt
    /// failed.
    pub fn cancel_exec(&mut self, plan: ExecPlan) {
        let reservation = self.exec_reservation.take();
        assert_eq!(
            reservation,
            Some(ExecReservation {
                revision: plan.revision,
                epoch: plan.epoch,
            }),
            "rseq exec-cancel token does not belong to this state"
        );
    }

    /// Reserves and fences the child snapshot before an adapter starts its
    /// external fork side effect.  `commit_fork` is infallible for the
    /// returned token.  The parent revision and registration epoch are
    /// consumed at preparation time and are not rolled back on cancellation,
    /// so a failed external fork cannot reuse a transaction identity.
    pub fn prepare_fork(&mut self, mode: ForkMode) -> Result<ForkPlan, RseqError> {
        if self.has_pending_operation() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        let registration = match mode {
            ForkMode::CloneVm => self.registration.fork_clone_vm()?,
            ForkMode::PrivateVm => self.registration.fork_private()?,
        };
        let pending_events = match mode {
            ForkMode::CloneVm => RseqEventMask::EMPTY,
            ForkMode::PrivateVm => self.pending_events,
        };
        self.revision = revision;
        self.fork_reservation = Some(ForkReservation { revision, mode });
        Ok(ForkPlan {
            child: Self {
                registration,
                pending_events,
                revision,
                resume_reservation: None,
                exec_reservation: None,
                fork_reservation: None,
            },
            mode,
        })
    }

    /// Finalizes a successful external fork without a post-side-effect
    /// revision failure.
    pub fn commit_fork(&mut self, plan: ForkPlan) -> Self {
        let reservation = self.fork_reservation.take();
        assert_eq!(
            reservation,
            Some(ForkReservation {
                revision: plan.child.revision,
                mode: plan.mode,
            }),
            "rseq fork finalize token does not belong to this state"
        );
        plan.child
    }

    /// Cancels a failed external fork.  The reserved revision/epoch remain
    /// consumed, preserving non-wrapping transaction identity, while the
    /// parent becomes available for the next operation.
    pub fn cancel_fork(&mut self, plan: ForkPlan) {
        let reservation = self.fork_reservation.take();
        assert_eq!(
            reservation,
            Some(ForkReservation {
                revision: plan.child.revision,
                mode: plan.mode,
            }),
            "rseq fork cancel token does not belong to this state"
        );
    }

    /// Returns a child state according to Linux's explicit fork mode.  This
    /// convenience form performs preparation and finalization immediately;
    /// adapters with an external fork side effect should use
    /// `prepare_fork`/`commit_fork`/`cancel_fork`.
    pub fn fork_child(&mut self, mode: ForkMode) -> Result<Self, RseqError> {
        let plan = self.prepare_fork(mode)?;
        Ok(self.commit_fork(plan))
    }

    /// Explicit alias for adapters that call the operation `on_fork`.
    pub fn on_fork(&mut self, mode: ForkMode) -> Result<Self, RseqError> {
        self.fork_child(mode)
    }

    /// Immediate internal reset for teardown paths that have no external
    /// side effect.  Exec callers must use `prepare_exec`/`on_exec_success`.
    pub fn reset(&mut self) -> Result<Option<RseqRegistration>, RseqError> {
        self.ensure_lifecycle_idle()?;
        if self.registration.has_pending_operation() {
            return Err(RseqError::OperationInProgress);
        }
        let revision = self.revision.next()?;
        let epoch = self.registration.epoch().next()?;
        self.revision = revision;
        let old = self.registration.reset_after_exec(epoch);
        self.pending_events = RseqEventMask::EMPTY;
        Ok(old)
    }
}

impl Default for ThreadRseq {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread registration plan fenced by the non-wrapping resume revision that
/// was reserved before the external registration side effect.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "finalize or cancel after the adapter-side registration attempt"]
pub struct ThreadRegisterPlan {
    revision: RseqRevision,
    plan: RseqRegisterPlan,
}

impl ThreadRegisterPlan {
    /// Revision reserved during preparation.
    pub const fn revision(&self) -> RseqRevision {
        self.revision
    }

    /// Underlying registration epoch reserved during preparation.
    pub const fn epoch(&self) -> RseqEpoch {
        self.plan.epoch()
    }

    /// Request frozen by this plan.
    pub const fn request(&self) -> RseqRegistrationRequest {
        self.plan.request()
    }
}

/// Thread unregister plan fenced by the non-wrapping resume revision reserved
/// before the external unregister side effect.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "finalize or cancel after the adapter-side unregister attempt"]
pub struct ThreadUnregisterPlan {
    revision: RseqRevision,
    plan: RseqUnregisterPlan,
}

impl ThreadUnregisterPlan {
    /// Revision reserved during preparation.
    pub const fn revision(&self) -> RseqRevision {
        self.revision
    }

    /// Underlying registration epoch reserved during preparation.
    pub const fn epoch(&self) -> RseqEpoch {
        self.plan.epoch()
    }

    /// Exact active registration frozen by this plan.
    pub const fn registration(&self) -> RseqRegistration {
        self.plan.registration()
    }
}

/// Resume decision plus the exact pending-event/revision reservation it may
/// consume.  A plan reserves every non-empty event snapshot, regardless of
/// its decision; an event-free plan has no state to reserve.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "finalize or cancel after adapter-side resume handling"]
pub struct ResumePlan {
    revision: RseqRevision,
    events: RseqEventMask,
    decision: RestartDecision,
}

impl ResumePlan {
    /// Revision captured when the decision was prepared.
    pub const fn revision(&self) -> RseqRevision {
        self.revision
    }

    /// Pending events reserved by this decision.
    pub const fn events(&self) -> RseqEventMask {
        self.events
    }

    /// Pure restart decision for the adapter to execute.
    pub const fn decision(&self) -> RestartDecision {
        self.decision
    }
}

/// Exec transaction token.  It must be finalized only after an external exec
/// has committed successfully; otherwise call `cancel_exec`.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "call on_exec_success after successful exec or cancel_exec on failure"]
pub struct ExecPlan {
    revision: RseqRevision,
    epoch: RseqEpoch,
}

impl ExecPlan {
    /// Revision reserved for this exec transition.
    pub const fn revision(&self) -> RseqRevision {
        self.revision
    }

    /// Epoch reserved for the post-exec unregistered state.
    pub const fn epoch(&self) -> RseqEpoch {
        self.epoch
    }
}

/// Fork child snapshot reserved before an adapter's external fork commit.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "commit the child snapshot after successful fork or cancel_fork on failure"]
pub struct ForkPlan {
    child: ThreadRseq,
    mode: ForkMode,
}

impl ForkPlan {
    /// Fork mode used to build this child snapshot.
    pub const fn mode(&self) -> ForkMode {
        self.mode
    }

    /// Revision already reserved for the child state.
    pub const fn revision(&self) -> RseqRevision {
        self.child.revision
    }

    /// Epoch already reserved for the child registration state.
    pub const fn epoch(&self) -> RseqEpoch {
        self.child.registration.epoch()
    }
}
