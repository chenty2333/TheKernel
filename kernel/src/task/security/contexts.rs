//! Typed, per-hook context objects.
//!
//! Each hook receives an immutable snapshot of exactly the subject, object,
//! and operation it authorizes. Constructing a context is where the
//! Linux-visible shape of a hook is decided; dispatch only walks modules and
//! can add nothing a context does not already state.

use super::*;

pub(crate) type CorePtraceAccessContext<'a> =
    thekernel_linux_cred::PtraceAccessContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
pub(crate) type CorePtraceTracemeContext<'a> =
    thekernel_linux_cred::PtraceTracemeContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
pub(crate) type CoreSchedulerSecurityContext<'a> =
    thekernel_linux_cred::SchedulerSecurityContext<'a, UserNamespace>;
pub(crate) type CoreTaskGetSchedulerContext<'a> =
    thekernel_linux_cred::TaskGetSchedulerContext<'a, UserNamespace>;
pub(crate) type CoreSignalSecurityContext<'a> =
    thekernel_linux_cred::SignalSecurityContext<'a, UserNamespace, SignalTargetSecurityRef<'a>>;
pub(crate) type CoreInodePermissionContext<'context, 'location> =
    thekernel_linux_cred::InodePermissionContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
    >;
pub(crate) type CoreInodeXattrContext<'context, 'location> =
    thekernel_linux_cred::InodeXattrContext<'context, UserNamespace, InodeSecurityRef<'location>>;
pub(crate) type CoreInodeSetattrContext<'context, 'location> =
    thekernel_linux_cred::InodeSetattrContext<'context, UserNamespace, InodeSecurityRef<'location>>;
pub(crate) type CoreInodePostSetattrContext<'context, 'location> =
    thekernel_linux_cred::InodePostSetattrContext<
        'context,
        UserNamespace,
        InodeSetattrCommittedSecurityRef<'location>,
    >;
pub(crate) type CoreFileOpenContext<'context, 'location> =
    thekernel_linux_cred::FileOpenContext<'context, UserNamespace, InodeSecurityRef<'location>>;
pub(crate) type CoreInodeCreateContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeCreateContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeMkdirContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeMkdirContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeMknodContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeMknodContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeSymlinkContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeSymlinkContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
        str,
    >;
pub(crate) type CoreInodeLinkContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeLinkContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeUnlinkContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeUnlinkContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        ExistingInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeRmdirContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeRmdirContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        ExistingInodeSecurityRef<'name, 'location>,
    >;
pub(crate) type CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location> =
    thekernel_linux_cred::InodeRenameContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        ExistingInodeSecurityRef<'old_name, 'location>,
        InodeSecurityRef<'location>,
        RenameDestinationSecurityRef<'new_name, 'location>,
    >;

/// Kind of exact kernel object selected for one userspace signal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalTargetKind {
    /// Linux thread-group/process target.
    Process,
    /// Exact task selected by PID for thread-group/shared-queue delivery.
    ProcessTask,
    /// One exact live thread target.
    Thread,
    /// Retained thread-group leader PID identity after its task has exited.
    ExitedLeader,
    /// Durable process identity after final exit.
    Zombie,
    /// Exact process named by a process pidfd (or supported proc-dir fd).
    PidFdProcess,
    /// Exact task named by a thread pidfd.
    PidFdThread,
}

/// Opaque stable identity for one process/thread/zombie selected before signal
/// authorization. The borrowed `Arc` pins numeric-ID reuse out of the hook
/// window; `stable_id` distinguishes threads within one live process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SignalTargetSecurityRef<'a> {
    pub(super) owner_pointer: *const (),
    pub(super) stable_id: u32,
    pub(super) visible_id: u32,
    pub(super) kind: SignalTargetKind,
    pub(super) _target: PhantomData<&'a ()>,
}

impl<'a> SignalTargetSecurityRef<'a> {
    pub(in crate::task) fn new<T>(
        owner: &'a Arc<T>,
        stable_id: u32,
        visible_id: u32,
        kind: SignalTargetKind,
    ) -> Self {
        Self {
            owner_pointer: Arc::as_ptr(owner).cast(),
            stable_id,
            visible_id,
            kind,
            _target: PhantomData,
        }
    }

