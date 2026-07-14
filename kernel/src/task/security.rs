//! Typed, allocation-free security-hook dispatch.
//!
//! Security modules are admitted fallibly during boot as complete units, then
//! frozen and published exactly once before the initial credential exists.
//! Runtime dispatch only walks that immutable declaration order: it cannot
//! allocate, register, remove, or silently skip a module.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
#[cfg(test)]
extern crate std;
#[cfg(test)]
use core::cell::Cell;
use core::{any::Any, fmt, marker::PhantomData};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, Metadata, NodeType};
use spin::Once;
use thekernel_linux_cred::{
    AuthorizationError, authorize_signal_core as external_authorize_signal_core,
    commoncap_ptrace_access as external_commoncap_ptrace_access,
    commoncap_ptrace_traceme as external_commoncap_ptrace_traceme,
    commoncap_scheduler as external_commoncap_scheduler,
};
pub(crate) use thekernel_linux_cred::{
    FileOpenAccess, FileOpenOperation, InodePermissionAccess, PtraceAccessKind,
    PtraceCredentialKind, SchedulerSecurityOperation, SignalCoreAuthorizationReason,
    SignalDeliveryScope, SignalNumber, SignalSecurityOperation, SignalSecuritySource,
};

use super::{
    ExecCommitRuntime, ExecCredentialSecurityContext, ExecFileSecurityObject, UserNamespace,
    creds::{Cred, DacCredentialView, PreparedCred},
    exec_cred::{ExecCredentialEffects, authorize_commoncap_exec},
};

const SECURITY_MODULE_LIMIT: usize = 8;
const COMMONCAP_MODULE_KEY: ModuleKey = ModuleKey(0);
const NOOP_POLICY_MODULE_KEY: ModuleKey = ModuleKey(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleId(u8);

type CoreCred = thekernel_linux_cred::Credential<UserNamespace>;

/// The way an unpublished composite credential was derived from its exact
/// immutable predecessor. Modules may use this to keep fork, namespace, exec,
/// and ordinary transition state distinct without learning syscall details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::task) enum CredentialStateTransition {
    Fork,
    Normal,
    UserNamespace,
    Exec,
}

/// Immutable facts delivered to one module after a credential replacement
/// has become visible.  The callback sees the exact old/new core values and
/// the exact typed states that were preflighted before publication; it must
/// never resample a task's current credential.
struct CredentialPostCommitContext<'a, S> {
    old_credential: &'a CoreCred,
    old_state: &'a S,
    new_credential: &'a CoreCred,
    new_state: &'a S,
    transition: CredentialStateTransition,
}

impl<'a, S> CredentialPostCommitContext<'a, S> {
    const fn old_credential(&self) -> &'a CoreCred {
        self.old_credential
    }

    const fn old_state(&self) -> &'a S {
        self.old_state
    }

    const fn new_credential(&self) -> &'a CoreCred {
        self.new_credential
    }

    const fn new_state(&self) -> &'a S {
        self.new_state
    }

    const fn transition(&self) -> CredentialStateTransition {
        self.transition
    }
}

/// Opaque proof that the complete boot-time module stack was frozen and
/// published. Credentials retain this exact identity for their whole life;
/// state preparation and dispatch never rediscover it through a global.
#[derive(Clone, Copy)]
pub(crate) struct FrozenSecurityRegistry(&'static SecurityRegistry);

impl FrozenSecurityRegistry {
    fn registry(self) -> &'static SecurityRegistry {
        self.0
    }

    pub(in crate::task) fn same_registry(self, other: Self) -> bool {
        core::ptr::eq(self.0, other.0)
    }
}

/// Boot-time registry construction and publication failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum RegistryBuildError {
    NoMemory,
    Capacity,
    DuplicateModule,
    ReservedModuleKey,
    // Commoncap/noop cannot currently fail after their zero-resource init,
    // but the registry contract preserves this class for future built-ins.
    #[allow(dead_code)]
    ModuleInitFailed,
    AlreadyPublished,
}

impl fmt::Display for RegistryBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoMemory => "security registry allocation failed",
            Self::Capacity => "security module capacity exceeded",
            Self::DuplicateModule => "duplicate security module",
            Self::ReservedModuleKey => "reserved security module key",
            Self::ModuleInitFailed => "security module initialization failed",
            Self::AlreadyPublished => "security registry already published",
        })
    }
}

/// Maps policy-neutral authorization failures at the kernel adapter boundary.
pub(crate) const fn authorization_error(error: AuthorizationError) -> AxError {
    match error {
        AuthorizationError::NotPermitted => AxError::OperationNotPermitted,
        AuthorizationError::AccessDenied => AxError::PermissionDenied,
        _ => AxError::OperationNotPermitted,
    }
}

/// Opaque identity for the address-space generation checked by one hook run.
///
/// The lifetime is tied to a borrowed owning `Arc`, so a context cannot retain
/// an identity after the corresponding pinned image handle has gone away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessImageIdentity<'a> {
    pointer: *const (),
    _image: PhantomData<&'a ()>,
}

impl<'a> ProcessImageIdentity<'a> {
    fn from_arc<T>(image: &'a Arc<T>) -> Self {
        Self {
            pointer: Arc::as_ptr(image).cast(),
            _image: PhantomData,
        }
    }
}

/// Security facts for the exact process image authorized by the caller.
///
/// `owner_user_ns` is the Linux `mm->user_ns` analogue. It intentionally need
/// not equal the target credential namespace. `identity` identifies the
/// already-pinned image generation and prevents hooks from having to resample
/// mutable process state.
#[derive(Clone, Copy)]
pub(crate) struct ProcessImageSecurityRef<'a> {
    owner_user_ns: &'a Arc<UserNamespace>,
    identity: ProcessImageIdentity<'a>,
}

impl<'a> ProcessImageSecurityRef<'a> {
    pub(crate) fn new<T>(owner_user_ns: &'a Arc<UserNamespace>, image: &'a Arc<T>) -> Self {
        Self {
            owner_user_ns,
            identity: ProcessImageIdentity::from_arc(image),
        }
    }

    pub(crate) const fn owner_user_ns(self) -> &'a Arc<UserNamespace> {
        self.owner_user_ns
    }

    pub(crate) const fn identity(self) -> ProcessImageIdentity<'a> {
        self.identity
    }
}

/// Stable Linux-facing identity for one pinned VFS location.
///
/// A mount ID is retained separately from the backing device/inode pair so
/// bind-style mount aliases cannot be collapsed accidentally by policy.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct InodeIdentity {
    mount_id: u64,
    device: u64,
    inode: u64,
}

impl InodeIdentity {
    const fn new(mount_id: u64, device: u64, inode: u64) -> Self {
        Self {
            mount_id,
            device,
            inode,
        }
    }

    pub(crate) const fn mount_id(self) -> u64 {
        self.mount_id
    }

    pub(crate) const fn device(self) -> u64 {
        self.device
    }

    pub(crate) const fn inode(self) -> u64 {
        self.inode
    }
}

/// Frozen, lookup-free inode facts bound to one borrowed VFS location.
///
/// Construction projects the metadata snapshot supplied by the caller. The
/// retained lifetime prevents a hook context from outliving the pinned
/// `Location`, while the type deliberately exposes no location handle or VFS
/// method through which a module could repeat lookup.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InodeSecurityRef<'location> {
    identity: InodeIdentity,
    mode: u16,
    node_kind: NodeType,
    uid: u32,
    gid: u32,
    size: u64,
    _location: PhantomData<&'location Location>,
}

impl<'location> InodeSecurityRef<'location> {
    pub(crate) fn new(location: &'location Location, metadata: &Metadata) -> Self {
        Self {
            identity: InodeIdentity::new(
                location.mountpoint().mount_id(),
                metadata.device,
                metadata.inode,
            ),
            mode: metadata.mode.bits(),
            node_kind: metadata.node_type,
            uid: metadata.uid,
            gid: metadata.gid,
            size: metadata.size,
            _location: PhantomData,
        }
    }

    pub(crate) const fn identity(&self) -> InodeIdentity {
        self.identity
    }

    pub(crate) const fn mode(&self) -> u16 {
        self.mode
    }

    pub(crate) const fn node_kind(&self) -> NodeType {
        self.node_kind
    }

    pub(crate) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(crate) const fn gid(&self) -> u32 {
        self.gid
    }

    pub(crate) const fn size(&self) -> u64 {
        self.size
    }
}

/// Returns the initial ancestor which owns ordinary, non-idmapped VFS objects.
///
/// The explicit seam can later be replaced by per-superblock or idmapped-mount
/// ownership without teaching typed hook contexts to infer object ownership
/// from the actor credential.
pub(crate) fn initial_user_namespace(namespace: &Arc<UserNamespace>) -> Arc<UserNamespace> {
    let mut initial = namespace.clone();
    while let Some(parent) = initial.parent() {
        initial = parent;
    }
    initial
}

type CorePtraceAccessContext<'a> =
    thekernel_linux_cred::PtraceAccessContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
type CorePtraceTracemeContext<'a> =
    thekernel_linux_cred::PtraceTracemeContext<'a, UserNamespace, ProcessImageSecurityRef<'a>>;
type CoreSchedulerSecurityContext<'a> =
    thekernel_linux_cred::SchedulerSecurityContext<'a, UserNamespace>;
type CoreSignalSecurityContext<'a> =
    thekernel_linux_cred::SignalSecurityContext<'a, UserNamespace, SignalTargetSecurityRef<'a>>;
type CoreInodePermissionContext<'context, 'location> = thekernel_linux_cred::InodePermissionContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
>;
type CoreFileOpenContext<'context, 'location> =
    thekernel_linux_cred::FileOpenContext<'context, UserNamespace, InodeSecurityRef<'location>>;

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
    owner_pointer: *const (),
    stable_id: u32,
    visible_id: u32,
    kind: SignalTargetKind,
    _target: PhantomData<&'a ()>,
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
    actor: &'a Cred,
    target: &'a Cred,
    core: CorePtraceAccessContext<'a>,
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

    fn core(&self) -> &CorePtraceAccessContext<'a> {
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
    parent_actor: &'a Cred,
    child_target: &'a Cred,
    core: CorePtraceTracemeContext<'a>,
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

    fn core(&self) -> &CorePtraceTracemeContext<'a> {
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
    actor: &'a Cred,
    target: &'a Cred,
    core: CoreSchedulerSecurityContext<'a>,
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

    fn core(&self) -> &CoreSchedulerSecurityContext<'a> {
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

/// Kernel wrapper retaining the exact composite actor/target credentials and
/// their module states around one already-core-authorized signal request.
pub(crate) struct SecuritySignalContext<'a> {
    actor: &'a Cred,
    target: &'a Cred,
    core: CoreSignalSecurityContext<'a>,
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
    actor: &'context Cred,
    core: CoreInodePermissionContext<'context, 'location>,
}

impl<'context, 'location> InodePermissionSecurityContext<'context, 'location> {
    pub(crate) fn new(
        actor: &'context Cred,
        dac_credential: &'context DacCredentialView,
        target_owner_user_ns: &'context Arc<UserNamespace>,
        target_object: &'context InodeSecurityRef<'location>,
        access: InodePermissionAccess,
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
        }
    }

    fn core(&self) -> &CoreInodePermissionContext<'context, 'location> {
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
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one leaf-typed file-open context.
pub(crate) struct FileOpenSecurityContext<'context, 'location> {
    actor: &'context Cred,
    core: CoreFileOpenContext<'context, 'location>,
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

    fn core(&self) -> &CoreFileOpenContext<'context, 'location> {
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

/// One already-resolved executable component checked during binary-handler
/// discovery. The exact immutable actor credential and stable object facts are
/// supplied by the loader; hooks cannot resample `current()` or repeat lookup.
pub(crate) struct ExecExecutableSecurityContext<'a> {
    actor: &'a Cred,
    executable: &'a ExecFileSecurityObject,
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
struct ExecCommitSecurityFacts<'a> {
    old: &'a Cred,
    new: &'a Cred,
    source: &'a ExecFileSecurityObject,
    effects: ExecCredentialEffects,
    runtime: &'a ExecCommitRuntime,
}

impl<'a> ExecCommitSecurityFacts<'a> {
    const fn new(
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

    const fn old(&self) -> &'a Cred {
        self.old
    }

    const fn new_credential(&self) -> &'a Cred {
        self.new
    }

    const fn source(&self) -> &'a ExecFileSecurityObject {
        self.source
    }

    const fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    const fn runtime(&self) -> &'a ExecCommitRuntime {
        self.runtime
    }
}

