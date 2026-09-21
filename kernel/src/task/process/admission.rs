//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

/// An exec group-leader handoff whose pointer publication is complete but
/// whose credential post-commit notification has not run yet.
pub(crate) struct GroupLeaderCommit<'a> {
pub(crate)     publication: Option<crate::task::creds::CredentialPublication<'a>>,
pub(crate)     retired_slot: Arc<CredentialSlot>,
pub(crate)     retired_signal: Option<GroupLeaderSignalIdentity>,
}

impl GroupLeaderCommit<'_> {
    pub(crate) fn complete_post_commit(self) -> GroupLeaderRetirement {
        let Self {
            publication,
            retired_slot,
            retired_signal,
        } = self;
        let credential = publication.map(|publication| {
            let (new, retirement) = publication.complete_post_commit();
            // The published slot retains the new credential. This owner is
            // needed only through the callback itself.
            drop(new);
            retirement
        });
        if let Some(retired) = retired_signal.as_ref() {
            // A nonleader exec replaces Linux's old group-leader task. Disable
            // exact publication and drain its private queue before retaining
            // the Arc for the caller's post-switch destruction boundary.
            retired
                .manager
                .retire_registration(retired.registration_tid, false);
        }
        GroupLeaderRetirement {
            _credential: credential,
            _slot: retired_slot,
            _signal: retired_signal,
        }
    }
}

/// Old group-leader and credential ownership retained after notification.
pub(crate) struct GroupLeaderRetirement {
    _credential: Option<crate::task::creds::CredentialRetirement>,
    _slot: Arc<CredentialSlot>,
    _signal: Option<GroupLeaderSignalIdentity>,
}

/// An exec image/credential publication returned only after image,
/// group-leader, and task-alias locks have been released. The caller must run
/// post-commit notification before acquiring later process locks.
#[must_use = "exec publication must complete its credential notification"]
pub(crate) struct ExecImageCommit<'a> {
pub(crate)     group_leader: GroupLeaderCommit<'a>,
pub(crate)     image: LiveProcessImageBinding,
    pub(in crate::task) security: crate::task::security::CommittingExecSecurity,
pub(crate)     credential_lease: CredentialReadLease,
}

impl ExecImageCommit<'_> {
pub(crate) fn complete_post_commit(self) -> ExecImageRetirement {
        let Self {
            group_leader,
            image,
            security,
            credential_lease,
        } = self;
        ExecImageRetirement {
            _group_leader: group_leader.complete_post_commit(),
            _image: image,
            security: Some(security),
            credential_lease: Some(credential_lease),
        }
    }
}

/// Retired image and credential ownership after the generic credential
/// notification but before the full-image committed notification.
#[must_use = "exec retirement must finish its executable lease and full-image notification"]
pub(crate) struct ExecImageRetirement {
    _group_leader: GroupLeaderRetirement,
    _image: LiveProcessImageBinding,
    security: Option<crate::task::security::CommittingExecSecurity>,
    credential_lease: Option<CredentialReadLease>,
}

impl ExecImageRetirement {
    /// Converts the source metadata/content lease into the persistent active
    /// executable reference installed in the new process image. The pending
    /// exec notification and old image remain owned by this token.
pub(crate) fn finish_executable_lease(&mut self) -> AxResult<Option<ExecutableKey>> {
        self.credential_lease
            .take()
            .ok_or(AxError::BadState)?
            .finish()
    }

    /// Emits the full-image committed notification and returns the still-owned
    /// retirement state. The caller must already have installed the hardware
    /// root, executable identity, metadata, signal state, and user context and
    /// released the ptrace action gate; it drops the returned owner only after
    /// releasing the exec and vfork gates.
pub(crate) fn complete_exec_committed(mut self) -> CompletedExecImageRetirement {
        assert!(
            self.credential_lease.is_none(),
            "exec committed before executable lease conversion"
        );
        let security = self
            .security
            .take()
            .expect("exec committed security notification is pending")
            .committed();
        let Self {
            _group_leader,
            _image,
            security: pending_security,
            credential_lease,
        } = self;
        debug_assert!(pending_security.is_none());
        debug_assert!(credential_lease.is_none());
        CompletedExecImageRetirement {
            _group_leader,
            _image,
            _security: security,
        }
    }
}

/// Old image and exact security ownership retained after the full-image hook.
/// Exec drops this only after new thread admission and the vfork parent are
/// released to observe the completed image.
#[must_use = "completed exec retirement must outlive exec and vfork gate release"]
pub(crate) struct CompletedExecImageRetirement {
    _group_leader: GroupLeaderRetirement,
    _image: LiveProcessImageBinding,
    _security: crate::task::security::CompletedExecSecurity,
}

pub(crate) fn process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive | ProcessError::NotInitialized => {
            AxError::NoSuchProcess
        }
        ProcessError::Busy => AxError::ResourceBusy,
        ProcessError::WrongDomain => AxError::BadState,
        _ => AxError::BadState,
    }
}