    pub(crate) const fn stable_id(self) -> u32 {
        self.stable_id
    }

    pub(crate) const fn visible_id(self) -> u32 {
        self.visible_id
    }

    pub(crate) const fn kind(self) -> SignalTargetKind {
        self.kind
    }

    pub(in crate::task) fn owner_matches<T>(self, owner: &Arc<T>) -> bool {
        self.owner_pointer == Arc::as_ptr(owner).cast()
    }
}

/// Kernel context retaining the exact composite actor and target credentials.
/// Commoncap sees the policy-neutral core view; other modules receive their
/// own typed state at the same dense registry index.
pub(crate) struct PtraceAccessContext<'a> {
    pub(super) actor: &'a Cred,
    pub(super) target: &'a Cred,
    pub(super) core: CorePtraceAccessContext<'a>,
}

impl<'a> PtraceAccessContext<'a> {
    pub(crate) fn new(
        actor: &'a Cred,
        target: &'a Cred,
        target_image_owner_user_ns: &'a Arc<UserNamespace>,
        target_object: &'a ProcessImageSecurityRef<'a>,
        access_kind: PtraceAccessKind,
        credential_kind: PtraceCredentialKind,
    ) -> Self {
        Self {
            actor,
            target,
            core: CorePtraceAccessContext::new(
                actor.core(),
                target.core(),
                target_image_owner_user_ns,
                target_object,
                access_kind,
                credential_kind,
            ),
        }
    }

    pub(super) fn core(&self) -> &CorePtraceAccessContext<'a> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn target(&self) -> &'a Cred {
        self.target
    }

    pub(crate) const fn access_kind(&self) -> PtraceAccessKind {
        self.core.access_kind()
    }
}

pub(crate) struct PtraceTracemeContext<'a> {
    pub(super) parent_actor: &'a Cred,
    pub(super) child_target: &'a Cred,
    pub(super) core: CorePtraceTracemeContext<'a>,
}

impl<'a> PtraceTracemeContext<'a> {
    pub(crate) fn new(
        parent_actor: &'a Cred,
        child_target: &'a Cred,
        child_image_owner_user_ns: &'a Arc<UserNamespace>,
        child_object: &'a ProcessImageSecurityRef<'a>,
    ) -> Self {
        Self {
            parent_actor,
            child_target,
            core: CorePtraceTracemeContext::new(
                parent_actor.core(),
                child_target.core(),
                child_image_owner_user_ns,
                child_object,
            ),
        }
    }

    pub(super) fn core(&self) -> &CorePtraceTracemeContext<'a> {
        &self.core
    }

    pub(crate) const fn parent_actor(&self) -> &'a Cred {
        self.parent_actor
    }

    pub(crate) const fn child_target(&self) -> &'a Cred {
        self.child_target
    }
}

pub(crate) struct SecuritySchedulerContext<'a> {
    pub(super) actor: &'a Cred,
    pub(super) target: &'a Cred,
    pub(super) core: CoreSchedulerSecurityContext<'a>,
}

impl<'a> SecuritySchedulerContext<'a> {
    pub(crate) fn new(
        actor: &'a Cred,
        target: &'a Cred,
        operation: SchedulerSecurityOperation,
    ) -> Self {
        Self {
            actor,
            target,
            core: CoreSchedulerSecurityContext::new(actor.core(), target.core(), operation),
        }
    }

    pub(super) fn core(&self) -> &CoreSchedulerSecurityContext<'a> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn target(&self) -> &'a Cred {
        self.target
    }

    pub(crate) const fn owner_match(&self) -> bool {
        self.core.owner_match()
    }
}

/// Frozen target credential for Linux's `security_task_getsid` hook.
pub(crate) struct SecurityTaskGetsidContext<'a> {
    target: &'a Cred,
}

/// Frozen actor/target credentials for Linux's `security_task_getscheduler`
/// hook.  Both sides are retained before hook dispatch, so an exec or
/// credential transition cannot splice together two authorization epochs.
pub(crate) struct SecurityTaskGetSchedulerContext<'a> {
    actor: &'a Cred,
    target: &'a Cred,
    core: CoreTaskGetSchedulerContext<'a>,
}