macro_rules! exec_commit_context {
    ($name:ident) => {
        pub(crate) struct $name<'a> {
            facts: ExecCommitSecurityFacts<'a>,
        }

        impl<'a> $name<'a> {
            const fn new(
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

/// One security module owns every hook family as one registration unit.
///
/// The defaults are explicit no-ops so a module cannot be partially inserted
/// into independent per-hook registries. Boot initialization must return an
/// owned runtime object; dropping that object rolls back all module-local boot
/// resources if a later registry step fails.
trait SecurityModule: Send + Sync + 'static {
    const KEY: ModuleKey;
    type CredentialState: Send + Sync + 'static;

    fn try_boot_init() -> Result<Self, RegistryBuildError>
    where
        Self: Sized;

    /// Constructs this module's state for the initial root credential.
    fn try_init_credential(&self, credential: &CoreCred) -> AxResult<Self::CredentialState>;

    /// Constructs a complete unpublished replacement state from the exact
    /// old composite credential. This is the only fallible state callback.
    fn try_prepare_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &Self::CredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState>;

    /// Authorizes one fully prepared old/new state pair. Dispatch is ordered
    /// and deny-first; a denial drops every proposed state before publication.
    fn authorize_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _proposed_state: &Self::CredentialState,
        _transition: CredentialStateTransition,
    ) -> AxResult<()> {
        Ok(())
    }

    /// Observes one successfully published credential replacement.
    ///
    /// This is an infallible Linux-LSM-style atomic notification. It runs in
    /// frozen registry order after every credential publication, process-
    /// security, image, group-leader, and task-alias lock has been released.
    /// The credential writer mutex is also released before entry. The method
    /// must not allocate, block, fail, panic, resample `current()`, reenter a
    /// credential update, or acquire task/process/image/alias locks. Work that
    /// can sleep must use a resource reserved before publication and perform
    /// only an allocation-free handoff here. Registry order is guaranteed
    /// within this exact transition; notifications from other slots or a
    /// later transaction may run concurrently, so module-owned aggregation
    /// must already be concurrency-safe without allocating here. This hook is
    /// for replacement of an already-live slot (`Normal` and `Exec`) only;
    /// initial, fork-child, and user-namespace object publication require
    /// distinct lifecycle notifications and do not masquerade as a commit.
    fn credential_committed(
        &self,
        _context: CredentialPostCommitContext<'_, Self::CredentialState>,
    ) {
    }

    /// Releases one module state through the module runtime that created it.
    /// The owner wrapper invokes this directly and never consults a registry.
    ///
    /// This is a Linux-LSM-style atomic teardown callback: it must not block,
    /// allocate, reenter credential update, or acquire task/process/image
    /// locks. Rollback may call it while the sleepable credential-writer mutex
    /// is owned, but never under credential publication, image, alias, or
    /// registry spin locks. Committed retired states are freed after the writer
    /// mutex as well. A module needing sleepable cleanup must transfer an owned
    /// token to an independently preallocated deferred-work mechanism.
    fn free_credential(&self, state: Self::CredentialState) {
        drop(state);
    }

    /// Authorizes one exact inode after the caller has completed Linux DAC.
    ///
    /// Dispatch may run inside the filesystem-context and pathwalk lock
    /// domains. Implementations must remain allocation-free and nonblocking,
    /// must not call `current()`, and must not reenter VFS lookup, credential
    /// update, or security dispatch.
    fn inode_permission(&self, _context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_permission(context)
    }

    /// Authorizes one exact, fully resolved location before the open
    /// transaction publishes an fd, persistent executable-write reservation,
    /// filesystem-open side effect, fanotify open permission, POSIX lease
    /// conflict handling, or truncate. Dispatch is deny-first and stops on the
    /// first error. Implementations must not allocate, block, call `current()`,
    /// or reenter VFS, open, credential, or security operations.
    fn file_open(&self, _context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.file_open(context)
    }

    fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        _actor_state: &Self::CredentialState,
        _target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.ptrace_access(context)
    }

    fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        _parent_state: &Self::CredentialState,
        _child_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.ptrace_traceme(context)
    }

    fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _proposed_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.exec_credential(context)
    }

    /// Authorizes one executable component after DAC/open admission and before
    /// its binary handler or mapping is used. This fallible hook is invoked for
    /// the requested object, every shebang interpreter, and `PT_INTERP`.
    ///
    /// Dispatch currently runs inside the global ELF-loader cache mutex. A hook
    /// must not block, allocate, resample task state, or reenter loader/VFS
    /// operations. Any fallible policy state must be reserved before dispatch.
    fn exec_executable(&self, _context: &ExecExecutableSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.exec_executable(context)
    }

    /// Observes the exec point of no return after CLOEXEC/AIO cleanup and
    /// immediately before composite credential/image publication. This hook is
    /// infallible and allocation-free. The credential writer mutex is still
    /// held, but no publication/process/image/alias spin lock is held.
    /// Implementations must not block, allocate, resample task state, or
    /// reenter credential/VFS/loader operations.
    fn exec_committing(
        &self,
        _context: &ExecCommittingSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _new_state: &Self::CredentialState,
    ) {
    }

    /// Observes the complete installed exec image after the generic credential
    /// notification, hardware page-table-root switch, executable/metadata and
    /// userspace-context installation, ptrace exec stop, and release of every
    /// writer/publication/process/image/alias/action lock. It must remain
    /// infallible and allocation-free and must not resample or reenter task,
    /// credential, VFS, or loader operations.
    fn exec_committed(
        &self,
        _context: &ExecCommittedSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _new_state: &Self::CredentialState,
    ) {
    }

    fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &Self::CredentialState,
        target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (actor_state, target_state);
        self.scheduler(context)
    }

    fn signal(&self, _context: &SecuritySignalContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &Self::CredentialState,
        target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (actor_state, target_state);
        self.signal(context)
    }
}

/// Object-safe runtime view of a source-facing module. The adapter keeps the
/// compile-time key and fallible initializer out of dispatch's trait object.
trait ErasedSecurityModule: Send + Sync {
    fn validate_credential_state(&self, state: &dyn ErasedOwnedCredentialState) -> AxResult<()>;
    fn try_init_credential(
        self: Arc<Self>,
        credential: &CoreCred,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>>;
    fn try_prepare_credential(
        self: Arc<Self>,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>>;
    fn authorize_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        proposed_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) -> AxResult<()>;
    fn credential_committed(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        new_credential: &CoreCred,
        new_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    );
    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()>;
    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()>;
    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()>;
    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        parent_state: &dyn ErasedOwnedCredentialState,
        child_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()>;
    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_committing(
        &self,
        context: &ExecCommittingSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    );
    fn exec_committed(
        &self,
        context: &ExecCommittedSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    );
    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()>;
    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()>;
    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
}

trait ErasedOwnedCredentialState: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

/// Keeps the creating runtime alive until its last credential state is freed.
/// Its explicit `Drop` callback means teardown never has to look up a module
/// by ID in global or registry-owned storage.
struct OwnedCredentialState<M: SecurityModule> {
    module: Arc<M>,
    state: Option<M::CredentialState>,
}

impl<M: SecurityModule> OwnedCredentialState<M> {
    fn state(&self) -> &M::CredentialState {
        self.state
            .as_ref()
            .expect("owned credential state was already released")
    }
}

impl<M: SecurityModule> ErasedOwnedCredentialState for OwnedCredentialState<M> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<M: SecurityModule> Drop for OwnedCredentialState<M> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            self.module.free_credential(state);
        }
    }
}

fn try_own_credential_state<M: SecurityModule>(
    module: Arc<M>,
    state: M::CredentialState,
) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
    try_own_credential_state_with(module, state, |state| {
        Box::try_new(state).map_err(|_| AxError::NoMemory)
    })
}

fn try_own_credential_state_with<M, F>(
    module: Arc<M>,
    state: M::CredentialState,
    allocate: F,
) -> AxResult<Box<dyn ErasedOwnedCredentialState>>
where
    M: SecurityModule,
    F: FnOnce(OwnedCredentialState<M>) -> AxResult<Box<OwnedCredentialState<M>>>,
{
    let state = allocate(OwnedCredentialState {
        module,
        state: Some(state),
    })?;
    Ok(state)
}

fn owned_credential_state<'a, M: SecurityModule>(
    module: &M,
    state: &'a dyn ErasedOwnedCredentialState,
) -> AxResult<&'a M::CredentialState> {
    let state = state
        .as_any()
        .downcast_ref::<OwnedCredentialState<M>>()
        .ok_or(AxError::OperationNotPermitted)?;
    if !core::ptr::eq(module, Arc::as_ptr(&state.module)) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(state.state())
}

impl<M: SecurityModule> ErasedSecurityModule for M {
    fn validate_credential_state(&self, state: &dyn ErasedOwnedCredentialState) -> AxResult<()> {
        owned_credential_state(self, state).map(|_| ())
    }

    fn try_init_credential(
        self: Arc<Self>,
        credential: &CoreCred,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
        let state = SecurityModule::try_init_credential(self.as_ref(), credential)?;
        try_own_credential_state(self, state)
    }

    fn try_prepare_credential(
        self: Arc<Self>,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
        let old_state = owned_credential_state(self.as_ref(), old_state)?;
        let proposed_state = SecurityModule::try_prepare_credential(
            self.as_ref(),
            old_credential,
            old_state,
            proposed_credential,
            transition,
        )?;
        try_own_credential_state(self, proposed_state)
    }

    fn authorize_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        proposed_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) -> AxResult<()> {
        SecurityModule::authorize_credential(
            self,
            old_credential,
            owned_credential_state(self, old_state)?,
            proposed_credential,
            owned_credential_state(self, proposed_state)?,
            transition,
        )
    }

    fn credential_committed(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        new_credential: &CoreCred,
        new_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) {
        // A PendingCredentialPostCommit is created only after a complete
        // release-build preflight of both immutable state vectors. Repeating
        // a fallible downcast after publication would introduce a failure
        // branch at a point where rollback is impossible, so this adapter may
        // rely on that linear token's invariant.
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old credential state changed before notification");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new credential state changed before notification");
        SecurityModule::credential_committed(
            self,
            CredentialPostCommitContext {
                old_credential,
                old_state,
                new_credential,
                new_state,
                transition,
            },
        );
    }

    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_permission(self, context)
    }

    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_permission_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::file_open(self, context)
    }

    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::file_open_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_access(self, context)
    }

    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::ptrace_access_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_traceme(self, context)
    }

    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        parent_state: &dyn ErasedOwnedCredentialState,
        child_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::ptrace_traceme_with_credential_state(
            self,
            context,
            owned_credential_state(self, parent_state)?,
            owned_credential_state(self, child_state)?,
        )
    }

    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        SecurityModule::exec_credential(self, context)
    }

    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::exec_credential_with_credential_state(
            self,
            context,
            owned_credential_state(self, old_state)?,
            owned_credential_state(self, proposed_state)?,
        )
    }

    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::exec_executable_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn exec_committing(
        &self,
        context: &ExecCommittingSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    ) {
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old exec credential state changed before committing");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new exec credential state changed before committing");
        SecurityModule::exec_committing(self, context, old_state, new_state);
    }

    fn exec_committed(
        &self,
        context: &ExecCommittedSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    ) {
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old exec credential state changed before committed callback");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new exec credential state changed before committed callback");
        SecurityModule::exec_committed(self, context, old_state, new_state);
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SecurityModule::scheduler(self, context)
    }

    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::scheduler_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }

    fn signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()> {
        SecurityModule::signal(self, context)
    }

    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::signal_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }
}

#[cfg(test)]
fn assert_post_commit_callback_locks_released() {
    assert!(!super::creds::credential_writer_lock_held());
    assert!(!super::creds::credential_publication_lock_held());
    assert!(!super::process::process_security_lock_held());
    assert!(!super::process::process_image_lock_held());
    assert!(!super::process::group_leader_lock_held());
    assert!(!super::process::ptrace_action_lock_held());
    assert!(!super::ops::task_alias_lock_held());
}

#[cfg(test)]
std::thread_local! {
    static COMMONCAP_COMMIT_COUNT: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_OLD_UID: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_NEW_UID: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_TRANSITION: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
pub(in crate::task) fn reset_commoncap_post_commit_probe() {
    COMMONCAP_COMMIT_COUNT.with(|value| value.set(0));
    COMMONCAP_COMMIT_OLD_UID.with(|value| value.set(0));
    COMMONCAP_COMMIT_NEW_UID.with(|value| value.set(0));
    COMMONCAP_COMMIT_TRANSITION.with(|value| value.set(0));
}

#[cfg(test)]
pub(in crate::task) fn commoncap_post_commit_probe() -> (u32, u32, u32, u32) {
    (
        COMMONCAP_COMMIT_COUNT.with(Cell::get),
        COMMONCAP_COMMIT_OLD_UID.with(Cell::get),
        COMMONCAP_COMMIT_NEW_UID.with(Cell::get),
        COMMONCAP_COMMIT_TRANSITION.with(Cell::get),
    )
}

struct CommoncapModule;

impl SecurityModule for CommoncapModule {
    const KEY: ModuleKey = COMMONCAP_MODULE_KEY;
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }

    fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn try_prepare_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn credential_committed(
        &self,
        context: CredentialPostCommitContext<'_, Self::CredentialState>,
    ) {
        #[cfg(test)]
        {
            assert_post_commit_callback_locks_released();
            COMMONCAP_COMMIT_COUNT.with(|value| value.set(value.get() + 1));
            COMMONCAP_COMMIT_OLD_UID
                .with(|value| value.set(context.old_credential().ids().ruid.into_raw()));
            COMMONCAP_COMMIT_NEW_UID
                .with(|value| value.set(context.new_credential().ids().ruid.into_raw()));
            let transition = match context.transition() {
                CredentialStateTransition::Fork => 1,
                CredentialStateTransition::Normal => 1 << 1,
                CredentialStateTransition::UserNamespace => 1 << 2,
                CredentialStateTransition::Exec => 1 << 3,
            };
            COMMONCAP_COMMIT_TRANSITION.with(|value| value.set(transition));
        }
        // Commoncap has no post-publication work today. Touch every immutable
        // fact here so the no-op implementation still exercises the complete
        // typed context in production builds.
        let _ = (
            context.old_credential(),
            context.old_state(),
            context.new_credential(),
            context.new_state(),
            context.transition(),
        );
    }

    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_access(context.core()).map_err(authorization_error)
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_traceme(context.core()).map_err(authorization_error)
    }

    /// Validates the invariants that must still hold after commoncap's exec
    /// credential algebra has produced its proposed value. Keeping this in the
    /// mandatory module prevents an allow-by-default call-site closure from
    /// becoming the effective exec policy.
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        authorize_commoncap_exec(context)
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        external_commoncap_scheduler(context.core()).map_err(authorization_error)
    }
}

/// A deliberately inert second module keeps stacked dispatch exercised in the
/// production shape without selecting a mandatory access-control policy.
struct NoopPolicyModule;

impl SecurityModule for NoopPolicyModule {
    const KEY: ModuleKey = NOOP_POLICY_MODULE_KEY;
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }

    fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn try_prepare_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState> {
        Ok(())
    }
}

struct RegisteredModule {
    id: ModuleId,
    key: ModuleKey,
    module: Arc<dyn ErasedSecurityModule>,
}

struct NeedsCommoncap;
struct HasCommoncap;

/// Fallible, bounded boot builder. Only `HasCommoncap` can be frozen.
struct SecurityRegistryBuilder<State> {
    modules: Option<Vec<RegisteredModule>>,
    _state: PhantomData<State>,
}

impl SecurityRegistryBuilder<NeedsCommoncap> {
    fn try_new() -> Result<Self, RegistryBuildError> {
        Self::try_new_with_reservation(SECURITY_MODULE_LIMIT)
    }

    fn try_new_with_reservation(reservation: usize) -> Result<Self, RegistryBuildError> {
        let mut modules = Vec::new();
        modules
            .try_reserve_exact(reservation.max(SECURITY_MODULE_LIMIT))
            .map_err(|_| RegistryBuildError::NoMemory)?;
        Ok(Self {
            modules: Some(modules),
            _state: PhantomData,
        })
    }

