//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

/// Linux process dumpability values implemented by this kernel.
///
/// `SUID_DUMP_ROOT` remains unsupported until core-pattern ownership and the
/// corresponding sysctl policy exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Dumpability {
    NotDumpable  = 0,
    UserDumpable = 1,
}

/// The ABI-visible portion of Linux's `mm_struct`.  This is the sole owner of
/// the `/proc/<pid>/stat` image layout and of PR_SET_MM updates; `brk` uses the
/// same lock through `ProcessData`, so metadata can never publish a heap end
/// that differs from the address-space operation which established it.
#[derive(Clone, Debug)]
pub(crate) struct ProcessMmLayout {
    pub start_code: usize,
    pub end_code: usize,
    pub start_data: usize,
    pub end_data: usize,
    pub start_brk: usize,
    pub brk: usize,
    pub start_stack: usize,
    pub arg_start: usize,
    pub arg_end: usize,
    pub env_start: usize,
    pub env_end: usize,
    pub auxv: Vec<u8>,
    // These describe the real initial anonymous heap VMA installed by exec.
    // PR_SET_MM_START_BRK changes the ABI metadata above but never retargets a
    // live VMA behind the memory manager's back.
pub(crate)     heap_mapping_base: usize,
pub(crate)     heap_mapping_initial_end: usize,
}

impl ProcessMmLayout {
pub(crate) fn initial() -> Self {
        let start_brk = crate::config::USER_HEAP_BASE;
        Self {
            start_code: 0,
            end_code: 0,
            start_data: start_brk,
            end_data: start_brk,
            start_brk,
            brk: start_brk + crate::config::USER_HEAP_SIZE,
            start_stack: crate::config::USER_STACK_TOP,
            arg_start: 0,
            arg_end: 0,
            env_start: 0,
            env_end: 0,
            auxv: Vec::new(),
            heap_mapping_base: start_brk,
            heap_mapping_initial_end: start_brk + crate::config::USER_HEAP_SIZE,
        }
    }
}

impl TryFrom<usize> for Dumpability {
    type Error = AxError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NotDumpable),
            1 => Ok(Self::UserDumpable),
            _ => Err(AxError::InvalidInput),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ProcessSecurityState {
pub(crate)     dumpability: Dumpability,
}

/// Credential/dumpability publication barrier owned by one Linux address
/// space. Processes created with `CLONE_VM` share this owner; ordinary fork
/// receives a new owner initialized from the same snapshot.
pub(crate) struct ProcessAccessState {
pub(crate)     owner_user_ns: Arc<UserNamespace>,
pub(crate)     security: SpinNoIrq<ProcessSecurityState>,
}

impl ProcessAccessState {
pub(crate) fn try_new(
        dumpability: Dumpability,
        owner_user_ns: Arc<UserNamespace>,
    ) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            owner_user_ns,
            security: SpinNoIrq::new(ProcessSecurityState { dumpability }),
        })
        .map_err(|_| AxError::NoMemory)
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

pub(crate) fn dumpability(&self) -> Dumpability {
        self.security.lock().dumpability
    }

pub(crate) fn set_dumpability(&self, dumpability: Dumpability) {
        self.security.lock().dumpability = dumpability;
    }

    #[cfg(test)]
pub(crate) fn set_dumpability_for_test(&self, dumpability: Dumpability) {
        self.set_dumpability(dumpability);
    }

pub(crate) fn publish_credential<'a>(
        &self,
        prepared: PreparedCred<'a>,
        pdeath_signal: &AtomicU32,
    ) -> crate::task::creds::CredentialPublication<'a> {
        let mut security = self.security.lock();
        #[cfg(test)]
        let security_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::ProcessSecurity);
        if prepared.requires_dumpability_drop() {
            security.dumpability = Dumpability::NotDumpable;
            pdeath_signal.store(0, Ordering::Release);
        }
        let publication = prepared.publish();
        drop(security);
        #[cfg(test)]
        drop(security_lock_probe);
        publication
    }
}

/// Address space and the exact access-state owner governing it.
///
/// Keeping the pair under one lock makes exec replace both atomically and
/// prevents process_vm from authorizing one image and then operating on a
/// later image.
pub(crate) struct ProcessImageBinding<A> {
pub(crate)     aspace: A,
pub(crate)     access_state: Arc<ProcessAccessState>,
}