impl<'a> SecurityTaskGetSchedulerContext<'a> {
    pub(crate) fn new(actor: &'a Cred, target: &'a Cred) -> Self {
        Self {
            actor,
            target,
            core: CoreTaskGetSchedulerContext::new(actor.core(), target.core()),
        }
    }

    pub(super) fn core(&self) -> &CoreTaskGetSchedulerContext<'a> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn target(&self) -> &'a Cred {
        self.target
    }
}

impl<'a> SecurityTaskGetsidContext<'a> {
    pub(crate) const fn new(target: &'a Cred) -> Self {
        Self { target }
    }

    pub(crate) const fn target(&self) -> &'a Cred {
        self.target
    }
}

/// Kernel wrapper retaining the exact composite actor/target credentials and
/// their module states around one already-core-authorized signal request.
pub(crate) struct SecuritySignalContext<'a> {
    pub(super) actor: &'a Cred,
    pub(super) target: &'a Cred,
    pub(super) core: CoreSignalSecurityContext<'a>,
}

impl<'a> SecuritySignalContext<'a> {
    pub(in crate::task) fn authorize(
        actor: &'a Cred,
        target: &'a Cred,
        target_object: &'a SignalTargetSecurityRef<'a>,
        operation: SignalSecurityOperation,
        same_thread_group: bool,
        same_session: bool,
    ) -> AxResult<Self> {
        let authorization = external_authorize_signal_core(
            actor.core(),
            target.core(),
            operation,
            same_thread_group,
            same_session,
        )
        .map_err(authorization_error)?;
        Ok(Self {
            actor,
            target,
            core: CoreSignalSecurityContext::new(authorization, target_object),
        })
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn target(&self) -> &'a Cred {
        self.target
    }

    pub(crate) const fn target_object(&self) -> &'a SignalTargetSecurityRef<'a> {
        self.core.target_object()
    }

    pub(crate) const fn operation(&self) -> SignalSecurityOperation {
        self.core.operation()
    }

    pub(crate) const fn core_reason(&self) -> SignalCoreAuthorizationReason {
        self.core.core_reason()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one leaf-typed inode permission context.
pub(crate) struct InodePermissionSecurityContext<'context, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodePermissionContext<'context, 'location>,
    operation: InodePermissionOperation,
}

/// The VFS intent associated with an inode permission check.
///
/// Linux represents `fchdir(2)` as `MAY_EXEC | MAY_CHDIR`.  The generic
/// inode-permission contract carries the executable/search access bit, while
/// this explicit operation retains the otherwise non-DAC `MAY_CHDIR` intent
/// for security modules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InodePermissionOperation {
    Generic,
    FchdirMayChdir,
}

impl<'context, 'location> InodePermissionSecurityContext<'context, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: &'context InodeSecurityRef<'location>,
        access: InodePermissionAccess,
    ) -> Self {
        Self::new_for_operation(
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            access,
            InodePermissionOperation::Generic,
        )
    }

    pub(crate) fn new_for_operation(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: &'context InodeSecurityRef<'location>,
        access: InodePermissionAccess,
        operation: InodePermissionOperation,
    ) -> Self {
        Self {
            actor,
            core: CoreInodePermissionContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                target_object,
                access,
            ),
            operation,
        }
    }

    pub(super) fn core(&self) -> &CoreInodePermissionContext<'context, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn target_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.target_object()
    }

    pub(crate) const fn access(&self) -> InodePermissionAccess {
        self.core.access()
    }

    pub(crate) const fn operation(&self) -> InodePermissionOperation {
        self.operation
    }
}

/// Kernel-owned input to one typed inode-xattr pre/post hook transaction.
///
/// The target snapshot is retained by value so an admission can cross the
/// provider call without retaining a VFS handle or repeating lookup. The
/// borrowed operation preserves the caller's exact validated name/value bytes,
/// while the actor, DAC snapshot, and filesystem owner namespace remain the
/// same immutable objects throughout both hook passes.
pub(crate) struct InodeXattrSecurityContext<'context, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) dac_credential: &'context DacCredentialView,
    pub(super) target_owner_user_ns: &'context Arc<UserNamespace>,
    pub(super) target_object: InodeSecurityRef<'location>,
    pub(super) operation: InodeXattrOperation<'context>,
}