    fn try_register_commoncap(
        self,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError> {
        self.try_register_commoncap_with(CommoncapModule::try_boot_init)
    }

    fn try_register_commoncap_with<F>(
        mut self,
        init: F,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError>
    where
        F: FnOnce() -> Result<CommoncapModule, RegistryBuildError>,
    {
        debug_assert!(self.modules().is_empty());
        let module = init()?;
        self.push_commoncap(module)?;
        let modules = self.modules.take();
        Ok(SecurityRegistryBuilder {
            modules,
            _state: PhantomData,
        })
    }

    fn push_commoncap(&mut self, module: CommoncapModule) -> Result<ModuleId, RegistryBuildError> {
        try_push_registered_module(&mut self.modules, module, try_allocate_security_module)
    }
}

impl SecurityRegistryBuilder<HasCommoncap> {
    fn try_register<M: SecurityModule>(&mut self) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    fn try_register_initialized<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    fn try_register_with_allocator<M, F>(
        &mut self,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated_with(module, allocate)
    }

    fn validate_registration(&self, key: ModuleKey) -> Result<(), RegistryBuildError> {
        if key == COMMONCAP_MODULE_KEY {
            return Err(RegistryBuildError::ReservedModuleKey);
        }
        if self.modules().iter().any(|module| module.key == key) {
            return Err(RegistryBuildError::DuplicateModule);
        }
        if self.modules().len() >= SECURITY_MODULE_LIMIT {
            return Err(RegistryBuildError::Capacity);
        }
        Ok(())
    }

    fn push_prevalidated<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.push_prevalidated_with(module, try_allocate_security_module)
    }

    fn push_prevalidated_with<M, F>(
        &mut self,
        module: M,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        try_push_registered_module(&mut self.modules, module, allocate)
    }

    fn freeze(mut self) -> SecurityRegistry {
        let modules = self.modules.take().expect("registry builder was consumed");
        debug_assert!(!modules.is_empty());
        debug_assert_eq!(modules[0].key, COMMONCAP_MODULE_KEY);
        SecurityRegistry { modules }
    }
}

impl<State> SecurityRegistryBuilder<State> {
    fn modules(&self) -> &[RegisteredModule] {
        self.modules
            .as_deref()
            .expect("registry builder was consumed")
    }
}

fn try_allocate_security_module<M: SecurityModule>(
    module: M,
) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError> {
    let module: Arc<dyn ErasedSecurityModule> =
        Arc::try_new(module).map_err(|_| RegistryBuildError::NoMemory)?;
    Ok(module)
}

fn try_push_registered_module<M, F>(
    modules: &mut Option<Vec<RegisteredModule>>,
    module: M,
    allocate: F,
) -> Result<ModuleId, RegistryBuildError>
where
    M: SecurityModule,
    F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
{
    let modules = modules.as_mut().expect("registry builder was consumed");
    debug_assert!(modules.len() < SECURITY_MODULE_LIMIT);
    debug_assert!(modules.len() < modules.capacity());
    let id = ModuleId(u8::try_from(modules.len()).expect("bounded module index fits u8"));
    let key = M::KEY;
    let module = allocate(module)?;
    modules.push(RegisteredModule { id, key, module });
    Ok(id)
}

impl<State> Drop for SecurityRegistryBuilder<State> {
    fn drop(&mut self) {
        if let Some(modules) = &mut self.modules {
            while modules.pop().is_some() {}
        }
    }
}

/// Immutable, allocation-free runtime dispatch table.
struct SecurityRegistry {
    modules: Vec<RegisteredModule>,
}

struct OwnedModuleCredState {
    module_id: ModuleId,
    erased: Box<dyn ErasedOwnedCredentialState>,
}

/// Complete immutable per-module state carried by one composite credential.
/// The layout identity and dense ModuleId order are checked before every
/// prepare/authorize pass, so a foreign or malformed state fails closed.
pub(in crate::task) struct CredentialSecurityState {
    registry: FrozenSecurityRegistry,
    slots: Vec<OwnedModuleCredState>,
}

impl CredentialSecurityState {
    pub(in crate::task) fn registry(&self) -> FrozenSecurityRegistry {
        self.registry
    }

    fn validate_for(&self, registry: FrozenSecurityRegistry) -> AxResult<()> {
        if !self.registry.same_registry(registry)
            || self.slots.len() != registry.registry().modules.len()
        {
            return Err(AxError::OperationNotPermitted);
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if usize::from(slot.module_id.0) != index
                || registry.registry().modules[index].id != slot.module_id
            {
                return Err(AxError::OperationNotPermitted);
            }
        }
        Ok(())
    }
}

impl Drop for CredentialSecurityState {
    fn drop(&mut self) {
        while self.slots.pop().is_some() {}
    }
}

/// Linear, pre-publication proof for one post-commit notification pass.
///
/// The exact old/new composite credentials are retained by this token, so a
/// later writer cannot make the callback observe a different transition. A
/// dropped unpublished token is an ordinary abort and emits no notification.
#[must_use = "a published credential must consume its post-commit notification token"]
pub(in crate::task) struct PendingCredentialPostCommit {
    registry: FrozenSecurityRegistry,
    old: Arc<Cred>,
    new: Arc<Cred>,
    transition: CredentialStateTransition,
}

impl PendingCredentialPostCommit {
    pub(in crate::task) fn try_new(
        old: &Arc<Cred>,
        new: &Arc<Cred>,
        transition: CredentialStateTransition,
    ) -> AxResult<Self> {
        if !matches!(
            transition,
            CredentialStateTransition::Normal | CredentialStateTransition::Exec
        ) {
            return Err(AxError::BadState);
        }
        let registry = old.security().registry();
        registry
            .registry()
            .validate_credential_post_commit_pair(old, new)?;
        Ok(Self {
            registry,
            old: old.clone(),
            new: new.clone(),
            transition,
        })
    }

    pub(in crate::task) fn notify(self) {
        let Self {
            registry,
            old,
            new,
            transition,
        } = self;
        registry
            .registry()
            .notify_credential_committed(&old, &new, transition);
    }
}

/// Linear preflight for the exec-only committing/committed hook pair.
///
/// It retains the exact old/new credentials and terminal credential source.
/// Dropping this value before `committing()` is a normal exec rollback and
/// emits no lifecycle notification.
#[must_use = "an admitted exec must either abort or enter the committing phase"]
pub(in crate::task) struct PendingExecSecurity {
    registry: FrozenSecurityRegistry,
    old: Arc<Cred>,
    new: Arc<Cred>,
    source: ExecFileSecurityObject,
    effects: ExecCredentialEffects,
}

impl PendingExecSecurity {
    pub(in crate::task) fn try_new(
        prepared: &PreparedCred<'_>,
        source: ExecFileSecurityObject,
        effects: ExecCredentialEffects,
    ) -> AxResult<Self> {
        let old = prepared.old_arc();
        let new = prepared.proposed_arc();
        let registry = old.security().registry();
        registry
            .registry()
            .validate_credential_post_commit_pair(old, new)?;
        Ok(Self {
            registry,
            old: old.clone(),
            new: new.clone(),
            source,
            effects,
        })
    }

    pub(in crate::task) fn committing(self, runtime: ExecCommitRuntime) -> CommittingExecSecurity {
        let context = ExecCommittingSecurityContext::new(
            &self.old,
            &self.new,
            &self.source,
            self.effects,
            &runtime,
        );
        self.registry.registry().notify_exec_committing(&context);
        CommittingExecSecurity {
            registry: self.registry,
            old: Some(self.old),
            new: Some(self.new),
            source: Some(self.source),
            effects: self.effects,
            runtime: Some(runtime),
            armed: true,
        }
    }
}

/// Exec lifecycle after the point-of-no-return callback and before the
/// matching committed callback. Composite image publication carries this
/// token so committed notification cannot be forgotten or detached from the
/// exact credentials which were installed.
#[must_use = "a committing exec must complete its committed notification"]
pub(in crate::task) struct CommittingExecSecurity {
    registry: FrozenSecurityRegistry,
    old: Option<Arc<Cred>>,
    new: Option<Arc<Cred>>,
    source: Option<ExecFileSecurityObject>,
    effects: ExecCredentialEffects,
    runtime: Option<ExecCommitRuntime>,
    armed: bool,
}

impl CommittingExecSecurity {
    pub(in crate::task) fn committed(mut self) -> CompletedExecSecurity {
        let old = self
            .old
            .take()
            .expect("committing exec old credential is live");
        let new = self
            .new
            .take()
            .expect("committing exec new credential is live");
        let source = self.source.take().expect("committing exec source is live");
        let runtime = self
            .runtime
            .take()
            .expect("committing exec runtime facts are live");
        let context =
            ExecCommittedSecurityContext::new(&old, &new, &source, self.effects, &runtime);
        self.registry.registry().notify_exec_committed(&context);
        self.armed = false;
        CompletedExecSecurity {
            _old: old,
            _new: new,
            _source: source,
            _runtime: runtime,
        }
    }
}

impl Drop for CommittingExecSecurity {
    fn drop(&mut self) {
        assert!(
            !self.armed,
            "committing exec dropped without committed security notification"
        );
    }
}

/// Exact exec security facts retained after the full-image callback until the
/// caller has released the exec and vfork publication gates.
#[must_use = "completed exec security ownership must reach the retirement boundary"]
pub(in crate::task) struct CompletedExecSecurity {
    _old: Arc<Cred>,
    _new: Arc<Cred>,
    _source: ExecFileSecurityObject,
    _runtime: ExecCommitRuntime,
}

impl SecurityRegistry {
    fn validate_erased_slots(&self, slots: &[OwnedModuleCredState]) -> AxResult<()> {
        if slots.len() != self.modules.len() {
            return Err(AxError::OperationNotPermitted);
        }
        for (registered, slot) in self.modules.iter().zip(slots) {
            if registered.id != slot.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .validate_credential_state(slot.erased.as_ref())?;
        }
        Ok(())
    }

    fn security_slots<'a>(
        &self,
        security: &'a CredentialSecurityState,
    ) -> AxResult<&'a [OwnedModuleCredState]> {
        let registry = security.registry();
        if !core::ptr::eq(self, registry.registry()) {
            return Err(AxError::OperationNotPermitted);
        }
        security.validate_for(registry)?;
        self.validate_erased_slots(&security.slots)?;
        Ok(&security.slots)
    }

    fn credential_slots<'a>(&self, credential: &'a Cred) -> AxResult<&'a [OwnedModuleCredState]> {
        self.security_slots(credential.security())
    }

    /// Performs the complete fallible layout, ModuleId, erased-type, and
    /// exact-runtime validation for both sides before publication. No module
    /// callback has run if either (including a late) slot is malformed.
    fn validate_credential_post_commit_pair(&self, old: &Cred, new: &Cred) -> AxResult<()> {
        self.credential_slots(old)?;
        self.credential_slots(new)?;
        Ok(())
    }

    /// Dispatches an already-preflighted, immutable pair in registry order.
    /// Publication cannot be rolled back, so this pass is intentionally
    /// infallible and has no allocation or short-circuit path.
    fn notify_credential_committed(
        &self,
        old: &Cred,
        new: &Cred,
        transition: CredentialStateTransition,
    ) {
        let old_slots = &old.security().slots;
        let new_slots = &new.security().slots;
        debug_assert_eq!(old_slots.len(), self.modules.len());
        debug_assert_eq!(new_slots.len(), self.modules.len());
        for ((registered, old_state), new_state) in
            self.modules.iter().zip(old_slots).zip(new_slots)
        {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.credential_committed(
                old.core(),
                old_state.erased.as_ref(),
                new.core(),
                new_state.erased.as_ref(),
                transition,
            );
        }
    }

    fn try_empty_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
    ) -> AxResult<CredentialSecurityState> {
        self.try_empty_credential_state_with_reservation(registry, self.modules.len())
    }

    fn try_empty_credential_state_with_reservation(
        &'static self,
        registry: FrozenSecurityRegistry,
        reservation: usize,
    ) -> AxResult<CredentialSecurityState> {
        if !core::ptr::eq(self, registry.registry()) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(reservation)
            .map_err(|_| AxError::NoMemory)?;
        Ok(CredentialSecurityState { registry, slots })
    }

    fn try_init_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        credential: &CoreCred,
    ) -> AxResult<CredentialSecurityState> {
        let mut candidate = self.try_empty_credential_state(registry)?;
        for registered in &self.modules {
            let erased = registered.module.clone().try_init_credential(credential)?;
            candidate.slots.push(OwnedModuleCredState {
                module_id: registered.id,
                erased,
            });
        }
        candidate.validate_for(registry)?;
        self.validate_erased_slots(&candidate.slots)?;
        Ok(candidate)
    }

    fn try_prepare_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        old_credential: &CoreCred,
        old_state: &CredentialSecurityState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<CredentialSecurityState> {
        old_state.validate_for(registry)?;
        self.validate_erased_slots(&old_state.slots)?;
        let mut candidate = self.try_empty_credential_state(registry)?;
        for (registered, old_slot) in self.modules.iter().zip(&old_state.slots) {
            if registered.id != old_slot.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            let erased = registered.module.clone().try_prepare_credential(
                old_credential,
                old_slot.erased.as_ref(),
                proposed_credential,
                transition,
            )?;
            candidate.slots.push(OwnedModuleCredState {
                module_id: registered.id,
                erased,
            });
        }
        candidate.validate_for(registry)?;
        self.validate_erased_slots(&candidate.slots)?;

        // Authorization is a separate, allocation-free pass over a complete
        // proposal. First denial aborts and reverse-drops the whole candidate.
        for ((registered, old_slot), proposed_slot) in self
            .modules
            .iter()
            .zip(&old_state.slots)
            .zip(&candidate.slots)
        {
            registered.module.authorize_credential(
                old_credential,
                old_slot.erased.as_ref(),
                proposed_credential,
                proposed_slot.erased.as_ref(),
                transition,
            )?;
        }
        Ok(candidate)
    }

    fn dispatch_inode_permission(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_permission(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_permission_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.file_open(context)?;
        }
        Ok(())
    }

    fn dispatch_file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .file_open_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_access(context)?;
        }
        Ok(())
    }

    fn dispatch_ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.ptrace_access_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    fn dispatch_ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_traceme(context)?;
        }
        Ok(())
    }

    fn dispatch_ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
    ) -> AxResult<()> {
        let parent = self.credential_slots(context.parent_actor())?;
        let child = self.credential_slots(context.child_target())?;
        for ((registered, parent_state), child_state) in self.modules.iter().zip(parent).zip(child)
        {
            if registered.id != parent_state.module_id || registered.id != child_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.ptrace_traceme_with_credential_state(
                context,
                parent_state.erased.as_ref(),
                child_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    fn dispatch_exec_credential(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.exec_credential(context)?;
        }
        Ok(())
    }

    fn dispatch_exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
    ) -> AxResult<()> {
        let old = self.credential_slots(context.old())?;
        let proposed = self.security_slots(context.draft().proposed_security())?;
        for ((registered, old_state), proposed_state) in self.modules.iter().zip(old).zip(proposed)
        {
            if registered.id != old_state.module_id || registered.id != proposed_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.exec_credential_with_credential_state(
                context,
                old_state.erased.as_ref(),
                proposed_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    fn dispatch_exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .exec_executable_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    /// Runs a preflighted infallible phase over exact old/new module states.
    /// `PendingExecSecurity::try_new` has already validated every erased slot,
    /// so no failure branch can appear after exec crosses its point of no
    /// return.
    fn notify_exec_committing(&self, context: &ExecCommittingSecurityContext<'_>) {
        let old = &context.old().security().slots;
        let new = &context.new_credential().security().slots;
        debug_assert_eq!(old.len(), self.modules.len());
        debug_assert_eq!(new.len(), self.modules.len());
        for ((registered, old_state), new_state) in self.modules.iter().zip(old).zip(new) {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.exec_committing(
                context,
                old_state.erased.as_ref(),
                new_state.erased.as_ref(),
            );
        }
    }

    fn notify_exec_committed(&self, context: &ExecCommittedSecurityContext<'_>) {
        let old = &context.old().security().slots;
        let new = &context.new_credential().security().slots;
        debug_assert_eq!(old.len(), self.modules.len());
        debug_assert_eq!(new.len(), self.modules.len());
        for ((registered, old_state), new_state) in self.modules.iter().zip(old).zip(new) {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.exec_committed(
                context,
                old_state.erased.as_ref(),
                new_state.erased.as_ref(),
            );
        }
    }

    fn dispatch_scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.scheduler(context)?;
        }
        Ok(())
    }

    fn dispatch_scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.scheduler_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    fn dispatch_signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.signal(context)?;
        }
        Ok(())
    }

    fn dispatch_signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.signal_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }
}