pub(crate) type LiveProcessImageBinding = ProcessImageBinding<Arc<Mutex<AddrSpace>>>;

pub(crate) struct ProcessImageAccessSnapshot {
pub(crate)     credential: Arc<Cred>,
pub(crate)     dumpability: Dumpability,
pub(crate)     owner_user_ns: Arc<UserNamespace>,
pub(crate)     aspace: Arc<Mutex<AddrSpace>>,
pub(crate)     access_state: Arc<ProcessAccessState>,
pub(crate)     exact_target: Option<(Pid, Arc<CredentialSlot>)>,
}

impl ProcessImageAccessSnapshot {
pub(crate) fn credential(&self) -> &Cred {
        &self.credential
    }

pub(crate) fn dumpability(&self) -> Dumpability {
        self.dumpability
    }

pub(crate) fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    /// Borrows the exact image identity presented to an authorization hook.
pub(crate) fn aspace(&self) -> &Arc<Mutex<AddrSpace>> {
        &self.aspace
    }

pub(crate) fn into_aspace(self) -> Arc<Mutex<AddrSpace>> {
        self.aspace
    }

pub(crate) fn exact_target_matches(&self, target: &crate::task::Thread) -> bool {
        let Some((tid, slot)) = &self.exact_target else {
            return false;
        };
        *tid == target.kernel_tid() && Arc::ptr_eq(slot, &target.credential_slot())
    }
}

pub(crate) fn snapshot_credential_image<A: Clone>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    slot: &CredentialSlot,
) -> (Arc<Cred>, Dumpability, A, Arc<ProcessAccessState>) {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let credential = slot.current();
    let dumpability = security.dumpability;
    let aspace = image.aspace.clone();
    let access_state = image.access_state.clone();
    drop(security);
    drop(image);
    (credential, dumpability, aspace, access_state)
}

pub(crate) fn snapshot_group_credential_image<A: Clone>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    group_leader: &GroupLeaderIdentityBinding,
) -> (
    Arc<Cred>,
    Dumpability,
    Arc<UserNamespace>,
    A,
    Arc<ProcessAccessState>,
) {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let credential = group_leader.current_cred();
    let dumpability = security.dumpability;
    let owner_user_ns = image.access_state.owner_user_ns.clone();
    let aspace = image.aspace.clone();
    let access_state = image.access_state.clone();
    drop(security);
    drop(image);
    (credential, dumpability, owner_user_ns, aspace, access_state)
}

pub(crate) fn coredump_image_snapshot<A: Clone>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
) -> Option<A> {
    let image = image_binding.read();
    let security = image.access_state.security.lock();
    let snapshot =
        (security.dumpability == Dumpability::UserDumpable).then(|| image.aspace.clone());
    drop(security);
    drop(image);
    snapshot
}

pub(crate) fn ptrace_image_snapshot_if_session<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    session: PtraceSession,
) -> Option<A> {
    // Keep the global ptrace/image order aligned with relationship
    // publication: image first, then ptrace control. Holding both gates across
    // the clone gives remote-memory operations one linearization point and
    // prevents a detach/reattach ABA without deadlocking a competing attach
    // which has already pinned the image and is waiting for ptrace control.
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    if ptrace_ctl.active_session() != Some(session) {
        return None;
    }
    let snapshot = image.aspace.clone();
    drop(ptrace_ctl);
    drop(image);
    Some(snapshot)
}

pub(crate) fn ptrace_image_snapshot_if_owned<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    tracer: Pid,
) -> Option<(PtraceSession, A)> {
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    let session = ptrace_ctl
        .active_session()
        .filter(|session| session.tracer == tracer)?;
    let snapshot = image.aspace.clone();
    drop(ptrace_ctl);
    drop(image);
    Some((session, snapshot))
}