impl<'context, 'location> InodeXattrSecurityContext<'context, 'location> {
    pub(crate) const fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: InodeSecurityRef<'location>,
        operation: InodeXattrOperation<'context>,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            operation,
        }
    }

    pub(super) fn core(&self) -> CoreInodeXattrContext<'_, 'location> {
        CoreInodeXattrContext::new(
            self.actor.core(),
            self.dac_credential,
            self.target_owner_user_ns,
            &self.target_object,
            self.operation,
        )
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.dac_credential
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.target_owner_user_ns
    }

    pub(crate) const fn target_object(&self) -> &InodeSecurityRef<'location> {
        &self.target_object
    }

    pub(crate) const fn operation(&self) -> InodeXattrOperation<'context> {
        self.operation
    }
}

/// Kernel-owned, self-contained input to one fallible inode-setattr hook pass.
///
/// The old inode snapshot is retained by value. This is deliberate: callers
/// commonly construct it from metadata read under an inode writer gate, and
/// the admission returned by [`dispatch_inode_setattr`] must remain valid after
/// that construction frame is gone. The ABI context is projected only while a
/// module callback is running, so no self-reference or repeat VFS lookup is
/// needed.
pub(crate) struct InodeSetattrSecurityContext<'context, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) dac_credential: &'context DacCredentialView,
    pub(super) target_owner_user_ns: &'context Arc<UserNamespace>,
    pub(super) target_object: InodeSecurityRef<'location>,
    pub(super) proposal: InodeSetattrProposal,
}

impl<'context, 'location> InodeSetattrSecurityContext<'context, 'location> {
    pub(crate) const fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: InodeSecurityRef<'location>,
        proposal: InodeSetattrProposal,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            proposal,
        }
    }

    pub(super) fn core(&self) -> CoreInodeSetattrContext<'_, 'location> {
        CoreInodeSetattrContext::new(
            self.actor.core(),
            self.dac_credential,
            self.target_owner_user_ns,
            &self.target_object,
            self.proposal,
        )
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.dac_credential
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.target_owner_user_ns
    }

    pub(crate) const fn target_object(&self) -> &InodeSecurityRef<'location> {
        &self.target_object
    }

    pub(crate) const fn proposal(&self) -> InodeSetattrProposal {
        self.proposal
    }

    pub(crate) const fn intent(&self) -> InodeSetattrIntent {
        self.proposal.intent()
    }
}

/// Kernel wrapper for one successful inode-setattr publication.
///
/// This type is intentionally distinct from the fallible pre-hook context. It
/// owns the committed snapshot supplied by the backend adapter and can only be
/// constructed by a successful linear [`InodeSetattrSecurityAdmission`].
pub(crate) struct InodePostSetattrSecurityContext<'context, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) dac_credential: &'context DacCredentialView,
    pub(super) target_owner_user_ns: &'context Arc<UserNamespace>,
    pub(super) committed_object: InodeSetattrCommittedSecurityRef<'location>,
    pub(super) proposal: InodeSetattrProposal,
}