impl FrozenSecurityRegistry {
    pub(in crate::task) fn try_init_credential_state(
        self,
        credential: &CoreCred,
    ) -> AxResult<CredentialSecurityState> {
        self.registry().try_init_credential_state(self, credential)
    }

    pub(in crate::task) fn try_prepare_credential_state(
        self,
        old_credential: &CoreCred,
        old_state: &CredentialSecurityState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<CredentialSecurityState> {
        self.registry().try_prepare_credential_state(
            self,
            old_credential,
            old_state,
            proposed_credential,
            transition,
        )
    }
}

impl Drop for SecurityRegistry {
    fn drop(&mut self) {
        while self.modules.pop().is_some() {}
    }
}

struct SecurityRegistryPublication {
    registry: Once<SecurityRegistry>,
}

impl SecurityRegistryPublication {
    const fn new() -> Self {
        Self {
            registry: Once::new(),
        }
    }

    /// Serializes construction as well as publication. `spin::Once` retries
    /// after a failed initializer and never invokes a losing caller's closure
    /// after another caller succeeds. The local flag distinguishes that first
    /// success from a later call that merely observed the published value.
    fn try_publish_with<F>(&self, build: F) -> Result<&SecurityRegistry, RegistryBuildError>
    where
        F: FnOnce() -> Result<SecurityRegistry, RegistryBuildError>,
    {
        let mut initialized_here = false;
        let registry = self.registry.try_call_once(|| {
            initialized_here = true;
            build()
        })?;
        if initialized_here {
            Ok(registry)
        } else {
            Err(RegistryBuildError::AlreadyPublished)
        }
    }

    #[cfg(test)]
    fn get(&self) -> Option<&SecurityRegistry> {
        self.registry.get()
    }
}

static SECURITY_REGISTRY: SecurityRegistryPublication = SecurityRegistryPublication::new();

#[cfg(test)]
static TEST_SECURITY_REGISTRY: Once<SecurityRegistry> = Once::new();

fn try_build_builtin_registry() -> Result<SecurityRegistry, RegistryBuildError> {
    let mut builder = SecurityRegistryBuilder::try_new()?.try_register_commoncap()?;
    builder.try_register::<NoopPolicyModule>()?;
    Ok(builder.freeze())
}

/// Builds, freezes, and publishes the complete registry before userspace.
pub(crate) fn init() -> Result<FrozenSecurityRegistry, RegistryBuildError> {
    let registry = SECURITY_REGISTRY.try_publish_with(try_build_builtin_registry)?;
    Ok(FrozenSecurityRegistry(registry))
}

#[cfg(test)]
pub(in crate::task) fn test_frozen_registry() -> FrozenSecurityRegistry {
    FrozenSecurityRegistry(
        TEST_SECURITY_REGISTRY
            .call_once(|| try_build_builtin_registry().expect("failed to build test registry")),
    )
}

#[cfg(test)]
fn require_published_registry(registry: Option<&SecurityRegistry>) -> AxResult<&SecurityRegistry> {
    registry.ok_or(AxError::OperationNotPermitted)
}

/// Runs typed inode-permission hooks after the caller has completed DAC
/// admission over the exact frozen object. The first denial is returned
/// immediately.
///
/// The current call-site contract is the open/pathwalk vertical slice, not a
/// claim that every VFS permission path has already migrated. Dispatch may be
/// inside filesystem-context/pathwalk lock domains, so hooks are
/// allocation-free, nonblocking, and forbidden from VFS/current/credential
/// reentry.
pub(crate) fn dispatch_inode_permission(
    context: &InodePermissionSecurityContext<'_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_permission_with_credential_state(context)
}

/// Runs typed file-open hooks for one already-resolved, still-unpublished open
/// transaction. The first denial is returned immediately.
///
/// This entry point serves the current open vertical slice rather than every
/// possible kernel-internal file construction. Callers invoke it before fd,
/// persistent executable-write reservation, fanotify open permission, POSIX
/// lease conflict handling, filesystem-open, or truncate side effects become
/// visible. Hooks are allocation-free, nonblocking, and forbidden from
/// VFS/current/credential or nested open reentry.
pub(crate) fn dispatch_file_open(context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_file_open_with_credential_state(context)
}

/// Runs the frozen ptrace access hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_access(context: &PtraceAccessContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_ptrace_access_with_credential_state(context)
}

/// Runs the frozen traceme hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_traceme(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
    context
        .parent_actor()
        .security()
        .registry()
        .registry()
        .dispatch_ptrace_traceme_with_credential_state(context)
}

/// Runs the frozen exec-credential hooks in declaration order.
/// The first denial aborts the still-unpublished prepared credential.
pub(crate) fn dispatch_exec_credential(
    context: &ExecCredentialSecurityContext<'_>,
) -> AxResult<()> {
    context
        .old()
        .security()
        .registry()
        .registry()
        .dispatch_exec_credential_with_credential_state(context)
}

/// Runs typed executable-component hooks for the already-resolved object in
/// declaration order. Denial happens before the loader consumes that
/// component and drops every transient executable lease on unwind.
pub(crate) fn dispatch_exec_executable(
    context: &ExecExecutableSecurityContext<'_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_exec_executable_with_credential_state(context)
}

/// Runs the frozen scheduler hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_scheduler(context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_scheduler_with_credential_state(context)
}

