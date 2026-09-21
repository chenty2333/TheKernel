//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

#[derive(Clone)]
pub(crate) struct GroupLeaderSignalIdentity {
pub(crate)     registration_tid: Pid,
pub(crate)     manager: Arc<ThreadSignalManager>,
    /// PID namespace in which the retained process identity lives. This is
    /// needed to filter a zombie from callers in unrelated namespaces.
pub(crate)     pid_ns: Option<Arc<PidNamespace>>,
    /// Shared scheduler snapshot updated by successful scheduler transactions
    /// and retained by the zombie owner after the live scheduler node disappears.
pub(crate)     scheduler: Option<Arc<SpinNoIrq<ZombieSchedulerSnapshot>>>,
    /// Uniquely identifies this installed leader endpoint.  It changes on
    /// every exec replacement, including when per-task scheduler versions
    /// restart from zero on the executor.
    scheduler_identity_token: u64,
    /// Shared with the durable group-leader binding so an exec replacement is
    /// reflected in the owner retained by a zombie payload.
pub(crate)     landlock: Arc<SpinNoIrq<LandlockDomain>>,
    /// The process's resource limits, shared with the live `ProcessData`.
pub(crate)     rlimits: Arc<RwLock<Rlimits>>,
}

impl GroupLeaderSignalIdentity {
pub(crate) fn new(registration_tid: Pid, manager: Arc<ThreadSignalManager>) -> Self {
        Self {
            registration_tid,
            manager,
            pid_ns: None,
            scheduler: None,
            scheduler_identity_token: 0,
            landlock: Arc::new(SpinNoIrq::new(LandlockDomain::default())),
            rlimits: Arc::new(RwLock::default()),
        }
    }

    fn with_pid_namespace_and_scheduler(
        registration_tid: Pid,
        manager: Arc<ThreadSignalManager>,
        pid_ns: Option<Arc<PidNamespace>>,
        scheduler: Arc<SpinNoIrq<ZombieSchedulerSnapshot>>,
        landlock: Arc<SpinNoIrq<LandlockDomain>>,
        rlimits: Arc<RwLock<Rlimits>>,
    ) -> Self {
        Self {
            registration_tid,
            manager,
            pid_ns,
            scheduler: Some(scheduler),
            scheduler_identity_token: 0,
            landlock,
            rlimits,
        }
    }

    fn same_endpoint(&self, other: &Self) -> bool {
        self.registration_tid == other.registration_tid
            && Arc::ptr_eq(&self.manager, &other.manager)
    }
}

/// Shared owner moved through exec handoff and retained in the durable zombie
/// payload. Successful reap takes the sole endpoint from this slot even when a
/// pidfd or wait event still owns the surrounding snapshot.
pub(crate) type GroupLeaderSignalOwner = Arc<SpinNoIrq<Option<GroupLeaderSignalIdentity>>>;

/// Immutable kernel-owned identity of one Linux process, kept by the core
/// registry entry from publication until successful reap.
///
/// Monotonically renewed token for one published group-leader identity.
///
/// A successful exec publishes a new token even when the executor already
/// owns the same credential slot and private signal endpoint.  Consumers that
/// authorize an operation before acquiring their final lifecycle gate can use
/// it to reject that otherwise pointer-identical exec handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GroupLeaderIdentityToken(u64);

/// One coherent, pinned view of the group-leader identity.
///
/// The token is deliberately paired with both owners: an exec can retain the
/// same endpoint, and leader exec can retain the same credential slot.  The
/// token therefore supplies the generation edge those pointer comparisons
/// cannot represent by themselves.
#[derive(Clone)]
pub(crate) struct GroupLeaderIdentitySnapshot {
    token: GroupLeaderIdentityToken,
    credential: Arc<Cred>,
    signal: Arc<ThreadSignalManager>,
}

impl GroupLeaderIdentitySnapshot {
pub(crate) fn token(&self) -> GroupLeaderIdentityToken {
        self.token
    }

pub(crate) fn credential(&self) -> &Arc<Cred> {
        &self.credential
    }

pub(crate) fn signal(&self) -> &Arc<ThreadSignalManager> {
        &self.signal
    }
}

