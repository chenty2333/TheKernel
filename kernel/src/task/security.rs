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
use core::{
    any::Any,
    fmt,
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, Metadata, NodeType};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use axtask::AxTaskRef;
use memory_addr::VirtAddr;
use spin::Once;
use thekernel_linux_cred::{
    AuthorizationError, CapabilityNumber, FileMprotectContext, MemoryProtection,
    MmapAddressContext, MmapFileContext, MmapFileFlags, MmapFileOperation, MmapFileSecurityRef,
    MmapFileTarget, SocketAcceptContext, SocketBindContext, SocketConnectContext,
    SocketCreateContext, SocketGetOptionContext, SocketGetPeerNameContext,
    SocketGetSockNameContext, SocketListenContext, SocketPairContext, SocketPostCreateContext,
    SocketReceiveMessageContext, SocketSendMessageContext, SocketSetOptionContext,
    SocketShutdownContext, UnixMaySendContext, UnixStreamConnectContext,
    authorize_capability_core as external_authorize_capability_core,
    authorize_prepared_credential_capability_core as external_authorize_prepared_credential_capability_core,
    authorize_signal_core as external_authorize_signal_core,
    commoncap_ptrace_access as external_commoncap_ptrace_access,
    commoncap_ptrace_traceme as external_commoncap_ptrace_traceme,
    commoncap_scheduler as external_commoncap_scheduler,
};
pub(crate) use thekernel_linux_cred::{
    CapabilitySecurityOperation, CredentialPublicationOperation, FileOpenAccess, FileOpenOperation,
    InodeChmodIntent, InodeCreateMode, InodeMknodKind, InodeMknodOperation, InodePermissionAccess,
    InodeSetattrIntent, InodeSetattrMode, InodeSetattrProposal, InodeXattrOperation,
    PreparedCredentialCapabilityOperation, PtraceAccessKind, PtraceCredentialKind,
    SchedulerSecurityOperation, SignalCoreAuthorizationReason, SignalDeliveryScope, SignalNumber,
    SignalSecurityOperation, SignalSecuritySource, SocketCreateSpec, SocketListenBacklog,
    SocketOption, XattrSetFlags, XattrValueClass,
};

use super::{
    ExecCommitRuntime, ExecCredentialSecurityContext, ExecFileSecurityObject, UserNamespace,
    creds::{Cred, DacCredentialView, PreparedCred},
    exec_cred::{ExecCredentialEffects, authorize_commoncap_exec},
};
use crate::{
    file::{
        AcceptedSocketSecurityRef, File, FileHandle, PreparedSocketAddress, PreparedSocketMessage,
        SocketSecurityRef, UnixEndpointSecurityRef,
    },
    mm::{AddrSpace, PreparedProtectSegment},
};

const SECURITY_MODULE_LIMIT: usize = 8;
const COMMONCAP_MODULE_KEY: ModuleKey = ModuleKey(0);
const NOOP_POLICY_MODULE_KEY: ModuleKey = ModuleKey(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModuleId(u8);

type CoreCred = thekernel_linux_cred::Credential<UserNamespace>;
type CoreCapabilitySecurityContext<'a> =
    thekernel_linux_cred::CapabilitySecurityContext<'a, UserNamespace>;
type CorePreparedCredentialCapabilityContext<'a> =
    thekernel_linux_cred::PreparedCredentialCapabilityContext<'a, UserNamespace>;
type CoreCredentialPublicationContext<'a> = thekernel_linux_cred::CredentialPublicationContext<
    'a,
    UserNamespace,
    CredentialPublicationTarget,
>;
type CoreMmapFileContext<'a> = MmapFileContext<'a, UserNamespace, FileHandle<File>>;
type CoreMmapAddressContext<'a> = MmapAddressContext<'a, UserNamespace, MmapImageSecurityRef>;
type CoreFileMprotectContext<'context, 'segment> =
    FileMprotectContext<'context, UserNamespace, PreparedProtectSegment<'segment>>;

/// Opaque identity of the exact retained address-space image selected by mmap.
///
/// Security modules can compare this stable identity without receiving an
/// address-space lock or mutable MM internals. The syscall constructs and
/// dispatches this projection while retaining the source `Arc`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MmapImageSecurityRef {
    identity: usize,
}

impl MmapImageSecurityRef {
    fn from_arc<T>(image: &Arc<T>) -> Self {
        Self {
            identity: Arc::as_ptr(image).cast::<()>() as usize,
        }
    }

    pub(in crate::task) const fn identity(self) -> usize {
        self.identity
    }
}

/// Opaque identity of the exact child task whose credential became visible.
///
/// Modules receive only the stable object identity, never an `AxTaskRef` which
/// could reenter scheduler or task-table operations from an atomic callback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CredentialPublicationTarget {
    identity: usize,
}

impl CredentialPublicationTarget {
    pub(in crate::task) const fn identity(self) -> usize {
        self.identity
    }
}

/// Retained owner of a child publication target. Production uses the exact
/// prepared scheduler task; host tests may provide an inert owner with the
/// same immutable projection contract.
pub(crate) trait CredentialPublicationTargetOwner {
    fn credential_publication_target(&self) -> CredentialPublicationTarget;
}

impl CredentialPublicationTargetOwner for AxTaskRef {
    fn credential_publication_target(&self) -> CredentialPublicationTarget {
        CredentialPublicationTarget {
            identity: Arc::as_ptr(self).cast::<()>() as usize,
        }
    }
}

/// Field families changed by one ordinary credential publication.
///
/// This is derived from the exact immutable old/proposed pair rather than a
/// syscall label. A set-ID transition can therefore report both identity and
/// capability changes, while modules remain independent of ABI entry points.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::task) struct CredentialMutationKind(u8);

impl CredentialMutationKind {
    pub(in crate::task) const IDENTITIES: Self = Self(1 << 0);
    pub(in crate::task) const GROUPS: Self = Self(1 << 1);
    pub(in crate::task) const CAPABILITIES: Self = Self(1 << 2);
    pub(in crate::task) const SECUREBITS: Self = Self(1 << 3);
    pub(in crate::task) const NO_NEW_PRIVS: Self = Self(1 << 4);

    pub(in crate::task) const fn empty() -> Self {
        Self(0)
    }

    pub(in crate::task) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub(in crate::task) const fn bits(self) -> u8 {
        self.0
    }

    pub(in crate::task) const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(in crate::task) fn between(old: &CoreCred, proposed: &CoreCred) -> Self {
        let mut kind = Self::empty();
        if old.ids() != proposed.ids() {
            kind = kind.with(Self::IDENTITIES);
        }
        if old.groups().as_slice() != proposed.groups().as_slice() {
            kind = kind.with(Self::GROUPS);
        }

        let old_caps = old.capabilities();
        let proposed_caps = proposed.capabilities();
        if old_caps.effective() != proposed_caps.effective()
            || old_caps.permitted() != proposed_caps.permitted()
            || old_caps.inheritable() != proposed_caps.inheritable()
            || old_caps.bounding() != proposed_caps.bounding()
            || old_caps.ambient() != proposed_caps.ambient()
        {
            kind = kind.with(Self::CAPABILITIES);
        }
        if old_caps.securebits() != proposed_caps.securebits() {
            kind = kind.with(Self::SECUREBITS);
        }
        if old.no_new_privs() != proposed.no_new_privs() {
            kind = kind.with(Self::NO_NEW_PRIVS);
        }
        kind
    }
}

/// The way an unpublished composite credential was derived from its exact
/// immutable predecessor. Modules may use this to keep fork, namespace, exec,
/// and typed ordinary mutation state distinct without learning syscall details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::task) enum CredentialStateTransition {
    Fork,
    Mutation(CredentialMutationKind),
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

/// Exact successfully committed inode facts exposed to post-setattr policy.
///
/// This is deliberately narrower than [`InodeSecurityRef`]. The setattr
/// backend's successful publication contract makes its projected identity,
/// kind, mode, and owner outcome authoritative before the post notification.
/// The adapter must not synthesize or carry unrelated size/time facts from the
/// pre-hook snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InodeSetattrCommittedSecurityRef<'location> {
    identity: InodeIdentity,
    mode: u16,
    node_kind: NodeType,
    uid: u32,
    gid: u32,
    _location: PhantomData<&'location Location>,
}

impl<'location> InodeSetattrCommittedSecurityRef<'location> {
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
}

/// Frozen destination facts for one planned named inode creation.
///
/// The parent snapshot is retained by value together with the exact final
/// component selected by VFS. The type deliberately exposes neither a
/// [`Location`] nor a lookup operation, so policy modules cannot resample the
/// parent, restart path resolution, or substitute a different destination
/// while dispatch is in progress.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlannedInodeSecurityRef<'name, 'location> {
    parent: InodeSecurityRef<'location>,
    name: &'name str,
}

impl<'name, 'location> PlannedInodeSecurityRef<'name, 'location> {
    pub(crate) const fn new(parent: InodeSecurityRef<'location>, name: &'name str) -> Self {
        Self { parent, name }
    }

    pub(crate) const fn parent_object(&self) -> &InodeSecurityRef<'location> {
        &self.parent
    }

    pub(crate) const fn name(&self) -> &'name str {
        self.name
    }
}

/// Frozen parent, victim, and final-name facts for one existing named entry.
///
/// Both inode snapshots are retained by value and the exact final component is
/// borrowed from the caller's prepared namespace transaction. Policy modules
/// receive no [`Location`] and cannot repeat lookup, substitute a different
/// victim, or collapse the parent and target identities.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExistingInodeSecurityRef<'name, 'location> {
    parent: InodeSecurityRef<'location>,
    target: InodeSecurityRef<'location>,
    name: &'name str,
}

impl<'name, 'location> ExistingInodeSecurityRef<'name, 'location> {
    pub(crate) const fn new(
        parent: InodeSecurityRef<'location>,
        target: InodeSecurityRef<'location>,
        name: &'name str,
    ) -> Self {
        Self {
            parent,
            target,
            name,
        }
    }

    pub(crate) const fn parent_object(&self) -> &InodeSecurityRef<'location> {
        &self.parent
    }

    pub(crate) const fn target_object(&self) -> &InodeSecurityRef<'location> {
        &self.target
    }

    pub(crate) const fn name(&self) -> &'name str {
        self.name
    }
}

/// Frozen destination facts for one rename leaf hook.
///
/// The parent snapshot and exact destination name are always retained. An
/// optional target snapshot distinguishes a destination which was absent from
/// one which named an existing inode without collapsing either case into a
/// planned-create or existing-source entry type. The type exposes no VFS
/// handle, so policy cannot repeat destination lookup during dispatch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RenameDestinationSecurityRef<'name, 'location> {
    parent: InodeSecurityRef<'location>,
    target: Option<InodeSecurityRef<'location>>,
    name: &'name str,
}

impl<'name, 'location> RenameDestinationSecurityRef<'name, 'location> {
    pub(crate) const fn absent(parent: InodeSecurityRef<'location>, name: &'name str) -> Self {
        Self {
            parent,
            target: None,
            name,
        }
    }

    pub(crate) const fn existing(
        parent: InodeSecurityRef<'location>,
        target: InodeSecurityRef<'location>,
        name: &'name str,
    ) -> Self {
        Self {
            parent,
            target: Some(target),
            name,
        }
    }

    pub(crate) const fn parent_object(&self) -> &InodeSecurityRef<'location> {
        &self.parent
    }

    pub(crate) const fn target_object(&self) -> Option<&InodeSecurityRef<'location>> {
        self.target.as_ref()
    }

    pub(crate) const fn name(&self) -> &'name str {
        self.name
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
type CoreInodeXattrContext<'context, 'location> =
    thekernel_linux_cred::InodeXattrContext<'context, UserNamespace, InodeSecurityRef<'location>>;
type CoreInodeSetattrContext<'context, 'location> =
    thekernel_linux_cred::InodeSetattrContext<'context, UserNamespace, InodeSecurityRef<'location>>;
type CoreInodePostSetattrContext<'context, 'location> =
    thekernel_linux_cred::InodePostSetattrContext<
        'context,
        UserNamespace,
        InodeSetattrCommittedSecurityRef<'location>,
    >;
type CoreFileOpenContext<'context, 'location> =
    thekernel_linux_cred::FileOpenContext<'context, UserNamespace, InodeSecurityRef<'location>>;
type CoreInodeCreateContext<'context, 'name, 'location> = thekernel_linux_cred::InodeCreateContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    PlannedInodeSecurityRef<'name, 'location>,
>;
type CoreInodeMkdirContext<'context, 'name, 'location> = thekernel_linux_cred::InodeMkdirContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    PlannedInodeSecurityRef<'name, 'location>,
>;
type CoreInodeMknodContext<'context, 'name, 'location> = thekernel_linux_cred::InodeMknodContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    PlannedInodeSecurityRef<'name, 'location>,
>;
type CoreInodeSymlinkContext<'context, 'name, 'location> =
    thekernel_linux_cred::InodeSymlinkContext<
        'context,
        UserNamespace,
        InodeSecurityRef<'location>,
        PlannedInodeSecurityRef<'name, 'location>,
        str,
    >;
type CoreInodeLinkContext<'context, 'name, 'location> = thekernel_linux_cred::InodeLinkContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    InodeSecurityRef<'location>,
    PlannedInodeSecurityRef<'name, 'location>,
>;
type CoreInodeUnlinkContext<'context, 'name, 'location> = thekernel_linux_cred::InodeUnlinkContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    ExistingInodeSecurityRef<'name, 'location>,
>;
type CoreInodeRmdirContext<'context, 'name, 'location> = thekernel_linux_cred::InodeRmdirContext<
    'context,
    UserNamespace,
    InodeSecurityRef<'location>,
    ExistingInodeSecurityRef<'name, 'location>,
>;
type CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location> =
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

/// Kernel-owned input to one typed inode-xattr pre/post hook transaction.
///
/// The target snapshot is retained by value so an admission can cross the
/// provider call without retaining a VFS handle or repeating lookup. The
/// borrowed operation preserves the caller's exact validated name/value bytes,
/// while the actor, DAC snapshot, and filesystem owner namespace remain the
/// same immutable objects throughout both hook passes.
pub(crate) struct InodeXattrSecurityContext<'context, 'location> {
    actor: &'context Cred,
    dac_credential: &'context DacCredentialView,
    target_owner_user_ns: &'context Arc<UserNamespace>,
    target_object: InodeSecurityRef<'location>,
    operation: InodeXattrOperation<'context>,
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

    fn core(&self) -> CoreInodeXattrContext<'_, 'location> {
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
    actor: &'context Cred,
    dac_credential: &'context DacCredentialView,
    target_owner_user_ns: &'context Arc<UserNamespace>,
    target_object: InodeSecurityRef<'location>,
    proposal: InodeSetattrProposal,
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

    fn core(&self) -> CoreInodeSetattrContext<'_, 'location> {
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
struct InodePostSetattrSecurityContext<'context, 'location> {
    actor: &'context Cred,
    dac_credential: &'context DacCredentialView,
    target_owner_user_ns: &'context Arc<UserNamespace>,
    committed_object: InodeSetattrCommittedSecurityRef<'location>,
    proposal: InodeSetattrProposal,
}

impl<'context, 'location> InodePostSetattrSecurityContext<'context, 'location> {
    const fn new(
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

    fn core(&self) -> CoreInodePostSetattrContext<'_, 'location> {
        CoreInodePostSetattrContext::new(
            self.actor.core(),
            self.dac_credential,
            self.target_owner_user_ns,
            &self.committed_object,
            self.proposal,
        )
    }

    const fn actor(&self) -> &'context Cred {
        self.actor
    }

    const fn dac_credential(&self) -> &'context DacCredentialView {
        self.dac_credential
    }

    const fn target_owner_user_ns(&self) -> &'context Arc<UserNamespace> {
        self.target_owner_user_ns
    }

    const fn committed_object(&self) -> &InodeSetattrCommittedSecurityRef<'location> {
        &self.committed_object
    }

    const fn proposal(&self) -> InodeSetattrProposal {
        self.proposal
    }

    const fn intent(&self) -> InodeSetattrIntent {
        self.proposal.intent()
    }
}