pub(crate) fn ptrace_inactive_image_snapshot_if_session<A: Clone>(
    ptrace_ctl: &SpinNoIrq<PtraceControlState>,
    job_ctl: &SpinNoIrq<JobControlState>,
    image_binding: &RwLock<ProcessImageBinding<A>>,
    session: PtraceSession,
) -> Option<A> {
    // Preserve the image -> ptrace order used by every image/session
    // snapshot, then include the job-control gate in the same
    // linearization point. A successful clone therefore belongs to this
    // exact stopped relationship, never merely to a reused tracer PID.
    let image = image_binding.read();
    let ptrace_ctl = ptrace_ctl.lock();
    let job_ctl = job_ctl.lock();
    if ptrace_ctl.active_session() != Some(session) || !job_ctl.is_ptrace_inactive_for(session) {
        return None;
    }
    let snapshot = image.aspace.clone();
    drop(job_ctl);
    drop(ptrace_ctl);
    drop(image);
    Some(snapshot)
}

/// Returns `true` only when `kernel_tid` is the exact task whose relationship
/// option word carries `PTRACE_O_SUSPEND_SECCOMP`.
///
/// Linux stores `PT_SUSPEND_SECCOMP` in the traced task's own `ptrace` word
/// (`ptrace_setoptions()`: `child->ptrace |= PT_SUSPEND_SECCOMP`) and reads it
/// from `current` in `__secure_computing()` (kernel/seccomp.c), so attaching
/// to one thread never suspends a sibling's filters. TheKernel keeps one
/// option word for the whole thread group, so the tag written at relationship
/// publication supplies the missing task identity. Reading the tag inside the
/// same `ptrace_ctl` critical section which publishes the option word means no
/// reader observes a `SUSPEND_SECCOMP` word without its matching tracee, and
/// the session generation keeps a tag left behind by a detached or
/// rolled-back relationship inert.
pub(crate) fn ptrace_seccomp_suspended_for_tid(
    ptrace_ctl: &PtraceControlState,
    suspended_tracee: &SpinNoIrq<Option<(u64, Pid)>>,
    kernel_tid: Pid,
) -> bool {
    if ptrace_ctl.options & tk_linux_process::ptrace_options::SUSPEND_SECCOMP == 0 {
        return false;
    }
    let Some(session) = ptrace_ctl.active_session() else {
        return false;
    };
    matches!(
        *suspended_tracee.lock(),
        Some((generation, tracee)) if generation == session.generation && tracee == kernel_tid
    )
}

/// Records the exact tracee which the just-published relationship's option
/// word describes. The caller holds the `ptrace_ctl` guard across this write,
/// so it linearizes with [`ptrace_seccomp_suspended_for_tid`].
pub(crate) fn record_ptrace_suspended_tracee(
    suspended_tracee: &SpinNoIrq<Option<(u64, Pid)>>,
    session: PtraceSession,
    tracee_kernel_tid: Pid,
) {
    *suspended_tracee.lock() = Some((session.generation, tracee_kernel_tid));
}

pub(crate) fn replace_process_image_with_group_handoff<'a, A>(
    image_binding: &RwLock<ProcessImageBinding<A>>,
    group_leader: &GroupLeaderIdentityBinding,
    credential: Arc<CredentialSlot>,
    signal: Option<GroupLeaderSignalIdentity>,
    prepared: Option<PreparedCred<'a>>,
    executor_scheduler: Option<TaskSchedulingSnapshot>,
    new_image: ProcessImageBinding<A>,
    finish_image_publication: impl FnOnce(),
) -> (GroupLeaderCommit<'a>, ProcessImageBinding<A>) {
    let mut image = image_binding.write();
    #[cfg(test)]
    let image_lock_probe = PostCommitLockProbe::new(PostCommitLockKind::ProcessImage);
    let group_leader =
        group_leader.publish_handoff(credential, signal, prepared, executor_scheduler);
    let retired_image = core::mem::replace(&mut *image, new_image);
    finish_image_publication();
    drop(image);
    #[cfg(test)]
    drop(image_lock_probe);
    (group_leader, retired_image)
}

/// Clones the scheduler-facing TLB owner without joining the process-image
/// lock domain. Exec serializes replacement against scheduling on its sole
/// surviving thread, while this `Arc` keeps either observed generation alive.
pub(crate) fn scheduler_tlb_state_snapshot<T>(image_tlb_state: &RwLock<Arc<T>>) -> Arc<T> {
    image_tlb_state.read().clone()
}