impl<'context, 'location> InodePostSetattrSecurityContext<'context, 'location> {
    pub(super) const fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        committed_object: InodeSetattrCommittedSecurityRef<'location>,
        proposal: InodeSetattrProposal,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            committed_object,
            proposal,
        }
    }

    pub(super) fn core(&self) -> CoreInodePostSetattrContext<'_, 'location> {
        CoreInodePostSetattrContext::new(
            self.actor.core(),
            self.dac_credential,
            self.target_owner_user_ns,
            &self.committed_object,
            self.proposal,
        )
    }

    pub(super) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(super) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.dac_credential
    }

    pub(super) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.target_owner_user_ns
    }

    pub(super) const fn committed_object(&self) -> &InodeSetattrCommittedSecurityRef<'location> {
        &self.committed_object
    }

    pub(super) const fn proposal(&self) -> InodeSetattrProposal {
        self.proposal
    }

    pub(super) const fn intent(&self) -> InodeSetattrIntent {
        self.proposal.intent()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned regular-file creation.
pub(crate) struct InodeCreateSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeCreateContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeCreateSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        new_entry_object: &'context PlannedInodeSecurityRef<'name, 'location>,
        mode: InodeCreateMode,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeCreateContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                new_entry_object.parent_object(),
                new_entry_object,
                mode,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeCreateContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context PlannedInodeSecurityRef<'name, 'location> {
        self.core.new_entry_object()
    }

    pub(crate) const fn mode(&self) -> InodeCreateMode {
        self.core.mode()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned directory creation.
pub(crate) struct InodeMkdirSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeMkdirContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeMkdirSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        new_entry_object: &'context PlannedInodeSecurityRef<'name, 'location>,
        mode: InodeCreateMode,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeMkdirContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                new_entry_object.parent_object(),
                new_entry_object,
                mode,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeMkdirContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context PlannedInodeSecurityRef<'name, 'location> {
        self.core.new_entry_object()
    }

    pub(crate) const fn mode(&self) -> InodeCreateMode {
        self.core.mode()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned special-node creation.
pub(crate) struct InodeMknodSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeMknodContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeMknodSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        new_entry_object: &'context PlannedInodeSecurityRef<'name, 'location>,
        operation: InodeMknodOperation,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeMknodContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                new_entry_object.parent_object(),
                new_entry_object,
                operation,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeMknodContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context PlannedInodeSecurityRef<'name, 'location> {
        self.core.new_entry_object()
    }

    pub(crate) const fn operation(&self) -> InodeMknodOperation {
        self.core.operation()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned symbolic-link creation.
pub(crate) struct InodeSymlinkSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeSymlinkContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeSymlinkSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        new_entry_object: &'context PlannedInodeSecurityRef<'name, 'location>,
        symlink_target: &'context str,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeSymlinkContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                new_entry_object.parent_object(),
                new_entry_object,
                symlink_target,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeSymlinkContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context PlannedInodeSecurityRef<'name, 'location> {
        self.core.new_entry_object()
    }

    pub(crate) const fn symlink_target(&self) -> &'context str {
        self.core.symlink_target()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned hard-link creation.
pub(crate) struct InodeLinkSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeLinkContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeLinkSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        source_object: &'context InodeSecurityRef<'location>,
        new_entry_object: &'context PlannedInodeSecurityRef<'name, 'location>,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeLinkContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                source_object,
                new_entry_object.parent_object(),
                new_entry_object,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeLinkContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn source_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.source_object()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context PlannedInodeSecurityRef<'name, 'location> {
        self.core.new_entry_object()
    }
}

/// Kernel wrapper retaining one exact non-directory removal context and the
/// composite actor whose module states must authorize it.
pub(crate) struct InodeUnlinkSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeUnlinkContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeUnlinkSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_entry_object: &'context ExistingInodeSecurityRef<'name, 'location>,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeUnlinkContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                target_entry_object.parent_object(),
                target_entry_object,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeUnlinkContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn target_entry_object(
        &self,
    ) -> &'context ExistingInodeSecurityRef<'name, 'location> {
        self.core.target_entry_object()
    }
}

/// Kernel wrapper retaining one exact directory-removal context. This remains
/// a distinct type from [`InodeUnlinkSecurityContext`], so hook selection can
/// never be reduced to a caller-provided boolean.
pub(crate) struct InodeRmdirSecurityContext<'context, 'name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeRmdirContext<'context, 'name, 'location>,
}

impl<'context, 'name, 'location> InodeRmdirSecurityContext<'context, 'name, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_entry_object: &'context ExistingInodeSecurityRef<'name, 'location>,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeRmdirContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                target_entry_object.parent_object(),
                target_entry_object,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreInodeRmdirContext<'context, 'name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.parent_object()
    }

    pub(crate) const fn target_entry_object(
        &self,
    ) -> &'context ExistingInodeSecurityRef<'name, 'location> {
        self.core.target_entry_object()
    }
}

