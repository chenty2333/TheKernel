use alloc::sync::Arc;

use thekernel_linux_process_adapter::Pid;

use super::Cred;

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::task) enum StopState {
    Running  = 0,
    Stopping = 1,
    Stopped  = 2,
}

impl From<u8> for StopState {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Stopping,
            2 => Self::Stopped,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::task) enum StopKind {
    JobControl = 0,
    Ptrace     = 1,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct StopReport {
    pub(crate) signal: u8,
    pub(crate) ptrace_session: Option<PtraceSession>,
}

impl StopReport {
    pub(crate) const fn traced(self) -> bool {
        self.ptrace_session.is_some()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ContinueResult {
    None,
    CanceledStopping,
    ResumedStopped,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::task) struct JobControlState {
    pub(in crate::task) state: StopState,
    pub(in crate::task) stop_signal: u8,
    pub(in crate::task) stop_kind: StopKind,
    /// Exact relationship which published a ptrace stop.
    ///
    /// Keeping the generation with the stop prevents a detach/reattach by the
    /// same numeric tracer PID from treating an earlier stop as its own.
    pub(in crate::task) ptrace_session: Option<PtraceSession>,
    pub(in crate::task) continued: bool,
    pub(in crate::task) stop_reported: bool,
}

impl JobControlState {
    pub(in crate::task) fn is_ptrace_inactive_for(&self, session: PtraceSession) -> bool {
        self.state == StopState::Stopped
            && self.stop_kind == StopKind::Ptrace
            && self.ptrace_session == Some(session)
    }

    pub(in crate::task) fn stop_report_for(
        &self,
        expected_ptrace_session: Option<PtraceSession>,
    ) -> Option<StopReport> {
        if self.state != StopState::Stopped || self.stop_reported {
            return None;
        }
        let ptrace_session = match self.stop_kind {
            StopKind::JobControl => None,
            StopKind::Ptrace => Some(self.ptrace_session?),
        };
        if ptrace_session.is_some() && ptrace_session != expected_ptrace_session {
            return None;
        }
        Some(StopReport {
            signal: self.stop_signal,
            ptrace_session,
        })
    }

    pub(in crate::task) fn current_stop_report(&self) -> Option<StopReport> {
        if self.state != StopState::Stopped {
            return None;
        }
        let ptrace_session = match self.stop_kind {
            StopKind::JobControl => None,
            StopKind::Ptrace => Some(self.ptrace_session?),
        };
        Some(StopReport {
            signal: self.stop_signal,
            ptrace_session,
        })
    }
}

impl Default for JobControlState {
    fn default() -> Self {
        Self {
            state: StopState::Running,
            stop_signal: 0,
            stop_kind: StopKind::JobControl,
            ptrace_session: None,
            continued: false,
            stop_reported: false,
        }
    }
}

/// Identity of one published ptrace relationship.
///
/// The generation is process-lifetime monotonic.  It is deliberately retained
/// across detach so cleanup from an earlier relationship cannot remove or act
/// on a later relationship owned by the same numeric tracer PID.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PtraceSession {
    /// Tracer thread-group identity used for signal/wait integration.
    pub(crate) tracer: Pid,
    /// Immutable kernel identity of the exact task which owns this relation.
    pub(crate) tracer_kernel_tid: Pid,
    pub(crate) generation: u64,
}

/// Linux chooses the stored `ptracer_cred` source from the operation which
/// creates the relationship.  ATTACH/SEIZE store the tracer's credential,
/// while TRACEME's `ptrace_link(current, real_parent)` stores the calling
/// tracee's current credential even though its parent is the hook actor.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum PtraceRelationshipOrigin {
    Attach,
    Traceme,
}

/// One atomically observed ptrace relationship and the immutable credential
/// which authorized its publication.
///
/// This is Linux's relationship-time `ptracer_cred`: ATTACH/SEIZE capture the
/// exact ptracer task, while TRACEME captures the calling tracee. Consumers
/// must use this owner together with `session`; looking the tracer up by PID
/// later could observe a different credential or a recycled task identity.
#[derive(Clone)]
pub(crate) struct PtraceRelationshipSnapshot {
    session: PtraceSession,
    origin: PtraceRelationshipOrigin,
    ptracer_cred: Arc<Cred>,
}

impl PtraceRelationshipSnapshot {
    pub(crate) const fn session(&self) -> PtraceSession {
        self.session
    }

    pub(crate) const fn origin(&self) -> PtraceRelationshipOrigin {
        self.origin
    }

    pub(crate) fn ptracer_cred(&self) -> &Arc<Cred> {
        &self.ptracer_cred
    }
}

#[derive(Default)]
pub(in crate::task) struct PtraceControlState {
    relationship: Option<PtraceRelationshipSnapshot>,
    /// Last generation successfully published. Never reset on detach.
    pub(in crate::task) generation: u64,
    /// Whether the current relationship was created by `PTRACE_SEIZE`.
    pub(in crate::task) seized: bool,
    pub(in crate::task) options: u32,
    pub(in crate::task) event_message: usize,
}

impl PtraceControlState {
    pub(in crate::task) fn active_session(&self) -> Option<PtraceSession> {
        self.relationship
            .as_ref()
            .map(PtraceRelationshipSnapshot::session)
    }

    /// Clones the session and its relationship-time credential under one state
    /// observation.  The clone is destroyed by the caller after the ptrace
    /// control guard is released.
    pub(in crate::task) fn active_relationship(&self) -> Option<PtraceRelationshipSnapshot> {
        self.relationship.clone()
    }

    pub(in crate::task) fn active_session_if_owned_by(
        &self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
    ) -> Option<PtraceSession> {
        self.active_session().filter(|session| {
            session.tracer == tracer && session.tracer_kernel_tid == tracer_kernel_tid
        })
    }

    pub(in crate::task) fn try_begin(
        &mut self,
        tracer: Pid,
        tracer_kernel_tid: Pid,
        seized: bool,
        initial_options: u32,
        origin: PtraceRelationshipOrigin,
        ptracer_cred: &Arc<Cred>,
    ) -> Option<PtraceSession> {
        if self.relationship.is_some() {
            return None;
        }
        let generation = self.generation.checked_add(1)?;
        let session = PtraceSession {
            tracer,
            tracer_kernel_tid,
            generation,
        };
        self.relationship = Some(PtraceRelationshipSnapshot {
            session,
            origin,
            ptracer_cred: ptracer_cred.clone(),
        });
        self.generation = generation;
        self.seized = seized;
        self.options = initial_options;
        self.event_message = 0;
        Some(session)
    }

    /// Removes an exact relationship without destroying its credential under
    /// the caller's ptrace/job-control spin guards.  The returned retirement
    /// owner must be dropped only after those guards have been released.
    pub(in crate::task) fn clear_session(
        &mut self,
        session: PtraceSession,
    ) -> Option<PtraceRelationshipSnapshot> {
        if self.active_session() != Some(session) {
            return None;
        }
        let relationship = self.relationship.take();
        self.seized = false;
        self.options = 0;
        self.event_message = 0;
        relationship
    }

    pub(in crate::task) fn clear_active(&mut self) -> Option<PtraceRelationshipSnapshot> {
        let session = self.active_session()?;
        let relationship = self.clear_session(session);
        debug_assert!(relationship.is_some());
        relationship
    }

    /// Aborts a relationship whose reverse link could not be published.  Its
    /// generation never became externally visible, so restore the previous
    /// value while carrying the credential owner out to a safe drop boundary.
    pub(in crate::task) fn rollback_begin(
        &mut self,
        session: PtraceSession,
        previous_generation: u64,
    ) -> Option<PtraceRelationshipSnapshot> {
        let relationship = self.clear_session(session)?;
        self.generation = previous_generation;
        Some(relationship)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct ExecControlState {
    pub(in crate::task) owner: Option<Pid>,
    pub(in crate::task) pending_thread_additions: usize,
    pub(in crate::task) group_exit: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct VforkControlState {
    pub(in crate::task) parent_tid: Option<Pid>,
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::{
        JobControlState, PtraceControlState, PtraceRelationshipOrigin, StopKind, StopState,
    };
    use crate::task::{Cred, UserNamespace};

    fn credential() -> Arc<Cred> {
        Cred::try_root(UserNamespace::try_new_root().unwrap()).unwrap()
    }

    #[test]
    fn process_access_ptrace_seize_publishes_options_with_relationship() {
        let mut control = PtraceControlState::default();
        let ptracer_cred = credential();
        let session = control
            .try_begin(
                7,
                70,
                true,
                0x40,
                PtraceRelationshipOrigin::Attach,
                &ptracer_cred,
            )
            .unwrap();
        assert_eq!(control.active_session(), Some(session));
        assert_eq!(control.active_session_if_owned_by(7, 70), Some(session));
        assert_eq!(control.active_session_if_owned_by(7, 71), None);
        assert!(control.seized);
        assert_eq!(control.options, 0x40);

        assert!(control.clear_session(session).is_some());
        let attached = control
            .try_begin(
                7,
                70,
                false,
                0,
                PtraceRelationshipOrigin::Attach,
                &ptracer_cred,
            )
            .unwrap();
        assert_ne!(attached, session);
        assert!(!control.seized);
        assert_eq!(control.options, 0);
    }

    #[test]
    fn process_access_ptrace_inactive_and_wait_report_require_exact_generation() {
        let mut control = PtraceControlState::default();
        let ptracer_cred = credential();
        let old = control
            .try_begin(
                7,
                70,
                false,
                0,
                PtraceRelationshipOrigin::Attach,
                &ptracer_cred,
            )
            .unwrap();
        let mut job = JobControlState {
            state: StopState::Stopped,
            stop_signal: 19,
            stop_kind: StopKind::Ptrace,
            ptrace_session: Some(old),
            continued: false,
            stop_reported: false,
        };
        assert!(job.is_ptrace_inactive_for(old));
        let old_report = job.stop_report_for(Some(old)).unwrap();
        assert!(old_report.traced());

        assert!(control.clear_session(old).is_some());
        let new = control
            .try_begin(
                7,
                70,
                false,
                0,
                PtraceRelationshipOrigin::Attach,
                &ptracer_cred,
            )
            .unwrap();
        assert_ne!(new, old);
        assert!(!job.is_ptrace_inactive_for(new));
        assert_eq!(job.stop_report_for(Some(new)), None);

        // A late restore from the old waiter cannot match a newly published
        // stop owned by a later relationship using the same tracer PID.
        job.ptrace_session = Some(new);
        assert_ne!(job.current_stop_report(), Some(old_report));
        assert_eq!(job.stop_report_for(Some(old)), None);
        assert_eq!(
            job.stop_report_for(Some(new)).unwrap().ptrace_session,
            Some(new)
        );

        // Ordinary child job-control reports remain sessionless and are not
        // accidentally hidden merely because the parent is also a tracer.
        job.stop_kind = StopKind::JobControl;
        job.ptrace_session = None;
        let report = job.stop_report_for(Some(new)).unwrap();
        assert!(!report.traced());
        assert_eq!(report.ptrace_session, None);
    }
}