/// Runs the frozen signal policy hooks after Linux core signal permission has
/// admitted the exact actor/target pair. The first denial is returned without
/// invoking later modules.
pub(crate) fn dispatch_signal(context: &SecuritySignalContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_signal_with_credential_state(context)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::{
        sync::{Barrier, Mutex, MutexGuard},
        thread,
    };

    use axfs_ng_vfs::{Mountpoint, NodePermission};
    use linux_raw_sys::general::{CAP_CHOWN, CAP_SYS_NICE, CAP_SYS_PTRACE};

    use super::*;
    use crate::{
        pseudofs::tmp::MemoryFs,
        task::{
            CapabilityState, Cred, CredentialSlot, Credentials, ExecCommitRuntime,
            ExecFileIdentity, ExecImageIdentity, Kgid, Kuid,
            creds::{CAPABILITY_WORDS, credential_publication_lock_held},
        },
    };

    static ORDER_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DIRECTION: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static EXEC_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static FILE_OPEN_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SCHEDULER_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SIGNAL_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_HOOK_TRACE: AtomicU64 = AtomicU64::new(0);
    static MODULE_DROP_TRACE: AtomicU32 = AtomicU32::new(0);
    static RESERVED_MODULE_INIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_AUTHORIZE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_GENERATION_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_TRANSITION_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_OLD_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_NEW_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DROP_AT_COMMIT: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DROP_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DISPATCH_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_PERMISSION_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_FILE_OPEN_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXEC_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXECUTABLE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXECUTABLE_ROLE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXEC_COMMITTING_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXEC_COMMITTED_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_HOOK_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_TRANSITION_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_FAIL_INIT_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_FAIL_PREPARE_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXEC_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXECUTABLE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn append_trace(trace: &AtomicU32, value: u32) {
        trace
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(old * 10 + value)
            })
            .unwrap();
    }

    fn reset_credential_state_probes() -> MutexGuard<'static, ()> {
        let guard = CRED_STATE_TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for trace in [
            &CRED_STATE_INIT_TRACE,
            &CRED_STATE_PREPARE_TRACE,
            &CRED_STATE_AUTHORIZE_TRACE,
            &CRED_STATE_COMMIT_TRACE,
            &CRED_STATE_COMMIT_GENERATION_TRACE,
            &CRED_STATE_COMMIT_TRANSITION_MASK,
            &CRED_STATE_COMMIT_OLD_UID,
            &CRED_STATE_COMMIT_NEW_UID,
            &CRED_STATE_DROP_AT_COMMIT,
            &CRED_STATE_DROP_TRACE,
            &CRED_STATE_DISPATCH_TRACE,
            &CRED_STATE_INODE_PERMISSION_TRACE,
            &CRED_STATE_FILE_OPEN_TRACE,
            &CRED_STATE_EXEC_TRACE,
            &CRED_STATE_EXECUTABLE_TRACE,
            &CRED_STATE_EXECUTABLE_ROLE_TRACE,
            &CRED_STATE_EXEC_COMMITTING_TRACE,
            &CRED_STATE_EXEC_COMMITTED_TRACE,
            &CRED_STATE_HOOK_MASK,
            &CRED_STATE_TRANSITION_MASK,
            &CRED_STATE_FAIL_INIT_KEY,
            &CRED_STATE_FAIL_PREPARE_KEY,
            &CRED_STATE_DENY_KEY,
            &CRED_STATE_EXEC_DENY_KEY,
            &CRED_STATE_EXECUTABLE_DENY_KEY,
        ] {
            trace.store(0, Ordering::SeqCst);
        }
        guard
    }

    fn security_test_inode() -> Location {
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o755)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        mount
            .root_location()
            .create(
                "security-hook",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o640),
            )
            .unwrap()
    }

    fn security_test_dac(uid: u32, gid: u32) -> DacCredentialView {
        DacCredentialView::new(
            Kuid::from_raw(uid).unwrap(),
            Kgid::from_raw(gid).unwrap(),
            thekernel_linux_cred::GroupInfo::try_new(Vec::new()).unwrap(),
            [0; CAPABILITY_WORDS],
            true,
        )
    }

    struct ProbeCredentialState {
        key: u32,
        generation: u32,
        committed: AtomicBool,
    }

    struct CredentialStateProbeModule<const KEY: u64>;

    impl<const KEY: u64> SecurityModule for CredentialStateProbeModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);
        type CredentialState = ProbeCredentialState;

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            append_trace(&CRED_STATE_INIT_TRACE, key);
            if CRED_STATE_FAIL_INIT_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::NoMemory);
            }
            Ok(ProbeCredentialState {
                key,
                generation: 0,
                committed: AtomicBool::new(true),
            })
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(old_state.key, key);
            let transition_bit = match transition {
                CredentialStateTransition::Fork => 1,
                CredentialStateTransition::Normal => 1 << 1,
                CredentialStateTransition::UserNamespace => 1 << 2,
                CredentialStateTransition::Exec => 1 << 3,
            };
            CRED_STATE_TRANSITION_MASK.fetch_or(transition_bit, Ordering::SeqCst);
            append_trace(&CRED_STATE_PREPARE_TRACE, key);
            if CRED_STATE_FAIL_PREPARE_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::NoMemory);
            }
            Ok(ProbeCredentialState {
                key,
                generation: old_state.generation + 1,
                committed: AtomicBool::new(false),
            })
        }

        fn authorize_credential(
            &self,
            _old_credential: &CoreCred,
            old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            proposed_state: &Self::CredentialState,
            _transition: CredentialStateTransition,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(old_state.key, key);
            assert_eq!(proposed_state.key, key);
            assert_eq!(proposed_state.generation, old_state.generation + 1);
            append_trace(&CRED_STATE_AUTHORIZE_TRACE, key);
            if CRED_STATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn credential_committed(
            &self,
            context: CredentialPostCommitContext<'_, Self::CredentialState>,
        ) {
            assert_post_commit_callback_locks_released();
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(context.old_state().key, key);
            assert_eq!(context.new_state().key, key);
            assert!(context.old_state().committed.load(Ordering::SeqCst));
            assert!(!context.new_state().committed.swap(true, Ordering::SeqCst));
            assert_eq!(
                context.new_state().generation,
                context.old_state().generation + 1
            );
            append_trace(&CRED_STATE_COMMIT_TRACE, key);
            if key == 2 {
                append_trace(
                    &CRED_STATE_COMMIT_GENERATION_TRACE,
                    context.new_state().generation,
                );
                CRED_STATE_COMMIT_OLD_UID.store(
                    context.old_credential().ids().ruid.into_raw(),
                    Ordering::SeqCst,
                );
                CRED_STATE_COMMIT_NEW_UID.store(
                    context.new_credential().ids().ruid.into_raw(),
                    Ordering::SeqCst,
                );
                CRED_STATE_DROP_AT_COMMIT.store(
                    CRED_STATE_DROP_TRACE.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            }
            let transition_bit = match context.transition() {
                CredentialStateTransition::Fork => 1,
                CredentialStateTransition::Normal => 1 << 1,
                CredentialStateTransition::UserNamespace => 1 << 2,
                CredentialStateTransition::Exec => 1 << 3,
            };
            CRED_STATE_COMMIT_TRANSITION_MASK.fetch_or(transition_bit, Ordering::SeqCst);
        }

        fn inode_permission_with_credential_state(
            &self,
            context: &InodePermissionSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            assert!(core::ptr::eq(
                context.core().actor(),
                context.actor().core()
            ));
            assert!(core::ptr::eq(
                context.core().dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                context.core().target_object(),
                context.target_object()
            ));
            append_trace(&CRED_STATE_INODE_PERMISSION_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 6, Ordering::SeqCst);
            Ok(())
        }

        fn file_open_with_credential_state(
            &self,
            context: &FileOpenSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            assert!(core::ptr::eq(
                context.core().actor(),
                context.actor().core()
            ));
            assert!(core::ptr::eq(
                context.core().dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                context.core().target_object(),
                context.target_object()
            ));
            append_trace(&CRED_STATE_FILE_OPEN_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 7, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_access_with_credential_state(
            &self,
            context: &PtraceAccessContext<'_>,
            actor_state: &Self::CredentialState,
            target_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert_eq!(target_state.key, key);
            assert!(core::ptr::eq(context.actor(), context.target()));
            append_trace(&CRED_STATE_DISPATCH_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_traceme_with_credential_state(
            &self,
            context: &PtraceTracemeContext<'_>,
            parent_state: &Self::CredentialState,
            child_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(parent_state.key, key);
            assert_eq!(child_state.key, key);
            assert!(core::ptr::eq(
                context.parent_actor(),
                context.child_target()
            ));
            CRED_STATE_HOOK_MASK.fetch_or(1 << 1, Ordering::SeqCst);
            Ok(())
        }

        fn exec_credential_with_credential_state(
            &self,
            _context: &ExecCredentialSecurityContext<'_>,
            old_state: &Self::CredentialState,
            proposed_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(old_state.key, key);
            assert_eq!(proposed_state.key, key);
            assert_eq!(proposed_state.generation, old_state.generation + 1);
            append_trace(&CRED_STATE_EXEC_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 2, Ordering::SeqCst);
            if CRED_STATE_EXEC_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn exec_executable_with_credential_state(
            &self,
            context: &ExecExecutableSecurityContext<'_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let executable = context.executable();
            assert_eq!(executable.identity(), ExecFileIdentity::new(17, 23));
            assert_eq!(executable.identity().device(), 17);
            assert_eq!(executable.identity().inode(), 23);
            assert!(Arc::ptr_eq(
                executable.owner_user_ns(),
                context.actor().user_ns()
            ));
            assert!(executable.readable());
            if key == 2 {
                let role = match executable.role() {
                    crate::task::ExecExecutableRole::Requested => 1,
                    crate::task::ExecExecutableRole::ScriptInterpreter => 2,
                    crate::task::ExecExecutableRole::DynamicLinker => 3,
                };
                append_trace(&CRED_STATE_EXECUTABLE_ROLE_TRACE, role);
            }
            append_trace(&CRED_STATE_EXECUTABLE_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 5, Ordering::SeqCst);
            if CRED_STATE_EXECUTABLE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn exec_committing(
            &self,
            context: &ExecCommittingSecurityContext<'_>,
            old_state: &Self::CredentialState,
            new_state: &Self::CredentialState,
        ) {
            assert!(super::super::creds::credential_writer_lock_held());
            assert!(!super::super::creds::credential_publication_lock_held());
            assert!(!super::super::process::process_security_lock_held());
            assert!(!super::super::process::process_image_lock_held());
            assert!(!super::super::process::group_leader_lock_held());
            assert!(!super::super::process::ptrace_action_lock_held());
            assert!(!super::super::ops::task_alias_lock_held());
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(old_state.key, key);
            assert_eq!(new_state.key, key);
            assert!(old_state.committed.load(Ordering::SeqCst));
            assert!(!new_state.committed.load(Ordering::SeqCst));
            assert_eq!(context.source().identity(), ExecFileIdentity::new(17, 23));
            assert_eq!(context.runtime().process_id(), 41);
            assert_eq!(context.runtime().executing_tid(), 43);
            assert_eq!(context.runtime().post_exec_tid(), 41);
            assert_ne!(context.runtime().image_identity().as_usize(), 0);
            assert!(Arc::ptr_eq(
                context.runtime().image_owner_user_ns(),
                context.new_credential().user_ns()
            ));
            assert_eq!(
                context.effects().dumpability(),
                crate::task::exec_cred::ExecDumpability::UserDumpable
            );
            append_trace(&CRED_STATE_EXEC_COMMITTING_TRACE, key);
        }

        fn exec_committed(
            &self,
            context: &ExecCommittedSecurityContext<'_>,
            old_state: &Self::CredentialState,
            new_state: &Self::CredentialState,
        ) {
            assert_post_commit_callback_locks_released();
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(old_state.key, key);
            assert_eq!(new_state.key, key);
            assert!(old_state.committed.load(Ordering::SeqCst));
            assert!(new_state.committed.load(Ordering::SeqCst));
            assert_eq!(context.source().identity(), ExecFileIdentity::new(17, 23));
            assert_eq!(context.runtime().process_id(), 41);
            assert_ne!(context.runtime().image_identity().as_usize(), 0);
            assert_eq!(
                context.effects().dumpability(),
                crate::task::exec_cred::ExecDumpability::UserDumpable
            );
            assert!(Arc::ptr_eq(
                context.runtime().image_owner_user_ns(),
                context.new_credential().user_ns()
            ));
            append_trace(&CRED_STATE_EXEC_COMMITTED_TRACE, key);
        }

        fn scheduler_with_credential_state(
            &self,
            context: &SecuritySchedulerContext<'_>,
            actor_state: &Self::CredentialState,
            target_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert_eq!(target_state.key, key);
            assert!(core::ptr::eq(context.actor(), context.target()));
            CRED_STATE_HOOK_MASK.fetch_or(1 << 3, Ordering::SeqCst);
            Ok(())
        }

        fn signal_with_credential_state(
            &self,
            context: &SecuritySignalContext<'_>,
            actor_state: &Self::CredentialState,
            target_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert_eq!(target_state.key, key);
            assert!(core::ptr::eq(context.actor(), context.target()));
            assert_eq!(context.target_object().kind(), SignalTargetKind::Process);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 4, Ordering::SeqCst);
            Ok(())
        }

        fn free_credential(&self, state: Self::CredentialState) {
            assert!(!credential_publication_lock_held());
            if state.committed.load(Ordering::SeqCst) {
                assert_post_commit_callback_locks_released();
            }
            append_trace(&CRED_STATE_DROP_TRACE, state.key);
        }
    }

    type TestInodePermissionHook = for<'context, 'location> fn(
        &InodePermissionSecurityContext<'context, 'location>,
    ) -> AxResult<()>;
    type TestFileOpenHook =
        for<'context, 'location> fn(&FileOpenSecurityContext<'context, 'location>) -> AxResult<()>;
    type TestPtraceAccessHook = for<'a> fn(&PtraceAccessContext<'a>) -> AxResult<()>;
    type TestPtraceTracemeHook = for<'a> fn(&PtraceTracemeContext<'a>) -> AxResult<()>;
    type TestExecCredentialHook = for<'a> fn(&ExecCredentialSecurityContext<'a>) -> AxResult<()>;
    type TestSchedulerHook = for<'a> fn(&SecuritySchedulerContext<'a>) -> AxResult<()>;
    type TestSignalHook = for<'a> fn(&SecuritySignalContext<'a>) -> AxResult<()>;

    struct TestSecurityModule<const KEY: u64> {
        inode_permission: Option<TestInodePermissionHook>,
        file_open: Option<TestFileOpenHook>,
        ptrace_access: Option<TestPtraceAccessHook>,
        ptrace_traceme: Option<TestPtraceTracemeHook>,
        exec_credential: Option<TestExecCredentialHook>,
        scheduler: Option<TestSchedulerHook>,
        signal: Option<TestSignalHook>,
    }

    impl<const KEY: u64> TestSecurityModule<KEY> {
        const fn empty() -> Self {
            Self {
                inode_permission: None,
                file_open: None,
                ptrace_access: None,
                ptrace_traceme: None,
                exec_credential: None,
                scheduler: None,
                signal: None,
            }
        }
    }

    impl<const KEY: u64> SecurityModule for TestSecurityModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self::empty())
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn inode_permission(
            &self,
            context: &InodePermissionSecurityContext<'_, '_>,
        ) -> AxResult<()> {
            self.inode_permission.map_or(Ok(()), |hook| hook(context))
        }

        fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
            self.file_open.map_or(Ok(()), |hook| hook(context))
        }

        fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
            self.ptrace_access.map_or(Ok(()), |hook| hook(context))
        }

        fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            self.ptrace_traceme.map_or(Ok(()), |hook| hook(context))
        }

        fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            self.exec_credential.map_or(Ok(()), |hook| hook(context))
        }

        fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            self.scheduler.map_or(Ok(()), |hook| hook(context))
        }

        fn signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()> {
            self.signal.map_or(Ok(()), |hook| hook(context))
        }
    }

    struct FailingModule<const KEY: u64>;

    impl<const KEY: u64> SecurityModule for FailingModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Err(RegistryBuildError::ModuleInitFailed)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }
    }

    struct WholeHookModule;

    impl SecurityModule for WholeHookModule {
        const KEY: ModuleKey = ModuleKey(10);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn inode_permission(
            &self,
            _context: &InodePermissionSecurityContext<'_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 48, Ordering::SeqCst);
            Ok(())
        }

        fn file_open(&self, _context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 56, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn exec_executable(&self, _context: &ExecExecutableSecurityContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 40, Ordering::SeqCst);
            Ok(())
        }

        fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }

        fn signal(&self, _context: &SecuritySignalContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 32, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingWholeHookModule;

    impl SecurityModule for FailingWholeHookModule {
        const KEY: ModuleKey = ModuleKey(11);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Err(RegistryBuildError::ModuleInitFailed)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn inode_permission(
            &self,
            _context: &InodePermissionSecurityContext<'_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 48, Ordering::SeqCst);
            Ok(())
        }

        fn file_open(&self, _context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 56, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }

        fn signal(&self, _context: &SecuritySignalContext<'_>) -> AxResult<()> {
            WHOLE_MODULE_HOOK_TRACE.fetch_add(1 << 32, Ordering::SeqCst);
            Ok(())
        }
    }

    struct ReservedKeyModule;

    impl SecurityModule for ReservedKeyModule {
        const KEY: ModuleKey = COMMONCAP_MODULE_KEY;
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            RESERVED_MODULE_INIT_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }
    }

    struct DroppingModule<const KEY: u64>;

    impl<const KEY: u64> SecurityModule for DroppingModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            Ok(Self)
        }

        fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
            Ok(())
        }

        fn try_prepare_credential(
            &self,
            _old_credential: &CoreCred,
            _old_state: &Self::CredentialState,
            _proposed_credential: &CoreCred,
            _transition: CredentialStateTransition,
        ) -> AxResult<Self::CredentialState> {
            Ok(())
        }
    }

    impl<const KEY: u64> Drop for DroppingModule<KEY> {
        fn drop(&mut self) {
            let key = u32::try_from(KEY).expect("test key fits u32");
            MODULE_DROP_TRACE
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |trace| {
                    Some(trace * 10 + key)
                })
                .unwrap();
        }
    }

    fn test_registry_builder() -> SecurityRegistryBuilder<HasCommoncap> {
        SecurityRegistryBuilder::try_new()
            .unwrap()
            .try_register_commoncap()
            .unwrap()
    }

    fn probe_registry() -> FrozenSecurityRegistry {
        let mut builder = test_registry_builder();
        builder
            .try_register::<CredentialStateProbeModule<2>>()
            .unwrap();
        builder
            .try_register::<CredentialStateProbeModule<3>>()
            .unwrap();
        freeze_test_registry(builder.freeze())
    }

    fn freeze_test_registry(registry: SecurityRegistry) -> FrozenSecurityRegistry {
        let registry = Box::try_new(registry).unwrap();
        FrozenSecurityRegistry(Box::leak(registry))
    }

    fn dispatch_all_hook_families(registry: SecurityRegistry) {
        let registry = freeze_test_registry(registry);
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let dispatch = registry.registry();
        let inode_location = security_test_inode();
        let inode_metadata = inode_location.metadata().unwrap();
        let inode_object = InodeSecurityRef::new(&inode_location, &inode_metadata);
        let dac_credential = root.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(root.user_ns());
        let inode_permission = InodePermissionSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            InodePermissionAccess::READ,
        );
        let file_open = FileOpenSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            FileOpenOperation::new(FileOpenAccess::Read, false, false, false, false).unwrap(),
        );
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let access = PtraceAccessContext::new(
            &root,
            &root,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let traceme =
            PtraceTracemeContext::new(&root, &root, image_ref.owner_user_ns(), &image_ref);
        let draft = exec_draft(&root, crate::task::ExecTraceState::NotSuppressingPrivilege);
        let exec = ExecCredentialSecurityContext::new(&draft);
        let executable = ExecExecutableSecurityContext::new(&root, draft.source());
        let scheduler = scheduler_context(&root, &root, SchedulerSecurityOperation::SetAffinity);
        let signal_target = SignalTargetSecurityRef::new(&image, 1, 1, SignalTargetKind::Process);
        let signal = SecuritySignalContext::authorize(
            &root,
            &root,
            &signal_target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            true,
            true,
        )
        .unwrap();

        dispatch
            .dispatch_inode_permission(&inode_permission)
            .unwrap();
        dispatch.dispatch_file_open(&file_open).unwrap();
        dispatch.dispatch_ptrace_access(&access).unwrap();
        dispatch.dispatch_ptrace_traceme(&traceme).unwrap();
        dispatch.dispatch_exec_credential(&exec).unwrap();
        dispatch
            .dispatch_exec_executable_with_credential_state(&executable)
            .unwrap();
        dispatch.dispatch_scheduler(&scheduler).unwrap();
        dispatch.dispatch_signal(&signal).unwrap();
    }

    fn capability_set(capabilities: &[u32]) -> [u32; CAPABILITY_WORDS] {
        let mut result = [0; CAPABILITY_WORDS];
        for capability in capabilities {
            let (word, mask) = CapabilityState::cap_mask(*capability).unwrap();
            result[word] |= mask;
        }
        result
    }

    fn credential_with_caps(base: &Arc<Cred>, permitted: &[u32], effective: &[u32]) -> Arc<Cred> {
        let slot = CredentialSlot::new(base.clone());
        let mut update = slot.prepare();
        update.builder.caps.permitted = capability_set(permitted);
        update.builder.caps.effective = capability_set(effective);
        update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit()
    }

    fn credential_with_identity_and_caps(
        base: &Arc<Cred>,
        uid: u32,
        permitted: &[u32],
        effective: &[u32],
    ) -> Arc<Cred> {
        let slot = CredentialSlot::new(base.clone());
        let mut update = slot.prepare();
        let gid = Kgid::from_raw(uid).unwrap();
        let uid = Kuid::from_raw(uid).unwrap();
        update.builder.ids = Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        update.builder.caps.permitted = capability_set(permitted);
        update.builder.caps.effective = capability_set(effective);
        update.builder.caps.inheritable = [0; CAPABILITY_WORDS];
        update.builder.caps.ambient = [0; CAPABILITY_WORDS];
        update.finish().unwrap().commit()
    }

    fn scheduler_context<'a>(
        actor: &'a Cred,
        target: &'a Cred,
        operation: SchedulerSecurityOperation,
    ) -> SecuritySchedulerContext<'a> {
        SecuritySchedulerContext::new(actor, target, operation)
    }

    fn access_context<'a>(
        actor: &'a Cred,
        target: &'a Cred,
        image: &'a ProcessImageSecurityRef<'a>,
        credential_kind: PtraceCredentialKind,
    ) -> PtraceAccessContext<'a> {
        PtraceAccessContext::new(
            actor,
            target,
            image.owner_user_ns(),
            image,
            PtraceAccessKind::Attach,
            credential_kind,
        )
    }

    fn ordered_inode_first(context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        assert_eq!(context.access(), InodePermissionAccess::READ);
        assert_eq!(INODE_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_second(_context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        assert_eq!(INODE_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_first(_context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_must_not_run(_context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_file_open_first(context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        assert_eq!(context.operation().access(), FileOpenAccess::Read);
        assert_eq!(FILE_OPEN_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_file_open_second(_context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        assert_eq!(FILE_OPEN_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_file_open_first(_context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        FILE_OPEN_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn file_open_must_not_run(_context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        FILE_OPEN_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_first(context: &PtraceAccessContext<'_>) -> AxResult<()> {
        assert_eq!(context.access_kind(), PtraceAccessKind::Read);
        assert_eq!(ORDER_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_second(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        assert_eq!(ORDER_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_first(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn must_not_run(_: &PtraceAccessContext<'_>) -> AxResult<()> {
        DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn record_traceme_direction(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        let parent = context.parent_actor().ids().euid;
        let child = context.child_target().ids().euid;
        if parent == Kuid::INITIAL_ROOT && child == Kuid::from_raw(1000).unwrap() {
            TRACEME_DIRECTION.store(1, Ordering::SeqCst);
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }

    fn deny_traceme_first(_: &PtraceTracemeContext<'_>) -> AxResult<()> {
        TRACEME_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn traceme_must_not_run(_: &PtraceTracemeContext<'_>) -> AxResult<()> {
        TRACEME_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn deny_exec_first(_: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        EXEC_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn exec_must_not_run(_: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        EXEC_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn deny_scheduler_first(_: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SCHEDULER_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn scheduler_must_not_run(_: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SCHEDULER_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn deny_signal_first(context: &SecuritySignalContext<'_>) -> AxResult<()> {
        assert_eq!(context.target_object().kind(), SignalTargetKind::Zombie);
        assert_eq!(
            context.operation(),
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            )
        );
        SIGNAL_DENY_HOOK_TRACE.store(1, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn signal_must_not_run(_: &SecuritySignalContext<'_>) -> AxResult<()> {
        SIGNAL_DENY_HOOK_TRACE.store(2, Ordering::SeqCst);
        Ok(())
    }

    fn exec_draft(
        credential: &Arc<Cred>,
        trace_state: crate::task::ExecTraceState,
    ) -> crate::task::exec_cred::ExecCredentialDraft {
        let input = crate::task::ExecCredentialInput::new(
            0,
            Some(crate::task::ExecFileOwner::new(
                Kuid::INITIAL_ROOT,
                Kgid::INITIAL_ROOT,
            )),
            crate::task::ExecMountPrivilege::Honor,
            trace_state,
            crate::task::ExecImageReadability::Readable,
            None,
        );
        let source = crate::task::ExecFileSecurityObject::new(
            crate::task::ExecFileIdentity::new(17, 23),
            credential.user_ns().clone(),
            Some(crate::task::ExecFileOwner::new(
                Kuid::INITIAL_ROOT,
                Kgid::INITIAL_ROOT,
            )),
            0o755,
            true,
            crate::task::ExecExecutableRole::Requested,
        );
        crate::task::exec_cred::ExecCredentialDraft::try_new(credential, input, source).unwrap()
    }

    #[test]
    fn registry_builder_reports_reservation_failure() {
        assert!(matches!(
            SecurityRegistryBuilder::<NeedsCommoncap>::try_new_with_reservation(usize::MAX),
            Err(RegistryBuildError::NoMemory)
        ));
    }

    #[test]
    fn registry_builder_requires_and_preserves_commoncap_first() {
        let mut builder = test_registry_builder();
        assert_eq!(builder.modules().len(), 1);
        assert_eq!(builder.modules()[0].id, ModuleId(0));
        assert_eq!(builder.modules()[0].key, COMMONCAP_MODULE_KEY);

        assert_eq!(
            builder.try_register::<TestSecurityModule<2>>().unwrap(),
            ModuleId(1)
        );
        builder.try_register::<TestSecurityModule<3>>().unwrap();
        let allocation = builder.modules().as_ptr();
        let capacity = builder
            .modules
            .as_ref()
            .expect("builder is live")
            .capacity();
        let registry = builder.freeze();

        assert_eq!(registry.modules.as_ptr(), allocation);
        assert_eq!(registry.modules.capacity(), capacity);
        assert_eq!(
            registry
                .modules
                .iter()
                .map(|module| module.key)
                .collect::<Vec<_>>(),
            [COMMONCAP_MODULE_KEY, ModuleKey(2), ModuleKey(3)]
        );
    }

    #[test]
    fn registry_builder_enforces_total_capacity() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        builder.try_register::<TestSecurityModule<3>>().unwrap();
        builder.try_register::<TestSecurityModule<4>>().unwrap();
        builder.try_register::<TestSecurityModule<5>>().unwrap();
        builder.try_register::<TestSecurityModule<6>>().unwrap();
        builder.try_register::<TestSecurityModule<7>>().unwrap();
        assert_eq!(builder.modules().len(), 7);

        assert_eq!(
            builder.try_register::<TestSecurityModule<8>>().unwrap(),
            ModuleId(7)
        );
        assert_eq!(builder.modules().len(), SECURITY_MODULE_LIMIT);
        assert_eq!(
            builder.try_register::<TestSecurityModule<9>>(),
            Err(RegistryBuildError::Capacity)
        );
        assert_eq!(builder.modules().len(), SECURITY_MODULE_LIMIT);
    }

    #[test]
    fn registry_registration_rejects_duplicate_and_reserved_keys_before_init() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original_len = builder.modules().len();

        assert_eq!(
            builder.try_register::<TestSecurityModule<2>>(),
            Err(RegistryBuildError::DuplicateModule)
        );
        assert_eq!(builder.modules().len(), original_len);

        RESERVED_MODULE_INIT_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            builder.try_register::<ReservedKeyModule>(),
            Err(RegistryBuildError::ReservedModuleKey)
        );
        assert_eq!(RESERVED_MODULE_INIT_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(builder.modules().len(), original_len);
    }

    #[test]
    fn registry_module_init_failure_leaves_builder_unchanged() {
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original = builder
            .modules()
            .iter()
            .map(|module| (module.id, module.key))
            .collect::<Vec<_>>();

        assert_eq!(
            builder.try_register::<FailingModule<3>>(),
            Err(RegistryBuildError::ModuleInitFailed)
        );
        assert_eq!(
            builder
                .modules()
                .iter()
                .map(|module| (module.id, module.key))
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn registry_module_allocation_failure_drops_candidate_without_mutation() {
        MODULE_DROP_TRACE.store(0, Ordering::SeqCst);
        let mut builder = test_registry_builder();
        builder.try_register::<TestSecurityModule<2>>().unwrap();
        let original = builder
            .modules()
            .iter()
            .map(|module| (module.id, module.key))
            .collect::<Vec<_>>();

        assert_eq!(
            builder.try_register_with_allocator::<DroppingModule<4>, _>(|module| {
                drop(module);
                Err(RegistryBuildError::NoMemory)
            }),
            Err(RegistryBuildError::NoMemory)
        );
        assert_eq!(MODULE_DROP_TRACE.load(Ordering::SeqCst), 4);
        assert_eq!(
            builder
                .modules()
                .iter()
                .map(|module| (module.id, module.key))
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn registry_build_rollback_drops_initialized_modules_in_reverse_order() {
        MODULE_DROP_TRACE.store(0, Ordering::SeqCst);
        {
            let mut builder = test_registry_builder();
            builder.try_register::<DroppingModule<2>>().unwrap();
            builder.try_register::<DroppingModule<3>>().unwrap();
            assert_eq!(
                builder.try_register::<FailingModule<4>>(),
                Err(RegistryBuildError::ModuleInitFailed)
            );
        }
        assert_eq!(MODULE_DROP_TRACE.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn commoncap_init_failure_cannot_produce_a_freezable_registry() {
        let builder = SecurityRegistryBuilder::<NeedsCommoncap>::try_new().unwrap();
        assert!(matches!(
            builder.try_register_commoncap_with(|| Err(RegistryBuildError::ModuleInitFailed)),
            Err(RegistryBuildError::ModuleInitFailed)
        ));
    }

    #[test]
    fn frozen_registry_publication_is_one_shot() {
        let publication = SecurityRegistryPublication::new();
        let builds = AtomicU32::new(0);
        assert!(publication.get().is_none());
        assert!(matches!(
            require_published_registry(publication.get()),
            Err(AxError::OperationNotPermitted)
        ));

        let first = publication.try_publish_with(|| {
            builds.fetch_add(1, Ordering::SeqCst);
            Err(RegistryBuildError::ModuleInitFailed)
        });
        assert!(matches!(first, Err(RegistryBuildError::ModuleInitFailed)));
        assert!(publication.get().is_none());

        let first = publication
            .try_publish_with(|| {
                builds.fetch_add(1, Ordering::SeqCst);
                try_build_builtin_registry()
            })
            .unwrap();
        assert!(require_published_registry(publication.get()).is_ok());
        assert!(core::ptr::eq(publication.get().unwrap(), first));
        assert!(matches!(
            publication.try_publish_with(|| {
                builds.fetch_add(1, Ordering::SeqCst);
                try_build_builtin_registry()
            }),
            Err(RegistryBuildError::AlreadyPublished)
        ));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(core::ptr::eq(publication.get().unwrap(), first));
    }

    #[test]
    fn concurrent_registry_publishers_run_exactly_one_builder() {
        let publication = Arc::new(SecurityRegistryPublication::new());
        let builds = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(Barrier::new(3));
        let mut publishers = Vec::new();

        for _ in 0..2 {
            let publication = publication.clone();
            let builds = builds.clone();
            let barrier = barrier.clone();
            publishers.push(thread::spawn(move || {
                barrier.wait();
                match publication.try_publish_with(|| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    try_build_builtin_registry()
                }) {
                    Ok(_) => true,
                    Err(RegistryBuildError::AlreadyPublished) => false,
                    Err(error) => panic!("unexpected publication error: {error}"),
                }
            }));
        }

        barrier.wait();
        let winners = publishers
            .into_iter()
            .map(|publisher| publisher.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(publication.get().is_some());
    }

    #[test]
    fn inode_and_file_contexts_bind_exact_actor_dac_owner_and_frozen_object() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let child_namespace = namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let location = security_test_inode();
        let mut metadata = location.metadata().unwrap();
        metadata.mode = NodePermission::from_bits_truncate(0o6754);
        metadata.uid = 1001;
        metadata.gid = 1002;
        metadata.size = 0x1234_5678;
        let expected_device = metadata.device;
        let expected_inode = metadata.inode;
        let expected_mount_id = location.mountpoint().mount_id();
        let object = InodeSecurityRef::new(&location, &metadata);
        metadata.mode = NodePermission::empty();
        metadata.uid = 0;
        metadata.gid = 0;
        metadata.size = 0;

        let dac_credential = security_test_dac(2001, 2002);
        let owner_user_ns = initial_user_namespace(&child_namespace);
        assert!(Arc::ptr_eq(&owner_user_ns, &namespace));
        let inode = InodePermissionSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &object,
            InodePermissionAccess::READ | InodePermissionAccess::EXECUTE,
        );
        assert!(core::ptr::eq(inode.actor(), credential.as_ref()));
        assert!(core::ptr::eq(inode.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(inode.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(inode.target_object(), &object));
        assert!(core::ptr::eq(inode.core().actor(), credential.core()));
        assert_eq!(inode.core().access(), inode.access());
        assert_eq!(object.identity().mount_id(), expected_mount_id);
        assert_eq!(object.identity().device(), expected_device);
        assert_eq!(object.identity().inode(), expected_inode);
        assert_eq!(object.mode(), 0o6754);
        assert_eq!(object.node_kind(), NodeType::RegularFile);
        assert_eq!(object.uid(), 1001);
        assert_eq!(object.gid(), 1002);
        assert_eq!(object.size(), 0x1234_5678);

        let operation =
            FileOpenOperation::new(FileOpenAccess::ReadWrite, true, true, true, false).unwrap();
        let open = FileOpenSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &object,
            operation,
        );
        assert!(core::ptr::eq(open.actor(), credential.as_ref()));
        assert!(core::ptr::eq(open.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(open.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(open.target_object(), &object));
        assert!(core::ptr::eq(open.core().actor(), credential.core()));
        assert_eq!(open.core().operation(), open.operation());
        assert_eq!(open.operation(), operation);
    }

    #[test]
    fn whole_module_registration_is_atomic_across_every_hook_family() {
        let mut builder = test_registry_builder();
        builder.try_register::<WholeHookModule>().unwrap();
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(registry);
        assert_eq!(
            WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst),
            0x0101_0101_0101_0101
        );

        let mut builder = test_registry_builder();
        assert_eq!(
            builder.try_register::<FailingWholeHookModule>(),
            Err(RegistryBuildError::ModuleInitFailed)
        );
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(registry);
        assert_eq!(WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inode_and_file_hook_stacks_order_and_short_circuit_denials() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let location = security_test_inode();
        let metadata = location.metadata().unwrap();
        let object = InodeSecurityRef::new(&location, &metadata);
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let inode = InodePermissionSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &object,
            InodePermissionAccess::READ,
        );
        let open = FileOpenSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &object,
            FileOpenOperation::new(FileOpenAccess::Read, false, false, false, false).unwrap(),
        );

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_permission: Some(ordered_inode_first),
                file_open: Some(ordered_file_open_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_permission: Some(ordered_inode_second),
                file_open: Some(ordered_file_open_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_HOOK_TRACE.store(0, Ordering::SeqCst);
        FILE_OPEN_HOOK_TRACE.store(0, Ordering::SeqCst);
        registry.dispatch_inode_permission(&inode).unwrap();
        registry.dispatch_file_open(&open).unwrap();
        assert_eq!(INODE_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(FILE_OPEN_HOOK_TRACE.load(Ordering::SeqCst), 2);

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_permission: Some(deny_inode_first),
                file_open: Some(deny_file_open_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_permission: Some(inode_must_not_run),
                file_open: Some(file_open_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_HOOK_TRACE.store(0, Ordering::SeqCst);
        FILE_OPEN_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_inode_permission(&inode),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_file_open(&open),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(INODE_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(FILE_OPEN_HOOK_TRACE.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn security_hook_stack_runs_in_declaration_order() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = PtraceAccessContext::new(
            &credential,
            &credential,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_access: Some(ordered_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_access: Some(ordered_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        ORDER_HOOK_TRACE.store(0, Ordering::SeqCst);
        registry.dispatch_ptrace_access(&context).unwrap();
        assert_eq!(ORDER_HOOK_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = access_context(
            &credential,
            &credential,
            &image_ref,
            PtraceCredentialKind::Real,
        );
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_access: Some(deny_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_access: Some(must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_ptrace_access(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exec_security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace).unwrap();
        let draft = exec_draft(
            &credential,
            crate::task::ExecTraceState::NotSuppressingPrivilege,
        );
        let context = ExecCredentialSecurityContext::new(&draft);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                exec_credential: Some(deny_exec_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                exec_credential: Some(exec_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        EXEC_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_exec_credential(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(EXEC_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn traceme_security_hook_stack_short_circuits_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context =
            PtraceTracemeContext::new(&root, &root, image_ref.owner_user_ns(), &image_ref);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_traceme: Some(deny_traceme_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                ptrace_traceme: Some(traceme_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        TRACEME_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_ptrace_traceme(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(TRACEME_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn credential_caller_production_exec_commoncap_accepts_valid_external_proposal() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let unprivileged = credential_with_caps(&root, &[], &[]);
        let draft = exec_draft(
            &unprivileged,
            crate::task::ExecTraceState::SuppressingPrivilege,
        );
        let context = ExecCredentialSecurityContext::new(&draft);

        dispatch_exec_credential(&context).unwrap();
    }

    #[test]
    fn commoncap_selects_effective_caps_for_fs_and_permitted_for_real() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let actor = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let target = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);

        dispatch_ptrace_access(&access_context(
            &actor,
            &target,
            &image_ref,
            PtraceCredentialKind::Real,
        ))
        .unwrap();
        assert_eq!(
            dispatch_ptrace_access(&access_context(
                &actor,
                &target,
                &image_ref,
                PtraceCredentialKind::Fs,
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn traceme_treats_parent_as_actor_and_child_as_target() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace.clone()).unwrap();
        let parent = credential_with_caps(&root, &[], &[]);
        let child_slot = CredentialSlot::new(credential_with_caps(&root, &[CAP_CHOWN], &[]));
        let mut child_update = child_slot.prepare();
        let child_uid = Kuid::from_raw(1000).unwrap();
        let child_gid = Kgid::from_raw(1000).unwrap();
        child_update.builder.ids.ruid = child_uid;
        child_update.builder.ids.euid = child_uid;
        child_update.builder.ids.suid = child_uid;
        child_update.builder.ids.fsuid = child_uid;
        child_update.builder.ids.rgid = child_gid;
        child_update.builder.ids.egid = child_gid;
        child_update.builder.ids.sgid = child_gid;
        child_update.builder.ids.fsgid = child_gid;
        let child = child_update.finish().unwrap().commit();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context =
            PtraceTracemeContext::new(&parent, &child, image_ref.owner_user_ns(), &image_ref);

        // Reversing actor and target would incorrectly allow this relation:
        // the child's CAP_CHOWN set contains the empty parent set.
        assert_eq!(
            dispatch_ptrace_traceme(&context),
            Err(AxError::OperationNotPermitted)
        );

        let allowed_context =
            PtraceTracemeContext::new(&root, &child, image_ref.owner_user_ns(), &image_ref);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                ptrace_traceme: Some(record_traceme_direction),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        TRACEME_DIRECTION.store(0, Ordering::SeqCst);
        registry.dispatch_ptrace_traceme(&allowed_context).unwrap();
        assert_eq!(TRACEME_DIRECTION.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn commoncap_honors_namespaced_cap_sys_ptrace() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(1000).unwrap(),
                false,
            )
            .unwrap();
        let target_parent = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target =
            Cred::try_with_user_namespace(&target_parent, child_namespace.clone()).unwrap();
        let actor = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[CAP_SYS_PTRACE]);
        let unprivileged_actor = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[]);
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&child_namespace, &image);

        dispatch_ptrace_access(&access_context(
            &actor,
            &target,
            &image_ref,
            PtraceCredentialKind::Real,
        ))
        .unwrap();
        assert_eq!(
            dispatch_ptrace_access(&access_context(
                &unprivileged_actor,
                &target,
                &image_ref,
                PtraceCredentialKind::Real,
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn image_security_ref_keeps_mm_owner_distinct_from_credential_namespace() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let target = Cred::try_with_user_namespace(&root, child_namespace).unwrap();
        let first_image = Arc::new(());
        let second_image = Arc::new(());
        let first = ProcessImageSecurityRef::new(&root_namespace, &first_image);
        let second = ProcessImageSecurityRef::new(&root_namespace, &second_image);

        assert!(Arc::ptr_eq(first.owner_user_ns(), &root_namespace));
        assert!(!Arc::ptr_eq(first.owner_user_ns(), target.user_ns()));
        assert_ne!(first.identity(), second.identity());
    }

    #[test]
    fn credential_caller_scheduler_child_cannot_administer_ancestor() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(1000).unwrap(),
                false,
            )
            .unwrap();
        let child_parent = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let child_root = Cred::try_with_user_namespace(&child_parent, child_namespace).unwrap();
        let actor =
            credential_with_identity_and_caps(&child_root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);

        for operation in [
            SchedulerSecurityOperation::SetAffinity,
            SchedulerSecurityOperation::SetParam { realtime: false },
        ] {
            assert_eq!(
                dispatch_scheduler(&scheduler_context(&actor, &root, operation)),
                Err(AxError::OperationNotPermitted)
            );
        }
    }

    #[test]
    fn credential_caller_scheduler_capable_ancestor_administers_child() {
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, false)
            .unwrap();
        let child_root = Cred::try_with_user_namespace(&actor, child_namespace).unwrap();
        let target = credential_with_identity_and_caps(&child_root, 1000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetParam { realtime: true },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_uid_zero_with_dropped_cap_cannot_enter_rt() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let dropped = credential_with_caps(&root, &[], &[]);

        assert_eq!(
            dispatch_scheduler(&scheduler_context(
                &dropped,
                &root,
                SchedulerSecurityOperation::SetPolicy { realtime: true },
            )),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn credential_caller_scheduler_nonroot_capability_crosses_owner_boundary() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor =
            credential_with_identity_and_caps(&root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: -20,
                rlimit_nice: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_nice_uses_owner_and_frozen_rlimit() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: -5,
                rlimit_nice: 25,
            },
        ))
        .unwrap();
        assert_eq!(
            dispatch_scheduler(&scheduler_context(
                &actor,
                &target,
                SchedulerSecurityOperation::SetNice {
                    current_nice: 0,
                    requested_nice: -5,
                    rlimit_nice: 24,
                },
            )),
            Err(AxError::PermissionDenied)
        );
        dispatch_scheduler(&scheduler_context(
            &actor,
            &target,
            SchedulerSecurityOperation::SetNice {
                current_nice: 0,
                requested_nice: 5,
                rlimit_nice: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn credential_caller_scheduler_context_keeps_exact_snapshots() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let old_actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let old_target = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        let actor_slot = CredentialSlot::new(old_actor.clone());
        let target_slot = CredentialSlot::new(old_target.clone());
        let context = scheduler_context(
            &old_actor,
            &old_target,
            SchedulerSecurityOperation::SetAffinity,
        );

        let mut actor_update = actor_slot.prepare();
        actor_update.builder.caps.permitted = capability_set(&[CAP_SYS_NICE]);
        actor_update.builder.caps.effective = capability_set(&[CAP_SYS_NICE]);
        actor_update.finish().unwrap().commit();

        let mut target_update = target_slot.prepare();
        let actor_uid = Kuid::from_raw(1000).unwrap();
        target_update.builder.ids.ruid = actor_uid;
        target_update.builder.ids.euid = actor_uid;
        target_update.finish().unwrap().commit();

        assert_eq!(context.actor().ids().euid, Kuid::from_raw(1000).unwrap());
        assert_eq!(context.target().ids().euid, Kuid::from_raw(2000).unwrap());
        assert!(!context.owner_match());
        assert_eq!(
            dispatch_scheduler(&context),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn credential_caller_scheduler_hooks_stop_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let context = scheduler_context(&root, &root, SchedulerSecurityOperation::SetAffinity);
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                scheduler: Some(deny_scheduler_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                scheduler: Some(scheduler_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        SCHEDULER_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_scheduler(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(SCHEDULER_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn signal_policy_hooks_run_after_core_allow_and_stop_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let owner = Arc::new(());
        let target = SignalTargetSecurityRef::new(&owner, 91, 91, SignalTargetKind::Zombie);
        let context = SecuritySignalContext::authorize(
            &root,
            &root,
            &target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            context.core_reason(),
            SignalCoreAuthorizationReason::CredentialMatch
        );
        assert!(core::ptr::eq(context.actor(), root.as_ref()));
        assert!(core::ptr::eq(context.target(), root.as_ref()));
        assert_eq!(context.target_object().stable_id(), 91);
        assert_eq!(context.target_object().visible_id(), 91);
        assert!(context.target_object().owner_matches(&owner));
        assert!(!context.target_object().owner_matches(&Arc::new(())));

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                signal: Some(deny_signal_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                signal: Some(signal_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();

        SIGNAL_DENY_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_signal(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(SIGNAL_DENY_HOOK_TRACE.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn denied_signal_core_never_constructs_a_policy_context() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        let owner = Arc::new(());
        let target_object = SignalTargetSecurityRef::new(&owner, 7, 7, SignalTargetKind::Process);

        assert_eq!(
            SecuritySignalContext::authorize(
                &actor,
                &target,
                &target_object,
                SignalSecurityOperation::probe(
                    SignalSecuritySource::Kill,
                    SignalDeliveryScope::ThreadGroup,
                ),
                false,
                false,
            )
            .err(),
            Some(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn composite_root_initializes_and_reverse_drops_every_module_state() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();

        assert_eq!(CRED_STATE_INIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(credential.security().slots.len(), 3);
        drop(credential);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn initial_state_failure_reverse_rolls_back_without_a_credential() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        CRED_STATE_FAIL_INIT_KEY.store(3, Ordering::SeqCst);
        let namespace = UserNamespace::try_new_root().unwrap();

        assert_eq!(
            Cred::try_root_with_registry(registry, namespace).err(),
            Some(AxError::NoMemory)
        );
        assert_eq!(CRED_STATE_INIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn credential_state_vector_reservation_failure_is_zero_effect() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();

        assert!(matches!(
            registry
                .registry()
                .try_empty_credential_state_with_reservation(registry, usize::MAX),
            Err(AxError::NoMemory)
        ));
        assert_eq!(CRED_STATE_INIT_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn credential_state_owner_allocation_failure_frees_typed_candidate() {
        let _probe_guard = reset_credential_state_probes();
        let module = Arc::new(CredentialStateProbeModule::<2>);
        let state = ProbeCredentialState {
            key: 2,
            generation: 0,
            committed: AtomicBool::new(false),
        };

        assert!(matches!(
            try_own_credential_state_with(module, state, |_| Err(AxError::NoMemory)),
            Err(AxError::NoMemory)
        ));
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn outer_credential_allocation_failure_reverse_drops_complete_state() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let core = CoreCred::try_root(namespace).unwrap();
        let security = registry.try_init_credential_state(&core).unwrap();
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);

        assert!(matches!(
            Cred::try_from_prepared_parts_with_allocator(core, security, |_| {
                Err(AxError::NoMemory)
            }),
            Err(AxError::NoMemory)
        ));
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn module_prepare_failure_reverse_rolls_back_and_preserves_exact_old() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        CRED_STATE_PREPARE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_FAIL_PREPARE_KEY.store(3, Ordering::SeqCst);

        assert_eq!(
            Cred::try_clone_for_fork(&old).err(),
            Some(AxError::NoMemory)
        );
        assert_eq!(CRED_STATE_PREPARE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_TRANSITION_MASK.load(Ordering::SeqCst), 1);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(old.ids().euid, Kuid::INITIAL_ROOT);

        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);
        drop(old);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
    }

    #[test]
    fn module_authorization_denial_drops_complete_candidate_in_reverse() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        CRED_STATE_PREPARE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_AUTHORIZE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_DENY_KEY.store(2, Ordering::SeqCst);

        assert_eq!(
            Cred::try_clone_for_fork(&old).err(),
            Some(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_PREPARE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_AUTHORIZE_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        assert_eq!(old.ids().euid, Kuid::INITIAL_ROOT);
    }

    #[test]
    fn state_aware_dispatch_uses_exact_layout_and_typed_slots() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let inode_location = security_test_inode();
        let inode_metadata = inode_location.metadata().unwrap();
        let inode_object = InodeSecurityRef::new(&inode_location, &inode_metadata);
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let inode_permission = InodePermissionSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            InodePermissionAccess::READ,
        );
        let file_open = FileOpenSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            FileOpenOperation::new(FileOpenAccess::Read, false, false, false, false).unwrap(),
        );
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = PtraceAccessContext::new(
            &credential,
            &credential,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let traceme = PtraceTracemeContext::new(
            &credential,
            &credential,
            image_ref.owner_user_ns(),
            &image_ref,
        );
        let scheduler = SecuritySchedulerContext::new(
            &credential,
            &credential,
            SchedulerSecurityOperation::SetAffinity,
        );
        let draft = exec_draft(
            &credential,
            crate::task::ExecTraceState::NotSuppressingPrivilege,
        );
        let exec = ExecCredentialSecurityContext::new(&draft);
        let executable = ExecExecutableSecurityContext::new(&credential, draft.source());
        let signal_target = SignalTargetSecurityRef::new(&image, 44, 44, SignalTargetKind::Process);
        let signal = SecuritySignalContext::authorize(
            &credential,
            &credential,
            &signal_target,
            SignalSecurityOperation::send(
                SignalNumber::try_new(15).unwrap(),
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            true,
            false,
        )
        .unwrap();
        CRED_STATE_DISPATCH_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_HOOK_MASK.store(0, Ordering::SeqCst);

        dispatch_inode_permission(&inode_permission).unwrap();
        dispatch_file_open(&file_open).unwrap();
        dispatch_ptrace_access(&context).unwrap();
        dispatch_ptrace_traceme(&traceme).unwrap();
        dispatch_scheduler(&scheduler).unwrap();
        dispatch_exec_credential(&exec).unwrap();
        dispatch_exec_executable(&executable).unwrap();
        dispatch_signal(&signal).unwrap();
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_PERMISSION_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_FILE_OPEN_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_EXECUTABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0xff);
    }

    #[test]
    fn namespace_and_exec_use_distinct_state_prepare_contracts() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let ids = old.ids();
        let child_namespace = old.user_ns().try_fork(ids.euid, ids.egid, false).unwrap();
        CRED_STATE_TRANSITION_MASK.store(0, Ordering::SeqCst);

        let child = Cred::try_with_user_namespace(&old, child_namespace).unwrap();
        assert_eq!(CRED_STATE_TRANSITION_MASK.load(Ordering::SeqCst), 1 << 2);
        drop(child);

        CRED_STATE_TRANSITION_MASK.store(0, Ordering::SeqCst);
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        assert_eq!(CRED_STATE_TRANSITION_MASK.load(Ordering::SeqCst), 1 << 3);
        drop(draft);
    }

    #[test]
    fn exec_hook_denial_releases_all_proposed_states_and_keeps_old() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_EXEC_DENY_KEY.store(2, Ordering::SeqCst);
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        {
            let context = ExecCredentialSecurityContext::new(&draft);
            assert_eq!(
                dispatch_exec_credential(&context),
                Err(AxError::PermissionDenied)
            );
        }
        assert_eq!(CRED_STATE_EXEC_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);
        drop(draft);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        assert_eq!(old.ids().euid, Kuid::INITIAL_ROOT);
    }

    #[test]
    fn executable_component_hook_denial_short_circuits_in_registry_order() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let draft = exec_draft(&actor, crate::task::ExecTraceState::NotSuppressingPrivilege);
        let context = ExecExecutableSecurityContext::new(&actor, draft.source());
        CRED_STATE_EXECUTABLE_DENY_KEY.store(2, Ordering::SeqCst);

        assert_eq!(
            dispatch_exec_executable(&context),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_EXECUTABLE_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(CRED_STATE_EXEC_COMMITTING_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_EXEC_COMMITTED_TRACE.load(Ordering::SeqCst), 0);
        assert!(core::ptr::eq(context.actor(), actor.as_ref()));
    }

    #[test]
    fn executable_component_roles_preserve_exec_chain_order() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();

        for role in [
            crate::task::ExecExecutableRole::Requested,
            crate::task::ExecExecutableRole::ScriptInterpreter,
            crate::task::ExecExecutableRole::DynamicLinker,
        ] {
            let executable = crate::task::ExecFileSecurityObject::new(
                ExecFileIdentity::new(17, 23),
                namespace.clone(),
                Some(crate::task::ExecFileOwner::new(
                    Kuid::INITIAL_ROOT,
                    Kgid::INITIAL_ROOT,
                )),
                0o755,
                true,
                role,
            );
            dispatch_exec_executable(&ExecExecutableSecurityContext::new(&actor, &executable))
                .unwrap();
        }

        assert_eq!(CRED_STATE_EXECUTABLE_ROLE_TRACE.load(Ordering::SeqCst), 123);
        assert_eq!(CRED_STATE_EXECUTABLE_TRACE.load(Ordering::SeqCst), 232_323);
    }

    #[test]
    fn foreign_layout_dispatch_fails_before_any_module_hook() {
        let _probe_guard = reset_credential_state_probes();
        let actor_registry = probe_registry();
        let target_registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(actor_registry, namespace.clone()).unwrap();
        let target = Cred::try_root_with_registry(target_registry, namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = PtraceAccessContext::new(
            &actor,
            &target,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let signal_target = SignalTargetSecurityRef::new(&image, 31, 31, SignalTargetKind::Process);
        let signal = SecuritySignalContext::authorize(
            &actor,
            &target,
            &signal_target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            false,
            false,
        )
        .unwrap();
        CRED_STATE_DISPATCH_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_HOOK_MASK.store(0, Ordering::SeqCst);

        assert_eq!(
            dispatch_ptrace_access(&context),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(
            dispatch_signal(&signal),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn malformed_module_index_dispatch_fails_closed_before_hooks() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let core = actor.core_arc().clone();
        let mut malformed_security = registry.try_init_credential_state(&core).unwrap();
        malformed_security.slots[2].module_id = ModuleId(7);
        let malformed = Cred::try_from_prepared_parts(core, malformed_security).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);
        let context = PtraceAccessContext::new(
            &actor,
            &malformed,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let signal_target = SignalTargetSecurityRef::new(&image, 32, 32, SignalTargetKind::Process);
        let signal = SecuritySignalContext::authorize(
            &actor,
            &malformed,
            &signal_target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            false,
            false,
        )
        .unwrap();
        CRED_STATE_DISPATCH_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_HOOK_MASK.store(0, Ordering::SeqCst);

        assert_eq!(
            dispatch_ptrace_access(&context),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(
            dispatch_signal(&signal),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn wrong_state_type_or_runtime_fails_preflight_before_hooks() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let image = Arc::new(());
        let image_ref = ProcessImageSecurityRef::new(&namespace, &image);

        let mut wrong_type_security = registry.try_init_credential_state(actor.core()).unwrap();
        wrong_type_security.slots[1].erased = try_own_credential_state(
            Arc::new(CredentialStateProbeModule::<3>),
            ProbeCredentialState {
                key: 3,
                generation: 0,
                committed: AtomicBool::new(true),
            },
        )
        .unwrap();
        let wrong_type =
            Cred::try_from_prepared_parts(actor.core_arc().clone(), wrong_type_security).unwrap();
        let context = PtraceAccessContext::new(
            &actor,
            &wrong_type,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let signal_target = SignalTargetSecurityRef::new(&image, 33, 33, SignalTargetKind::Process);
        let wrong_type_signal = SecuritySignalContext::authorize(
            &actor,
            &wrong_type,
            &signal_target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            false,
            false,
        )
        .unwrap();
        CRED_STATE_DISPATCH_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_HOOK_MASK.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_ptrace_access(&context),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(
            dispatch_signal(&wrong_type_signal),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0);

        let mut wrong_runtime_security = registry.try_init_credential_state(actor.core()).unwrap();
        wrong_runtime_security.slots[1].erased = try_own_credential_state(
            Arc::new(CredentialStateProbeModule::<2>),
            ProbeCredentialState {
                key: 2,
                generation: 0,
                committed: AtomicBool::new(true),
            },
        )
        .unwrap();
        let wrong_runtime =
            Cred::try_from_prepared_parts(actor.core_arc().clone(), wrong_runtime_security)
                .unwrap();
        let context = PtraceAccessContext::new(
            &actor,
            &wrong_runtime,
            image_ref.owner_user_ns(),
            &image_ref,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let wrong_runtime_signal = SecuritySignalContext::authorize(
            &actor,
            &wrong_runtime,
            &signal_target,
            SignalSecurityOperation::probe(
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            dispatch_ptrace_access(&context),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(
            dispatch_signal(&wrong_runtime_signal),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inode_and_file_dispatch_fail_closed_on_wrong_actor_state() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let mut malformed_security = registry.try_init_credential_state(actor.core()).unwrap();
        malformed_security.slots[1].erased = try_own_credential_state(
            Arc::new(CredentialStateProbeModule::<3>),
            ProbeCredentialState {
                key: 3,
                generation: 0,
                committed: AtomicBool::new(true),
            },
        )
        .unwrap();
        let malformed =
            Cred::try_from_prepared_parts(actor.core_arc().clone(), malformed_security).unwrap();
        let location = security_test_inode();
        let metadata = location.metadata().unwrap();
        let object = InodeSecurityRef::new(&location, &metadata);
        let dac_credential = malformed.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let inode = InodePermissionSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &object,
            InodePermissionAccess::WRITE,
        );
        let open = FileOpenSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &object,
            FileOpenOperation::new(FileOpenAccess::Write, false, false, false, false).unwrap(),
        );
        CRED_STATE_INODE_PERMISSION_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_FILE_OPEN_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_HOOK_MASK.store(0, Ordering::SeqCst);

        assert_eq!(
            dispatch_inode_permission(&inode),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_file_open(&open),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_INODE_PERMISSION_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_FILE_OPEN_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ordinary_post_commit_notifies_once_in_order_before_retirement() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());
        CRED_STATE_COMMIT_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);

        let mut update = slot.prepare();
        update.builder.ids.ruid = Kuid::from_raw(1000).unwrap();
        let prepared = update.finish().unwrap();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        let publication = prepared.publish();
        assert_eq!(slot.current().ids().ruid, Kuid::from_raw(1000).unwrap());
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        let (new, retirement) = publication.complete_post_commit();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_COMMIT_TRANSITION_MASK.load(Ordering::SeqCst),
            1 << 1
        );
        assert_eq!(CRED_STATE_COMMIT_GENERATION_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(CRED_STATE_COMMIT_OLD_UID.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_COMMIT_NEW_UID.load(Ordering::SeqCst), 1000);
        assert_eq!(CRED_STATE_DROP_AT_COMMIT.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(&slot.current(), &new));

        drop(old);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
        drop(retirement);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        drop(new);
        drop(slot);
    }

    #[test]
    fn exec_post_commit_notifies_once_with_exec_transition() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());
        CRED_STATE_COMMIT_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);

        let update = slot.prepare();
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        dispatch_exec_credential(&ExecCredentialSecurityContext::new(&draft)).unwrap();
        let prepared = update.finish_exec_draft(draft).unwrap();
        let publication = prepared.publish();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        let (new, retirement) = publication.complete_post_commit();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_COMMIT_TRANSITION_MASK.load(Ordering::SeqCst),
            1 << 3
        );
        assert_eq!(CRED_STATE_COMMIT_GENERATION_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(CRED_STATE_DROP_AT_COMMIT.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);

        drop(old);
        drop(retirement);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        drop(new);
        drop(slot);
    }

    #[test]
    fn exec_lifecycle_notifies_committing_then_generic_then_full_committed() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());
        let update = slot.prepare();
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        dispatch_exec_credential(&ExecCredentialSecurityContext::new(&draft)).unwrap();
        let source = draft.source().clone();
        let effects = draft.proposal().effects();
        let prepared = update.finish_exec_draft(draft).unwrap();
        let pending = PendingExecSecurity::try_new(&prepared, source, effects).unwrap();
        let image = Arc::new(());
        let runtime = ExecCommitRuntime::new(
            41,
            43,
            41,
            ExecImageIdentity::from_arc(&image),
            old.user_ns().clone(),
        );

        let committing = pending.committing(runtime);
        assert_eq!(CRED_STATE_EXEC_COMMITTING_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_EXEC_COMMITTED_TRACE.load(Ordering::SeqCst), 0);

        let publication = prepared.publish();
        let (new, retirement) = publication.complete_post_commit();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_EXEC_COMMITTED_TRACE.load(Ordering::SeqCst), 0);

        let completed = committing.committed();
        assert_eq!(CRED_STATE_EXEC_COMMITTED_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
        drop(old);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
        drop(retirement);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 0);
        drop(completed);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        drop(new);
        drop(slot);
    }

    #[test]
    fn aborting_a_prepared_exec_emits_no_commit_phase_notification() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());
        let update = slot.prepare();
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        let source = draft.source().clone();
        let effects = draft.proposal().effects();
        let prepared = update.finish_exec_draft(draft).unwrap();
        let pending = PendingExecSecurity::try_new(&prepared, source, effects).unwrap();

        drop(pending);
        drop(prepared);
        assert_eq!(CRED_STATE_EXEC_COMMITTING_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_EXEC_COMMITTED_TRACE.load(Ordering::SeqCst), 0);
        assert!(Arc::ptr_eq(&old, &slot.current()));
    }

    #[test]
    #[should_panic(expected = "committing exec dropped without committed security notification")]
    fn dropping_an_armed_exec_commit_token_fails_stop() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());
        let update = slot.prepare();
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        let source = draft.source().clone();
        let effects = draft.proposal().effects();
        let prepared = update.finish_exec_draft(draft).unwrap();
        let pending = PendingExecSecurity::try_new(&prepared, source, effects).unwrap();
        let image = Arc::new(());
        let runtime = ExecCommitRuntime::new(
            41,
            43,
            41,
            ExecImageIdentity::from_arc(&image),
            old.user_ns().clone(),
        );
        drop(pending.committing(runtime));
    }

    #[test]
    fn failed_or_aborted_replacements_emit_no_post_commit_notification() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old.clone());

        CRED_STATE_FAIL_PREPARE_KEY.store(3, Ordering::SeqCst);
        assert_eq!(slot.prepare().finish().err(), Some(AxError::NoMemory));
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        CRED_STATE_FAIL_PREPARE_KEY.store(0, Ordering::SeqCst);
        CRED_STATE_DENY_KEY.store(2, Ordering::SeqCst);
        assert_eq!(
            slot.prepare().finish().err(),
            Some(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        CRED_STATE_DENY_KEY.store(0, Ordering::SeqCst);
        drop(slot.prepare().finish().unwrap());
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        let update = slot.prepare();
        let draft = exec_draft(&old, crate::task::ExecTraceState::NotSuppressingPrivilege);
        drop(update.finish_exec_draft(draft).unwrap());
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn malformed_late_post_commit_slot_fails_before_any_notification() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let core = old.core_arc().clone();
        let mut malformed_security = registry.try_init_credential_state(&core).unwrap();
        malformed_security.slots[2].module_id = ModuleId(7);
        let malformed = Cred::try_from_prepared_parts(core, malformed_security).unwrap();

        assert!(matches!(
            PendingCredentialPostCommit::try_new(
                &old,
                &malformed,
                CredentialStateTransition::Normal,
            ),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(matches!(
            PendingCredentialPostCommit::try_new(&old, &old, CredentialStateTransition::Fork,),
            Err(AxError::BadState)
        ));
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[should_panic(expected = "published credential dropped without post-commit notification")]
    fn dropping_a_published_pending_notification_fails_stop() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old);
        let publication = slot.prepare().finish().unwrap().publish();
        drop(publication);
    }

    #[test]
    fn retired_module_state_is_freed_outside_publication_spin_lock() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(credential);
        CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);

        let proposed = slot.prepare().finish().unwrap().commit();
        assert_eq!(CRED_STATE_TRANSITION_MASK.load(Ordering::SeqCst), 1 << 1);
        assert_eq!(CRED_STATE_DROP_TRACE.load(Ordering::SeqCst), 32);
        assert!(!credential_publication_lock_held());
        drop(proposed);
        drop(slot);
    }

    #[test]
    fn authorization_errors_map_to_linux_errno_classes() {
        assert_eq!(
            authorization_error(AuthorizationError::NotPermitted),
            AxError::OperationNotPermitted
        );
        assert_eq!(
            authorization_error(AuthorizationError::AccessDenied),
            AxError::PermissionDenied
        );
    }
}