/// Kernel wrapper retaining the four ordered object roles for one rename leaf
/// hook. The old entry is necessarily existing, while the destination entry
/// preserves whether lookup found an existing target or exact absence.
pub(crate) struct InodeRenameSecurityContext<'context, 'old_name, 'new_name, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location>,
}

impl<'context, 'old_name, 'new_name, 'location>
    InodeRenameSecurityContext<'context, 'old_name, 'new_name, 'location>
{
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        old_entry_object: &'context ExistingInodeSecurityRef<'old_name, 'location>,
        new_entry_object: &'context RenameDestinationSecurityRef<'new_name, 'location>,
    ) -> Self {
        Self {
            actor,
            core: CoreInodeRenameContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                old_entry_object.parent_object(),
                old_entry_object,
                new_entry_object.parent_object(),
                new_entry_object,
            ),
        }
    }

    pub(super) fn core(
        &self,
    ) -> &CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn old_parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.old_parent_object()
    }

    pub(crate) const fn old_entry_object(
        &self,
    ) -> &'context ExistingInodeSecurityRef<'old_name, 'location> {
        self.core.old_entry_object()
    }

    pub(crate) const fn new_parent_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.new_parent_object()
    }

    pub(crate) const fn new_entry_object(
        &self,
    ) -> &'context RenameDestinationSecurityRef<'new_name, 'location> {
        self.core.new_entry_object()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one leaf-typed file-open context.
pub(crate) struct FileOpenSecurityContext<'context, 'location> {
    pub(super) actor: &'context Cred,
    pub(super) core: CoreFileOpenContext<'context, 'location>,
}

impl<'context, 'location> FileOpenSecurityContext<'context, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: &'context InodeSecurityRef<'location>,
        operation: FileOpenOperation,
    ) -> Self {
        Self {
            actor,
            core: CoreFileOpenContext::new(
                actor.core(),
                dac_credential,
                target_owner_user_ns,
                target_object,
                operation,
            ),
        }
    }

    pub(super) fn core(&self) -> &CoreFileOpenContext<'context, 'location> {
        &self.core
    }

    pub(crate) const fn actor(&self) -> &'context Cred {
        self.actor
    }

    pub(crate) const fn dac_credential(&self) -> &'context DacCredentialView {
        self.core.dac_credential()
    }

    pub(crate) const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.core.target_owner_user_ns()
    }

    pub(crate) const fn target_object(&self) -> &'context InodeSecurityRef<'location> {
        self.core.target_object()
    }

    pub(crate) const fn operation(&self) -> FileOpenOperation {
        self.core.operation()
    }
}