/// Persistent binding to the credential slot and private signal endpoint that
/// currently own Linux thread-group-leader identity.
pub(crate) struct GroupLeaderIdentityBinding {
    current: SpinNoIrq<Arc<CredentialSlot>>,
pub(crate)     signal: GroupLeaderSignalOwner,
    landlock: Arc<SpinNoIrq<LandlockDomain>>,
    /// Starts nonzero and is renewed under the current/signal publication
    /// locks by every exec handoff.  It is read only while those locks are
    /// held, so a snapshot can never pair a new token with old owners.
    identity_token: AtomicU64,
    /// The process PID namespace copied into the durable owner identity.
    pid_ns: Option<Arc<PidNamespace>>,
    /// `signal_struct::rlim`, shared with the live `ProcessData` that serves
    /// `prlimit64(2)` and retained through the durable owner identity.
    ///
    /// Linux keeps the resource limits on the `signal_struct`, which outlives
    /// `do_exit()` until `release_task()`, so a lookup that reaches an
    /// unreaped zombie through `find_task_by_vpid()` still reads the target's
    /// own limits (`include/linux/sched/signal.h:758-762`, `kernel/sys.c:243`).
    /// Sharing one cell instead of copying the values keeps every live writer
    /// authoritative without a second update path.
pub(crate)     rlimits: Arc<RwLock<Rlimits>>,
    /// Changes with each replacement of the private endpoint that owns the
    /// process's group-leader identity. Access is serialized with `current`
    /// and `signal`, which makes a handoff and its scheduler reseed one
    /// durable binding transaction.
    scheduler_identity_epoch: SpinNoIrq<u64>,
    scheduler_identity_token: SpinNoIrq<u64>,
    scheduler: Arc<SpinNoIrq<ZombieSchedulerSnapshot>>,
}

impl GroupLeaderIdentityBinding {
    pub(crate) fn try_new(initial: Arc<CredentialSlot>) -> AxResult<Self> {
        Self::try_new_with_pid_ns(initial, None)
    }

pub(crate) fn try_new_with_pid_ns(
        initial: Arc<CredentialSlot>,
        pid_ns: Option<Arc<PidNamespace>>,
    ) -> AxResult<Self> {
        Ok(Self {
            current: SpinNoIrq::new(initial),
            signal: Arc::try_new(SpinNoIrq::new(None)).map_err(|_| AxError::NoMemory)?,
            landlock: Arc::try_new(SpinNoIrq::new(LandlockDomain::default()))
                .map_err(|_| AxError::NoMemory)?,
            rlimits: Arc::try_new(RwLock::default()).map_err(|_| AxError::NoMemory)?,
            identity_token: AtomicU64::new(1),
            pid_ns,
            scheduler_identity_epoch: SpinNoIrq::new(0),
            scheduler_identity_token: SpinNoIrq::new(0),
            scheduler: Arc::try_new(SpinNoIrq::new(ZombieSchedulerSnapshot::default()))
                .map_err(|_| AxError::NoMemory)?,
        })
    }

pub(crate) fn current_cred(&self) -> Arc<Cred> {
        let slot = self.current.lock().clone();
        slot.current()
    }

pub(crate) fn landlock_domain(&self) -> LandlockDomain {
        self.landlock.lock().clone()
    }
pub(crate) fn replace_landlock_domain(&self, domain: LandlockDomain) {
        *self.landlock.lock() = domain;
    }

pub(crate) fn bind_initial_signal(
        &self,
        registration_tid: Pid,
        signal: Arc<ThreadSignalManager>,
    ) -> AxResult<()> {
        let mut current = self.signal.lock();
        if current.is_some() {
            return Err(AxError::BadState);
        }
        *current = Some(GroupLeaderSignalIdentity::with_pid_namespace_and_scheduler(
            registration_tid,
            signal,
            self.pid_ns.clone(),
            self.scheduler.clone(),
            self.landlock.clone(),
            self.rlimits.clone(),
        ));
        Ok(())
    }

pub(crate) fn current_cred_and_signal(
        &self,
    ) -> AxResult<(Arc<Cred>, Arc<ThreadSignalManager>)> {
        let snapshot = self.identity_snapshot()?;
        Ok((snapshot.credential, snapshot.signal))
    }

pub(crate) fn identity_snapshot(&self) -> AxResult<GroupLeaderIdentitySnapshot> {
        let current = self.current.lock();
        let signal_guard = self.signal.lock();
        let slot = current.clone();
        let signal = signal_guard
            .as_ref()
            .map(|identity| identity.manager.clone())
            .ok_or(AxError::BadState)?;
        let token = GroupLeaderIdentityToken(self.identity_token.load(Ordering::Acquire));
        drop(signal_guard);
        drop(current);
        Ok(GroupLeaderIdentitySnapshot {
            token,
            credential: slot.current(),
            signal,
        })
    }

pub(crate) fn signal_matches(&self, expected: &Arc<ThreadSignalManager>) -> bool {
        self.signal
            .lock()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.manager, expected))
    }