/// Kernel wrapper retaining the exact composite actor and module state around
/// one planned regular-file creation.
pub(crate) struct InodeCreateSecurityContext<'context, 'name, 'location> {
    actor: &'context Cred,
    core: CoreInodeCreateContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeCreateContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeMkdirContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeMkdirContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeMknodContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeMknodContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeSymlinkContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeSymlinkContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeLinkContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeLinkContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeUnlinkContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeUnlinkContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeRmdirContext<'context, 'name, 'location>,
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

    fn core(&self) -> &CoreInodeRmdirContext<'context, 'name, 'location> {
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
    actor: &'context Cred,
    core: CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location>,
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

    fn core(&self) -> &CoreInodeRenameContext<'context, 'old_name, 'new_name, 'location> {
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
    actor: &'a Cred,
    operation: SocketSecurityOperation<'a>,
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
    /// for replacement of an already-live slot (`Mutation` and `Exec`) only;
    /// initial, fork-child, and user-namespace object publication require
    /// distinct lifecycle notifications and do not masquerade as a commit.
    fn credential_committed(
        &self,
        _context: CredentialPostCommitContext<'_, Self::CredentialState>,
    ) {
    }

    /// Narrows one exact commoncap-approved capability request.
    ///
    /// The external context can only be produced after validated-number and
    /// namespace/effective-set authorization. Dispatch has also validated the
    /// actor's complete composite state vector before the first module runs.
    /// Hooks are ordered, deny-first, allocation-free, and nonblocking; they
    /// must not call `current()` or reenter capability/security dispatch.
    fn capable(&self, _context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn capable_with_credential_state(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = actor_state;
        self.capable(context)
    }

    /// Narrows commoncap authority derived from a fully prepared credential
    /// without treating that credential as an already-live actor.
    fn prepared_credential_capable(
        &self,
        _context: &CorePreparedCredentialCapabilityContext<'_>,
    ) -> AxResult<()> {
        Ok(())
    }

    fn prepared_credential_capable_with_state(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &Self::CredentialState,
        proposed_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (source_state, proposed_state);
        self.prepared_credential_capable(context)
    }

    /// Observes a separately prepared fork or user-namespace credential after
    /// its exact child target has become visible.
    ///
    /// Preparation, state authorization, and complete source/published layout
    /// validation finish before visibility. This callback is infallible and
    /// runs in frozen registry order after task-table and parent-publication
    /// locks are released. It must not allocate, block, fail, panic, resample
    /// `current()`, or reenter task, credential, or security operations.
    fn credential_published(
        &self,
        _context: &CoreCredentialPublicationContext<'_>,
        _source_state: &Self::CredentialState,
        _published_state: &Self::CredentialState,
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

    /// Authorizes one exact typed xattr operation before provider dispatch.
    ///
    /// The caller has frozen the actor, DAC identity, filesystem owner
    /// namespace, target identity, and validated borrowed operation. Hooks are
    /// allocation-free and nonblocking and must not call `current()`, reenter
    /// VFS/xattr/security dispatch, or mutate provider state.
    fn inode_xattr(&self, _context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_xattr(context)
    }

    /// Observes one successfully completed provider xattr operation.
    ///
    /// The infallible callback receives the exact context admitted by the pre
    /// hook. It is never emitted for pre-hook denial or provider failure and
    /// must not allocate, block, fail, panic, resample `current()`, or reenter
    /// VFS/xattr/security dispatch.
    fn inode_post_xattr(&self, _context: &InodeXattrSecurityContext<'_, '_>) {}

    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) {
        self.inode_post_xattr(context);
    }

    /// Authorizes one exact inode-attribute proposal at the Linux
    /// `security_inode_setattr` point. The caller has frozen the actor, DAC
    /// identity, owner namespace, old inode snapshot, typed intent, and
    /// hook-point proposal, but has not yet run its fallible privilege cleanup
    /// or backend publication.
    fn inode_setattr(&self, _context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_setattr(context)
    }

    /// Observes one successfully committed inode-attribute change.
    ///
    /// This is an infallible, allocation-free post hook. It receives the same
    /// actor, DAC identity, owner namespace, and proposal admitted by the pre
    /// hook together with a caller-frozen committed inode snapshot. It must not
    /// call `current()`, reenter VFS/security dispatch, block, allocate, fail,
    /// or panic.
    fn inode_post_setattr(&self, _context: &InodePostSetattrSecurityContext<'_, '_>) {}

    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) {
        self.inode_post_setattr(context);
    }

    /// Authorizes one exact planned named entry after the caller has found the
    /// destination absent and before a regular file is created. The parent
    /// snapshot, final component, actor, DAC identity, owner namespace, and
    /// final normalized mode are all frozen by the caller. Implementations
    /// must remain allocation-free and nonblocking and must not call
    /// `current()` or reenter VFS, credential, or security operations.
    fn inode_create(&self, _context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_create(context)
    }

    /// Authorizes one exact planned named entry before a directory is created,
    /// under the same frozen, lookup-free contract as [`Self::inode_create`].
    fn inode_mkdir(&self, _context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_mkdir(context)
    }

    /// Authorizes one exact planned named entry before a FIFO, device, or
    /// socket inode is created, with an already-validated kind/mode/rdev
    /// operation and no VFS handle exposed to policy.
    fn inode_mknod(&self, _context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_mknod(context)
    }

    /// Authorizes one exact planned symbolic link after destination DAC and
    /// absence revalidation, before the target or name is published. The
    /// caller freezes the exact target which the filesystem will store.
    fn inode_symlink(&self, _context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_symlink(context)
    }

    /// Authorizes one exact hard-link source and prospective destination after
    /// source eligibility, destination DAC, cross-filesystem rejection, and
    /// absence revalidation. The caller must publish a new name for that same
    /// frozen source. Hooks are allocation-free, nonblocking, and forbidden
    /// from VFS/current/credential/security reentry.
    fn inode_link(&self, _context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_link(context)
    }

    /// Authorizes removal of one exact existing non-directory entry after the
    /// caller has completed its may-delete-style admission and revalidation.
    /// The frozen entry binds the exact parent, victim, and final name which the
    /// transaction must remove. Hooks must not reenter VFS or resample state.
    fn inode_unlink(&self, _context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_unlink(context)
    }

    /// Authorizes removal of one exact existing directory entry. Directory
    /// removal has its own hook contract and is never selected by a boolean on
    /// [`Self::inode_unlink`].
    fn inode_rmdir(&self, _context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_rmdir(context)
    }

    /// Authorizes one exact rename leaf after the caller has frozen the old
    /// parent/source entry and new parent/destination entry. The destination
    /// entry explicitly preserves absent versus existing-target state. Linux
    /// rename flags and exchange's ordered reverse dispatch remain caller
    /// responsibilities and are not represented by this leaf contract.
    fn inode_rename(&self, _context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_rename(context)
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

    /// Authorizes one already prepared socket leaf. Every variant borrows only
    /// immutable kernel-owned facts and dispatch runs before backend mutation.
    fn socket(&self, _context: &SocketSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.socket(context)
    }

    /// Authorizes one normalized file/anonymous mmap request after local ABI
    /// and backend validation, before address policy or backend construction.
    fn mmap_file(&self, _context: &CoreMmapFileContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.mmap_file(context)
    }

    /// Authorizes the final selected address in one exact retained image.
    fn mmap_addr(&self, _context: &CoreMmapAddressContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.mmap_addr(context)
    }

    /// Authorizes one exact pre-change VMA segment while the prepared
    /// protection transaction can still be dropped without side effects.
    fn file_mprotect(&self, _context: &CoreFileMprotectContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.file_mprotect(context)
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
    fn capable(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn prepared_credential_capable(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        published_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>);
    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_setattr(&self, context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>);
    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_unlink(&self, context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_rmdir(&self, context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()>;
    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()>;
    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
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

    fn capable(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::capable_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn prepared_credential_capable(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::prepared_credential_capable_with_state(
            self,
            context,
            owned_credential_state(self, source_state)?,
            owned_credential_state(self, proposed_state)?,
        )
    }

    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        published_state: &dyn ErasedOwnedCredentialState,
    ) {
        let source_state = owned_credential_state(self, source_state)
            .expect("preflighted source credential state changed before publication callback");
        let published_state = owned_credential_state(self, published_state)
            .expect("preflighted child credential state changed before publication callback");
        SecurityModule::credential_published(self, context, source_state, published_state);
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

    fn inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_xattr(self, context)
    }

    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_xattr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) {
        SecurityModule::inode_post_xattr(self, context);
    }

    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) {
        // The successful pre-hook pass validates every actor state before the
        // caller can receive an admission. Provider success cannot be rolled
        // back, so the matching post pass has no fallible downcast branch.
        let actor_state = owned_credential_state(self, actor_state)
            .expect("preflighted xattr actor state changed before post notification");
        SecurityModule::inode_post_xattr_with_credential_state(self, context, actor_state);
    }

    fn inode_setattr(&self, context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_setattr(self, context)
    }

    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_setattr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>) {
        SecurityModule::inode_post_setattr(self, context);
    }

    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) {
        // The fallible pre-hook pass validates the complete actor state vector
        // before it returns an admission token. Publication cannot be rolled
        // back, so the matching post pass has no downcast failure branch.
        let actor_state = owned_credential_state(self, actor_state)
            .expect("preflighted setattr actor state changed before post notification");
        SecurityModule::inode_post_setattr_with_credential_state(self, context, actor_state);
    }

    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_create(self, context)
    }

    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_create_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_mkdir(self, context)
    }

    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_mkdir_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_mknod(self, context)
    }

    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_mknod_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_symlink(self, context)
    }

    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_symlink_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_link(self, context)
    }

    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_link_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_unlink(&self, context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_unlink(self, context)
    }

    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_unlink_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_rmdir(&self, context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_rmdir(self, context)
    }

    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_rmdir_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_rename(self, context)
    }

    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_rename_with_credential_state(
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

    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::socket_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::mmap_file_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::mmap_addr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::file_mprotect_with_credential_state(
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
                CredentialStateTransition::Mutation(_) => 1 << 1,
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

    fn capable(&self, context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        // The ABI leaf has already executed commoncap and only exposes this
        // field-private context on success. Keep the mandatory first module in
        // the dispatch shape without repeating or weakening that decision.
        let _ = (
            context.actor(),
            context.target_user_ns(),
            context.capability(),
            context.operation(),
        );
        Ok(())
    }

    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &Self::CredentialState,
        published_state: &Self::CredentialState,
    ) {
        #[cfg(test)]
        assert_post_commit_callback_locks_released();
        let _ = (
            context.source_credential(),
            context.published_credential(),
            context.source_user_ns(),
            context.target_user_ns(),
            context.target_object(),
            context.operation(),
            source_state,
            published_state,
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

    fn capable(&self, _context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn credential_published(
        &self,
        _context: &CoreCredentialPublicationContext<'_>,
        _source_state: &Self::CredentialState,
        _published_state: &Self::CredentialState,
    ) {
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

struct CredentialStateIdentity {
    publication_claimed: AtomicBool,
    live: AtomicBool,
}

enum CredentialStateDerivation {
    Initial,
    Prepared {
        source: Arc<CredentialStateIdentity>,
        transition: CredentialStateTransition,
    },
}

/// Complete immutable per-module state carried by one composite credential.
/// The layout identity and dense ModuleId order are checked before every
/// prepare/authorize pass, so a foreign or malformed state fails closed.
pub(in crate::task) struct CredentialSecurityState {
    registry: FrozenSecurityRegistry,
    slots: Vec<OwnedModuleCredState>,
    identity: Arc<CredentialStateIdentity>,
    derivation: CredentialStateDerivation,
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

    fn validate_live(&self) -> AxResult<()> {
        if self.identity.live.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }

    fn prepared_transition_from(
        &self,
        source: &CredentialSecurityState,
    ) -> AxResult<CredentialStateTransition> {
        let CredentialStateDerivation::Prepared {
            source: expected_source,
            transition: expected_transition,
        } = &self.derivation
        else {
            return Err(AxError::BadState);
        };
        if !Arc::ptr_eq(expected_source, &source.identity)
            || Arc::ptr_eq(&self.identity, &source.identity)
        {
            return Err(AxError::BadState);
        }
        Ok(*expected_transition)
    }

    fn claim_transition_from(
        &self,
        source: &CredentialSecurityState,
        transition: CredentialStateTransition,
    ) -> AxResult<()> {
        if self.prepared_transition_from(source)? != transition {
            return Err(AxError::BadState);
        }
        self.identity
            .publication_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AxError::BadState)
    }

    fn activate_claimed(&self) {
        assert!(
            self.identity.publication_claimed.load(Ordering::Acquire),
            "credential state activated without a publication claim"
        );
        self.identity
            .live
            .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
            .expect("credential state activated more than once");
    }

    #[cfg(test)]
    pub(in crate::task) fn activate_fixture(&self) {
        self.identity
            .publication_claimed
            .store(true, Ordering::Release);
        self.identity.live.store(true, Ordering::Release);
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
            CredentialStateTransition::Mutation(_) | CredentialStateTransition::Exec
        ) {
            return Err(AxError::BadState);
        }
        let registry = old.security().registry();
        registry.registry().validate_credential_pair(old, new)?;
        new.security()
            .claim_transition_from(old.security(), transition)?;
        Ok(Self {
            registry,
            old: old.clone(),
            new: new.clone(),
            transition,
        })
    }

    pub(in crate::task) fn activate(&self) {
        self.new.security().activate_claimed();
    }

    pub(in crate::task) fn notify(self) {
        let Self {
            registry,
            old,
            new,
            transition,
        } = self;
        debug_assert!(new.security().validate_live().is_ok());
        registry
            .registry()
            .notify_credential_committed(&old, &new, transition);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialPublicationKind {
    Fork,
    UserNamespace,
}

/// Linear pre-publication ownership for a separately prepared child
/// credential and its exact target object.
///
/// Both composite state vectors and their creating runtimes are validated
/// before this token is returned. Dropping it while the child is still private
/// is the ordinary rollback path and emits no callback. Once TASK_TABLE has
/// committed and all publication locks are released, [`Self::notify`] consumes
/// the token, constructs the external success-only context, and performs one
/// infallible frozen-order notification pass.
#[must_use = "an admitted child publication must either abort or notify after visibility"]
pub(crate) struct PendingCredentialPublication<T: CredentialPublicationTargetOwner> {
    registry: FrozenSecurityRegistry,
    source: Arc<Cred>,
    published: Arc<Cred>,
    target_owner: T,
    kind: CredentialPublicationKind,
}

impl<T: CredentialPublicationTargetOwner> PendingCredentialPublication<T> {
    pub(crate) fn try_fork(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
    ) -> AxResult<Self> {
        if !source.same_linux_credential(published) {
            return Err(AxError::BadState);
        }
        Self::try_new(
            source,
            published,
            target_owner,
            CredentialPublicationKind::Fork,
        )
    }

    pub(crate) fn try_user_namespace(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
    ) -> AxResult<Self> {
        let published_parent = published.user_ns().parent();
        if source.same_linux_credential(published)
            || !published_parent
                .as_ref()
                .is_some_and(|parent| Arc::ptr_eq(parent, source.user_ns()))
        {
            return Err(AxError::BadState);
        }
        Self::try_new(
            source,
            published,
            target_owner,
            CredentialPublicationKind::UserNamespace,
        )
    }

    fn try_new(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
        kind: CredentialPublicationKind,
    ) -> AxResult<Self> {
        let registry = source.security().registry();
        registry
            .registry()
            .validate_credential_pair(source, published)?;
        let transition = match kind {
            CredentialPublicationKind::Fork => CredentialStateTransition::Fork,
            CredentialPublicationKind::UserNamespace => CredentialStateTransition::UserNamespace,
        };
        published
            .security()
            .claim_transition_from(source.security(), transition)?;
        Ok(Self {
            registry,
            source: source.clone(),
            published: published.clone(),
            target_owner,
            kind,
        })
    }

    /// Makes the fully prepared state usable immediately before the first
    /// externally visible child-identity publication.
    pub(crate) fn activate(&self) {
        self.published.security().activate_claimed();
    }

    /// Delivers the successful-publication event. The caller owns the external
    /// visibility linearization point and must invoke this only after releasing
    /// TASK_TABLE, task-parent, and process-lifecycle publication locks.
    pub(crate) fn notify(self) {
        let Self {
            registry,
            source,
            published,
            target_owner,
            kind,
        } = self;
        debug_assert!(published.security().validate_live().is_ok());
        let target = target_owner.credential_publication_target();
        let context = match kind {
            CredentialPublicationKind::Fork => {
                thekernel_linux_cred::CredentialPublicationContext::fork(
                    source.core(),
                    published.core(),
                    &target,
                )
            }
            CredentialPublicationKind::UserNamespace => {
                thekernel_linux_cred::CredentialPublicationContext::user_namespace(
                    source.core(),
                    published.core(),
                    &target,
                )
            }
        };
        registry
            .registry()
            .notify_credential_published(&context, &source, &published);
        drop(target_owner);
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
        registry.registry().validate_credential_pair(old, new)?;
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
        credential.security().validate_live()?;
        self.security_slots(credential.security())
    }

    /// Performs the complete fallible layout, ModuleId, erased-type, and
    /// exact-runtime validation for both composites before publication. No
    /// module callback has run if either (including a late) slot is malformed.
    fn validate_credential_pair(&self, source: &Cred, published: &Cred) -> AxResult<()> {
        self.credential_slots(source)?;
        self.security_slots(published.security())?;
        Ok(())
    }

    /// Dispatches one field-private commoncap success token with the exact
    /// actor's composite state. The complete vector is validated before the
    /// mandatory commoncap module or any stacked policy callback runs.
    fn dispatch_capable_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreCapabilitySecurityContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .capable(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_prepared_credential_capable(
        &self,
        source: &Cred,
        proposed: &Cred,
        context: &CorePreparedCredentialCapabilityContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(source.core(), context.source_credential())
            || !core::ptr::eq(proposed.core(), context.proposed_credential())
        {
            return Err(AxError::OperationNotPermitted);
        }
        let source_slots = self.credential_slots(source)?;
        let proposed_slots = self.security_slots(proposed.security())?;
        for ((registered, source_state), proposed_state) in
            self.modules.iter().zip(source_slots).zip(proposed_slots)
        {
            if registered.id != source_state.module_id || registered.id != proposed_state.module_id
            {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.prepared_credential_capable(
                context,
                source_state.erased.as_ref(),
                proposed_state.erased.as_ref(),
            )?;
        }
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

    /// Delivers one already-visible child credential lifecycle event in frozen
    /// order. `PendingCredentialPublication` completed every fallible registry,
    /// layout, and erased-runtime check before TASK_TABLE publication.
    fn notify_credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source: &Cred,
        published: &Cred,
    ) {
        debug_assert!(core::ptr::eq(context.source_credential(), source.core()));
        debug_assert!(core::ptr::eq(
            context.published_credential(),
            published.core()
        ));
        let source_slots = &source.security().slots;
        let published_slots = &published.security().slots;
        debug_assert_eq!(source_slots.len(), self.modules.len());
        debug_assert_eq!(published_slots.len(), self.modules.len());
        for ((registered, source_state), published_state) in
            self.modules.iter().zip(source_slots).zip(published_slots)
        {
            debug_assert_eq!(registered.id, source_state.module_id);
            debug_assert_eq!(registered.id, published_state.module_id);
            registered.module.credential_published(
                context,
                source_state.erased.as_ref(),
                published_state.erased.as_ref(),
            );
        }
    }

    fn try_empty_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        derivation: CredentialStateDerivation,
    ) -> AxResult<CredentialSecurityState> {
        self.try_empty_credential_state_with_reservation(registry, self.modules.len(), derivation)
    }

    fn try_empty_credential_state_with_reservation(
        &'static self,
        registry: FrozenSecurityRegistry,
        reservation: usize,
        derivation: CredentialStateDerivation,
    ) -> AxResult<CredentialSecurityState> {
        if !core::ptr::eq(self, registry.registry()) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(reservation)
            .map_err(|_| AxError::NoMemory)?;
        let initial = matches!(&derivation, CredentialStateDerivation::Initial);
        let identity = Arc::try_new(CredentialStateIdentity {
            publication_claimed: AtomicBool::new(initial),
            live: AtomicBool::new(initial),
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(CredentialSecurityState {
            registry,
            slots,
            identity,
            derivation,
        })
    }

    fn try_init_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        credential: &CoreCred,
    ) -> AxResult<CredentialSecurityState> {
        let mut candidate =
            self.try_empty_credential_state(registry, CredentialStateDerivation::Initial)?;
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
        old_state.validate_live()?;
        self.validate_erased_slots(&old_state.slots)?;
        let mut candidate = self.try_empty_credential_state(
            registry,
            CredentialStateDerivation::Prepared {
                source: old_state.identity.clone(),
                transition,
            },
        )?;
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

    fn dispatch_inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_xattr(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        // Validate the complete immutable actor vector before the first hook.
        // The successful pass is the proof retained by the linear admission.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_xattr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn notify_inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_post_xattr(context);
        }
    }

    fn notify_inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
    ) {
        let actor = &context.actor().security().slots;
        debug_assert!(core::ptr::eq(
            self,
            context.actor().security().registry().registry()
        ));
        debug_assert_eq!(actor.len(), self.modules.len());
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .inode_post_xattr_with_credential_state(context, actor_state.erased.as_ref());
        }
    }

    fn dispatch_inode_setattr(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_setattr(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        // Validate the complete immutable actor vector before the first module
        // runs. A successful return is the preflight proof carried into the
        // infallible post-publication pass.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_setattr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn notify_inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>) {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_post_setattr(context);
        }
    }

    fn notify_inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
    ) {
        let actor = &context.actor().security().slots;
        debug_assert!(core::ptr::eq(
            self,
            context.actor().security().registry().registry()
        ));
        debug_assert_eq!(actor.len(), self.modules.len());
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .inode_post_setattr_with_credential_state(context, actor_state.erased.as_ref());
        }
    }

    fn dispatch_inode_create(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_create(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_create_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_mkdir(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_mkdir(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_mkdir_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_mknod(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_mknod(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_mknod_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_symlink(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_symlink(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_symlink_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_link(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_link_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_unlink(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_unlink(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_unlink_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_rmdir(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_rmdir(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_rmdir_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_inode_rename(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_rename(context)?;
        }
        Ok(())
    }

    fn dispatch_inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_rename_with_credential_state(context, actor_state.erased.as_ref())?;
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

    fn dispatch_socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
    ) -> AxResult<()> {
        // Validate the complete vector before the first callback so a malformed
        // late slot cannot produce a partial policy trace.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .socket_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_mmap_file_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreMmapFileContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .mmap_file_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_mmap_addr_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreMmapAddressContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .mmap_addr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    fn dispatch_file_mprotect_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreFileMprotectContext<'_, '_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .file_mprotect_with_credential_state(context, actor_state.erased.as_ref())?;
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

/// Linear proof that every inode-xattr pre-hook admitted one exact frozen
/// operation and that the complete actor module-state vector was preflighted.
///
/// Dropping this token is the provider-failure or caller-abort path and emits
/// no post notification. Only [`Self::committed`] emits the success-only post
/// pass, using the exact context admitted before provider dispatch.
#[must_use = "an admitted inode xattr operation must either abort or publish its post notification"]
pub(crate) struct InodeXattrSecurityAdmission<'context, 'location> {
    registry: FrozenSecurityRegistry,
    context: InodeXattrSecurityContext<'context, 'location>,
}

impl<'context, 'location> InodeXattrSecurityAdmission<'context, 'location> {
    /// Emits the ordered, infallible post-xattr pass after provider success.
    /// No actor, credential-state, DAC, namespace, inode, or operation input is
    /// resampled between the pre and post callbacks.
    pub(crate) fn committed(self) {
        self.registry
            .registry()
            .notify_inode_post_xattr_with_credential_state(&self.context);
    }
}

/// Runs the ordered, deny-first inode-xattr hook stack and returns a linear
/// admission which the caller may commit only after the provider succeeds.
pub(crate) fn dispatch_inode_xattr<'context, 'location>(
    context: InodeXattrSecurityContext<'context, 'location>,
) -> AxResult<InodeXattrSecurityAdmission<'context, 'location>> {
    let registry = context.actor().security().registry();
    registry
        .registry()
        .dispatch_inode_xattr_with_credential_state(&context)?;
    Ok(InodeXattrSecurityAdmission { registry, context })
}

/// Linear proof that every inode-setattr pre-hook admitted one exact frozen
/// proposal and that the complete actor module-state vector was preflighted.
///
/// Dropping this token is the ordinary backend-failure path and emits no post
/// notification. Only [`Self::committed`] can construct and publish the
/// infallible post context, so a caller cannot accidentally notify after a
/// denied or failed mutation.
#[must_use = "an admitted inode setattr must either abort or publish its post notification"]
pub(crate) struct InodeSetattrSecurityAdmission<'context, 'location> {
    registry: FrozenSecurityRegistry,
    context: InodeSetattrSecurityContext<'context, 'location>,
}

impl<'context, 'location> InodeSetattrSecurityAdmission<'context, 'location> {
    /// Emits the ordered, infallible post-setattr pass for one exact committed
    /// inode snapshot. No registry, credential-state, DAC, or VFS revalidation
    /// occurs after the backend has reported success.
    pub(crate) fn committed(self, committed_object: InodeSetattrCommittedSecurityRef<'location>) {
        let context = InodePostSetattrSecurityContext::new(
            self.context.actor(),
            self.context.dac_credential(),
            self.context.target_owner_user_ns(),
            committed_object,
            self.context.proposal(),
        );
        self.registry
            .registry()
            .notify_inode_post_setattr_with_credential_state(&context);
    }
}

/// Runs the ordered, deny-first inode-setattr hook stack and returns a linear
/// admission retaining every frozen input required by the success-only post
/// phase. The context is consumed so the token owns the old inode snapshot
/// rather than borrowing a construction-frame local.
pub(crate) fn dispatch_inode_setattr<'context, 'location>(
    context: InodeSetattrSecurityContext<'context, 'location>,
) -> AxResult<InodeSetattrSecurityAdmission<'context, 'location>> {
    let registry = context.actor().security().registry();
    registry
        .registry()
        .dispatch_inode_setattr_with_credential_state(&context)?;
    Ok(InodeSetattrSecurityAdmission { registry, context })
}

/// Runs typed regular-file creation hooks for one frozen planned entry. The
/// first denial is returned before VFS creates the object.
pub(crate) fn dispatch_inode_create(
    context: &InodeCreateSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_create_with_credential_state(context)
}

/// Runs typed directory creation hooks for one frozen planned entry. The first
/// denial is returned before VFS creates the object.
pub(crate) fn dispatch_inode_mkdir(
    context: &InodeMkdirSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_mkdir_with_credential_state(context)
}

/// Runs typed special-node creation hooks for one frozen planned entry and
/// already-validated kind/mode/rdev operation.
pub(crate) fn dispatch_inode_mknod(
    context: &InodeMknodSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_mknod_with_credential_state(context)
}

/// Runs typed symbolic-link creation hooks for one frozen planned entry and
/// exact target. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_symlink(
    context: &InodeSymlinkSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_symlink_with_credential_state(context)
}

/// Runs typed hard-link hooks for one frozen source and prospective
/// destination. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_link(context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_link_with_credential_state(context)
}

/// Runs typed non-directory removal hooks for one frozen parent, victim, and
/// final name. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_unlink(
    context: &InodeUnlinkSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_unlink_with_credential_state(context)
}

/// Runs typed directory-removal hooks for one frozen parent, directory victim,
/// and final name. This entry point cannot dispatch the unlink hook family.
pub(crate) fn dispatch_inode_rmdir(
    context: &InodeRmdirSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_rmdir_with_credential_state(context)
}

/// Runs one typed rename leaf over the exact frozen old and new object roles.
/// The first denial stops dispatch before the caller publishes a namespace
/// mutation. Exchange's reverse-then-forward sequencing remains explicit at
/// the transaction layer rather than being inferred here.
pub(crate) fn dispatch_inode_rename(
    context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_rename_with_credential_state(context)
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

/// Runs one typed socket hook stack in declaration order after the caller has
/// completed required usercopy and before backend mutation or publication.
pub(crate) fn dispatch_socket(context: &SocketSecurityContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_socket_with_credential_state(context)
}

fn mmap_memory_protection(flags: MappingFlags) -> MemoryProtection {
    let mut bits = 0;
    if flags.contains(MappingFlags::READ) {
        bits |= MemoryProtection::READ.bits();
    }
    if flags.contains(MappingFlags::WRITE) {
        bits |= MemoryProtection::WRITE.bits();
    }
    if flags.contains(MappingFlags::EXECUTE) {
        bits |= MemoryProtection::EXECUTE.bits();
    }
    MemoryProtection::try_from_bits(bits).expect("adapter emits only PROT_READ/WRITE/EXEC bits")
}

/// Runs the typed `mmap_file` stack over either an anonymous target or the
/// exact retained file/OFD selected by the syscall adapter.
pub(crate) fn mmap_file(
    actor: &Cred,
    target: Option<(&Arc<UserNamespace>, &FileHandle<File>)>,
    requested: MappingFlags,
    effective: MappingFlags,
    raw_flags: usize,
) -> AxResult<()> {
    let target = match target {
        Some((filesystem_owner_user_ns, file)) => {
            MmapFileTarget::File(MmapFileSecurityRef::new(filesystem_owner_user_ns, file))
        }
        None => MmapFileTarget::Anonymous,
    };
    let operation = MmapFileOperation::new(
        mmap_memory_protection(requested),
        mmap_memory_protection(effective),
        MmapFileFlags::from_raw(raw_flags),
    );
    let context = MmapFileContext::new(actor.core(), target, operation);
    actor
        .security()
        .registry()
        .registry()
        .dispatch_mmap_file_with_credential_state(actor, &context)
}

/// Runs the typed `mmap_addr` stack over the final candidate in one exact
/// retained address-space image.
pub(crate) fn mmap_addr(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    image: &Arc<Mutex<AddrSpace>>,
    final_address: VirtAddr,
) -> AxResult<()> {
    let image = MmapImageSecurityRef::from_arc(image);
    dispatch_mmap_addr(actor, image_owner_user_ns, &image, final_address)
}

fn dispatch_mmap_addr(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    image: &MmapImageSecurityRef,
    final_address: VirtAddr,
) -> AxResult<()> {
    let context = MmapAddressContext::new(
        actor.core(),
        image_owner_user_ns,
        image,
        final_address.as_usize(),
    );
    actor
        .security()
        .registry()
        .registry()
        .dispatch_mmap_addr_with_credential_state(actor, &context)
}

/// Runs the typed `file_mprotect` stack for one exact pre-change VMA segment.
/// The caller owns the prepared transaction and commits only after every
/// segment has passed this dispatch.
pub(crate) fn file_mprotect<'segment>(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    segment: PreparedProtectSegment<'segment>,
    requested: MappingFlags,
    effective: MappingFlags,
) -> AxResult<()> {
    let context = FileMprotectContext::new(
        actor.core(),
        image_owner_user_ns,
        &segment,
        mmap_memory_protection(requested),
        mmap_memory_protection(effective),
    );
    actor
        .security()
        .registry()
        .registry()
        .dispatch_file_mprotect_with_credential_state(actor, &context)
}

/// Runs Linux commoncap first, then lets the exact actor's frozen module stack
/// narrow the successful decision in declaration order.
///
/// Invalid raw numbers and commoncap denials return before registry lookup or
/// any module callback. Complete composite-state validation likewise finishes
/// before the mandatory commoncap module and later policy modules run.
fn authorize_capability_with_operation(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
    operation: CapabilitySecurityOperation,
) -> AxResult<()> {
    let capability = CapabilityNumber::try_new(raw_capability).ok_or(AxError::InvalidInput)?;
    let context =
        external_authorize_capability_core(actor.core(), target_user_ns, capability, operation)
            .map_err(authorization_error)?;
    actor
        .security()
        .registry()
        .registry()
        .dispatch_capable_with_credential_state(actor, &context)
}

/// Ordinary audited capability check used by every general kernel capability
/// entry point, including `task::access::ns_capable`.
pub(in crate::task) fn capable(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    authorize_capability_with_operation(
        actor,
        target_user_ns,
        raw_capability,
        CapabilitySecurityOperation::Use,
    )
    .is_ok()
}

/// Set-ID-family capability check. Keeping this helper specific prevents an
/// arbitrary caller from relabeling an ordinary check as `CAP_OPT_INSETID`.
pub(in crate::task) fn capable_for_setid(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    authorize_capability_with_operation(
        actor,
        target_user_ns,
        raw_capability,
        CapabilitySecurityOperation::SetId,
    )
    .is_ok()
}

/// Checks namespace-creation authority carried by one exact prepared child.
/// Commoncap evaluates the proposed credential, while stacked modules receive
/// both the live source state and the still-private proposed state.
pub(crate) fn prepared_credential_namespace_capable(
    source: &Cred,
    proposed: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    let authorize = || -> AxResult<()> {
        let registry = source.security().registry();
        registry
            .registry()
            .validate_credential_pair(source, proposed)?;
        if !matches!(
            proposed
                .security()
                .prepared_transition_from(source.security())?,
            CredentialStateTransition::Fork | CredentialStateTransition::UserNamespace
        ) {
            return Err(AxError::BadState);
        }
        let capability = CapabilityNumber::try_new(raw_capability).ok_or(AxError::InvalidInput)?;
        let context = external_authorize_prepared_credential_capability_core(
            source.core(),
            proposed.core(),
            target_user_ns,
            capability,
            PreparedCredentialCapabilityOperation::NamespaceCreate,
        )
        .map_err(authorization_error)?;
        registry
            .registry()
            .dispatch_prepared_credential_capable(source, proposed, &context)
    };
    authorize().is_ok()
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenameSecurityTestObjectFacts {
    identity: InodeIdentity,
    mode: u16,
    node_kind: NodeType,
    uid: u32,
    gid: u32,
    size: u64,
}

#[cfg(test)]
impl RenameSecurityTestObjectFacts {
    pub(crate) fn from_location(location: &Location) -> AxResult<Self> {
        let metadata = location.metadata()?;
        let object = InodeSecurityRef::new(location, &metadata);
        Ok(Self::from_object(&object))
    }

    fn from_object(object: &InodeSecurityRef<'_>) -> Self {
        Self {
            identity: object.identity(),
            mode: object.mode(),
            node_kind: object.node_kind(),
            uid: object.uid(),
            gid: object.gid(),
            size: object.size(),
        }
    }

    fn assert_matches(&self, object: &InodeSecurityRef<'_>) {
        assert_eq!(object.identity(), self.identity);
        assert_eq!(object.mode(), self.mode);
        assert_eq!(object.node_kind(), self.node_kind);
        assert_eq!(object.uid(), self.uid);
        assert_eq!(object.gid(), self.gid);
        assert_eq!(object.size(), self.size);
    }
}

/// Immutable test-only expectation consumed by a per-test rename probe.
///
/// Each namespace test owns its probe through an `Arc`; there is no shared
/// global callback or mutable expected-value slot, so parallel tests cannot
/// observe or reset one another's state.
#[cfg(test)]
pub(crate) struct RenameSecurityTestExpectation {
    old_parent: RenameSecurityTestObjectFacts,
    source: RenameSecurityTestObjectFacts,
    old_name: alloc::string::String,
    new_parent: RenameSecurityTestObjectFacts,
    replaced: Option<RenameSecurityTestObjectFacts>,
    new_name: alloc::string::String,
}

#[cfg(test)]
impl RenameSecurityTestExpectation {
    pub(crate) fn new(
        old_parent: &Location,
        source: &Location,
        old_name: &str,
        new_parent: &Location,
        replaced: Option<&Location>,
        new_name: &str,
    ) -> AxResult<Self> {
        Ok(Self {
            old_parent: RenameSecurityTestObjectFacts::from_location(old_parent)?,
            source: RenameSecurityTestObjectFacts::from_location(source)?,
            old_name: alloc::string::String::from(old_name),
            new_parent: RenameSecurityTestObjectFacts::from_location(new_parent)?,
            replaced: replaced
                .map(RenameSecurityTestObjectFacts::from_location)
                .transpose()?,
            new_name: alloc::string::String::from(new_name),
        })
    }

    fn assert_matches(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) {
        self.old_parent.assert_matches(context.old_parent_object());
        self.source
            .assert_matches(context.old_entry_object().target_object());
        assert_eq!(context.old_entry_object().name(), self.old_name);
        assert!(core::ptr::eq(
            context.old_parent_object(),
            context.old_entry_object().parent_object()
        ));

        self.new_parent.assert_matches(context.new_parent_object());
        assert_eq!(context.new_entry_object().name(), self.new_name);
        assert!(core::ptr::eq(
            context.new_parent_object(),
            context.new_entry_object().parent_object()
        ));
        match (&self.replaced, context.new_entry_object().target_object()) {
            (Some(expected), Some(actual)) => expected.assert_matches(actual),
            (None, None) => {}
            _ => panic!("rename destination presence did not match the frozen expectation"),
        }
    }
}

#[cfg(test)]
pub(crate) struct RenameSecurityTestProbe {
    expectation: RenameSecurityTestExpectation,
    deny: bool,
    calls: core::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl RenameSecurityTestProbe {
    pub(crate) fn new(expectation: RenameSecurityTestExpectation, deny: bool) -> Arc<Self> {
        Arc::new(Self {
            expectation,
            deny,
            calls: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(core::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
struct RenameSecurityTestModule<const KEY: u64> {
    probe: Arc<RenameSecurityTestProbe>,
}

#[cfg(test)]
impl<const KEY: u64> SecurityModule for RenameSecurityTestModule<KEY> {
    const KEY: ModuleKey = ModuleKey(KEY);
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        unreachable!("rename test modules are registered as initialized instances")
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

    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        self.probe.expectation.assert_matches(context);
        self.probe
            .calls
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        if self.probe.deny {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
fn rename_security_test_registry(
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
) -> FrozenSecurityRegistry {
    const FIRST_KEY: u64 = 0x7265_6e61_6d65_0100;
    const SECOND_KEY: u64 = 0x7265_6e61_6d65_0200;

    let mut builder = SecurityRegistryBuilder::try_new()
        .expect("rename test registry allocation failed")
        .try_register_commoncap()
        .expect("rename test commoncap registration failed");
    builder
        .try_register_initialized(RenameSecurityTestModule::<FIRST_KEY> { probe: first })
        .expect("rename test first module registration failed");
    if let Some(second) = second {
        builder
            .try_register_initialized(RenameSecurityTestModule::<SECOND_KEY> { probe: second })
            .expect("rename test second module registration failed");
    }
    let registry = Box::new(builder.freeze());
    FrozenSecurityRegistry(Box::leak(registry))
}

#[cfg(test)]
pub(crate) fn rename_security_test_credential(
    namespace: Arc<UserNamespace>,
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
) -> Arc<Cred> {
    Cred::try_root_with_registry(rename_security_test_registry(first, second), namespace)
        .expect("rename test credential construction failed")
}

#[cfg(test)]
pub(crate) fn rename_security_test_unprivileged_credential(
    namespace: Arc<UserNamespace>,
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
    uid: u32,
    gid: u32,
) -> Arc<Cred> {
    let root = rename_security_test_credential(namespace, first, second);
    let slot = super::creds::CredentialSlot::new(root);
    let mut update = slot.prepare();
    let uid = super::Kuid::from_raw(uid).expect("rename test uid must be representable");
    let gid = super::Kgid::from_raw(gid).expect("rename test gid must be representable");
    update.builder.ids = super::Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        fsuid: uid,
        rgid: gid,
        egid: gid,
        sgid: gid,
        fsgid: gid,
    };
    let caps = update.builder.caps;
    update.builder.caps = super::creds::capability_state_for_test(
        [0; super::creds::CAPABILITY_WORDS],
        [0; super::creds::CAPABILITY_WORDS],
        [0; super::creds::CAPABILITY_WORDS],
        caps.bounding(),
        [0; super::creds::CAPABILITY_WORDS],
        caps.securebits(),
    );
    update
        .finish()
        .expect("rename test unprivileged credential preparation failed")
        .commit()
}

#[cfg(test)]
pub(crate) fn malformed_rename_security_test_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<RenameSecurityTestProbe>,
) -> Arc<Cred> {
    const WRONG_KEY: u64 = 0x7265_6e61_6d65_0300;

    let registry = rename_security_test_registry(probe.clone(), None);
    let actor = Cred::try_root_with_registry(registry, namespace)
        .expect("rename malformed test actor construction failed");
    let mut malformed = registry
        .registry()
        .try_init_credential_state(registry, actor.core())
        .expect("rename malformed test state construction failed");
    malformed.slots[1].erased = try_own_credential_state(
        Arc::new(RenameSecurityTestModule::<WRONG_KEY> { probe }),
        (),
    )
    .expect("rename malformed test state ownership failed");
    Cred::try_from_prepared_parts(actor.core_arc().clone(), malformed)
        .expect("rename malformed test credential construction failed")
}

/// Typed leaf expected from one named-create transaction test.
///
/// This deliberately mirrors the public hook family instead of collapsing all
/// creates into one untyped callback.  A transaction test can therefore prove
/// both that the final hook is reached exactly once and that it is reached
/// through the Linux hook selected for the frozen inode kind.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NamedCreateSecurityTestLeaf {
    RegularFile {
        mode: u16,
    },
    Directory {
        mode: u16,
    },
    Mknod {
        kind: InodeMknodKind,
        mode: u16,
        rdev: Option<u64>,
    },
    Symlink {
        target: alloc::string::String,
    },
}

/// Immutable per-test expectation for one planned named entry.
#[cfg(test)]
pub(crate) struct NamedCreateSecurityTestExpectation {
    parent: RenameSecurityTestObjectFacts,
    name: alloc::string::String,
    leaf: NamedCreateSecurityTestLeaf,
}

#[cfg(test)]
impl NamedCreateSecurityTestExpectation {
    pub(crate) fn new(
        parent: &Location,
        name: &str,
        leaf: NamedCreateSecurityTestLeaf,
    ) -> AxResult<Self> {
        Ok(Self {
            parent: RenameSecurityTestObjectFacts::from_location(parent)?,
            name: alloc::string::String::from(name),
            leaf,
        })
    }

    fn assert_parent(&self, object: &InodeSecurityRef<'_>) {
        self.parent.assert_matches(object);
    }

    fn assert_planned(&self, object: &PlannedInodeSecurityRef<'_, '_>) {
        self.assert_parent(object.parent_object());
        assert_eq!(object.name(), self.name);
    }
}

/// Per-test named-create security probe.
///
/// All observations are owned by the credential registry used by that one
/// test.  There is no mutable global callback or reset protocol, so separate
/// namespace tests remain safe under the Rust test runner's parallel schedule.
#[cfg(test)]
pub(crate) struct NamedCreateSecurityTestProbe {
    expectation: NamedCreateSecurityTestExpectation,
    deny_permission: bool,
    deny_leaf: bool,
    permission_calls: core::sync::atomic::AtomicUsize,
    leaf_calls: core::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl NamedCreateSecurityTestProbe {
    pub(crate) fn new(
        expectation: NamedCreateSecurityTestExpectation,
        deny_permission: bool,
        deny_leaf: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            expectation,
            deny_permission,
            deny_leaf,
            permission_calls: core::sync::atomic::AtomicUsize::new(0),
            leaf_calls: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn permission_calls(&self) -> usize {
        self.permission_calls
            .load(core::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn leaf_calls(&self) -> usize {
        self.leaf_calls.load(core::sync::atomic::Ordering::SeqCst)
    }

    fn observe_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        self.expectation.assert_parent(context.target_object());
        assert_eq!(
            context.access(),
            InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE
        );
        assert_eq!(
            self.permission_calls
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            0,
            "named-create parent permission hook ran more than once"
        );
        if self.deny_permission {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    fn begin_leaf(&self, planned: &PlannedInodeSecurityRef<'_, '_>) {
        self.expectation.assert_planned(planned);
        assert_eq!(
            self.permission_calls(),
            1,
            "named-create leaf ran before its one parent permission hook"
        );
        assert_eq!(
            self.leaf_calls
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            0,
            "named-create typed leaf ran more than once"
        );
    }

    fn finish_leaf(&self) -> AxResult<()> {
        if self.deny_leaf {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
struct NamedCreateSecurityTestModule<const KEY: u64> {
    probe: Arc<NamedCreateSecurityTestProbe>,
}

#[cfg(test)]
impl<const KEY: u64> SecurityModule for NamedCreateSecurityTestModule<KEY> {
    const KEY: ModuleKey = ModuleKey(KEY);
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        unreachable!("named-create test modules are registered as initialized instances")
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

    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        self.probe.observe_permission(context)
    }

    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::RegularFile { mode } = &self.probe.expectation.leaf else {
            panic!("regular-file create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.mode().bits(), *mode);
        self.probe.finish_leaf()
    }

    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Directory { mode } = &self.probe.expectation.leaf else {
            panic!("directory create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.mode().bits(), *mode);
        self.probe.finish_leaf()
    }

    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Mknod { kind, mode, rdev } = &self.probe.expectation.leaf
        else {
            panic!("special-node create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.operation().kind(), *kind);
        assert_eq!(context.operation().mode().bits(), *mode);
        assert_eq!(context.operation().rdev(), *rdev);
        self.probe.finish_leaf()
    }

    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Symlink { target } = &self.probe.expectation.leaf else {
            panic!("symlink create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.symlink_target(), target);
        self.probe.finish_leaf()
    }

    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        // Destination-order tests also use this probe as a canary for an
        // unexpectedly reached hard-link leaf.  The planned destination is
        // still checked against the exact per-test expectation.
        self.probe.begin_leaf(context.new_entry_object());
        self.probe.finish_leaf()
    }
}

#[cfg(test)]
fn named_create_security_test_registry(
    probe: Arc<NamedCreateSecurityTestProbe>,
) -> FrozenSecurityRegistry {
    const KEY: u64 = 0x6372_6561_7465_0100;

    let mut builder = SecurityRegistryBuilder::try_new()
        .expect("named-create test registry allocation failed")
        .try_register_commoncap()
        .expect("named-create test commoncap registration failed");
    builder
        .try_register_initialized(NamedCreateSecurityTestModule::<KEY> { probe })
        .expect("named-create test module registration failed");
    let registry = Box::new(builder.freeze());
    FrozenSecurityRegistry(Box::leak(registry))
}

#[cfg(test)]
pub(crate) fn named_create_security_test_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<NamedCreateSecurityTestProbe>,
) -> Arc<Cred> {
    Cred::try_root_with_registry(named_create_security_test_registry(probe), namespace)
        .expect("named-create test credential construction failed")
}

#[cfg(test)]
pub(crate) fn named_create_security_test_unprivileged_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<NamedCreateSecurityTestProbe>,
    uid: u32,
    gid: u32,
) -> Arc<Cred> {
    let root = named_create_security_test_credential(namespace, probe);
    let slot = super::creds::CredentialSlot::new(root);
    let mut update = slot.prepare();
    let uid = super::Kuid::from_raw(uid).expect("named-create test uid must be representable");
    let gid = super::Kgid::from_raw(gid).expect("named-create test gid must be representable");
    update.builder.ids = super::Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        fsuid: uid,
        rgid: gid,
        egid: gid,
        sgid: gid,
        fsgid: gid,
    };
    let caps = update.builder.caps;
    update.builder.caps = super::creds::capability_state_for_test(
        [0; super::creds::CAPABILITY_WORDS],
        [0; super::creds::CAPABILITY_WORDS],
        [0; super::creds::CAPABILITY_WORDS],
        caps.bounding(),
        [0; super::creds::CAPABILITY_WORDS],
        caps.securebits(),
    );
    update
        .finish()
        .expect("named-create test unprivileged credential preparation failed")
        .commit()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::{
        sync::{Barrier, Mutex, MutexGuard},
        thread,
    };

    use axfs_ng_vfs::{MetadataUpdate, Mountpoint, NodePermission};
    use axhal::paging::PageSize;
    use linux_raw_sys::general::{
        CAP_CHOWN, CAP_SETGID, CAP_SETUID, CAP_SYS_ADMIN, CAP_SYS_NICE, CAP_SYS_PTRACE,
        MAP_ANONYMOUS, MAP_PRIVATE,
    };
    use memory_addr::VirtAddrRange;
    use memory_set::MemoryArea;

    use super::*;
    use crate::{
        file::{
            namespace_mutation,
            permission::{SecurityFsContextExt, VfsSecurityContext},
        },
        mm::Backend,
        pseudofs::MemoryFs,
        task::{
            CapabilityState, Cred, CredentialSlot, Credentials, ExecCommitRuntime,
            ExecFileIdentity, ExecImageIdentity, IdMapInputExtent, Kgid, Kuid,
            creds::{
                CAPABILITY_WORDS, capability_state_for_test, credential_publication_lock_held,
            },
            thread_cred::{
                SetgroupsAdmission, prepare_setfsgid_update, prepare_setfsuid_update,
                prepare_user_id_update,
            },
        },
    };

    static ORDER_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DIRECTION: AtomicU32 = AtomicU32::new(0);
    static TRACEME_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static EXEC_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_XATTR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_POST_XATTR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_SETATTR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_POST_SETATTR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_CREATE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_MKDIR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_MKNOD_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_SYMLINK_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_LINK_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_UNLINK_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_RMDIR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static INODE_RENAME_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static HARDLINK_VERTICAL_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static UNLINK_VERTICAL_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static RMDIR_VERTICAL_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SYMLINK_VERTICAL_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static FILE_OPEN_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SCHEDULER_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static SIGNAL_DENY_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_HOOK_TRACE: AtomicU64 = AtomicU64::new(0);
    static WHOLE_MODULE_SETATTR_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_CREATE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_LINK_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static WHOLE_MODULE_REMOVE_HOOK_TRACE: AtomicU32 = AtomicU32::new(0);
    static MODULE_DROP_TRACE: AtomicU32 = AtomicU32::new(0);
    static RESERVED_MODULE_INIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARE_MUTATION_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_AUTHORIZE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_GENERATION_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_TRANSITION_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_MUTATION_MASK: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_OLD_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_COMMIT_NEW_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_CAPABLE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_CAPABLE_OPERATION: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_CAPABLE_NUMBER: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_CAPABLE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARED_CAPABLE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARED_CAPABLE_NUMBER: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PREPARED_CAPABLE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PUBLICATION_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PUBLICATION_OPERATION: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PUBLICATION_SOURCE_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PUBLICATION_CHILD_UID: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_PUBLICATION_TARGET: AtomicUsize = AtomicUsize::new(0);
    static CRED_STATE_DROP_AT_COMMIT: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DROP_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_DISPATCH_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_PERMISSION_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_XATTR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_POST_XATTR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_SETATTR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_POST_SETATTR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_CREATE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_MKDIR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_MKNOD_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_SYMLINK_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_LINK_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_UNLINK_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_RMDIR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_INODE_RENAME_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_FILE_OPEN_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_SOCKET_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_MMAP_FILE_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_MMAP_ADDR_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_MPROTECT_TRACE: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_MMAP_IMAGE_IDENTITY: AtomicUsize = AtomicUsize::new(0);
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
    static CRED_STATE_NAMED_CREATE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_REMOVE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_RENAME_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXEC_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_EXECUTABLE_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_SOCKET_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_MMAP_DENY_KEY: AtomicU32 = AtomicU32::new(0);
    static CRED_STATE_TEST_SERIAL: Mutex<()> = Mutex::new(());
    static HARDLINK_VERTICAL_TEST_SERIAL: Mutex<()> = Mutex::new(());
    static REMOVAL_VERTICAL_TEST_SERIAL: Mutex<()> = Mutex::new(());

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
            &CRED_STATE_PREPARE_MUTATION_MASK,
            &CRED_STATE_AUTHORIZE_TRACE,
            &CRED_STATE_COMMIT_TRACE,
            &CRED_STATE_COMMIT_GENERATION_TRACE,
            &CRED_STATE_COMMIT_TRANSITION_MASK,
            &CRED_STATE_COMMIT_MUTATION_MASK,
            &CRED_STATE_COMMIT_OLD_UID,
            &CRED_STATE_COMMIT_NEW_UID,
            &CRED_STATE_CAPABLE_TRACE,
            &CRED_STATE_CAPABLE_OPERATION,
            &CRED_STATE_CAPABLE_NUMBER,
            &CRED_STATE_CAPABLE_DENY_KEY,
            &CRED_STATE_PREPARED_CAPABLE_TRACE,
            &CRED_STATE_PREPARED_CAPABLE_NUMBER,
            &CRED_STATE_PREPARED_CAPABLE_DENY_KEY,
            &CRED_STATE_PUBLICATION_TRACE,
            &CRED_STATE_PUBLICATION_OPERATION,
            &CRED_STATE_PUBLICATION_SOURCE_UID,
            &CRED_STATE_PUBLICATION_CHILD_UID,
            &CRED_STATE_DROP_AT_COMMIT,
            &CRED_STATE_DROP_TRACE,
            &CRED_STATE_DISPATCH_TRACE,
            &CRED_STATE_INODE_PERMISSION_TRACE,
            &CRED_STATE_INODE_XATTR_TRACE,
            &CRED_STATE_INODE_POST_XATTR_TRACE,
            &CRED_STATE_INODE_SETATTR_TRACE,
            &CRED_STATE_INODE_POST_SETATTR_TRACE,
            &CRED_STATE_INODE_CREATE_TRACE,
            &CRED_STATE_INODE_MKDIR_TRACE,
            &CRED_STATE_INODE_MKNOD_TRACE,
            &CRED_STATE_INODE_SYMLINK_TRACE,
            &CRED_STATE_INODE_LINK_TRACE,
            &CRED_STATE_INODE_UNLINK_TRACE,
            &CRED_STATE_INODE_RMDIR_TRACE,
            &CRED_STATE_INODE_RENAME_TRACE,
            &CRED_STATE_FILE_OPEN_TRACE,
            &CRED_STATE_SOCKET_TRACE,
            &CRED_STATE_MMAP_FILE_TRACE,
            &CRED_STATE_MMAP_ADDR_TRACE,
            &CRED_STATE_MPROTECT_TRACE,
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
            &CRED_STATE_NAMED_CREATE_DENY_KEY,
            &CRED_STATE_REMOVE_DENY_KEY,
            &CRED_STATE_RENAME_DENY_KEY,
            &CRED_STATE_EXEC_DENY_KEY,
            &CRED_STATE_EXECUTABLE_DENY_KEY,
            &CRED_STATE_SOCKET_DENY_KEY,
            &CRED_STATE_MMAP_DENY_KEY,
        ] {
            trace.store(0, Ordering::SeqCst);
        }
        CRED_STATE_PUBLICATION_TARGET.store(0, Ordering::SeqCst);
        CRED_STATE_MMAP_IMAGE_IDENTITY.store(0, Ordering::SeqCst);
        guard
    }

    fn reset_hardlink_vertical_probe() -> MutexGuard<'static, ()> {
        let guard = HARDLINK_VERTICAL_TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        HARDLINK_VERTICAL_HOOK_TRACE.store(0, Ordering::SeqCst);
        guard
    }

    fn reset_removal_vertical_probes() -> MutexGuard<'static, ()> {
        let guard = REMOVAL_VERTICAL_TEST_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        UNLINK_VERTICAL_HOOK_TRACE.store(0, Ordering::SeqCst);
        RMDIR_VERTICAL_HOOK_TRACE.store(0, Ordering::SeqCst);
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

    #[derive(Clone, Copy)]
    struct TestCredentialPublicationTargetOwner {
        identity: usize,
    }

    impl CredentialPublicationTargetOwner for TestCredentialPublicationTargetOwner {
        fn credential_publication_target(&self) -> CredentialPublicationTarget {
            CredentialPublicationTarget {
                identity: self.identity,
            }
        }
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
                CredentialStateTransition::Mutation(kind) => {
                    CRED_STATE_PREPARE_MUTATION_MASK
                        .fetch_or(u32::from(kind.bits()), Ordering::SeqCst);
                    1 << 1
                }
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
                // Prepared module state is already usable; the framework keeps
                // it unreachable from live dispatch until publication activates
                // the composite state.
                committed: AtomicBool::new(true),
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

        fn capable_with_credential_state(
            &self,
            context: &CoreCapabilitySecurityContext<'_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            append_trace(&CRED_STATE_CAPABLE_TRACE, key);
            if key == 2 {
                let operation = match context.operation() {
                    CapabilitySecurityOperation::Use => 1,
                    CapabilitySecurityOperation::UseWithoutAudit => 1 << 1,
                    CapabilitySecurityOperation::SetId => 1 << 2,
                    _ => 1 << 3,
                };
                CRED_STATE_CAPABLE_OPERATION.store(operation, Ordering::SeqCst);
                CRED_STATE_CAPABLE_NUMBER.store(context.capability().get(), Ordering::SeqCst);
            }
            if CRED_STATE_CAPABLE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn prepared_credential_capable_with_state(
            &self,
            context: &CorePreparedCredentialCapabilityContext<'_>,
            source_state: &Self::CredentialState,
            proposed_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(source_state.key, key);
            assert_eq!(proposed_state.key, key);
            assert!(source_state.committed.load(Ordering::SeqCst));
            assert!(proposed_state.committed.load(Ordering::SeqCst));
            assert_eq!(proposed_state.generation, source_state.generation + 1);
            assert_eq!(
                context.operation(),
                PreparedCredentialCapabilityOperation::NamespaceCreate
            );
            append_trace(&CRED_STATE_PREPARED_CAPABLE_TRACE, key);
            if key == 2 {
                CRED_STATE_PREPARED_CAPABLE_NUMBER
                    .store(context.capability().get(), Ordering::SeqCst);
            }
            if CRED_STATE_PREPARED_CAPABLE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn credential_published(
            &self,
            context: &CoreCredentialPublicationContext<'_>,
            source_state: &Self::CredentialState,
            published_state: &Self::CredentialState,
        ) {
            assert_post_commit_callback_locks_released();
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(source_state.key, key);
            assert_eq!(published_state.key, key);
            assert!(source_state.committed.load(Ordering::SeqCst));
            assert!(published_state.committed.load(Ordering::SeqCst));
            assert_eq!(published_state.generation, source_state.generation + 1);
            append_trace(&CRED_STATE_PUBLICATION_TRACE, key);
            if key == 2 {
                let operation = match context.operation() {
                    CredentialPublicationOperation::Fork => 1,
                    CredentialPublicationOperation::UserNamespace => 1 << 1,
                    _ => 1 << 2,
                };
                CRED_STATE_PUBLICATION_OPERATION.store(operation, Ordering::SeqCst);
                CRED_STATE_PUBLICATION_SOURCE_UID.store(
                    context.source_credential().ids().euid.into_raw(),
                    Ordering::SeqCst,
                );
                CRED_STATE_PUBLICATION_CHILD_UID.store(
                    context.published_credential().ids().euid.into_raw(),
                    Ordering::SeqCst,
                );
                CRED_STATE_PUBLICATION_TARGET
                    .store(context.target_object().identity(), Ordering::SeqCst);
                assert!(Arc::ptr_eq(
                    context.source_user_ns(),
                    context.source_credential().user_ns()
                ));
                assert!(Arc::ptr_eq(
                    context.target_user_ns(),
                    context.published_credential().user_ns()
                ));
            }
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
            assert!(context.new_state().committed.load(Ordering::SeqCst));
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
                CredentialStateTransition::Mutation(kind) => {
                    CRED_STATE_COMMIT_MUTATION_MASK
                        .fetch_or(u32::from(kind.bits()), Ordering::SeqCst);
                    1 << 1
                }
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

        fn inode_xattr_with_credential_state(
            &self,
            context: &InodeXattrSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let core = context.core();
            assert!(core::ptr::eq(core.actor(), context.actor().core()));
            assert!(core::ptr::eq(
                core.dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                core.target_owner_user_ns(),
                context.target_owner_user_ns()
            ));
            assert!(core::ptr::eq(core.target_object(), context.target_object()));
            assert_eq!(
                core.target_object().identity(),
                context.target_object().identity()
            );
            let operation = context.operation();
            let core_operation = core.operation();
            assert_eq!(core_operation, operation);
            assert_eq!(
                core_operation.name().map(<[u8]>::as_ptr),
                operation.name().map(<[u8]>::as_ptr)
            );
            assert_eq!(
                core_operation.value().map(<[u8]>::as_ptr),
                operation.value().map(<[u8]>::as_ptr)
            );
            append_trace(&CRED_STATE_INODE_XATTR_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 18, Ordering::SeqCst);
            Ok(())
        }

        fn inode_post_xattr_with_credential_state(
            &self,
            context: &InodeXattrSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let core = context.core();
            assert!(core::ptr::eq(core.actor(), context.actor().core()));
            assert!(core::ptr::eq(
                core.dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                core.target_owner_user_ns(),
                context.target_owner_user_ns()
            ));
            assert!(core::ptr::eq(core.target_object(), context.target_object()));
            assert_eq!(core.operation(), context.operation());
            append_trace(&CRED_STATE_INODE_POST_XATTR_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 19, Ordering::SeqCst);
        }

        fn inode_setattr_with_credential_state(
            &self,
            context: &InodeSetattrSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let core = context.core();
            assert!(core::ptr::eq(core.actor(), context.actor().core()));
            assert!(core::ptr::eq(
                core.dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                core.target_owner_user_ns(),
                context.target_owner_user_ns()
            ));
            assert_eq!(
                core.target_object().identity(),
                context.target_object().identity()
            );
            assert_eq!(core.proposal(), context.proposal());
            assert_eq!(core.intent(), context.intent());
            append_trace(&CRED_STATE_INODE_SETATTR_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn inode_post_setattr_with_credential_state(
            &self,
            context: &InodePostSetattrSecurityContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let core = context.core();
            assert!(core::ptr::eq(core.actor(), context.actor().core()));
            assert!(core::ptr::eq(
                core.dac_credential(),
                context.dac_credential()
            ));
            assert!(core::ptr::eq(
                core.target_owner_user_ns(),
                context.target_owner_user_ns()
            ));
            assert_eq!(
                core.committed_object().identity(),
                context.committed_object().identity()
            );
            assert_eq!(core.proposal(), context.proposal());
            assert_eq!(core.intent(), context.intent());
            append_trace(&CRED_STATE_INODE_POST_SETATTR_TRACE, key);
            CRED_STATE_HOOK_MASK.fetch_or(1 << 17, Ordering::SeqCst);
        }

        fn inode_create_with_credential_state(
            &self,
            context: &InodeCreateSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_CREATE_TRACE, key);
            if CRED_STATE_NAMED_CREATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mkdir_with_credential_state(
            &self,
            context: &InodeMkdirSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_MKDIR_TRACE, key);
            if CRED_STATE_NAMED_CREATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 9, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mknod_with_credential_state(
            &self,
            context: &InodeMknodSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_MKNOD_TRACE, key);
            if CRED_STATE_NAMED_CREATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 10, Ordering::SeqCst);
            Ok(())
        }

        fn inode_symlink_with_credential_state(
            &self,
            context: &InodeSymlinkSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            assert!(core::ptr::eq(
                context.core().symlink_target(),
                context.symlink_target()
            ));
            append_trace(&CRED_STATE_INODE_SYMLINK_TRACE, key);
            if CRED_STATE_NAMED_CREATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 11, Ordering::SeqCst);
            Ok(())
        }

        fn inode_link_with_credential_state(
            &self,
            context: &InodeLinkSecurityContext<'_, '_, '_>,
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
                context.core().source_object(),
                context.source_object()
            ));
            assert!(core::ptr::eq(
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_LINK_TRACE, key);
            if CRED_STATE_NAMED_CREATE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 12, Ordering::SeqCst);
            Ok(())
        }

        fn inode_unlink_with_credential_state(
            &self,
            context: &InodeUnlinkSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().target_entry_object(),
                context.target_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_UNLINK_TRACE, key);
            if CRED_STATE_REMOVE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 13, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rmdir_with_credential_state(
            &self,
            context: &InodeRmdirSecurityContext<'_, '_, '_>,
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
                context.core().parent_object(),
                context.parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().target_entry_object(),
                context.target_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_RMDIR_TRACE, key);
            if CRED_STATE_REMOVE_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 14, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rename_with_credential_state(
            &self,
            context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
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
            assert!(Arc::ptr_eq(
                context.core().target_owner_user_ns(),
                context.target_owner_user_ns()
            ));
            assert!(core::ptr::eq(
                context.core().old_parent_object(),
                context.old_parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().old_entry_object(),
                context.old_entry_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_parent_object(),
                context.new_parent_object()
            ));
            assert!(core::ptr::eq(
                context.core().new_entry_object(),
                context.new_entry_object()
            ));
            append_trace(&CRED_STATE_INODE_RENAME_TRACE, key);
            if CRED_STATE_RENAME_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            CRED_STATE_HOOK_MASK.fetch_or(1 << 15, Ordering::SeqCst);
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

        fn socket_with_credential_state(
            &self,
            context: &SocketSecurityContext<'_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            let operation_actor = match context.operation() {
                SocketSecurityOperation::Create(operation) => operation.actor(),
                _ => panic!("socket registry probe expects create context"),
            };
            assert!(core::ptr::eq(context.actor().core(), operation_actor));
            append_trace(&CRED_STATE_SOCKET_TRACE, key);
            if CRED_STATE_SOCKET_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn mmap_file_with_credential_state(
            &self,
            context: &CoreMmapFileContext<'_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            if key == 2 {
                assert!(context.target().is_anonymous());
                assert_eq!(context.operation().requested(), MemoryProtection::NONE);
                assert_eq!(
                    context.operation().effective(),
                    MemoryProtection::READ | MemoryProtection::EXECUTE
                );
                assert_ne!(
                    context.operation().flags().raw() & (1usize << (usize::BITS - 1)),
                    0
                );
            }
            append_trace(&CRED_STATE_MMAP_FILE_TRACE, key);
            if CRED_STATE_MMAP_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn mmap_addr_with_credential_state(
            &self,
            context: &CoreMmapAddressContext<'_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            if key == 2 {
                assert_eq!(context.final_address(), 0x8000);
                assert!(Arc::ptr_eq(
                    context.image_owner_user_ns(),
                    context.actor().user_ns()
                ));
                assert_eq!(
                    context.image().identity(),
                    CRED_STATE_MMAP_IMAGE_IDENTITY.load(Ordering::SeqCst)
                );
            }
            append_trace(&CRED_STATE_MMAP_ADDR_TRACE, key);
            if CRED_STATE_MMAP_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
            Ok(())
        }

        fn file_mprotect_with_credential_state(
            &self,
            context: &CoreFileMprotectContext<'_, '_>,
            actor_state: &Self::CredentialState,
        ) -> AxResult<()> {
            let key = u32::try_from(KEY).expect("probe key fits u32");
            assert_eq!(actor_state.key, key);
            assert!(actor_state.committed.load(Ordering::SeqCst));
            if key == 2 {
                assert_eq!(context.pre_change_vma().area_start().as_usize(), 0x8000);
                assert_eq!(context.pre_change_vma().affected().start.as_usize(), 0x8000);
                assert_eq!(context.requested(), MemoryProtection::WRITE);
                assert_eq!(
                    context.effective(),
                    MemoryProtection::READ | MemoryProtection::WRITE
                );
            }
            append_trace(&CRED_STATE_MPROTECT_TRACE, key);
            if CRED_STATE_MMAP_DENY_KEY.load(Ordering::SeqCst) == key {
                return Err(AxError::PermissionDenied);
            }
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
            assert!(new_state.committed.load(Ordering::SeqCst));
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
            // Prepared state is fully usable but may still be rolled back while
            // the sleepable writer lock is held. Post-publication callback lock
            // ordering is asserted in the observer hooks above.
            append_trace(&CRED_STATE_DROP_TRACE, state.key);
        }
    }

    type TestInodePermissionHook = for<'context, 'location> fn(
        &InodePermissionSecurityContext<'context, 'location>,
    ) -> AxResult<()>;
    type TestInodeXattrHook = for<'context, 'location> fn(
        &InodeXattrSecurityContext<'context, 'location>,
    ) -> AxResult<()>;
    type TestInodePostXattrHook =
        for<'context, 'location> fn(&InodeXattrSecurityContext<'context, 'location>);
    type TestInodeSetattrHook = for<'context, 'location> fn(
        &InodeSetattrSecurityContext<'context, 'location>,
    ) -> AxResult<()>;
    type TestInodePostSetattrHook =
        for<'context, 'location> fn(&InodePostSetattrSecurityContext<'context, 'location>);
    type TestInodeCreateHook = for<'context, 'name, 'location> fn(
        &InodeCreateSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeMkdirHook = for<'context, 'name, 'location> fn(
        &InodeMkdirSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeMknodHook = for<'context, 'name, 'location> fn(
        &InodeMknodSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeSymlinkHook = for<'context, 'name, 'location> fn(
        &InodeSymlinkSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeLinkHook = for<'context, 'name, 'location> fn(
        &InodeLinkSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeUnlinkHook = for<'context, 'name, 'location> fn(
        &InodeUnlinkSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeRmdirHook = for<'context, 'name, 'location> fn(
        &InodeRmdirSecurityContext<'context, 'name, 'location>,
    ) -> AxResult<()>;
    type TestInodeRenameHook = for<'context, 'old_name, 'new_name, 'location> fn(
        &InodeRenameSecurityContext<'context, 'old_name, 'new_name, 'location>,
    )
        -> AxResult<()>;
    type TestFileOpenHook =
        for<'context, 'location> fn(&FileOpenSecurityContext<'context, 'location>) -> AxResult<()>;
    type TestPtraceAccessHook = for<'a> fn(&PtraceAccessContext<'a>) -> AxResult<()>;
    type TestPtraceTracemeHook = for<'a> fn(&PtraceTracemeContext<'a>) -> AxResult<()>;
    type TestExecCredentialHook = for<'a> fn(&ExecCredentialSecurityContext<'a>) -> AxResult<()>;
    type TestSchedulerHook = for<'a> fn(&SecuritySchedulerContext<'a>) -> AxResult<()>;
    type TestSignalHook = for<'a> fn(&SecuritySignalContext<'a>) -> AxResult<()>;

    struct TestSecurityModule<const KEY: u64> {
        inode_permission: Option<TestInodePermissionHook>,
        inode_xattr: Option<TestInodeXattrHook>,
        inode_post_xattr: Option<TestInodePostXattrHook>,
        inode_setattr: Option<TestInodeSetattrHook>,
        inode_post_setattr: Option<TestInodePostSetattrHook>,
        inode_create: Option<TestInodeCreateHook>,
        inode_mkdir: Option<TestInodeMkdirHook>,
        inode_mknod: Option<TestInodeMknodHook>,
        inode_symlink: Option<TestInodeSymlinkHook>,
        inode_link: Option<TestInodeLinkHook>,
        inode_unlink: Option<TestInodeUnlinkHook>,
        inode_rmdir: Option<TestInodeRmdirHook>,
        inode_rename: Option<TestInodeRenameHook>,
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
                inode_xattr: None,
                inode_post_xattr: None,
                inode_setattr: None,
                inode_post_setattr: None,
                inode_create: None,
                inode_mkdir: None,
                inode_mknod: None,
                inode_symlink: None,
                inode_link: None,
                inode_unlink: None,
                inode_rmdir: None,
                inode_rename: None,
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

        fn inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
            self.inode_xattr.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) {
            if let Some(hook) = self.inode_post_xattr {
                hook(context);
            }
        }

        fn inode_setattr(&self, context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
            self.inode_setattr.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>) {
            if let Some(hook) = self.inode_post_setattr {
                hook(context);
            }
        }

        fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_create.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_mkdir.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_mknod.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_symlink.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_link.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_unlink(&self, context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_unlink.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_rmdir(&self, context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            self.inode_rmdir.map_or(Ok(()), |hook| hook(context))
        }

        fn inode_rename(
            &self,
            context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        ) -> AxResult<()> {
            self.inode_rename.map_or(Ok(()), |hook| hook(context))
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

        fn inode_setattr(&self, _context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
            WHOLE_MODULE_SETATTR_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_post_setattr(&self, _context: &InodePostSetattrSecurityContext<'_, '_>) {
            WHOLE_MODULE_SETATTR_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
        }

        fn inode_create(&self, _context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mkdir(&self, _context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mknod(&self, _context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn inode_symlink(
            &self,
            _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }

        fn inode_link(&self, _context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_LINK_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_unlink(&self, _context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rmdir(&self, _context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rename(
            &self,
            _context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
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

        fn inode_setattr(&self, _context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
            WHOLE_MODULE_SETATTR_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_post_setattr(&self, _context: &InodePostSetattrSecurityContext<'_, '_>) {
            WHOLE_MODULE_SETATTR_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
        }

        fn inode_create(&self, _context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mkdir(&self, _context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn inode_mknod(&self, _context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
            Ok(())
        }

        fn inode_symlink(
            &self,
            _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_CREATE_HOOK_TRACE.fetch_add(1 << 24, Ordering::SeqCst);
            Ok(())
        }

        fn inode_link(&self, _context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_LINK_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_unlink(&self, _context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rmdir(&self, _context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1 << 8, Ordering::SeqCst);
            Ok(())
        }

        fn inode_rename(
            &self,
            _context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        ) -> AxResult<()> {
            WHOLE_MODULE_REMOVE_HOOK_TRACE.fetch_add(1 << 16, Ordering::SeqCst);
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

    struct ExecPathwalkSecurityTestProbe {
        denied_directory: InodeIdentity,
        observed_actor: AtomicPtr<Cred>,
        denial_trace: AtomicU32,
        terminal_hooks: AtomicU32,
    }

    impl ExecPathwalkSecurityTestProbe {
        fn new(denied_directory: InodeIdentity) -> Arc<Self> {
            Arc::new(Self {
                denied_directory,
                observed_actor: AtomicPtr::new(core::ptr::null_mut()),
                denial_trace: AtomicU32::new(0),
                terminal_hooks: AtomicU32::new(0),
            })
        }

        fn observe_actor(&self, actor: &Cred) {
            let actor = actor as *const Cred as *mut Cred;
            let previous = self.observed_actor.swap(actor, Ordering::SeqCst);
            assert!(previous.is_null() || previous == actor);
        }

        fn observe_search(
            &self,
            context: &InodePermissionSecurityContext<'_, '_>,
            order: u32,
            deny: bool,
        ) -> AxResult<()> {
            if context.target_object().identity() != self.denied_directory {
                return Ok(());
            }
            assert_eq!(context.access(), InodePermissionAccess::EXECUTE);
            self.observe_actor(context.actor());
            self.denial_trace
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |trace| {
                    Some(trace * 10 + order)
                })
                .unwrap();
            if deny {
                Err(AxError::PermissionDenied)
            } else {
                Ok(())
            }
        }

        fn observe_terminal(&self, actor: &Cred) {
            self.observe_actor(actor);
            self.terminal_hooks.fetch_add(1, Ordering::SeqCst);
        }

        fn reset(&self) {
            self.observed_actor
                .store(core::ptr::null_mut(), Ordering::SeqCst);
            self.denial_trace.store(0, Ordering::SeqCst);
            self.terminal_hooks.store(0, Ordering::SeqCst);
        }

        fn assert_denied_before_terminal(&self, actor: &Cred) {
            assert_eq!(self.denial_trace.load(Ordering::SeqCst), 12);
            assert_eq!(self.terminal_hooks.load(Ordering::SeqCst), 0);
            assert_eq!(
                self.observed_actor.load(Ordering::SeqCst),
                actor as *const Cred as *mut Cred
            );
        }
    }

    struct ExecPathwalkSecurityTestModule<const KEY: u64> {
        probe: Arc<ExecPathwalkSecurityTestProbe>,
        order: u32,
        deny: bool,
    }

    impl<const KEY: u64> SecurityModule for ExecPathwalkSecurityTestModule<KEY> {
        const KEY: ModuleKey = ModuleKey(KEY);
        type CredentialState = ();

        fn try_boot_init() -> Result<Self, RegistryBuildError> {
            unreachable!("exec pathwalk test modules are registered as initialized instances")
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
            self.probe.observe_search(context, self.order, self.deny)
        }

        fn exec_executable(&self, context: &ExecExecutableSecurityContext<'_>) -> AxResult<()> {
            self.probe.observe_terminal(context.actor());
            Ok(())
        }
    }

    fn exec_pathwalk_security_test_registry(
        probe: Arc<ExecPathwalkSecurityTestProbe>,
    ) -> FrozenSecurityRegistry {
        const FIRST_KEY: u64 = 0x7061_7468_0000_0001;
        const DENY_KEY: u64 = 0x7061_7468_0000_0002;
        const MUST_NOT_RUN_KEY: u64 = 0x7061_7468_0000_0003;

        let mut builder = test_registry_builder();
        for result in [
            builder.try_register_initialized(ExecPathwalkSecurityTestModule::<FIRST_KEY> {
                probe: probe.clone(),
                order: 1,
                deny: false,
            }),
            builder.try_register_initialized(ExecPathwalkSecurityTestModule::<DENY_KEY> {
                probe: probe.clone(),
                order: 2,
                deny: true,
            }),
            builder.try_register_initialized(ExecPathwalkSecurityTestModule::<MUST_NOT_RUN_KEY> {
                probe,
                order: 3,
                deny: false,
            }),
        ] {
            result.unwrap();
        }
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
        let parent_location = inode_location.parent().unwrap();
        let parent_metadata = parent_location.metadata().unwrap();
        let parent_object = InodeSecurityRef::new(&parent_location, &parent_metadata);
        let planned_entry = PlannedInodeSecurityRef::new(parent_object, "planned-entry");
        let directory_location = parent_location
            .create(
                "security-hook-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let directory_metadata = directory_location.metadata().unwrap();
        let directory_object = InodeSecurityRef::new(&directory_location, &directory_metadata);
        let unlink_entry =
            ExistingInodeSecurityRef::new(parent_object, inode_object, "security-hook");
        let rmdir_entry = ExistingInodeSecurityRef::new(
            parent_object,
            directory_object,
            "security-hook-directory",
        );
        let rename_destination =
            RenameDestinationSecurityRef::absent(directory_object, "renamed-entry");
        let dac_credential = root.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(root.user_ns());
        let inode_permission = InodePermissionSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            InodePermissionAccess::READ,
        );
        let inode_setattr = InodeSetattrSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            inode_object,
            InodeSetattrProposal::chmod(InodeChmodIntent::new(
                InodeSetattrMode::try_from_bits(0o600).unwrap(),
            )),
        );
        let file_open = FileOpenSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            FileOpenOperation::new(FileOpenAccess::Read, false, false, false, false).unwrap(),
        );
        let inode_create = InodeCreateSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o640).unwrap(),
        );
        let inode_mkdir = InodeMkdirSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o750).unwrap(),
        );
        let inode_mknod = InodeMknodSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeMknodOperation::new(
                InodeMknodKind::CharacterDevice,
                InodeCreateMode::try_from_bits(0o600).unwrap(),
                Some(0x1234),
            )
            .unwrap(),
        );
        let inode_symlink = InodeSymlinkSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            "../target",
        );
        let inode_link = InodeLinkSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            &planned_entry,
        );
        let inode_unlink =
            InodeUnlinkSecurityContext::new(&root, &dac_credential, &owner_user_ns, &unlink_entry);
        let inode_rmdir =
            InodeRmdirSecurityContext::new(&root, &dac_credential, &owner_user_ns, &rmdir_entry);
        let inode_rename = InodeRenameSecurityContext::new(
            &root,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
            &rename_destination,
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
        dispatch_inode_setattr(inode_setattr).unwrap().committed(
            InodeSetattrCommittedSecurityRef::new(&inode_location, &inode_metadata),
        );
        dispatch.dispatch_inode_create(&inode_create).unwrap();
        dispatch.dispatch_inode_mkdir(&inode_mkdir).unwrap();
        dispatch.dispatch_inode_mknod(&inode_mknod).unwrap();
        dispatch.dispatch_inode_symlink(&inode_symlink).unwrap();
        dispatch.dispatch_inode_link(&inode_link).unwrap();
        dispatch.dispatch_inode_unlink(&inode_unlink).unwrap();
        dispatch.dispatch_inode_rmdir(&inode_rmdir).unwrap();
        dispatch.dispatch_inode_rename(&inode_rename).unwrap();
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
        let caps = update.builder.caps;
        update.builder.caps = capability_state_for_test(
            capability_set(effective),
            capability_set(permitted),
            [0; CAPABILITY_WORDS],
            caps.bounding(),
            [0; CAPABILITY_WORDS],
            caps.securebits(),
        );
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
        let caps = update.builder.caps;
        update.builder.caps = capability_state_for_test(
            capability_set(effective),
            capability_set(permitted),
            [0; CAPABILITY_WORDS],
            caps.bounding(),
            [0; CAPABILITY_WORDS],
            caps.securebits(),
        );
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

    fn assert_ordered_inode_xattr(context: &InodeXattrSecurityContext<'_, '_>) {
        assert_eq!(context.target_object().mode(), 0o640);
        assert_eq!(
            context.operation().name(),
            Some(b"security.capability".as_slice())
        );
        assert_eq!(
            context.operation().value(),
            Some([0xd0, 0x01, 0x02].as_slice())
        );
        assert_eq!(
            context.operation().set_flags(),
            Some(XattrSetFlags::REPLACE)
        );
        assert_eq!(
            context.operation().value_class(),
            Some(XattrValueClass::SecurityCapability)
        );
    }

    fn ordered_inode_xattr_first(context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        assert_ordered_inode_xattr(context);
        assert_eq!(INODE_XATTR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_xattr_second(context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        assert_ordered_inode_xattr(context);
        assert_eq!(INODE_XATTR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn ordered_inode_post_xattr_first(context: &InodeXattrSecurityContext<'_, '_>) {
        assert_ordered_inode_xattr(context);
        assert_eq!(INODE_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
    }

    fn ordered_inode_post_xattr_second(context: &InodeXattrSecurityContext<'_, '_>) {
        assert_ordered_inode_xattr(context);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
    }

    fn deny_inode_xattr_first(_context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_XATTR_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_xattr_must_not_run(_context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_XATTR_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn inode_post_xattr_must_not_run(_context: &InodeXattrSecurityContext<'_, '_>) {
        INODE_POST_XATTR_HOOK_TRACE.store(4, Ordering::SeqCst);
    }

    fn ordered_inode_setattr_first(context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        assert_eq!(context.target_object().mode(), 0o640);
        let requested_mode = match context.intent() {
            InodeSetattrIntent::Chmod(intent) => intent.mode(),
            _ => panic!("ordered setattr test received a non-chmod intent"),
        };
        assert_eq!(
            context.proposal().mode().unwrap().bits(),
            requested_mode.bits()
        );
        assert_eq!(INODE_SETATTR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_setattr_second(
        _context: &InodeSetattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_SETATTR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn ordered_inode_post_setattr_first(context: &InodePostSetattrSecurityContext<'_, '_>) {
        assert_eq!(context.committed_object().mode(), 0o640);
        assert_eq!(context.proposal().mode().unwrap().bits(), 0o600);
        assert_eq!(INODE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
    }

    fn ordered_inode_post_setattr_second(_context: &InodePostSetattrSecurityContext<'_, '_>) {
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
    }

    fn deny_inode_setattr_first(_context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_SETATTR_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_setattr_must_not_run(_context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        INODE_SETATTR_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn inode_post_setattr_must_not_run(_context: &InodePostSetattrSecurityContext<'_, '_>) {
        INODE_POST_SETATTR_HOOK_TRACE.store(4, Ordering::SeqCst);
    }

    fn ordered_inode_create_first(
        context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(context.new_entry_object().name(), "ordered-entry");
        assert_eq!(context.mode().bits(), 0o640);
        assert_eq!(INODE_CREATE_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_create_second(
        _context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_CREATE_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_create_first(_context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_CREATE_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_create_must_not_run(
        _context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        INODE_CREATE_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_mkdir_first(context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(context.new_entry_object().name(), "ordered-entry");
        assert_eq!(context.mode().bits(), 0o750);
        assert_eq!(INODE_MKDIR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_mkdir_second(
        _context: &InodeMkdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_MKDIR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_mkdir_first(_context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_MKDIR_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_mkdir_must_not_run(_context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_MKDIR_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_mknod_first(context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(context.new_entry_object().name(), "ordered-entry");
        assert_eq!(context.operation().kind(), InodeMknodKind::CharacterDevice);
        assert_eq!(context.operation().rdev(), Some(0x1234));
        assert_eq!(INODE_MKNOD_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_mknod_second(
        _context: &InodeMknodSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_MKNOD_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_mknod_first(_context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_MKNOD_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_mknod_must_not_run(_context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_MKNOD_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_symlink_first(
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(context.new_entry_object().name(), "ordered-entry");
        assert_eq!(context.symlink_target(), "../ordered-target");
        assert_eq!(INODE_SYMLINK_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_symlink_second(
        _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_SYMLINK_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_symlink_first(
        _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        INODE_SYMLINK_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_symlink_must_not_run(
        _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        INODE_SYMLINK_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_link_first(context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(context.new_entry_object().name(), "ordered-entry");
        assert_eq!(context.source_object().node_kind(), NodeType::RegularFile);
        assert_ne!(
            context.source_object().identity(),
            context.parent_object().identity()
        );
        assert_eq!(INODE_LINK_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_link_second(_context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(INODE_LINK_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_link_first(_context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_LINK_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_link_must_not_run(_context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_LINK_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_unlink_first(
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(context.target_entry_object().name(), "security-hook");
        assert_eq!(
            context.target_entry_object().target_object().node_kind(),
            NodeType::RegularFile
        );
        assert_ne!(
            context.target_entry_object().target_object().identity(),
            context.parent_object().identity()
        );
        assert_eq!(INODE_UNLINK_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_unlink_second(
        _context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_UNLINK_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_unlink_first(_context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_UNLINK_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_unlink_must_not_run(
        _context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        INODE_UNLINK_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_rmdir_first(context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(context.target_entry_object().name(), "ordered-rmdir-entry");
        assert_eq!(
            context.target_entry_object().target_object().node_kind(),
            NodeType::Directory
        );
        assert_ne!(
            context.target_entry_object().target_object().identity(),
            context.parent_object().identity()
        );
        assert_eq!(INODE_RMDIR_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_rmdir_second(
        _context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_RMDIR_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_rmdir_first(_context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_RMDIR_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_rmdir_must_not_run(_context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        INODE_RMDIR_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn ordered_inode_rename_first(
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(context.old_entry_object().name(), "security-hook");
        assert_eq!(
            context.old_entry_object().target_object().node_kind(),
            NodeType::RegularFile
        );
        assert_eq!(context.new_entry_object().name(), "ordered-rename-entry");
        assert!(context.new_entry_object().target_object().is_none());
        assert_ne!(
            context.old_parent_object().identity(),
            context.new_parent_object().identity()
        );
        assert_eq!(INODE_RENAME_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn ordered_inode_rename_second(
        _context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(INODE_RENAME_HOOK_TRACE.swap(2, Ordering::SeqCst), 1);
        Ok(())
    }

    fn deny_inode_rename_first(
        _context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        INODE_RENAME_HOOK_TRACE.store(3, Ordering::SeqCst);
        Err(AxError::PermissionDenied)
    }

    fn inode_rename_must_not_run(
        _context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        INODE_RENAME_HOOK_TRACE.store(4, Ordering::SeqCst);
        Ok(())
    }

    fn observe_hardlink_transaction(
        context: &InodeLinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_ne!(
            context.source_object().identity(),
            context.parent_object().identity()
        );
        assert_eq!(
            HARDLINK_VERTICAL_HOOK_TRACE.fetch_add(1, Ordering::SeqCst),
            0
        );
        Ok(())
    }

    fn deny_hardlink_transaction(context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        observe_hardlink_transaction(context)?;
        Err(AxError::PermissionDenied)
    }

    fn hardlink_transaction_must_not_run(
        _context: &InodeLinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        HARDLINK_VERTICAL_HOOK_TRACE.fetch_add(100, Ordering::SeqCst);
        Ok(())
    }

    fn hardlink_vertical_registry(
        first: TestInodeLinkHook,
        second: Option<TestInodeLinkHook>,
    ) -> FrozenSecurityRegistry {
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_link: Some(first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        if let Some(second) = second {
            builder
                .try_register_initialized(TestSecurityModule::<2> {
                    inode_link: Some(second),
                    ..TestSecurityModule::empty()
                })
                .unwrap();
        }
        freeze_test_registry(builder.freeze())
    }

    fn assert_metadata_preserved(before: &Metadata, after: &Metadata) {
        assert_eq!(after.device, before.device);
        assert_eq!(after.inode, before.inode);
        assert_eq!(after.nlink, before.nlink);
        assert_eq!(after.mode.bits(), before.mode.bits());
        assert_eq!(after.node_type, before.node_type);
        assert_eq!(after.uid, before.uid);
        assert_eq!(after.gid, before.gid);
        assert_eq!(after.size, before.size);
        assert_eq!(after.block_size, before.block_size);
        assert_eq!(after.blocks, before.blocks);
        assert_eq!(after.rdev, before.rdev);
        assert_eq!(after.atime, before.atime);
        assert_eq!(after.btime, before.btime);
        assert_eq!(after.mtime, before.mtime);
        assert_eq!(after.ctime, before.ctime);
    }

    fn observe_unlink_transaction(
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        assert_eq!(context.target_entry_object().name(), "denied-unlink");
        assert!(core::ptr::eq(
            context.parent_object(),
            context.target_entry_object().parent_object()
        ));
        assert_eq!(context.parent_object().node_kind(), NodeType::Directory);
        assert_eq!(
            context.target_entry_object().target_object().node_kind(),
            NodeType::RegularFile
        );
        assert_ne!(
            context.parent_object().identity(),
            context.target_entry_object().target_object().identity()
        );
        assert_eq!(UNLINK_VERTICAL_HOOK_TRACE.fetch_add(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn deny_unlink_transaction(context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        observe_unlink_transaction(context)?;
        Err(AxError::PermissionDenied)
    }

    fn unlink_transaction_must_not_run(
        _context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        UNLINK_VERTICAL_HOOK_TRACE.fetch_add(100, Ordering::SeqCst);
        Ok(())
    }

    fn observe_rmdir_transaction(
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        expected_name: &str,
    ) -> AxResult<()> {
        assert_eq!(context.target_entry_object().name(), expected_name);
        assert!(core::ptr::eq(
            context.parent_object(),
            context.target_entry_object().parent_object()
        ));
        assert_eq!(context.parent_object().node_kind(), NodeType::Directory);
        assert_eq!(
            context.target_entry_object().target_object().node_kind(),
            NodeType::Directory
        );
        assert_ne!(
            context.parent_object().identity(),
            context.target_entry_object().target_object().identity()
        );
        assert_eq!(RMDIR_VERTICAL_HOOK_TRACE.fetch_add(1, Ordering::SeqCst), 0);
        Ok(())
    }

    fn deny_rmdir_transaction(context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        observe_rmdir_transaction(context, "denied-rmdir")?;
        Err(AxError::PermissionDenied)
    }

    fn observe_nonempty_rmdir_transaction(
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        observe_rmdir_transaction(context, "nonempty-directory")
    }

    fn rmdir_transaction_must_not_run(
        _context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        RMDIR_VERTICAL_HOOK_TRACE.fetch_add(100, Ordering::SeqCst);
        Ok(())
    }

    fn unlink_vertical_registry(
        first: TestInodeUnlinkHook,
        second: Option<TestInodeUnlinkHook>,
    ) -> FrozenSecurityRegistry {
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_unlink: Some(first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        if let Some(second) = second {
            builder
                .try_register_initialized(TestSecurityModule::<2> {
                    inode_unlink: Some(second),
                    ..TestSecurityModule::empty()
                })
                .unwrap();
        }
        freeze_test_registry(builder.freeze())
    }

    fn rmdir_vertical_registry(
        first: TestInodeRmdirHook,
        second: Option<TestInodeRmdirHook>,
    ) -> FrozenSecurityRegistry {
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_rmdir: Some(first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        if let Some(second) = second {
            builder
                .try_register_initialized(TestSecurityModule::<2> {
                    inode_rmdir: Some(second),
                    ..TestSecurityModule::empty()
                })
                .unwrap();
        }
        freeze_test_registry(builder.freeze())
    }

    fn deny_symlink_transaction(context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        assert_eq!(context.parent_object().node_kind(), NodeType::Directory);
        assert_eq!(context.parent_object().mode() & 0o777, 0o777);
        assert_eq!(context.new_entry_object().name(), "denied-symlink");
        assert_eq!(context.symlink_target(), "../unresolved-target");
        assert_eq!(SYMLINK_VERTICAL_HOOK_TRACE.swap(1, Ordering::SeqCst), 0);
        Err(AxError::PermissionDenied)
    }

    fn symlink_transaction_must_not_run(
        _context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        SYMLINK_VERTICAL_HOOK_TRACE.store(2, Ordering::SeqCst);
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
    fn named_entry_contexts_bind_exact_source_parent_entry_actor_and_final_facts() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let child_namespace = namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let child = security_test_inode();
        let source_metadata = child.metadata().unwrap();
        let source_object = InodeSecurityRef::new(&child, &source_metadata);
        let parent = child.parent().unwrap();
        let directory = parent
            .create(
                "exact-rmdir-target",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let directory_metadata = directory.metadata().unwrap();
        let directory_object = InodeSecurityRef::new(&directory, &directory_metadata);
        let mut metadata = parent.metadata().unwrap();
        metadata.mode = NodePermission::from_bits_truncate(0o3770);
        metadata.uid = 1101;
        metadata.gid = 1102;
        let expected_identity = InodeIdentity::new(
            parent.mountpoint().mount_id(),
            metadata.device,
            metadata.inode,
        );
        let parent_object = InodeSecurityRef::new(&parent, &metadata);
        let final_name = std::string::String::from("exact-final-name");
        let planned_entry = PlannedInodeSecurityRef::new(parent_object, final_name.as_str());
        let unlink_name = std::string::String::from("security-hook");
        let unlink_entry =
            ExistingInodeSecurityRef::new(parent_object, source_object, unlink_name.as_str());
        let rmdir_name = std::string::String::from("exact-rmdir-target");
        let rmdir_entry =
            ExistingInodeSecurityRef::new(parent_object, directory_object, rmdir_name.as_str());

        metadata.mode = NodePermission::empty();
        metadata.uid = 0;
        metadata.gid = 0;

        assert_eq!(planned_entry.parent_object().identity(), expected_identity);
        assert_eq!(planned_entry.parent_object().mode(), 0o3770);
        assert_eq!(planned_entry.parent_object().uid(), 1101);
        assert_eq!(planned_entry.parent_object().gid(), 1102);
        assert!(core::ptr::eq(planned_entry.name(), final_name.as_str()));
        assert_eq!(planned_entry.name(), "exact-final-name");

        let dac_credential = security_test_dac(2101, 2102);
        let owner_user_ns = initial_user_namespace(&child_namespace);
        let create_mode = InodeCreateMode::try_from_bits(0o2640).unwrap();
        let mkdir_mode = InodeCreateMode::try_from_bits(0o1750).unwrap();
        let mknod_operation = InodeMknodOperation::new(
            InodeMknodKind::BlockDevice,
            InodeCreateMode::try_from_bits(0o660).unwrap(),
            Some(0x1234_5678),
        )
        .unwrap();
        let create = InodeCreateSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            create_mode,
        );
        let mkdir = InodeMkdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            mkdir_mode,
        );
        let mknod = InodeMknodSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            mknod_operation,
        );
        let symlink_target = std::string::String::from("../exact-target");
        let symlink = InodeSymlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            symlink_target.as_str(),
        );
        let link = InodeLinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &source_object,
            &planned_entry,
        );
        let unlink = InodeUnlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
        );
        let rmdir = InodeRmdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &rmdir_entry,
        );

        assert!(core::ptr::eq(create.actor(), credential.as_ref()));
        assert!(core::ptr::eq(create.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(create.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            create.parent_object(),
            planned_entry.parent_object()
        ));
        assert!(core::ptr::eq(create.new_entry_object(), &planned_entry));
        assert!(core::ptr::eq(create.core().actor(), credential.core()));
        assert_eq!(create.mode(), create_mode);
        assert_eq!(create.core().mode(), create_mode);

        assert!(core::ptr::eq(mkdir.actor(), credential.as_ref()));
        assert!(core::ptr::eq(mkdir.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(mkdir.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            mkdir.parent_object(),
            planned_entry.parent_object()
        ));
        assert!(core::ptr::eq(mkdir.new_entry_object(), &planned_entry));
        assert!(core::ptr::eq(mkdir.core().actor(), credential.core()));
        assert_eq!(mkdir.mode(), mkdir_mode);
        assert_eq!(mkdir.core().mode(), mkdir_mode);

        assert!(core::ptr::eq(mknod.actor(), credential.as_ref()));
        assert!(core::ptr::eq(mknod.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(mknod.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            mknod.parent_object(),
            planned_entry.parent_object()
        ));
        assert!(core::ptr::eq(mknod.new_entry_object(), &planned_entry));
        assert!(core::ptr::eq(mknod.core().actor(), credential.core()));
        assert_eq!(mknod.operation(), mknod_operation);
        assert_eq!(mknod.core().operation(), mknod_operation);

        assert!(core::ptr::eq(symlink.actor(), credential.as_ref()));
        assert!(core::ptr::eq(symlink.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(symlink.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            symlink.parent_object(),
            planned_entry.parent_object()
        ));
        assert!(core::ptr::eq(symlink.new_entry_object(), &planned_entry));
        assert!(core::ptr::eq(symlink.core().actor(), credential.core()));
        assert!(core::ptr::eq(
            symlink.symlink_target(),
            symlink_target.as_str()
        ));
        assert_eq!(symlink.symlink_target(), "../exact-target");

        assert!(core::ptr::eq(link.actor(), credential.as_ref()));
        assert!(core::ptr::eq(link.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(link.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(link.source_object(), &source_object));
        assert!(core::ptr::eq(
            link.parent_object(),
            planned_entry.parent_object()
        ));
        assert!(core::ptr::eq(link.new_entry_object(), &planned_entry));
        assert!(core::ptr::eq(link.core().actor(), credential.core()));
        assert!(core::ptr::eq(link.core().source_object(), &source_object));
        assert_ne!(
            link.source_object().identity(),
            link.parent_object().identity()
        );

        assert!(core::ptr::eq(
            unlink_entry.parent_object(),
            unlink.parent_object()
        ));
        assert!(core::ptr::eq(
            unlink_entry.target_object(),
            unlink.target_entry_object().target_object()
        ));
        assert!(core::ptr::eq(unlink_entry.name(), unlink_name.as_str()));
        assert!(core::ptr::eq(unlink.actor(), credential.as_ref()));
        assert!(core::ptr::eq(unlink.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(unlink.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(unlink.target_entry_object(), &unlink_entry));
        assert!(core::ptr::eq(unlink.core().actor(), credential.core()));
        assert!(core::ptr::eq(
            unlink.core().dac_credential(),
            &dac_credential
        ));
        assert!(Arc::ptr_eq(
            unlink.core().target_owner_user_ns(),
            &namespace
        ));
        assert!(core::ptr::eq(
            unlink.core().parent_object(),
            unlink_entry.parent_object()
        ));
        assert!(core::ptr::eq(
            unlink.core().target_entry_object(),
            &unlink_entry
        ));

        assert!(core::ptr::eq(
            rmdir_entry.parent_object(),
            rmdir.parent_object()
        ));
        assert!(core::ptr::eq(
            rmdir_entry.target_object(),
            rmdir.target_entry_object().target_object()
        ));
        assert!(core::ptr::eq(rmdir_entry.name(), rmdir_name.as_str()));
        assert!(core::ptr::eq(rmdir.actor(), credential.as_ref()));
        assert!(core::ptr::eq(rmdir.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(rmdir.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(rmdir.target_entry_object(), &rmdir_entry));
        assert!(core::ptr::eq(rmdir.core().actor(), credential.core()));
        assert!(core::ptr::eq(
            rmdir.core().dac_credential(),
            &dac_credential
        ));
        assert!(Arc::ptr_eq(rmdir.core().target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            rmdir.core().parent_object(),
            rmdir_entry.parent_object()
        ));
        assert!(core::ptr::eq(
            rmdir.core().target_entry_object(),
            &rmdir_entry
        ));

        assert_eq!(unlink.target_entry_object().name(), "security-hook");
        assert_eq!(
            unlink.target_entry_object().target_object().node_kind(),
            NodeType::RegularFile
        );
        assert_eq!(rmdir.target_entry_object().name(), "exact-rmdir-target");
        assert_eq!(
            rmdir.target_entry_object().target_object().node_kind(),
            NodeType::Directory
        );
        assert_ne!(
            unlink.target_entry_object().target_object().identity(),
            rmdir.target_entry_object().target_object().identity()
        );
        assert_ne!(
            unlink.target_entry_object().name(),
            rmdir.target_entry_object().name()
        );
    }

    #[test]
    fn inode_rename_context_binds_four_roles_and_absent_or_existing_destination() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let child_namespace = namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let source = security_test_inode();
        let source_metadata = source.metadata().unwrap();
        let source_object = InodeSecurityRef::new(&source, &source_metadata);
        let old_parent = source.parent().unwrap();
        let old_parent_metadata = old_parent.metadata().unwrap();
        let old_parent_object = InodeSecurityRef::new(&old_parent, &old_parent_metadata);
        let old_name = std::string::String::from("security-hook");
        let old_entry =
            ExistingInodeSecurityRef::new(old_parent_object, source_object, old_name.as_str());

        let new_parent = old_parent
            .create(
                "exact-rename-parent",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o770),
            )
            .unwrap();
        let new_parent_metadata = new_parent.metadata().unwrap();
        let new_parent_object = InodeSecurityRef::new(&new_parent, &new_parent_metadata);
        let existing_target = new_parent
            .create(
                "existing-rename-target",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let existing_target_metadata = existing_target.metadata().unwrap();
        let existing_target_object =
            InodeSecurityRef::new(&existing_target, &existing_target_metadata);
        let absent_name = std::string::String::from("absent-rename-target");
        let existing_name = std::string::String::from("existing-rename-target");
        let absent_destination =
            RenameDestinationSecurityRef::absent(new_parent_object, absent_name.as_str());
        let existing_destination = RenameDestinationSecurityRef::existing(
            new_parent_object,
            existing_target_object,
            existing_name.as_str(),
        );
        let dac_credential = security_test_dac(2201, 2202);
        let owner_user_ns = initial_user_namespace(&child_namespace);
        let absent = InodeRenameSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &old_entry,
            &absent_destination,
        );
        let existing = InodeRenameSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &old_entry,
            &existing_destination,
        );

        assert!(core::ptr::eq(absent.actor(), credential.as_ref()));
        assert!(core::ptr::eq(absent.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(absent.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(
            absent.old_parent_object(),
            old_entry.parent_object()
        ));
        assert!(core::ptr::eq(absent.old_entry_object(), &old_entry));
        assert!(core::ptr::eq(
            absent.new_parent_object(),
            absent_destination.parent_object()
        ));
        assert!(core::ptr::eq(
            absent.new_entry_object(),
            &absent_destination
        ));
        assert!(core::ptr::eq(absent.core().actor(), credential.core()));
        assert!(core::ptr::eq(
            absent.core().dac_credential(),
            &dac_credential
        ));
        assert!(Arc::ptr_eq(
            absent.core().target_owner_user_ns(),
            &namespace
        ));
        assert!(core::ptr::eq(
            absent.core().old_parent_object(),
            old_entry.parent_object()
        ));
        assert!(core::ptr::eq(absent.core().old_entry_object(), &old_entry));
        assert!(core::ptr::eq(
            absent.core().new_parent_object(),
            absent_destination.parent_object()
        ));
        assert!(core::ptr::eq(
            absent.core().new_entry_object(),
            &absent_destination
        ));
        assert_eq!(absent.old_entry_object().name(), "security-hook");
        assert!(core::ptr::eq(
            absent.old_entry_object().name(),
            old_name.as_str()
        ));
        assert_eq!(
            absent.old_entry_object().target_object().identity(),
            source_object.identity()
        );
        assert_eq!(absent.new_entry_object().name(), "absent-rename-target");
        assert!(core::ptr::eq(
            absent.new_entry_object().name(),
            absent_name.as_str()
        ));
        assert!(absent.new_entry_object().target_object().is_none());
        assert_ne!(
            absent.old_parent_object().identity(),
            absent.new_parent_object().identity()
        );

        assert!(core::ptr::eq(existing.actor(), credential.as_ref()));
        assert!(core::ptr::eq(existing.old_entry_object(), &old_entry));
        assert!(core::ptr::eq(
            existing.new_entry_object(),
            &existing_destination
        ));
        assert_eq!(
            existing.new_parent_object().identity(),
            new_parent_object.identity()
        );
        assert_eq!(
            existing
                .new_entry_object()
                .target_object()
                .unwrap()
                .identity(),
            existing_target_object.identity()
        );
        assert_eq!(
            existing
                .new_entry_object()
                .target_object()
                .unwrap()
                .node_kind(),
            NodeType::RegularFile
        );
        assert_eq!(existing.new_entry_object().name(), "existing-rename-target");
        assert_ne!(
            existing.old_entry_object().target_object().identity(),
            existing
                .new_entry_object()
                .target_object()
                .unwrap()
                .identity()
        );
    }

    #[test]
    fn whole_module_registration_is_atomic_across_every_hook_family() {
        let mut builder = test_registry_builder();
        builder.try_register::<WholeHookModule>().unwrap();
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_CREATE_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_LINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_REMOVE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(registry);
        assert_eq!(
            WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst),
            0x0101_0101_0101_0101
        );
        assert_eq!(
            WHOLE_MODULE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst),
            0x0101
        );
        assert_eq!(
            WHOLE_MODULE_CREATE_HOOK_TRACE.load(Ordering::SeqCst),
            0x0101_0101
        );
        assert_eq!(WHOLE_MODULE_LINK_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(
            WHOLE_MODULE_REMOVE_HOOK_TRACE.load(Ordering::SeqCst),
            0x01_0101
        );

        let mut builder = test_registry_builder();
        assert_eq!(
            builder.try_register::<FailingWholeHookModule>(),
            Err(RegistryBuildError::ModuleInitFailed)
        );
        let registry = builder.freeze();

        WHOLE_MODULE_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_CREATE_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_LINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        WHOLE_MODULE_REMOVE_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_all_hook_families(registry);
        assert_eq!(WHOLE_MODULE_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(WHOLE_MODULE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(WHOLE_MODULE_CREATE_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(WHOLE_MODULE_LINK_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(WHOLE_MODULE_REMOVE_HOOK_TRACE.load(Ordering::SeqCst), 0);
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
    fn vfs_security_exec_pathwalk_denies_intermediate_and_symlink_target_before_terminal_hook() {
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let root = mount.root_location();
        let denied_directory = root
            .create(
                "denied-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let executable = denied_directory
            .create(
                "program",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        root.create_symlink(
            "jump",
            "denied-directory/program",
            NodePermission::from_bits_truncate(0o777),
            Some((0, 0)),
        )
        .unwrap();

        let denied_metadata = denied_directory.metadata().unwrap();
        let probe = ExecPathwalkSecurityTestProbe::new(
            InodeSecurityRef::new(&denied_directory, &denied_metadata).identity(),
        );
        let registry = exec_pathwalk_security_test_registry(probe.clone());
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(actor);
        let context = axfs::FsContext::new(root);
        let executable_metadata = executable.metadata().unwrap();
        let executable_object = ExecFileSecurityObject::new(
            ExecFileIdentity::new(executable.mountpoint().device(), executable.inode()),
            security.filesystem_owner_user_ns().clone(),
            None,
            executable_metadata.mode.bits(),
            true,
            crate::task::ExecExecutableRole::Requested,
        );

        for path in ["denied-directory/program", "jump"] {
            probe.reset();
            let result = context.resolve_security(path, &security).and_then(|_| {
                dispatch_exec_executable(&ExecExecutableSecurityContext::new(
                    security.actor(),
                    &executable_object,
                ))
            });
            assert_eq!(result, Err(AxError::PermissionDenied), "path: {path}");
            probe.assert_denied_before_terminal(security.actor());
        }
    }

    #[test]
    fn inode_xattr_admission_orders_hooks_and_notifies_only_after_provider_success() {
        let location = security_test_inode();
        let metadata = location.metadata().unwrap();
        let object = InodeSecurityRef::new(&location, &metadata);
        let value = [0xd0, 0x01, 0x02];
        let operation =
            InodeXattrOperation::set(b"security.capability", &value, XattrSetFlags::REPLACE)
                .unwrap();

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_xattr: Some(ordered_inode_xattr_first),
                inode_post_xattr: Some(ordered_inode_post_xattr_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_xattr: Some(ordered_inode_xattr_second),
                inode_post_xattr: Some(ordered_inode_post_xattr_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = freeze_test_registry(builder.freeze());
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);

        INODE_XATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_POST_XATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        let admission = dispatch_inode_xattr(InodeXattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            object,
            operation,
        ))
        .unwrap();
        assert_eq!(INODE_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);

        // A failed provider simply drops the admission and cannot emit post.
        drop(admission);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);

        INODE_XATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_inode_xattr(InodeXattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            object,
            operation,
        ))
        .unwrap()
        .committed();
        assert_eq!(INODE_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<3> {
                inode_xattr: Some(deny_inode_xattr_first),
                inode_post_xattr: Some(inode_post_xattr_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<4> {
                inode_xattr: Some(inode_xattr_must_not_run),
                inode_post_xattr: Some(inode_post_xattr_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = freeze_test_registry(builder.freeze());
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);

        INODE_XATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_POST_XATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_xattr(InodeXattrSecurityContext::new(
                &credential,
                &dac_credential,
                &owner_user_ns,
                object,
                operation,
            ))
            .err(),
            Some(AxError::PermissionDenied)
        );
        assert_eq!(INODE_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_POST_XATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inode_setattr_admission_orders_pre_and_emits_post_only_on_commit() {
        let location = security_test_inode();
        let metadata = location.metadata().unwrap();
        let object = InodeSecurityRef::new(&location, &metadata);
        let proposal = InodeSetattrProposal::chmod(InodeChmodIntent::new(
            InodeSetattrMode::try_from_bits(0o600).unwrap(),
        ));

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_setattr: Some(ordered_inode_setattr_first),
                inode_post_setattr: Some(ordered_inode_post_setattr_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_setattr: Some(ordered_inode_setattr_second),
                inode_post_setattr: Some(ordered_inode_post_setattr_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = freeze_test_registry(builder.freeze());
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);

        INODE_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_POST_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        let admission = dispatch_inode_setattr(InodeSetattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            object,
            proposal,
        ))
        .unwrap();
        assert_eq!(INODE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);
        drop(admission);
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);

        INODE_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        dispatch_inode_setattr(InodeSetattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            object,
            proposal,
        ))
        .unwrap()
        .committed(InodeSetattrCommittedSecurityRef::new(&location, &metadata));
        assert_eq!(INODE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 2);

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<3> {
                inode_setattr: Some(deny_inode_setattr_first),
                inode_post_setattr: Some(inode_post_setattr_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<4> {
                inode_setattr: Some(inode_setattr_must_not_run),
                inode_post_setattr: Some(inode_post_setattr_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = freeze_test_registry(builder.freeze());
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);

        INODE_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_POST_SETATTR_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_setattr(InodeSetattrSecurityContext::new(
                &credential,
                &dac_credential,
                &owner_user_ns,
                object,
                proposal,
            ))
            .err(),
            Some(AxError::PermissionDenied)
        );
        assert_eq!(INODE_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_POST_SETATTR_HOOK_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn named_entry_hook_stacks_preserve_order_and_stop_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let child = security_test_inode();
        let source_metadata = child.metadata().unwrap();
        let source_object = InodeSecurityRef::new(&child, &source_metadata);
        let parent = child.parent().unwrap();
        let directory = parent
            .create(
                "ordered-rmdir-entry",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let directory_metadata = directory.metadata().unwrap();
        let directory_object = InodeSecurityRef::new(&directory, &directory_metadata);
        let metadata = parent.metadata().unwrap();
        let parent_object = InodeSecurityRef::new(&parent, &metadata);
        let planned_entry = PlannedInodeSecurityRef::new(parent_object, "ordered-entry");
        let unlink_entry =
            ExistingInodeSecurityRef::new(parent_object, source_object, "security-hook");
        let rmdir_entry =
            ExistingInodeSecurityRef::new(parent_object, directory_object, "ordered-rmdir-entry");
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let create = InodeCreateSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o640).unwrap(),
        );
        let mkdir = InodeMkdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o750).unwrap(),
        );
        let mknod = InodeMknodSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeMknodOperation::new(
                InodeMknodKind::CharacterDevice,
                InodeCreateMode::try_from_bits(0o600).unwrap(),
                Some(0x1234),
            )
            .unwrap(),
        );
        let symlink = InodeSymlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            "../ordered-target",
        );
        let link = InodeLinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &source_object,
            &planned_entry,
        );
        let unlink = InodeUnlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
        );
        let rmdir = InodeRmdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &rmdir_entry,
        );

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_create: Some(ordered_inode_create_first),
                inode_mkdir: Some(ordered_inode_mkdir_first),
                inode_mknod: Some(ordered_inode_mknod_first),
                inode_symlink: Some(ordered_inode_symlink_first),
                inode_link: Some(ordered_inode_link_first),
                inode_unlink: Some(ordered_inode_unlink_first),
                inode_rmdir: Some(ordered_inode_rmdir_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_create: Some(ordered_inode_create_second),
                inode_mkdir: Some(ordered_inode_mkdir_second),
                inode_mknod: Some(ordered_inode_mknod_second),
                inode_symlink: Some(ordered_inode_symlink_second),
                inode_link: Some(ordered_inode_link_second),
                inode_unlink: Some(ordered_inode_unlink_second),
                inode_rmdir: Some(ordered_inode_rmdir_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_CREATE_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_MKDIR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_MKNOD_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_SYMLINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_LINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_UNLINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_RMDIR_HOOK_TRACE.store(0, Ordering::SeqCst);
        registry.dispatch_inode_create(&create).unwrap();
        registry.dispatch_inode_mkdir(&mkdir).unwrap();
        registry.dispatch_inode_mknod(&mknod).unwrap();
        registry.dispatch_inode_symlink(&symlink).unwrap();
        registry.dispatch_inode_link(&link).unwrap();
        registry.dispatch_inode_unlink(&unlink).unwrap();
        registry.dispatch_inode_rmdir(&rmdir).unwrap();
        assert_eq!(INODE_CREATE_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_MKDIR_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_MKNOD_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_SYMLINK_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_LINK_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_UNLINK_HOOK_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(INODE_RMDIR_HOOK_TRACE.load(Ordering::SeqCst), 2);

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_create: Some(deny_inode_create_first),
                inode_mkdir: Some(deny_inode_mkdir_first),
                inode_mknod: Some(deny_inode_mknod_first),
                inode_symlink: Some(deny_inode_symlink_first),
                inode_link: Some(deny_inode_link_first),
                inode_unlink: Some(deny_inode_unlink_first),
                inode_rmdir: Some(deny_inode_rmdir_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_create: Some(inode_create_must_not_run),
                inode_mkdir: Some(inode_mkdir_must_not_run),
                inode_mknod: Some(inode_mknod_must_not_run),
                inode_symlink: Some(inode_symlink_must_not_run),
                inode_link: Some(inode_link_must_not_run),
                inode_unlink: Some(inode_unlink_must_not_run),
                inode_rmdir: Some(inode_rmdir_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_CREATE_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_MKDIR_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_MKNOD_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_SYMLINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_LINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_UNLINK_HOOK_TRACE.store(0, Ordering::SeqCst);
        INODE_RMDIR_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_inode_create(&create),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_mkdir(&mkdir),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_mknod(&mknod),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_symlink(&symlink),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_link(&link),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_unlink(&unlink),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(
            registry.dispatch_inode_rmdir(&rmdir),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(INODE_CREATE_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_MKDIR_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_MKNOD_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_SYMLINK_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_LINK_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_UNLINK_HOOK_TRACE.load(Ordering::SeqCst), 3);
        assert_eq!(INODE_RMDIR_HOOK_TRACE.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn inode_rename_hook_stack_preserves_order_and_stops_on_first_denial() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace.clone()).unwrap();
        let source = security_test_inode();
        let source_metadata = source.metadata().unwrap();
        let source_object = InodeSecurityRef::new(&source, &source_metadata);
        let old_parent = source.parent().unwrap();
        let old_parent_metadata = old_parent.metadata().unwrap();
        let old_parent_object = InodeSecurityRef::new(&old_parent, &old_parent_metadata);
        let old_entry =
            ExistingInodeSecurityRef::new(old_parent_object, source_object, "security-hook");
        let new_parent = old_parent
            .create(
                "ordered-rename-parent",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o770),
            )
            .unwrap();
        let new_parent_metadata = new_parent.metadata().unwrap();
        let new_parent_object = InodeSecurityRef::new(&new_parent, &new_parent_metadata);
        let new_entry =
            RenameDestinationSecurityRef::absent(new_parent_object, "ordered-rename-entry");
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let rename = InodeRenameSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &old_entry,
            &new_entry,
        );

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_rename: Some(ordered_inode_rename_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_rename: Some(ordered_inode_rename_second),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_RENAME_HOOK_TRACE.store(0, Ordering::SeqCst);
        registry.dispatch_inode_rename(&rename).unwrap();
        assert_eq!(INODE_RENAME_HOOK_TRACE.load(Ordering::SeqCst), 2);

        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_rename: Some(deny_inode_rename_first),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_rename: Some(inode_rename_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = builder.freeze();
        INODE_RENAME_HOOK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            registry.dispatch_inode_rename(&rename),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(INODE_RENAME_HOOK_TRACE.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn symlink_registry_denial_prevents_namespace_publication() {
        let mut builder = test_registry_builder();
        builder
            .try_register_initialized(TestSecurityModule::<1> {
                inode_symlink: Some(deny_symlink_transaction),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        builder
            .try_register_initialized(TestSecurityModule::<2> {
                inode_symlink: Some(symlink_transaction_must_not_run),
                ..TestSecurityModule::empty()
            })
            .unwrap();
        let registry = freeze_test_registry(builder.freeze());
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();
        SYMLINK_VERTICAL_HOOK_TRACE.store(0, Ordering::SeqCst);

        assert!(matches!(
            namespace_mutation::create_symlink(
                &operation,
                &parent,
                "denied-symlink",
                "../unresolved-target",
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(SYMLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert!(matches!(
            parent.lookup_no_follow("denied-symlink"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn hardlink_registry_denial_is_once_and_preserves_namespace_and_source() {
        let _guard = reset_hardlink_vertical_probe();
        let registry = hardlink_vertical_registry(
            deny_hardlink_transaction,
            Some(hardlink_transaction_must_not_run),
        );
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let source = parent
            .create(
                "denied-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o640),
            )
            .unwrap();
        source
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o2640)),
                owner: Some((1200, 1300)),
                ..Default::default()
            })
            .unwrap();
        let source_before = source.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::link(&operation, &parent, "denied-hardlink", &source, &security,),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(HARDLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert!(matches!(
            parent.lookup_no_follow("denied-hardlink"),
            Err(AxError::NotFound)
        ));
        let source_after = source.metadata().unwrap();
        assert_eq!(source_after.nlink, source_before.nlink);
        assert_eq!(source_after.mode.bits(), source_before.mode.bits());
        assert_eq!(
            (source_after.uid, source_after.gid),
            (source_before.uid, source_before.gid)
        );
    }

    #[test]
    fn hardlink_success_publishes_same_inode_and_only_increments_nlink() {
        let _guard = reset_hardlink_vertical_probe();
        let registry = hardlink_vertical_registry(observe_hardlink_transaction, None);
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let source = parent
            .create(
                "success-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o640),
            )
            .unwrap();
        source
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o2640)),
                owner: Some((2200, 2300)),
                ..Default::default()
            })
            .unwrap();
        let source_before = source.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        let linked = namespace_mutation::link(
            &operation,
            &parent,
            "successful-hardlink",
            &source,
            &security,
        )
        .unwrap();
        assert_eq!(HARDLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert!(linked.same_node(&source));
        assert!(
            parent
                .lookup_no_follow("successful-hardlink")
                .unwrap()
                .same_node(&source)
        );
        assert_ne!(parent.namespace_generation().unwrap(), generation);
        let source_after = source.metadata().unwrap();
        let linked_metadata = linked.metadata().unwrap();
        assert_eq!(source_after.nlink, source_before.nlink + 1);
        assert_eq!(linked_metadata.nlink, source_after.nlink);
        assert_eq!(source_after.mode.bits(), source_before.mode.bits());
        assert_eq!(linked_metadata.mode.bits(), source_before.mode.bits());
        assert_eq!(
            (source_after.uid, source_after.gid),
            (source_before.uid, source_before.gid)
        );
        assert_eq!(
            (linked_metadata.uid, linked_metadata.gid),
            (source_before.uid, source_before.gid)
        );
    }

    #[test]
    fn unlink_registry_denial_is_once_and_preserves_exact_transaction() {
        let _guard = reset_removal_vertical_probes();
        let registry = unlink_vertical_registry(
            deny_unlink_transaction,
            Some(unlink_transaction_must_not_run),
        );
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let victim = parent
            .create(
                "denied-unlink",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o640),
            )
            .unwrap();
        victim
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o2640)),
                owner: Some((1200, 1300)),
                atime: Some(core::time::Duration::from_secs(201)),
                mtime: Some(core::time::Duration::from_secs(202)),
                ctime: Some(core::time::Duration::from_secs(203)),
                ..Default::default()
            })
            .unwrap();
        parent
            .update_metadata(MetadataUpdate {
                atime: Some(core::time::Duration::from_secs(101)),
                mtime: Some(core::time::Duration::from_secs(102)),
                ctime: Some(core::time::Duration::from_secs(103)),
                ..Default::default()
            })
            .unwrap();
        let parent_before = parent.metadata().unwrap();
        let victim_before = victim.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::unlink(
                &operation,
                &parent,
                "denied-unlink",
                &victim,
                false,
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(UNLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_metadata_preserved(&parent_before, &parent.metadata().unwrap());
        assert_metadata_preserved(&victim_before, &victim.metadata().unwrap());
        assert!(
            parent
                .lookup_no_follow("denied-unlink")
                .unwrap()
                .same_node(&victim)
        );
    }

    #[test]
    fn rmdir_registry_denial_is_once_and_preserves_exact_transaction() {
        let _guard = reset_removal_vertical_probes();
        let registry =
            rmdir_vertical_registry(deny_rmdir_transaction, Some(rmdir_transaction_must_not_run));
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let victim = parent
            .create(
                "denied-rmdir",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        victim
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o2750)),
                owner: Some((2200, 2300)),
                atime: Some(core::time::Duration::from_secs(401)),
                mtime: Some(core::time::Duration::from_secs(402)),
                ctime: Some(core::time::Duration::from_secs(403)),
                ..Default::default()
            })
            .unwrap();
        parent
            .update_metadata(MetadataUpdate {
                atime: Some(core::time::Duration::from_secs(301)),
                mtime: Some(core::time::Duration::from_secs(302)),
                ctime: Some(core::time::Duration::from_secs(303)),
                ..Default::default()
            })
            .unwrap();
        let parent_before = parent.metadata().unwrap();
        let victim_before = victim.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::unlink(
                &operation,
                &parent,
                "denied-rmdir",
                &victim,
                true,
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(RMDIR_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_metadata_preserved(&parent_before, &parent.metadata().unwrap());
        assert_metadata_preserved(&victim_before, &victim.metadata().unwrap());
        assert!(
            parent
                .lookup_no_follow("denied-rmdir")
                .unwrap()
                .same_node(&victim)
        );
    }

    #[test]
    fn allowed_rmdir_hook_runs_once_before_nonempty_backend_rejection() {
        let _guard = reset_removal_vertical_probes();
        let registry = rmdir_vertical_registry(observe_nonempty_rmdir_transaction, None);
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let directory = parent
            .create(
                "nonempty-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let child = directory
            .create(
                "child",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o640),
            )
            .unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::unlink(
                &operation,
                &parent,
                "nonempty-directory",
                &directory,
                true,
                &security,
            ),
            Err(AxError::DirectoryNotEmpty)
        ));
        assert_eq!(RMDIR_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 1);
        assert!(
            parent
                .lookup_no_follow("nonempty-directory")
                .unwrap()
                .same_node(&directory)
        );
        assert!(
            directory
                .lookup_no_follow("child")
                .unwrap()
                .same_node(&child)
        );
    }

    #[test]
    fn hardlink_cross_mount_rejection_precedes_inode_link() {
        let _guard = reset_hardlink_vertical_probe();
        let registry = hardlink_vertical_registry(observe_hardlink_transaction, None);
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let source_fs = MemoryFs::new().unwrap();
        let source_mount = Mountpoint::new_root(&source_fs);
        crate::mounts::initialize_test_mount(&source_mount, 0).unwrap();
        let source_parent = source_mount.root_location();
        let source = source_parent
            .create(
                "cross-mount-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let destination_fs =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let destination_mount = Mountpoint::new_root(&destination_fs);
        crate::mounts::initialize_test_mount(&destination_mount, 0).unwrap();
        let parent = destination_mount.root_location();
        let source_nlink = source.metadata().unwrap().nlink;
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        let error = match namespace_mutation::link(
            &operation,
            &parent,
            "cross-mount-link",
            &source,
            &security,
        ) {
            Ok(_) => panic!("cross-mount hard link unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.canonicalize(), AxError::CrossesDevices);
        assert_eq!(HARDLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_eq!(source.metadata().unwrap().nlink, source_nlink);
        assert!(matches!(
            parent.lookup_no_follow("cross-mount-link"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn protected_hardlink_rejection_precedes_inode_link() {
        let _guard = reset_hardlink_vertical_probe();
        let registry = hardlink_vertical_registry(observe_hardlink_transaction, None);
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root_with_registry(registry, namespace).unwrap();
        let credential = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let source = parent
            .create(
                "protected-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let source_nlink = source.metadata().unwrap().nlink;
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::link(&operation, &parent, "protected-link", &source, &security,),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!(HARDLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_eq!(source.metadata().unwrap().nlink, source_nlink);
        assert!(matches!(
            parent.lookup_no_follow("protected-link"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn hardlink_directory_rejection_precedes_inode_link() {
        let _guard = reset_hardlink_vertical_probe();
        let registry = hardlink_vertical_registry(observe_hardlink_transaction, None);
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root_with_registry(registry, namespace).unwrap();
        let security = VfsSecurityContext::new(credential);
        let filesystem =
            MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let source = parent
            .create(
                "directory-source",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap();
        let source_nlink = source.metadata().unwrap().nlink;
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            namespace_mutation::link(&operation, &parent, "directory-link", &source, &security,),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!(HARDLINK_VERTICAL_HOOK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_eq!(source.metadata().unwrap().nlink, source_nlink);
        assert!(matches!(
            parent.lookup_no_follow("directory-link"),
            Err(AxError::NotFound)
        ));
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
        let caps = actor_update.builder.caps;
        actor_update.builder.caps = capability_state_for_test(
            capability_set(&[CAP_SYS_NICE]),
            capability_set(&[CAP_SYS_NICE]),
            caps.inheritable(),
            caps.bounding(),
            caps.ambient(),
            caps.securebits(),
        );
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
    fn socket_dispatch_is_ordered_short_circuited_and_fully_preflighted() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let spec = SocketCreateSpec::try_new(2, 1, 0, false).unwrap();

        let context = SocketSecurityContext::create(&actor, spec);
        dispatch_socket(&context).unwrap();
        assert_eq!(CRED_STATE_SOCKET_TRACE.load(Ordering::SeqCst), 23);

        CRED_STATE_SOCKET_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_SOCKET_DENY_KEY.store(2, Ordering::SeqCst);
        assert_eq!(dispatch_socket(&context), Err(AxError::PermissionDenied));
        assert_eq!(CRED_STATE_SOCKET_TRACE.load(Ordering::SeqCst), 2);
        CRED_STATE_SOCKET_DENY_KEY.store(0, Ordering::SeqCst);

        let core = actor.core_arc().clone();
        let mut malformed_security = registry.try_init_credential_state(&core).unwrap();
        malformed_security.slots[2].module_id = ModuleId(7);
        let malformed = Cred::try_from_prepared_parts(core, malformed_security).unwrap();
        let malformed_context = SocketSecurityContext::create(&malformed, spec);
        CRED_STATE_SOCKET_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_socket(&malformed_context),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_SOCKET_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mmap_hook_families_are_state_aware_ordered_and_deny_first() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let raw_flags =
            (1usize << (usize::BITS - 1)) | MAP_PRIVATE as usize | MAP_ANONYMOUS as usize;
        let requested_file = MappingFlags::USER;
        let effective_file = MappingFlags::USER | MappingFlags::READ | MappingFlags::EXECUTE;

        mmap_file(&actor, None, requested_file, effective_file, raw_flags).unwrap();
        assert_eq!(CRED_STATE_MMAP_FILE_TRACE.load(Ordering::SeqCst), 23);

        let start = VirtAddr::from(0x8000);
        let image = Arc::new(());
        let image_ref = MmapImageSecurityRef::from_arc(&image);
        CRED_STATE_MMAP_IMAGE_IDENTITY.store(image_ref.identity(), Ordering::SeqCst);
        dispatch_mmap_addr(&actor, &namespace, &image_ref, start).unwrap();
        assert_eq!(CRED_STATE_MMAP_ADDR_TRACE.load(Ordering::SeqCst), 23);

        let initial = MappingFlags::USER | MappingFlags::READ;
        let requested_protect = MappingFlags::USER | MappingFlags::WRITE;
        let effective_protect = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
        let area = MemoryArea::new(
            start,
            0x1000,
            initial,
            Backend::new_alloc(start, PageSize::Size4K),
        );
        let segment =
            PreparedProtectSegment::for_test(&area, VirtAddrRange::new(start, start + 0x1000));
        file_mprotect(
            &actor,
            &namespace,
            segment,
            requested_protect,
            effective_protect,
        )
        .unwrap();
        assert_eq!(CRED_STATE_MPROTECT_TRACE.load(Ordering::SeqCst), 23);

        CRED_STATE_MMAP_DENY_KEY.store(2, Ordering::SeqCst);
        CRED_STATE_MMAP_FILE_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            mmap_file(&actor, None, requested_file, effective_file, raw_flags,),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_MMAP_FILE_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_MMAP_ADDR_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_mmap_addr(&actor, &namespace, &image_ref, start),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_MMAP_ADDR_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_MPROTECT_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            file_mprotect(
                &actor,
                &namespace,
                segment,
                requested_protect,
                effective_protect,
            ),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_MPROTECT_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn mmap_hook_families_fully_preflight_malformed_credential_state() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let core = actor.core_arc().clone();
        let mut malformed_security = registry.try_init_credential_state(&core).unwrap();
        malformed_security.slots[2].module_id = ModuleId(7);
        let malformed = Cred::try_from_prepared_parts(core, malformed_security).unwrap();

        let raw_flags = MAP_PRIVATE as usize | MAP_ANONYMOUS as usize;
        assert_eq!(
            mmap_file(
                &malformed,
                None,
                MappingFlags::USER,
                MappingFlags::USER | MappingFlags::READ,
                raw_flags,
            ),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_MMAP_FILE_TRACE.load(Ordering::SeqCst), 0);

        let start = VirtAddr::from(0x8000);
        let image = Arc::new(());
        let image_ref = MmapImageSecurityRef::from_arc(&image);
        CRED_STATE_MMAP_IMAGE_IDENTITY.store(image_ref.identity(), Ordering::SeqCst);
        assert_eq!(
            dispatch_mmap_addr(&malformed, &namespace, &image_ref, start),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_MMAP_ADDR_TRACE.load(Ordering::SeqCst), 0);

        let area = MemoryArea::new(
            start,
            0x1000,
            MappingFlags::USER | MappingFlags::READ,
            Backend::new_alloc(start, PageSize::Size4K),
        );
        let segment =
            PreparedProtectSegment::for_test(&area, VirtAddrRange::new(start, start + 0x1000));
        assert_eq!(
            file_mprotect(
                &malformed,
                &namespace,
                segment,
                MappingFlags::USER | MappingFlags::WRITE,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            ),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_MPROTECT_TRACE.load(Ordering::SeqCst), 0);
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
                .try_empty_credential_state_with_reservation(
                    registry,
                    usize::MAX,
                    CredentialStateDerivation::Initial,
                ),
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
        let parent_location = inode_location.parent().unwrap();
        let parent_metadata = parent_location.metadata().unwrap();
        let parent_object = InodeSecurityRef::new(&parent_location, &parent_metadata);
        let planned_entry = PlannedInodeSecurityRef::new(parent_object, "state-aware-entry");
        let directory_location = parent_location
            .create(
                "state-aware-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let directory_metadata = directory_location.metadata().unwrap();
        let directory_object = InodeSecurityRef::new(&directory_location, &directory_metadata);
        let unlink_entry =
            ExistingInodeSecurityRef::new(parent_object, inode_object, "security-hook");
        let rmdir_entry =
            ExistingInodeSecurityRef::new(parent_object, directory_object, "state-aware-directory");
        let rename_destination =
            RenameDestinationSecurityRef::absent(directory_object, "state-aware-renamed-entry");
        let dac_credential = credential.fs_dac_credentials();
        let owner_user_ns = initial_user_namespace(&namespace);
        let inode_permission = InodePermissionSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            InodePermissionAccess::READ,
        );
        let xattr_value = [0xd0, 0x01, 0x02];
        let inode_xattr = InodeXattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            inode_object,
            InodeXattrOperation::set(b"security.capability", &xattr_value, XattrSetFlags::REPLACE)
                .unwrap(),
        );
        let inode_setattr = InodeSetattrSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            inode_object,
            InodeSetattrProposal::chmod(InodeChmodIntent::new(
                InodeSetattrMode::try_from_bits(0o600).unwrap(),
            )),
        );
        let file_open = FileOpenSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            FileOpenOperation::new(FileOpenAccess::Read, false, false, false, false).unwrap(),
        );
        let inode_create = InodeCreateSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o640).unwrap(),
        );
        let inode_mkdir = InodeMkdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o750).unwrap(),
        );
        let inode_mknod = InodeMknodSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeMknodOperation::new(
                InodeMknodKind::Fifo,
                InodeCreateMode::try_from_bits(0o600).unwrap(),
                None,
            )
            .unwrap(),
        );
        let inode_symlink = InodeSymlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            "../state-aware-target",
        );
        let inode_link = InodeLinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &inode_object,
            &planned_entry,
        );
        let inode_unlink = InodeUnlinkSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
        );
        let inode_rmdir = InodeRmdirSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &rmdir_entry,
        );
        let inode_rename = InodeRenameSecurityContext::new(
            &credential,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
            &rename_destination,
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
        dispatch_inode_xattr(inode_xattr).unwrap().committed();
        dispatch_inode_setattr(inode_setattr).unwrap().committed(
            InodeSetattrCommittedSecurityRef::new(&inode_location, &inode_metadata),
        );
        dispatch_inode_create(&inode_create).unwrap();
        dispatch_inode_mkdir(&inode_mkdir).unwrap();
        dispatch_inode_mknod(&inode_mknod).unwrap();
        dispatch_inode_symlink(&inode_symlink).unwrap();
        dispatch_inode_link(&inode_link).unwrap();
        dispatch_inode_unlink(&inode_unlink).unwrap();
        dispatch_inode_rmdir(&inode_rmdir).unwrap();
        dispatch_inode_rename(&inode_rename).unwrap();
        dispatch_file_open(&file_open).unwrap();
        dispatch_ptrace_access(&context).unwrap();
        dispatch_ptrace_traceme(&traceme).unwrap();
        dispatch_scheduler(&scheduler).unwrap();
        dispatch_exec_credential(&exec).unwrap();
        dispatch_exec_executable(&executable).unwrap();
        dispatch_signal(&signal).unwrap();
        assert_eq!(CRED_STATE_DISPATCH_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_PERMISSION_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_XATTR_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_POST_XATTR_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_SETATTR_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_INODE_POST_SETATTR_TRACE.load(Ordering::SeqCst),
            23
        );
        assert_eq!(CRED_STATE_INODE_CREATE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_MKDIR_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_MKNOD_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_SYMLINK_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_LINK_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_UNLINK_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_RMDIR_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_INODE_RENAME_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_FILE_OPEN_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_EXECUTABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_HOOK_MASK.load(Ordering::SeqCst), 0xfffff);

        CRED_STATE_NAMED_CREATE_DENY_KEY.store(2, Ordering::SeqCst);
        CRED_STATE_INODE_CREATE_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_create(&inode_create),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_CREATE_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_INODE_MKDIR_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_mkdir(&inode_mkdir),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_MKDIR_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_INODE_MKNOD_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_mknod(&inode_mknod),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_MKNOD_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_INODE_SYMLINK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_symlink(&inode_symlink),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_SYMLINK_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_INODE_LINK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_link(&inode_link),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_LINK_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_REMOVE_DENY_KEY.store(2, Ordering::SeqCst);
        CRED_STATE_INODE_UNLINK_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_unlink(&inode_unlink),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_UNLINK_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_INODE_RMDIR_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_rmdir(&inode_rmdir),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_RMDIR_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_RENAME_DENY_KEY.store(2, Ordering::SeqCst);
        CRED_STATE_INODE_RENAME_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            dispatch_inode_rename(&inode_rename),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_INODE_RENAME_TRACE.load(Ordering::SeqCst), 2);
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
    fn capable_is_commoncap_first_typed_and_deny_first() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();

        assert!(crate::task::ns_capable(&actor, &namespace, CAP_CHOWN));
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_CAPABLE_OPERATION.load(Ordering::SeqCst), 1);
        assert_eq!(CRED_STATE_CAPABLE_NUMBER.load(Ordering::SeqCst), CAP_CHOWN);

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_OPERATION.store(0, Ordering::SeqCst);
        assert!(actor.has_effective_capability(CAP_CHOWN));
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_CAPABLE_OPERATION.load(Ordering::SeqCst), 1);

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_OPERATION.store(0, Ordering::SeqCst);
        assert!(actor.has_effective_capability_for_setid(CAP_CHOWN));
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_CAPABLE_OPERATION.load(Ordering::SeqCst), 1 << 2);

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_DENY_KEY.store(2, Ordering::SeqCst);
        assert_eq!(
            authorize_capability_with_operation(
                &actor,
                &namespace,
                CAP_CHOWN,
                CapabilitySecurityOperation::Use,
            ),
            Err(AxError::PermissionDenied)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 2);

        CRED_STATE_CAPABLE_DENY_KEY.store(0, Ordering::SeqCst);
        let slot = CredentialSlot::new(actor.clone());
        let mut update = slot.prepare();
        let caps = update.builder.caps;
        update.builder.caps = capability_state_for_test(
            [0; CAPABILITY_WORDS],
            [0; CAPABILITY_WORDS],
            caps.inheritable(),
            caps.bounding(),
            [0; CAPABILITY_WORDS],
            caps.securebits(),
        );
        let restricted = update.finish().unwrap().commit();

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        assert_eq!(
            authorize_capability_with_operation(
                &restricted,
                &namespace,
                CAP_CHOWN,
                CapabilitySecurityOperation::Use,
            ),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(
            authorize_capability_with_operation(
                &actor,
                &namespace,
                u32::MAX,
                CapabilitySecurityOperation::Use,
            ),
            Err(AxError::InvalidInput)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn setid_planner_honors_typed_capability_hook_denial_without_publication() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(actor);
        let before = slot.current();

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_OPERATION.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_NUMBER.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_DENY_KEY.store(2, Ordering::SeqCst);
        let error = prepare_user_id_update(
            &slot,
            thekernel_linux_cred::UserIdTransitionInput::setuid(Kuid::from_raw(1000).unwrap()),
        )
        .err()
        .unwrap();

        assert_eq!(error, AxError::OperationNotPermitted);
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 2);
        assert_eq!(CRED_STATE_CAPABLE_OPERATION.load(Ordering::SeqCst), 1 << 2);
        assert_eq!(CRED_STATE_CAPABLE_NUMBER.load(Ordering::SeqCst), CAP_SETUID);
        assert!(Arc::ptr_eq(&before, &slot.current()));
    }

    #[test]
    fn setfsid_prepare_and_security_failures_return_old_without_publication() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(actor);
        let before = slot.current();

        CRED_STATE_FAIL_PREPARE_KEY.store(2, Ordering::SeqCst);
        let (old_fsuid, uid_update) = prepare_setfsuid_update(&slot, Kuid::from_raw(1000).unwrap());
        assert_eq!(old_fsuid, Kuid::INITIAL_ROOT);
        assert!(uid_update.is_none());
        assert!(Arc::ptr_eq(&before, &slot.current()));

        CRED_STATE_FAIL_PREPARE_KEY.store(0, Ordering::SeqCst);
        CRED_STATE_DENY_KEY.store(2, Ordering::SeqCst);
        let (old_fsgid, gid_update) = prepare_setfsgid_update(&slot, Kgid::from_raw(100).unwrap());
        assert_eq!(old_fsgid, Kgid::INITIAL_ROOT);
        assert!(gid_update.is_none());
        assert!(Arc::ptr_eq(&before, &slot.current()));
    }

    #[test]
    fn setgroups_admission_is_typed_once_and_bound_to_exact_slot_credential() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let actor = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::try_new(actor.clone()).unwrap();

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_OPERATION.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_NUMBER.store(0, Ordering::SeqCst);
        let admission = SetgroupsAdmission::try_new(slot.clone()).unwrap();
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_CAPABLE_OPERATION.load(Ordering::SeqCst), 1 << 2);
        assert_eq!(CRED_STATE_CAPABLE_NUMBER.load(Ordering::SeqCst), CAP_SETGID);

        admission.validate_fixture(&slot, &slot.current()).unwrap();
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);

        let other_slot = CredentialSlot::try_new(actor.clone()).unwrap();
        assert_eq!(
            admission.validate_fixture(&other_slot, &actor),
            Err(AxError::OperationNotPermitted)
        );

        let mut replacement = slot.prepare();
        replacement.builder.no_new_privs = true;
        let replacement = replacement.finish().unwrap().commit();
        assert_eq!(
            admission.validate_fixture(&slot, &replacement),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 23);

        CRED_STATE_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_CAPABLE_DENY_KEY.store(2, Ordering::SeqCst);
        assert_eq!(
            SetgroupsAdmission::try_new(CredentialSlot::try_new(actor).unwrap()).err(),
            Some(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn prepared_namespace_capability_never_dispatches_live_capable_on_child() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let source = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        let ids = source.ids();
        let child_namespace = namespace.try_fork(ids.euid, ids.egid, true).unwrap();
        let child =
            Cred::try_prepare_with_user_namespace(&source, child_namespace.clone()).unwrap();

        assert_eq!(
            authorize_capability_with_operation(
                &child,
                &child_namespace,
                CAP_SYS_ADMIN,
                CapabilitySecurityOperation::Use,
            ),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 0);

        assert!(prepared_credential_namespace_capable(
            &source,
            &child,
            &child_namespace,
            CAP_SYS_ADMIN,
        ));
        assert_eq!(CRED_STATE_PREPARED_CAPABLE_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_PREPARED_CAPABLE_NUMBER.load(Ordering::SeqCst),
            CAP_SYS_ADMIN
        );

        CRED_STATE_PREPARED_CAPABLE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_PREPARED_CAPABLE_DENY_KEY.store(2, Ordering::SeqCst);
        assert!(!prepared_credential_namespace_capable(
            &source,
            &child,
            &child_namespace,
            CAP_SYS_ADMIN,
        ));
        assert_eq!(CRED_STATE_PREPARED_CAPABLE_TRACE.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unpublished_credential_cannot_seed_another_transition() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let source = Cred::try_root_with_registry(registry, namespace).unwrap();
        let child = Cred::try_prepare_clone_for_fork(&source).unwrap();

        assert_eq!(
            Cred::try_prepare_clone_for_fork(&child).err(),
            Some(AxError::OperationNotPermitted)
        );
        let child_slot = CredentialSlot::new(child.clone());
        let mut unpublished_update = child_slot.prepare();
        unpublished_update.builder.ids.ruid = Kuid::from_raw(1000).unwrap();
        assert_eq!(
            unpublished_update.finish().err(),
            Some(AxError::OperationNotPermitted)
        );

        let publication = PendingCredentialPublication::try_fork(
            &source,
            &child,
            TestCredentialPublicationTargetOwner { identity: 0x404 },
        )
        .unwrap();
        publication.activate();
        publication.notify();

        let grandchild = Cred::try_prepare_clone_for_fork(&child).unwrap();
        let mut published_update = child_slot.prepare();
        published_update.builder.ids.ruid = Kuid::from_raw(1000).unwrap();
        let prepared = published_update.finish().unwrap();
        drop(prepared);
        drop(grandchild);
    }

    #[test]
    fn malformed_capable_state_fails_before_any_module_callback() {
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

        assert_eq!(
            authorize_capability_with_operation(
                &malformed,
                &namespace,
                CAP_CHOWN,
                CapabilitySecurityOperation::Use,
            ),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_CAPABLE_TRACE.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn child_credential_publication_is_ordered_typed_and_success_only() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let source = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();

        let aborted_child = Cred::try_prepare_clone_for_fork(&source).unwrap();
        let aborted = PendingCredentialPublication::try_fork(
            &source,
            &aborted_child,
            TestCredentialPublicationTargetOwner { identity: 0x101 },
        )
        .unwrap();
        drop(aborted);
        assert_eq!(CRED_STATE_PUBLICATION_TRACE.load(Ordering::SeqCst), 0);

        let fork_child = Cred::try_prepare_clone_for_fork(&source).unwrap();
        let fork_publication = PendingCredentialPublication::try_fork(
            &source,
            &fork_child,
            TestCredentialPublicationTargetOwner { identity: 0x202 },
        )
        .unwrap();
        fork_publication.activate();
        fork_publication.notify();
        assert_eq!(CRED_STATE_PUBLICATION_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(CRED_STATE_PUBLICATION_OPERATION.load(Ordering::SeqCst), 1);
        assert_eq!(CRED_STATE_PUBLICATION_TARGET.load(Ordering::SeqCst), 0x202);
        assert_eq!(
            CRED_STATE_PUBLICATION_SOURCE_UID.load(Ordering::SeqCst),
            source.ids().euid.into_raw()
        );
        assert_eq!(
            CRED_STATE_PUBLICATION_CHILD_UID.load(Ordering::SeqCst),
            fork_child.ids().euid.into_raw()
        );

        CRED_STATE_PUBLICATION_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_PUBLICATION_OPERATION.store(0, Ordering::SeqCst);
        let ids = source.ids();
        let child_namespace = namespace.try_fork(ids.euid, ids.egid, true).unwrap();
        let userns_child =
            Cred::try_prepare_with_user_namespace(&source, child_namespace.clone()).unwrap();
        let userns_publication = PendingCredentialPublication::try_user_namespace(
            &source,
            &userns_child,
            TestCredentialPublicationTargetOwner { identity: 0x303 },
        )
        .unwrap();
        userns_publication.activate();
        userns_publication.notify();
        assert_eq!(CRED_STATE_PUBLICATION_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_PUBLICATION_OPERATION.load(Ordering::SeqCst),
            1 << 1
        );
        assert_eq!(CRED_STATE_PUBLICATION_TARGET.load(Ordering::SeqCst), 0x303);
        assert!(Arc::ptr_eq(userns_child.user_ns(), &child_namespace));
    }

    #[test]
    fn credential_publication_rejects_mislabeled_or_foreign_children() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let source = Cred::try_root_with_registry(registry, namespace.clone()).unwrap();
        assert!(matches!(
            PendingCredentialPublication::try_fork(
                &source,
                &source,
                TestCredentialPublicationTargetOwner { identity: 0 },
            ),
            Err(AxError::BadState)
        ));
        let fresh_security = registry.try_init_credential_state(source.core()).unwrap();
        let fresh =
            Cred::try_from_prepared_parts(source.core_arc().clone(), fresh_security).unwrap();
        assert!(matches!(
            PendingCredentialPublication::try_fork(
                &source,
                &fresh,
                TestCredentialPublicationTargetOwner { identity: 0 },
            ),
            Err(AxError::BadState)
        ));
        let fork_child = Cred::try_prepare_clone_for_fork(&source).unwrap();
        let ids = source.ids();
        let child_namespace = namespace.try_fork(ids.euid, ids.egid, true).unwrap();
        let userns_child =
            Cred::try_prepare_with_user_namespace(&source, child_namespace.clone()).unwrap();

        assert!(matches!(
            PendingCredentialPublication::try_fork(
                &source,
                &userns_child,
                TestCredentialPublicationTargetOwner { identity: 1 },
            ),
            Err(AxError::BadState)
        ));
        assert!(matches!(
            PendingCredentialPublication::try_user_namespace(
                &source,
                &fork_child,
                TestCredentialPublicationTargetOwner { identity: 2 },
            ),
            Err(AxError::BadState)
        ));

        let userns_publication = PendingCredentialPublication::try_user_namespace(
            &source,
            &userns_child,
            TestCredentialPublicationTargetOwner { identity: 0x304 },
        )
        .unwrap();
        userns_publication.activate();
        userns_publication.notify();
        CRED_STATE_PUBLICATION_TRACE.store(0, Ordering::SeqCst);

        child_namespace
            .publish_uid_map(
                child_namespace
                    .try_build_uid_map(vec![IdMapInputExtent::new(0, ids.euid.into_raw(), 1)])
                    .unwrap(),
            )
            .unwrap();
        child_namespace
            .publish_gid_map(
                child_namespace
                    .try_build_gid_map(vec![IdMapInputExtent::new(0, ids.egid.into_raw(), 1)])
                    .unwrap(),
                false,
            )
            .unwrap();
        let grandchild_namespace = child_namespace.try_fork(ids.euid, ids.egid, true).unwrap();
        let grandchild =
            Cred::try_prepare_with_user_namespace(&userns_child, grandchild_namespace).unwrap();
        assert!(matches!(
            PendingCredentialPublication::try_user_namespace(
                &source,
                &grandchild,
                TestCredentialPublicationTargetOwner { identity: 3 },
            ),
            Err(AxError::BadState)
        ));

        let foreign_registry = probe_registry();
        let foreign_security = foreign_registry
            .try_init_credential_state(source.core())
            .unwrap();
        let foreign =
            Cred::try_from_prepared_parts(source.core_arc().clone(), foreign_security).unwrap();
        assert!(matches!(
            PendingCredentialPublication::try_fork(
                &source,
                &foreign,
                TestCredentialPublicationTargetOwner { identity: 4 },
            ),
            Err(AxError::OperationNotPermitted)
        ));

        let mut wrong_type_security = registry.try_init_credential_state(source.core()).unwrap();
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
            Cred::try_from_prepared_parts(source.core_arc().clone(), wrong_type_security).unwrap();
        assert!(matches!(
            PendingCredentialPublication::try_fork(
                &source,
                &wrong_type,
                TestCredentialPublicationTargetOwner { identity: 5 },
            ),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!(CRED_STATE_PUBLICATION_TRACE.load(Ordering::SeqCst), 0);

        // This test intentionally creates more composite credentials than the
        // decimal u32 drop-order probe can encode at once. Release them in
        // bounded batches; drop order is covered by dedicated tests.
        for credential in [fresh, fork_child, grandchild, foreign, wrong_type] {
            drop(credential);
            CRED_STATE_DROP_TRACE.store(0, Ordering::SeqCst);
        }
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
    fn inode_file_and_named_entry_dispatch_fail_closed_on_wrong_actor_state() {
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
        let parent = location.parent().unwrap();
        let parent_metadata = parent.metadata().unwrap();
        let parent_object = InodeSecurityRef::new(&parent, &parent_metadata);
        let planned_entry = PlannedInodeSecurityRef::new(parent_object, "malformed-entry");
        let directory = parent
            .create(
                "malformed-directory",
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o750),
            )
            .unwrap();
        let directory_metadata = directory.metadata().unwrap();
        let directory_object = InodeSecurityRef::new(&directory, &directory_metadata);
        let unlink_entry = ExistingInodeSecurityRef::new(parent_object, object, "security-hook");
        let rmdir_entry =
            ExistingInodeSecurityRef::new(parent_object, directory_object, "malformed-directory");
        let rename_destination =
            RenameDestinationSecurityRef::absent(directory_object, "malformed-rename-entry");
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
        let create = InodeCreateSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o640).unwrap(),
        );
        let mkdir = InodeMkdirSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeCreateMode::try_from_bits(0o750).unwrap(),
        );
        let mknod = InodeMknodSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            InodeMknodOperation::new(
                InodeMknodKind::Socket,
                InodeCreateMode::try_from_bits(0o600).unwrap(),
                None,
            )
            .unwrap(),
        );
        let symlink = InodeSymlinkSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &planned_entry,
            "../malformed-target",
        );
        let link = InodeLinkSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &object,
            &planned_entry,
        );
        let unlink = InodeUnlinkSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
        );
        let rmdir = InodeRmdirSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &rmdir_entry,
        );
        let rename = InodeRenameSecurityContext::new(
            &malformed,
            &dac_credential,
            &owner_user_ns,
            &unlink_entry,
            &rename_destination,
        );
        CRED_STATE_INODE_PERMISSION_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_CREATE_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_MKDIR_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_MKNOD_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_SYMLINK_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_LINK_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_UNLINK_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_RMDIR_TRACE.store(0, Ordering::SeqCst);
        CRED_STATE_INODE_RENAME_TRACE.store(0, Ordering::SeqCst);
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
        assert_eq!(
            dispatch_inode_create(&create),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_mkdir(&mkdir),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_mknod(&mknod),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_symlink(&symlink),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_link(&link),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_unlink(&unlink),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_rmdir(&rmdir),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            dispatch_inode_rename(&rename),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(CRED_STATE_INODE_PERMISSION_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_CREATE_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_MKDIR_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_MKNOD_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_SYMLINK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_LINK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_UNLINK_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_RMDIR_TRACE.load(Ordering::SeqCst), 0);
        assert_eq!(CRED_STATE_INODE_RENAME_TRACE.load(Ordering::SeqCst), 0);
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
        assert_eq!(
            CRED_STATE_PREPARE_MUTATION_MASK.load(Ordering::SeqCst),
            u32::from(CredentialMutationKind::IDENTITIES.bits())
        );

        let publication = prepared.publish();
        assert_eq!(slot.current().ids().ruid, Kuid::from_raw(1000).unwrap());
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 0);

        let (new, retirement) = publication.complete_post_commit();
        assert_eq!(CRED_STATE_COMMIT_TRACE.load(Ordering::SeqCst), 23);
        assert_eq!(
            CRED_STATE_COMMIT_TRANSITION_MASK.load(Ordering::SeqCst),
            1 << 1
        );
        assert_eq!(
            CRED_STATE_COMMIT_MUTATION_MASK.load(Ordering::SeqCst),
            u32::from(CredentialMutationKind::IDENTITIES.bits())
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
    fn ordinary_mutation_reports_every_changed_credential_family() {
        let _probe_guard = reset_credential_state_probes();
        let registry = probe_registry();
        let namespace = UserNamespace::try_new_root().unwrap();
        let old = Cred::try_root_with_registry(registry, namespace).unwrap();
        let slot = CredentialSlot::new(old);

        let mut update = slot.prepare();
        update.builder.ids.ruid = Kuid::from_raw(1000).unwrap();
        update.builder.groups = thekernel_linux_cred::GroupInfo::try_new(vec![
            Kgid::from_raw(100).unwrap(),
            Kgid::from_raw(200).unwrap(),
        ])
        .unwrap();
        let caps = update.builder.caps;
        let mut inheritable = caps.inheritable();
        inheritable[0] = 1;
        update.builder.caps = capability_state_for_test(
            caps.effective(),
            caps.permitted(),
            inheritable,
            caps.bounding(),
            caps.ambient(),
            caps.securebits() | thekernel_linux_cred::SECBIT_KEEP_CAPS,
        );
        update.builder.no_new_privs = true;

        let prepared = update.finish().unwrap();
        let expected = CredentialMutationKind::IDENTITIES
            .with(CredentialMutationKind::GROUPS)
            .with(CredentialMutationKind::CAPABILITIES)
            .with(CredentialMutationKind::SECUREBITS)
            .with(CredentialMutationKind::NO_NEW_PRIVS);
        assert_eq!(
            CRED_STATE_PREPARE_MUTATION_MASK.load(Ordering::SeqCst),
            u32::from(expected.bits())
        );
        assert_eq!(CRED_STATE_COMMIT_MUTATION_MASK.load(Ordering::SeqCst), 0);

        prepared.commit();
        assert_eq!(
            CRED_STATE_COMMIT_MUTATION_MASK.load(Ordering::SeqCst),
            u32::from(expected.bits())
        );
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
                CredentialStateTransition::Mutation(CredentialMutationKind::empty()),
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