/// Exec exclusion charge held across fallible clone construction.
pub(crate) struct PendingThreadAddition {
pub(crate)     proc_data: Arc<ProcessData>,
pub(crate)     armed: bool,
}

impl PendingThreadAddition {
    fn finish_locked(&mut self, exec_ctl: &mut ExecControlState) -> bool {
        debug_assert!(exec_ctl.pending_thread_additions != 0);
        exec_ctl.pending_thread_additions -= 1;
        self.armed = false;
        exec_ctl.pending_thread_additions == 0
    }

    fn finish(mut self) {
        let proc_data = self.proc_data.clone();
        let mut exec_ctl = proc_data.exec_ctl.lock();
        let wake = self.finish_locked(&mut exec_ctl);
        drop(exec_ctl);
        if wake {
            proc_data.exec_event.wake();
        }
    }
}

impl Drop for PendingThreadAddition {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let proc_data = self.proc_data.clone();
        let mut exec_ctl = proc_data.exec_ctl.lock();
        let wake = self.finish_locked(&mut exec_ctl);
        drop(exec_ctl);
        if wake {
            proc_data.exec_event.wake();
        }
    }
}

/// Completion guard for a thread whose core identity is published but whose
/// signal/task-table/runqueue transaction is still externally incomplete.
#[must_use = "thread publication remains pending until this guard is finished"]
pub(crate) struct PendingThreadPublication {
    pending: PendingThreadAddition,
    group_exited_at_core: bool,
}

pub(crate) const fn group_exit_handoff_requires_kill(core_observed: bool, late_gate_observed: bool) -> bool {
    core_observed || late_gate_observed
}

impl PendingThreadPublication {
    /// Samples the permanent group-exit gate after TASK_TABLE publication.
    ///
    /// If core publication itself observed group exit, or the gate linearized
    /// between that point and this late sample, the exact prepared task must
    /// receive SIGKILL before entering the runqueue.
pub(crate) fn must_terminate_for_group_exit(&self) -> bool {
        // `group_exit_in_progress` acquires exec_ctl again after TASK_TABLE
        // publication. This is deliberately not a cached core-commit result:
        // it covers group_exit linearizing after core publication but before
        // TASK_TABLE, when the first TID scan cannot resolve the task yet.
        group_exit_handoff_requires_kill(
            self.group_exited_at_core,
            self.pending.proc_data.group_exit_in_progress(),
        )
    }

    /// Makes the completed task/runqueue publication visible to exec/group-exit.
pub(crate) fn finish(self) {
        self.pending.finish();
    }
}

/// Live-process thread membership held across fallible clone construction.
pub(crate) struct ProcessThreadAdmission {
    // Drop the core reservation before making exec observe no pending clone.
pub(crate)     membership: StarryThreadAdmission,
pub(crate)     pending: PendingThreadAddition,
}

impl ProcessThreadAdmission {
    /// Publishes the reserved TID while keeping exec exclusion atomic with the
    /// thread-group mutation.
pub(crate) fn commit(self) -> PendingThreadPublication {
        let Self {
            membership,
            pending,
        } = self;
        let outcome = membership.commit_infallible();
        pending.proc_data.note_thread_admitted();
        PendingThreadPublication {
            pending,
            group_exited_at_core: outcome
                == tk_linux_process_adapter::ThreadPublicationOutcome::GroupExited,
        }
    }
}

/// Unpublished process plus initial thread held across runtime construction.
pub(crate) struct InitialProcessThreadAdmission {
    // Roll back the core composite before making exec observe no pending clone.
pub(crate)     publication: ProcessInitialAdmission,
pub(crate)     pending: PendingThreadAddition,
}

pub(crate) enum ProcessInitialAdmission {
    Ordinary(InitialProcessAdmission),
    ScopeInit(ScopedInitialProcessAdmission),
}

impl ProcessInitialAdmission {
pub(crate) fn process(&self) -> &Arc<Process> {
        match self {
            Self::Ordinary(admission) => admission.process(),
            Self::ScopeInit(admission) => admission.process(),
        }
    }
}

impl InitialProcessThreadAdmission {
    /// Publishes the type-bound process/initial-thread pair before making exec
    /// observe that clone construction has completed.
pub(crate) fn commit(self) -> (Arc<Process>, PendingThreadPublication) {
        let Self {
            publication,
            pending,
        } = self;
        let process = match publication {
            ProcessInitialAdmission::Ordinary(publication) => publication.commit(),
            ProcessInitialAdmission::ScopeInit(publication) => publication
                .commit()
                .expect("scoped init publication lost its reserved reaper scope"),
        };
        pending.proc_data.note_thread_admitted();
        (
            process,
            PendingThreadPublication {
                pending,
                group_exited_at_core: false,
            },
        )
    }
}