pub(crate) fn identity_snapshot_matches(&self, expected: &GroupLeaderIdentitySnapshot) -> bool {
        let current = self.current.lock();
        let signal = self.signal.lock();
        let matches = self.identity_token.load(Ordering::Acquire) == expected.token.0
            && Arc::ptr_eq(&current.current(), &expected.credential)
            && signal
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(&current.manager, &expected.signal));
        drop(signal);
        drop(current);
        matches
    }

pub(crate) fn signal_owner(&self) -> GroupLeaderSignalOwner {
        self.signal.clone()
    }

pub(crate) fn publish_affinity_snapshot(
        &self,
        registration_tid: Pid,
        token: u64,
        affinity: AxCpuMask,
    ) {
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid
            || identity.scheduler_identity_token != token
        {
            return;
        }
        self.scheduler.lock().affinity = affinity;
    }

pub(crate) fn publication_token_for(&self, kernel_tid: Pid) -> Option<u64> {
        self.signal.lock().as_ref().and_then(|identity| {
            (identity.registration_tid == kernel_tid).then_some(identity.scheduler_identity_token)
        })
    }

    /// Seeds the not-yet-bound initial process identity. The first thread is
    /// constructed before its private endpoint exists, so no live identity
    /// can race this one-time initialization.
pub(crate) fn seed_scheduler_state(
        &self,
        state: SchedState,
        reset_on_fork: bool,
        uclamp: axtask::UclampRequest,
        utilization_bounds: axtask::UtilizationBounds,
        version: u64,
    ) {
        let mut snapshot = self.scheduler.lock();
        debug_assert_eq!(snapshot.identity_epoch, 0);
        // Serial numbers wrap. A candidate is newer when it lies less than
        // half the u64 sequence space ahead of the published version.
        if scheduler_version_is_newer_or_equal(version, snapshot.version) {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: 0,
                version,
                reset_on_fork,
                uclamp_min: uclamp.minimum,
                uclamp_max: uclamp.maximum,
                uclamp_min_user_defined: uclamp.minimum_user_defined,
                uclamp_max_user_defined: uclamp.maximum_user_defined,
                uclamp_effective_min: utilization_bounds.minimum as u16,
                uclamp_effective_max: utilization_bounds.maximum as u16,
                ..state.into()
            };
        }
    }

    /// Publishes a successful scheduler transaction only if its task still
    /// owns the bound group-leader endpoint. Holding the binding locks through
    /// the epoch check prevents a former leader from publishing into the new
    /// leader's durable snapshot after an exec handoff.