/// Leaf-typed socket policy operation over copied, lookup-free kernel facts.
pub(crate) enum SocketSecurityOperation<'a> {
    Create(SocketCreateContext<'a, UserNamespace>),
    PostCreate(SocketPostCreateContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    Pair(SocketPairContext<'a, UserNamespace, SocketSecurityRef<'a>, SocketSecurityRef<'a>>),
    Bind(SocketBindContext<'a, UserNamespace, SocketSecurityRef<'a>, PreparedSocketAddress>),
    Connect(SocketConnectContext<'a, UserNamespace, SocketSecurityRef<'a>, PreparedSocketAddress>),
    Listen(SocketListenContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    Accept(
        SocketAcceptContext<
            'a,
            UserNamespace,
            SocketSecurityRef<'a>,
            AcceptedSocketSecurityRef<'a>,
        >,
    ),
    SendMessage(
        SocketSendMessageContext<'a, UserNamespace, SocketSecurityRef<'a>, PreparedSocketMessage>,
    ),
    ReceiveMessage(
        SocketReceiveMessageContext<
            'a,
            UserNamespace,
            SocketSecurityRef<'a>,
            PreparedSocketMessage,
        >,
    ),
    GetSockName(SocketGetSockNameContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    GetPeerName(SocketGetPeerNameContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    GetOption(SocketGetOptionContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    SetOption(SocketSetOptionContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    Shutdown(SocketShutdownContext<'a, UserNamespace, SocketSecurityRef<'a>>),
    UnixStreamConnect(
        UnixStreamConnectContext<
            'a,
            UserNamespace,
            SocketSecurityRef<'a>,
            UnixEndpointSecurityRef<'a>,
            UnixEndpointSecurityRef<'a>,
        >,
    ),
    UnixMaySend(
        UnixMaySendContext<'a, UserNamespace, SocketSecurityRef<'a>, UnixEndpointSecurityRef<'a>>,
    ),
}

/// Kernel wrapper retaining the complete composite actor state around one
/// external typed socket context.
pub(crate) struct SocketSecurityContext<'a> {
    pub(super) actor: &'a Cred,
    pub(super) operation: SocketSecurityOperation<'a>,
}

impl<'a> SocketSecurityContext<'a> {
    pub(crate) fn create(actor: &'a Cred, spec: SocketCreateSpec) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Create(SocketCreateContext::new(
                actor.core(),
                spec,
            )),
        }
    }

    pub(crate) fn post_create(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        spec: SocketCreateSpec,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::PostCreate(SocketPostCreateContext::new(
                actor.core(),
                socket,
                spec,
            )),
        }
    }

    pub(crate) fn pair(
        actor: &'a Cred,
        first: &'a SocketSecurityRef<'a>,
        second: &'a SocketSecurityRef<'a>,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Pair(SocketPairContext::new(
                actor.core(),
                first,
                second,
            )),
        }
    }

    pub(crate) fn bind(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        address: &'a PreparedSocketAddress,
        address_length: usize,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Bind(SocketBindContext::new(
                actor.core(),
                socket,
                address,
                address_length,
            )),
        }
    }

    pub(crate) fn connect(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        address: &'a PreparedSocketAddress,
        address_length: usize,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Connect(SocketConnectContext::new(
                actor.core(),
                socket,
                address,
                address_length,
            )),
        }
    }

    pub(crate) fn listen(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        backlog: SocketListenBacklog,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Listen(SocketListenContext::new(
                actor.core(),
                socket,
                backlog,
            )),
        }
    }

    pub(crate) fn accept(
        actor: &'a Cred,
        listening: &'a SocketSecurityRef<'a>,
        accepted: &'a AcceptedSocketSecurityRef<'a>,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Accept(SocketAcceptContext::new(
                actor.core(),
                listening,
                accepted,
            )),
        }
    }

    pub(crate) fn send_message(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        message: &'a PreparedSocketMessage,
        size: usize,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::SendMessage(SocketSendMessageContext::new(
                actor.core(),
                socket,
                message,
                size,
            )),
        }
    }

    pub(crate) fn receive_message(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        message: &'a PreparedSocketMessage,
        size: usize,
        flags: i32,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::ReceiveMessage(SocketReceiveMessageContext::new(
                actor.core(),
                socket,
                message,
                size,
                flags,
            )),
        }
    }

    pub(crate) fn get_sock_name(actor: &'a Cred, socket: &'a SocketSecurityRef<'a>) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::GetSockName(SocketGetSockNameContext::new(
                actor.core(),
                socket,
            )),
        }
    }

    pub(crate) fn get_peer_name(actor: &'a Cred, socket: &'a SocketSecurityRef<'a>) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::GetPeerName(SocketGetPeerNameContext::new(
                actor.core(),
                socket,
            )),
        }
    }

    pub(crate) fn get_option(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        option: SocketOption,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::GetOption(SocketGetOptionContext::new(
                actor.core(),
                socket,
                option,
            )),
        }
    }

    pub(crate) fn set_option(
        actor: &'a Cred,
        socket: &'a SocketSecurityRef<'a>,
        option: SocketOption,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::SetOption(SocketSetOptionContext::new(
                actor.core(),
                socket,
                option,
            )),
        }
    }

    pub(crate) fn shutdown(actor: &'a Cred, socket: &'a SocketSecurityRef<'a>, how: i32) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::Shutdown(SocketShutdownContext::new(
                actor.core(),
                socket,
                how,
            )),
        }
    }

    pub(crate) fn unix_stream_connect(
        actor: &'a Cred,
        connecting: &'a SocketSecurityRef<'a>,
        listening: &'a UnixEndpointSecurityRef<'a>,
        accepted: &'a UnixEndpointSecurityRef<'a>,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::UnixStreamConnect(UnixStreamConnectContext::new(
                actor.core(),
                connecting,
                listening,
                accepted,
            )),
        }
    }

    pub(crate) fn unix_may_send(
        actor: &'a Cred,
        sending: &'a SocketSecurityRef<'a>,
        receiving: &'a UnixEndpointSecurityRef<'a>,
    ) -> Self {
        Self {
            actor,
            operation: SocketSecurityOperation::UnixMaySend(UnixMaySendContext::new(
                actor.core(),
                sending,
                receiving,
            )),
        }
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn operation(&self) -> &SocketSecurityOperation<'a> {
        &self.operation
    }
}

