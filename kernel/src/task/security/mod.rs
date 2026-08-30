//! Typed, allocation-free security-hook dispatch.
//!
//! Security modules are admitted fallibly during boot as complete units, then
//! frozen and published exactly once before the initial credential exists.
//! Runtime dispatch only walks that immutable declaration order: it cannot
//! allocate, register, remove, or silently skip a module.
//!
//! The file is split by responsibility: [`contexts`] decides the shape of each
//! hook, [`module`] states the contract a policy implements, [`builtin`] holds
//! the policies this kernel always admits, [`registry`] owns fallible
//! construction and immutable publication, [`credential`] owns per-credential
//! state transitions, and [`dispatch`] is what the rest of the kernel calls.

mod builtin;
mod contexts;
mod credential;
mod dispatch;
mod landlock;
mod module;
mod registry;
#[cfg(test)]
mod tests;

// These submodules were one namespace before the split and are re-exported as
// one namespace now, so `use super::*` inside each of them resolves exactly
// what the moved code was written against.
use alloc::{boxed::Box, sync::Arc, vec::Vec};

pub(crate) use builtin::*;
pub(crate) use contexts::*;
pub(crate) use credential::*;
pub(crate) use dispatch::*;
pub(crate) use landlock::*;
pub(crate) use module::*;
pub(crate) use registry::*;
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
// The crate-facing hook vocabulary. Which names a given build consumes is
// profile-dependent: the architecture kernel exercises one subset and the
// host test surface another, so an unused re-export here is a property of
// the profile rather than dead surface.
#[allow(unused_imports)]
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

pub(crate) const SECURITY_MODULE_LIMIT: usize = 8;
pub(crate) const COMMONCAP_MODULE_KEY: ModuleKey = ModuleKey(0);
pub(crate) const NOOP_POLICY_MODULE_KEY: ModuleKey = ModuleKey(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModuleKey(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModuleId(u8);

pub(crate) type CoreCred = thekernel_linux_cred::Credential<UserNamespace>;
pub(crate) type CoreCapabilitySecurityContext<'a> =
    thekernel_linux_cred::CapabilitySecurityContext<'a, UserNamespace>;
pub(crate) type CorePreparedCredentialCapabilityContext<'a> =
    thekernel_linux_cred::PreparedCredentialCapabilityContext<'a, UserNamespace>;
pub(crate) type CoreCredentialPublicationContext<'a> =
    thekernel_linux_cred::CredentialPublicationContext<
        'a,
        UserNamespace,
        CredentialPublicationTarget,
    >;
pub(crate) type CoreMmapFileContext<'a> = MmapFileContext<'a, UserNamespace, FileHandle<File>>;
pub(crate) type CoreMmapAddressContext<'a> =
    MmapAddressContext<'a, UserNamespace, MmapImageSecurityRef>;
pub(crate) type CoreFileMprotectContext<'context, 'segment> =
    FileMprotectContext<'context, UserNamespace, PreparedProtectSegment<'segment>>;

/// Opaque identity of the exact retained address-space image selected by mmap.
///
/// Security modules can compare this stable identity without receiving an
/// address-space lock or mutable MM internals. The syscall constructs and
/// dispatches this projection while retaining the source `Arc`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MmapImageSecurityRef {
    pub(super) identity: usize,
}

impl MmapImageSecurityRef {
    pub(super) fn from_arc<T>(image: &Arc<T>) -> Self {
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
    pub(super) identity: usize,
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
pub(crate) struct CredentialPostCommitContext<'a, S> {
    pub(super) old_credential: &'a CoreCred,
    pub(super) old_state: &'a S,
    pub(super) new_credential: &'a CoreCred,
    pub(super) new_state: &'a S,
    pub(super) transition: CredentialStateTransition,
}

impl<'a, S> CredentialPostCommitContext<'a, S> {
    pub(super) const fn old_credential(&self) -> &'a CoreCred {
        self.old_credential
    }

    pub(super) const fn old_state(&self) -> &'a S {
        self.old_state
    }

    pub(super) const fn new_credential(&self) -> &'a CoreCred {
        self.new_credential
    }

    pub(super) const fn new_state(&self) -> &'a S {
        self.new_state
    }

    pub(super) const fn transition(&self) -> CredentialStateTransition {
        self.transition
    }
}

/// Opaque proof that the complete boot-time module stack was frozen and
/// published. Credentials retain this exact identity for their whole life;
/// state preparation and dispatch never rediscover it through a global.
#[derive(Clone, Copy)]
pub(crate) struct FrozenSecurityRegistry(&'static SecurityRegistry);

impl FrozenSecurityRegistry {
    pub(super) fn registry(self) -> &'static SecurityRegistry {
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
    pub(super) pointer: *const (),
    pub(super) _image: PhantomData<&'a ()>,
}

impl<'a> ProcessImageIdentity<'a> {
    pub(super) fn from_arc<T>(image: &'a Arc<T>) -> Self {
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
    pub(super) owner_user_ns: &'a Arc<UserNamespace>,
    pub(super) identity: ProcessImageIdentity<'a>,
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
    pub(super) mount_id: u64,
    pub(super) device: u64,
    pub(super) inode: u64,
}

impl InodeIdentity {
    pub(super) const fn new(mount_id: u64, device: u64, inode: u64) -> Self {
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
    pub(super) identity: InodeIdentity,
    pub(super) mode: u16,
    pub(super) node_kind: NodeType,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) size: u64,
    pub(super) _location: PhantomData<&'location Location>,
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

    /// Constructs security facts for a descriptor-only pseudo inode.  Such
    /// objects have no mount path, so mount id zero is reserved for their
    /// stable device/inode identity.
    pub(crate) fn new_pseudo(metadata: &Metadata) -> InodeSecurityRef<'static> {
        InodeSecurityRef {
            identity: InodeIdentity::new(0, metadata.device, metadata.inode),
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
    pub(super) identity: InodeIdentity,
    pub(super) mode: u16,
    pub(super) node_kind: NodeType,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) _location: PhantomData<&'location Location>,
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

    pub(crate) fn new_pseudo(metadata: &Metadata) -> InodeSetattrCommittedSecurityRef<'static> {
        InodeSetattrCommittedSecurityRef {
            identity: InodeIdentity::new(0, metadata.device, metadata.inode),
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
    pub(super) parent: InodeSecurityRef<'location>,
    pub(super) name: &'name str,
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
    pub(super) parent: InodeSecurityRef<'location>,
    pub(super) target: InodeSecurityRef<'location>,
    pub(super) name: &'name str,
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
    pub(super) parent: InodeSecurityRef<'location>,
    pub(super) target: Option<InodeSecurityRef<'location>>,
    pub(super) name: &'name str,
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