pub(crate) fn publish_scheduler_commit(
        &self,
        registration_tid: Pid,
        token: u64,
        task: &AxTaskRef,
        commit: TaskSchedulingSnapshot,
    ) {
        let current = self.current.lock();
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid {
            return;
        }
        // Reject a delayed publisher after a newer transaction on this same
        // task.  The state and version are one run-queue-lock snapshot, never
        // independently sampled values.
        if !scheduler_publication_matches(
            identity.scheduler_identity_token,
            token,
            commit,
            task_scheduling_snapshot(task).ok(),
        ) {
            return;
        }
        let epoch = *self.scheduler_identity_epoch.lock();
        let mut snapshot = self.scheduler.lock();
        if snapshot.identity_epoch == epoch {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: epoch,
                version: commit.version,
                reset_on_fork: commit.reset_on_spawn,
                dl_flags: commit.deadline.flags,
                uclamp_min: commit.uclamp.minimum,
                uclamp_max: commit.uclamp.maximum,
                uclamp_min_user_defined: commit.uclamp.minimum_user_defined,
                uclamp_max_user_defined: commit.uclamp.maximum_user_defined,
                uclamp_effective_min: commit.utilization_bounds.minimum as u16,
                uclamp_effective_max: commit.utilization_bounds.maximum as u16,
                ..commit.state.into()
            };
        }
        drop(snapshot);
        drop(signal);
        drop(current);
    }

    #[cfg(test)]
    pub(crate) fn publish_scheduler_state(&self, registration_tid: Pid, state: SchedState, version: u64) {
        let signal = self.signal.lock();
        let Some(identity) = signal.as_ref() else {
            return;
        };
        if identity.registration_tid != registration_tid {
            return;
        }
        let epoch = *self.scheduler_identity_epoch.lock();
        let mut snapshot = self.scheduler.lock();
        if snapshot.identity_epoch == epoch
            && scheduler_version_is_newer_or_equal(version, snapshot.version)
        {
            *snapshot = ZombieSchedulerSnapshot {
                identity_epoch: epoch,
                version,
                ..state.into()
            };
        }
    }

pub(crate) fn publish_handoff<'a>(
        &self,
        credential: Arc<CredentialSlot>,
        signal: Option<GroupLeaderSignalIdentity>,
        prepared: Option<PreparedCred<'a>>,
        executor_scheduler: Option<TaskSchedulingSnapshot>,
    ) -> GroupLeaderCommit<'a> {
        let signal = signal.map(|mut signal| {
            signal.pid_ns = self.pid_ns.clone();
            signal.scheduler = Some(self.scheduler.clone());
            // Exec keeps the process's resource limits, so the replacement
            // leader endpoint must keep pointing at the same shared cell.
            signal.rlimits = self.rlimits.clone();
            signal
        });
        let mut current = self.current.lock();
        let mut current_signal = signal.as_ref().map(|_| self.signal.lock());
        #[cfg(test)]
        let group_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::GroupLeader);
        let publication = prepared.map(PreparedCred::publish);
        let retired = core::mem::replace(&mut *current, credential);
        let (retired_signal, identity_replaced) = match (current_signal.as_mut(), signal) {
            (Some(current), Some(signal)) => match current.as_ref() {
                Some(existing) if existing.same_endpoint(&signal) => (None, false),
                _ => ((**current).replace(signal), true),
            },
            _ => (None, false),
        };
        // Publish a new epoch while both identity owners remain locked.  This
        // must happen even for leader exec, whose slot and endpoint can both
        // be pointer-identical across the handoff.
        self.identity_token.fetch_add(1, Ordering::Release);
        if identity_replaced {
            // Task scheduler versions belong to individual scheduler nodes.
            // Advance the binding epoch and publish the executor's exact
            // state while the identity locks are still held; no old leader
            // publication can be ordered into this new generation.
            let mut epoch = self.scheduler_identity_epoch.lock();
            let mut token = self.scheduler_identity_token.lock();
            *epoch = epoch.wrapping_add(1);
            *token = token.wrapping_add(1);
            // The installed endpoint, epoch advance, and forced executor
            // seed form one leader-identity transaction.
            current_signal
                .as_mut()
                .and_then(|signal| signal.as_mut())
                .expect("replaced leader endpoint must be installed")
                .scheduler_identity_token = *token;
            if let Some(commit) = executor_scheduler {
                *self.scheduler.lock() = ZombieSchedulerSnapshot {
                    identity_epoch: *epoch,
                    version: commit.version,
                    reset_on_fork: commit.reset_on_spawn,
                    dl_flags: commit.deadline.flags,
                    uclamp_min: commit.uclamp.minimum,
                    uclamp_max: commit.uclamp.maximum,
                    uclamp_min_user_defined: commit.uclamp.minimum_user_defined,
                    uclamp_max_user_defined: commit.uclamp.maximum_user_defined,
                    uclamp_effective_min: commit.utilization_bounds.minimum as u16,
                    uclamp_effective_max: commit.utilization_bounds.maximum as u16,
                    ..commit.state.into()
                };
            }
        }
        drop(current_signal);
        drop(current);
        #[cfg(test)]
        drop(group_lock_probe);
        GroupLeaderCommit {
            publication,
            retired_slot: retired,
            retired_signal,
        }
    }
}