/// One already-resolved executable component checked during binary-handler
/// discovery. The exact immutable actor credential and stable object facts are
/// supplied by the loader; hooks cannot resample `current()` or repeat lookup.
pub(crate) struct ExecExecutableSecurityContext<'a> {
    pub(super) actor: &'a Cred,
    pub(super) executable: &'a ExecFileSecurityObject,
}

impl<'a> ExecExecutableSecurityContext<'a> {
    pub(crate) const fn new(actor: &'a Cred, executable: &'a ExecFileSecurityObject) -> Self {
        Self { actor, executable }
    }

    pub(crate) const fn actor(&self) -> &'a Cred {
        self.actor
    }

    pub(crate) const fn executable(&self) -> &'a ExecFileSecurityObject {
        self.executable
    }
}

/// Immutable facts shared by the infallible exec committing and committed
/// notifications. Both phases observe the same exact old/new composite
/// credentials, terminal credential source, and derived effects.
pub(crate) struct ExecCommitSecurityFacts<'a> {
    pub(super) old: &'a Cred,
    pub(super) new: &'a Cred,
    pub(super) source: &'a ExecFileSecurityObject,
    pub(super) effects: ExecCredentialEffects,
    pub(super) runtime: &'a ExecCommitRuntime,
}

impl<'a> ExecCommitSecurityFacts<'a> {
    pub(super) const fn new(
        old: &'a Cred,
        new: &'a Cred,
        source: &'a ExecFileSecurityObject,
        effects: ExecCredentialEffects,
        runtime: &'a ExecCommitRuntime,
    ) -> Self {
        Self {
            old,
            new,
            source,
            effects,
            runtime,
        }
    }

    pub(super) const fn old(&self) -> &'a Cred {
        self.old
    }

    pub(super) const fn new_credential(&self) -> &'a Cred {
        self.new
    }

    pub(super) const fn source(&self) -> &'a ExecFileSecurityObject {
        self.source
    }

    pub(super) const fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    pub(super) const fn runtime(&self) -> &'a ExecCommitRuntime {
        self.runtime
    }
}

macro_rules! exec_commit_context {
    ($name:ident) => {
        pub(crate) struct $name<'a> {
            facts: ExecCommitSecurityFacts<'a>,
        }

        impl<'a> $name<'a> {
            pub(super) const fn new(
                old: &'a Cred,
                new: &'a Cred,
                source: &'a ExecFileSecurityObject,
                effects: ExecCredentialEffects,
                runtime: &'a ExecCommitRuntime,
            ) -> Self {
                Self {
                    facts: ExecCommitSecurityFacts::new(old, new, source, effects, runtime),
                }
            }

            pub(crate) const fn old(&self) -> &'a Cred {
                self.facts.old()
            }

            pub(crate) const fn new_credential(&self) -> &'a Cred {
                self.facts.new_credential()
            }

            pub(crate) const fn source(&self) -> &'a ExecFileSecurityObject {
                self.facts.source()
            }

            pub(crate) const fn effects(&self) -> ExecCredentialEffects {
                self.facts.effects()
            }

            pub(crate) const fn runtime(&self) -> &'a ExecCommitRuntime {
                self.facts.runtime()
            }
        }
    };
}

exec_commit_context!(ExecCommittingSecurityContext);
exec_commit_context!(ExecCommittedSecurityContext);
