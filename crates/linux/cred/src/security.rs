//! Policy-neutral typed security contexts and Linux commoncap decisions.
//!
//! Contexts borrow immutable credentials, namespace ownership, and opaque
//! object payloads supplied by an embedding kernel. This module owns no task
//! lookup, `current()` access, process or address-space type, hook registry,
//! lock, publication mechanism, or errno mapping.

use alloc::sync::Arc;
use core::{
    fmt,
    num::NonZeroU32,
    ops::{BitOr, BitOrAssign},
};

use linux_raw_sys::general::{
    CAP_KILL, CAP_LAST_CAP, CAP_SYS_NICE, CAP_SYS_PTRACE, RLIM_INFINITY, SIGCONT,
    XATTR_NAME_MAX as LINUX_XATTR_NAME_MAX,
};

use crate::{
    CAPABILITY_WORDS, CapabilitySets, Credential, FsCredentialSnapshot, Kgid, Kuid,
    UserNamespaceView, ns_capable,
};

/// Policy-neutral authorization failures returned by commoncap helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AuthorizationError {
    /// The actor lacks authority over the target or requested operation.
    NotPermitted,
    /// Access is denied by an operation-specific discretionary limit.
    AccessDenied,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPermitted => formatter.write_str("security operation not permitted"),
            Self::AccessDenied => formatter.write_str("security access denied"),
        }
    }
}

/// Non-empty normalized access requested from an inode permission hook.
///
/// The representation uses the low-three POSIX read/write/execute layout, but
/// remains a crate-local typed value rather than Linux `MAY_*`, open, or file
/// descriptor flags. A consumer resolves its VFS request first and then uses
/// [`Self::try_from_bits`] or the typed constants to construct the exact access
/// combination presented to policy.
///
/// Empty permission checks have no stable hook meaning and are rejected, as
/// are bits outside [`Self::ALL`]. Fields remain private so external consumers
/// cannot bypass those invariants.
///
/// ```compile_fail
/// use thekernel_linux_cred::InodePermissionAccess;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = InodePermissionAccess(1);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodePermissionAccess(u8);

impl InodePermissionAccess {
    const READ_BIT: u8 = 0b100;
    const WRITE_BIT: u8 = 0b010;
    const EXECUTE_BIT: u8 = 0b001;
    const ALL_BITS: u8 = Self::READ_BIT | Self::WRITE_BIT | Self::EXECUTE_BIT;

    /// Read access to the exact target object.
    pub const READ: Self = Self(Self::READ_BIT);
    /// Write access to the exact target object.
    pub const WRITE: Self = Self(Self::WRITE_BIT);
    /// Execute or search access to the exact target object.
    pub const EXECUTE: Self = Self(Self::EXECUTE_BIT);
    /// Every access kind represented by this contract.
    pub const ALL: Self = Self(Self::ALL_BITS);

    /// Constructs a non-empty combination from crate-local normalized bits.
    ///
    /// Returns `None` for an empty request or when any unknown bit is present.
    /// Callers decoding Linux or VFS-specific masks must normalize them before
    /// calling this method.
    pub const fn try_from_bits(bits: u8) -> Option<Self> {
        if bits == 0 || bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Returns the crate-local normalized bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Reports whether every access kind in `other` is requested by `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Reports whether any access kind in `other` is requested by `self`.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Combines two non-empty access requests.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOr for InodePermissionAccess {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for InodePermissionAccess {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// Complete immutable input to one inode permission policy hook.
///
/// `O` is the embedding kernel's exact pinned inode/location identity. The
/// context only borrows that opaque object and does not define VFS identity,
/// look it up again, or dispatch policy modules. The target-owner namespace is
/// supplied independently of the actor credential so a consumer cannot
/// accidentally substitute the actor's namespace for the object's owner. The
/// separately frozen DAC credential is the identity actually selected for
/// this operation; it may intentionally differ from the actor's ordinary
/// filesystem snapshot, for example under a real-ID access check.
pub struct InodePermissionContext<'a, N: UserNamespaceView, O: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    target_object: &'a O,
    access: InodePermissionAccess,
}

impl<'a, N: UserNamespaceView, O: ?Sized> InodePermissionContext<'a, N, O> {
    /// Binds one exact actor, object owner, target object, and access request.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
        access: InodePermissionAccess,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            access,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for this DAC check.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the exact target object.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact target object identity.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the normalized non-empty access request.
    pub const fn access(&self) -> InodePermissionAccess {
        self.access
    }
}

/// Normalized permission and special-mode bits presented at the inode-setattr
/// hook point.
///
/// This value contains only the low `0o7777` bits. The embedding object's
/// opaque identity carries its inode kind, while the consumer remains
/// responsible for preserving any filesystem-internal file-type bits. For a
/// chmod request this is the requested hook-point mode, before a later
/// Linux-style `setattr_prepare` step may clear SGID.
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeSetattrMode;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = InodeSetattrMode(0o644);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeSetattrMode(u16);

impl InodeSetattrMode {
    const ALL_BITS: u16 = 0o7777;

    /// Constructs a mode from normalized permission and special bits.
    ///
    /// Mode zero is valid. A file-type or any other unknown bit is rejected.
    pub const fn try_from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Returns the normalized permission and special-mode bits.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// File-privilege cleanup visible at the inode-setattr hook point.
///
/// This is the semantic equivalent of whether a prepared Linux attribute
/// request still carries `ATTR_KILL_PRIV`; it is not a raw attribute bit or a
/// claim that cleanup has already succeeded. The consumer performs any
/// fallible cleanup after the pre-hook and before metadata publication under
/// its own transaction discipline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InodeSetattrPrivilegeCleanup {
    /// Preserve file privilege metadata during this attribute request.
    Preserve,
    /// Remove privilege metadata as part of the admitted attribute request.
    Kill,
}

/// Normalized chmod request before inode-setattr policy and core preparation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeChmodIntent {
    mode: InodeSetattrMode,
}

impl InodeChmodIntent {
    /// Constructs a chmod intent from its normalized requested mode.
    pub const fn new(mode: InodeSetattrMode) -> Self {
        Self { mode }
    }

    /// Returns the requested hook-point mode.
    pub const fn mode(self) -> InodeSetattrMode {
        self.mode
    }
}

/// Normalized chown request which preserves omitted UID and GID fields.
///
/// `None` means that the corresponding userspace field was the all-ones
/// omission sentinel. It is deliberately not folded into the inode's current
/// owner: Linux authorizes only fields actually present in the attribute
/// request, while an explicitly requested current value remains present.
/// Both fields may be omitted because a non-directory chown operation can
/// still request set-ID and privilege cleanup.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeChownIntent {
    user: Option<Kuid>,
    group: Option<Kgid>,
}

/// One timestamp field requested through inode setattr.
///
/// The security hook observes whether the caller omitted a field, requested
/// the kernel's current time, or supplied an explicit value.  The value itself
/// remains owned by the VFS adapter so this no-std policy crate does not impose
/// a kernel time representation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InodeTimestampValue {
    /// Leave this timestamp unchanged.
    Omit,
    /// Set this timestamp to the kernel's current time.
    Now,
    /// Set this timestamp to a caller-supplied value.
    Explicit,
}

/// Normalized atime/mtime request before inode-setattr policy and mutation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeTimestampIntent {
    atime: InodeTimestampValue,
    mtime: InodeTimestampValue,
}

impl InodeTimestampIntent {
    /// Constructs an omission-preserving timestamp intent.
    pub const fn new(atime: InodeTimestampValue, mtime: InodeTimestampValue) -> Self {
        Self { atime, mtime }
    }

    /// Returns the requested access-time operation.
    pub const fn atime(self) -> InodeTimestampValue {
        self.atime
    }

    /// Returns the requested modification-time operation.
    pub const fn mtime(self) -> InodeTimestampValue {
        self.mtime
    }
}

impl InodeChownIntent {
    /// Constructs an omission-aware chown intent.
    pub const fn new(user: Option<Kuid>, group: Option<Kgid>) -> Self {
        Self { user, group }
    }

    /// Returns the requested kernel-global user ID, or `None` when omitted.
    pub const fn user(self) -> Option<Kuid> {
        self.user
    }

    /// Returns the requested kernel-global group ID, or `None` when omitted.
    pub const fn group(self) -> Option<Kgid> {
        self.group
    }
}

/// Leaf-typed inode attribute request selected by the embedding adapter.
///
/// The variants identify Linux-visible operation families without raw iattr
/// masks or caller-provided booleans.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InodeSetattrIntent {
    /// Change permission and special-mode bits.
    Chmod(InodeChmodIntent),
    /// Change zero, one, or both ownership fields with omission preserved.
    Chown(InodeChownIntent),
    /// Change access and/or modification timestamps.
    Timestamp(InodeTimestampIntent),
    /// Change filesystem-native inode flags, project identity, or extent hints.
    FileAttr(InodeFileAttrIntent),
}

/// Normalized filesystem-native attribute request presented to the inode
/// setattr security hook.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeFileAttrIntent {
    xflags: u64,
    extsize: u32,
    project_id: u32,
    cowextsize: u32,
}

impl InodeFileAttrIntent {
    /// Creates one file-attribute security request.
    pub const fn new(xflags: u64, extsize: u32, project_id: u32, cowextsize: u32) -> Self {
        Self {
            xflags,
            extsize,
            project_id,
            cowextsize,
        }
    }

    /// Returns the requested `FS_XFLAG_*` mask.
    pub const fn xflags(self) -> u64 {
        self.xflags
    }
    /// Returns the requested extent-size hint.
    pub const fn extsize(self) -> u32 {
        self.extsize
    }
    /// Returns the requested project identifier.
    pub const fn project_id(self) -> u32 {
        self.project_id
    }
    /// Returns the requested copy-on-write extent-size hint.
    pub const fn cowextsize(self) -> u32 {
        self.cowextsize
    }
}

/// Normalized iattr-equivalent proposal observed by an inode-setattr hook.
///
/// The proposal is the consumer-frozen hook-point input, not the final result
/// of `setattr_prepare` or the backend. Its private construction keeps intent,
/// optional mode/owner fields, and privilege cleanup coherent:
/// [`Self::chmod`] always carries the requested mode, no owner fields, and no
/// privilege cleanup; [`Self::chown`] copies the omission-aware owner fields
/// from its intent and accepts only the implicit mode and cleanup selected by
/// the consumer from the same old-inode snapshot.
///
/// ```compile_fail
/// use thekernel_linux_cred::{InodeSetattrIntent, InodeSetattrProposal};
///
/// // External consumers cannot forge mismatched intent/proposal fields.
/// let _ = InodeSetattrProposal {
///     intent: todo!(),
///     mode: None,
///     user: None,
///     group: None,
///     privilege_cleanup: todo!(),
/// };
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeSetattrProposal {
    intent: InodeSetattrIntent,
    mode: Option<InodeSetattrMode>,
    user: Option<Kuid>,
    group: Option<Kgid>,
    privilege_cleanup: InodeSetattrPrivilegeCleanup,
}

impl InodeSetattrProposal {
    /// Constructs the only valid chmod hook-point proposal shape.
    pub const fn chmod(intent: InodeChmodIntent) -> Self {
        Self {
            intent: InodeSetattrIntent::Chmod(intent),
            mode: Some(intent.mode()),
            user: None,
            group: None,
            privilege_cleanup: InodeSetattrPrivilegeCleanup::Preserve,
        }
    }

    /// Constructs a chown hook-point proposal.
    ///
    /// `mode` is the optional implicit set-ID cleanup computed from the exact
    /// old inode snapshot. `privilege_cleanup` records whether file privilege
    /// metadata remains selected for later cleanup. Neither effect has been
    /// published merely by constructing this value.
    pub const fn chown(
        intent: InodeChownIntent,
        mode: Option<InodeSetattrMode>,
        privilege_cleanup: InodeSetattrPrivilegeCleanup,
    ) -> Self {
        Self {
            intent: InodeSetattrIntent::Chown(intent),
            mode,
            user: intent.user(),
            group: intent.group(),
            privilege_cleanup,
        }
    }

    /// Constructs the only valid timestamp hook-point proposal shape.
    pub const fn timestamps(intent: InodeTimestampIntent) -> Self {
        Self {
            intent: InodeSetattrIntent::Timestamp(intent),
            mode: None,
            user: None,
            group: None,
            privilege_cleanup: InodeSetattrPrivilegeCleanup::Preserve,
        }
    }

    /// Constructs a filesystem-native file-attribute hook proposal.
    pub const fn file_attr(intent: InodeFileAttrIntent) -> Self {
        Self {
            intent: InodeSetattrIntent::FileAttr(intent),
            mode: None,
            user: None,
            group: None,
            privilege_cleanup: InodeSetattrPrivilegeCleanup::Preserve,
        }
    }

    /// Returns the leaf-typed request which produced this proposal.
    pub const fn intent(self) -> InodeSetattrIntent {
        self.intent
    }

    /// Returns the hook-point mode field, if present.
    pub const fn mode(self) -> Option<InodeSetattrMode> {
        self.mode
    }

    /// Returns the hook-point user field, preserving omission as `None`.
    pub const fn user(self) -> Option<Kuid> {
        self.user
    }

    /// Returns the hook-point group field, preserving omission as `None`.
    pub const fn group(self) -> Option<Kgid> {
        self.group
    }

    /// Returns the selected file-privilege cleanup effect.
    pub const fn privilege_cleanup(self) -> InodeSetattrPrivilegeCleanup {
        self.privilege_cleanup
    }
}

/// Immutable facts shared privately by the pre- and post-setattr contexts.
///
/// The two public context types deliberately remain distinct even though they
/// retain the same categories of facts: one belongs to a fallible admission
/// point and the other can only describe a successful publication.
struct InodeSetattrFacts<'a, N: UserNamespaceView, O: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    target_object: &'a O,
    proposal: InodeSetattrProposal,
}

impl<'a, N: UserNamespaceView, O: ?Sized> InodeSetattrFacts<'a, N, O> {
    const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
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
}

/// Complete immutable input to one fallible inode-setattr policy hook.
///
/// `O` is the embedding kernel's exact pinned old-inode identity and metadata
/// snapshot. The caller freezes that object, the actor and selected DAC view,
/// the object's owner namespace, and one coherent hook-point proposal under
/// its own inode/metadata transaction. This context performs no DAC check,
/// lookup, `setattr_prepare`, privilege cleanup, backend mutation, registry
/// dispatch, or errno mapping.
pub struct InodeSetattrContext<'a, N: UserNamespaceView, O: ?Sized> {
    facts: InodeSetattrFacts<'a, N, O>,
}

impl<'a, N: UserNamespaceView, O: ?Sized> InodeSetattrContext<'a, N, O> {
    /// Binds one exact actor, old object, owner namespace, and proposal.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
        proposal: InodeSetattrProposal,
    ) -> Self {
        Self {
            facts: InodeSetattrFacts::new(
                actor,
                dac_credential,
                target_owner_user_ns,
                target_object,
                proposal,
            ),
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor
    }

    /// Borrows the exact filesystem identity selected for this operation.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.facts.dac_credential
    }

    /// Borrows the user namespace which owns the affected inode.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.facts.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact old-inode object snapshot.
    pub const fn target_object(&self) -> &'a O {
        self.facts.target_object
    }

    /// Returns the normalized hook-point proposal.
    pub const fn proposal(&self) -> InodeSetattrProposal {
        self.facts.proposal
    }

    /// Returns the leaf-typed request which produced the proposal.
    pub const fn intent(&self) -> InodeSetattrIntent {
        self.facts.proposal.intent()
    }
}

/// Immutable input to one infallible post-setattr notification.
///
/// This type is intentionally not interchangeable with
/// [`InodeSetattrContext`]. The embedding consumer constructs it only after
/// the backend has reported successful publication, using the same frozen
/// actor, DAC view, owner namespace, and proposal which passed the fallible
/// hook. `O` is the consumer's exact committed object/outcome snapshot. A
/// registry that needs a no-failure post phase should preflight module state
/// before publication and carry its own linear admission token; this leaf type
/// owns neither that token nor dispatch.
pub struct InodePostSetattrContext<'a, N: UserNamespaceView, O: ?Sized> {
    facts: InodeSetattrFacts<'a, N, O>,
}

impl<'a, N: UserNamespaceView, O: ?Sized> InodePostSetattrContext<'a, N, O> {
    /// Binds one successfully committed object to the admitted proposal.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        committed_object: &'a O,
        proposal: InodeSetattrProposal,
    ) -> Self {
        Self {
            facts: InodeSetattrFacts::new(
                actor,
                dac_credential,
                target_owner_user_ns,
                committed_object,
                proposal,
            ),
        }
    }

    /// Borrows the exact immutable actor credential retained from admission.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.facts.actor
    }

    /// Borrows the exact filesystem identity selected during admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.facts.dac_credential
    }

    /// Borrows the user namespace which owns the affected inode.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.facts.target_owner_user_ns
    }

    /// Borrows the embedding-defined successfully committed object snapshot.
    pub const fn committed_object(&self) -> &'a O {
        self.facts.target_object
    }

    /// Returns the exact proposal admitted before publication.
    pub const fn proposal(&self) -> InodeSetattrProposal {
        self.facts.proposal
    }

    /// Returns the leaf-typed request which produced the admitted proposal.
    pub const fn intent(&self) -> InodeSetattrIntent {
        self.facts.proposal.intent()
    }
}

/// Consumer-prepared final permission and special-mode bits for one named
/// inode creation.
///
/// The object kind selects the Linux-style `inode_create`, `inode_mkdir`, or
/// `inode_mknod` hook family, while the embedding adapter supplies the final
/// low `0o7777` permission and special bits which its creation transaction will
/// publish. This is a normalized policy fact, not a byte-for-byte copy of the
/// raw Linux `umode_t` hook payload. File-type bits, open flags, and unnamed
/// temporary-file state are deliberately not representable.
///
/// Mode zero is valid. Fields remain private so a consumer cannot bypass the
/// normalized-bit invariant.
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeCreateMode;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = InodeCreateMode(0o644);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeCreateMode(u16);

impl InodeCreateMode {
    const ALL_BITS: u16 = 0o7777;

    /// Constructs a mode from normalized permission and special bits.
    ///
    /// Returns `None` when a file-type or any other unknown bit is present.
    /// This validates only the bit domain; completing umask, set-ID, ownership,
    /// and other creation-mode preparation remains an adapter precondition.
    pub const fn try_from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    /// Returns the normalized permission and special-mode bits.
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// Complete immutable input to one regular-file `inode_create` policy hook.
///
/// `P` is the embedding kernel's caller-owned opaque parent-directory identity
/// and `E` is its opaque prospective named-entry identity. Keeping the two
/// payloads distinct preserves Linux's `dir`/`dentry` roles without importing
/// a VFS type. The caller completes DAC admission, freezes the final mode, and
/// ensures that the prospective destination remains eligible for creation
/// under its VFS transaction or locking discipline. This context does not
/// represent directory creation, special-node creation, symlinks, hard links,
/// or unnamed temporary files.
pub struct InodeCreateContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    new_entry_object: &'a E,
    mode: InodeCreateMode,
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> InodeCreateContext<'a, N, P, E> {
    /// Binds one exact actor, destination, prospective entry, and final mode.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        new_entry_object: &'a E,
        mode: InodeCreateMode,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            new_entry_object,
            mode,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the destination filesystem.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined prospective named-entry identity.
    pub const fn new_entry_object(&self) -> &'a E {
        self.new_entry_object
    }

    /// Returns the final normalized regular-file creation mode.
    pub const fn mode(&self) -> InodeCreateMode {
        self.mode
    }
}

/// Complete immutable input to one directory `inode_mkdir` policy hook.
///
/// The parent and prospective entry are caller-owned opaque identities. The
/// final mode is frozen after the embedding consumer's creation-mode
/// preparation. Symlink and hard-link operations use different hook topologies
/// and are deliberately outside this context.
pub struct InodeMkdirContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    new_entry_object: &'a E,
    mode: InodeCreateMode,
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> InodeMkdirContext<'a, N, P, E> {
    /// Binds one exact actor, destination, prospective entry, and final mode.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        new_entry_object: &'a E,
        mode: InodeCreateMode,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            new_entry_object,
            mode,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the destination filesystem.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined prospective named-entry identity.
    pub const fn new_entry_object(&self) -> &'a E {
        self.new_entry_object
    }

    /// Returns the final normalized directory creation mode.
    pub const fn mode(&self) -> InodeCreateMode {
        self.mode
    }
}

/// Node kind presented to one Linux-style `inode_mknod` policy hook.
///
/// Regular files and directories have distinct hook contexts. Symlinks and
/// hard links are namespace-link operations rather than `mknod` kinds.
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeMknodKind;
///
/// // Symlinks use the distinct InodeSymlinkContext contract.
/// let _ = InodeMknodKind::Symlink;
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeMknodKind;
///
/// // Hard links use the distinct InodeLinkContext contract.
/// let _ = InodeMknodKind::HardLink;
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeMknodKind;
///
/// // Named regular files use the inode_create contract.
/// let _ = InodeMknodKind::RegularFile;
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeMknodKind;
///
/// // Directories use the inode_mkdir contract.
/// let _ = InodeMknodKind::Directory;
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeMknodKind;
///
/// // O_TMPFILE is unnamed and never enters a named mknod hook.
/// let _ = InodeMknodKind::UnnamedTemporaryFile;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InodeMknodKind {
    /// A named FIFO.
    Fifo,
    /// A character device node.
    CharacterDevice,
    /// A block device node.
    BlockDevice,
    /// A named Unix-domain socket inode.
    Socket,
}

impl InodeMknodKind {
    const fn requires_device(self) -> bool {
        matches!(self, Self::CharacterDevice | Self::BlockDevice)
    }
}

/// Normalized, already-validated facts for one `inode_mknod` policy hook.
///
/// `rdev` is the embedding kernel's caller-normalized device number. It is
/// mandatory for character and block devices and forbidden for FIFO and socket
/// nodes. The field-private constructor prevents those combinations from being
/// forged. Linux ABI encoding and generic-VFS device types remain adapter
/// responsibilities.
///
/// ```compile_fail
/// use thekernel_linux_cred::{InodeCreateMode, InodeMknodKind, InodeMknodOperation};
///
/// // External code cannot forge an invalid kind/device combination.
/// let _ = InodeMknodOperation {
///     kind: InodeMknodKind::Fifo,
///     mode: InodeCreateMode::try_from_bits(0o600).unwrap(),
///     rdev: Some(1),
/// };
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InodeMknodOperation {
    kind: InodeMknodKind,
    mode: InodeCreateMode,
    rdev: Option<u64>,
}

impl InodeMknodOperation {
    /// Constructs one normalized special-node creation operation.
    ///
    /// Returns `None` unless character and block nodes have an `rdev` and FIFO
    /// and socket nodes do not.
    pub const fn new(
        kind: InodeMknodKind,
        mode: InodeCreateMode,
        rdev: Option<u64>,
    ) -> Option<Self> {
        if kind.requires_device() != rdev.is_some() {
            None
        } else {
            Some(Self { kind, mode, rdev })
        }
    }

    /// Returns the normalized special-node kind.
    pub const fn kind(self) -> InodeMknodKind {
        self.kind
    }

    /// Returns the final normalized creation mode.
    pub const fn mode(self) -> InodeCreateMode {
        self.mode
    }

    /// Returns the normalized device number for character or block nodes.
    pub const fn rdev(self) -> Option<u64> {
        self.rdev
    }
}

/// Complete immutable input to one `inode_mknod` policy hook.
///
/// The context binds an already-validated operation to distinct opaque parent
/// and prospective named-entry identities. It owns no lookup, VFS object,
/// device-number encoding, hook registry, dispatch, transaction, or
/// publication mechanism.
pub struct InodeMknodContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    new_entry_object: &'a E,
    operation: InodeMknodOperation,
}

/// Complete immutable input to one `inode_symlink` policy hook.
///
/// `P` and `E` preserve the embedding kernel's distinct parent-directory and
/// prospective named-entry identities. `T` is the exact caller-owned target
/// payload which the admitted filesystem transaction will store. Keeping the
/// target opaque lets byte-oriented consumers retain Linux pathname bytes
/// without imposing an encoding, allocation, or path-resolution policy on this
/// crate.
///
/// The caller completes pathname decoding, destination DAC admission, and
/// absence revalidation before dispatch. It must keep the destination eligible
/// and publish the same target under its VFS transaction or locking discipline.
/// Unlike `inode_create`, `inode_mkdir`, and `inode_mknod`, Linux's symlink hook
/// carries no creation mode or device number.
pub struct InodeSymlinkContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized, T: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    new_entry_object: &'a E,
    symlink_target: &'a T,
}

/// Complete immutable input to one `inode_link` policy hook.
///
/// `S` is the embedding kernel's exact caller-owned source-inode identity,
/// while `P` and `E` preserve the distinct destination parent-directory and
/// prospective named-entry identities. Keeping all three payloads opaque and
/// separate mirrors Linux's source/directory/new-entry hook topology without
/// importing a VFS type or permitting this leaf to perform a lookup.
///
/// The caller completes source eligibility (including protected-hardlink and
/// ownership/capability policy), destination DAC admission, cross-filesystem
/// rejection, and absence revalidation before dispatch. It must keep the
/// source and destination eligible and publish a new name for the same source
/// object under its VFS transaction or locking discipline. A hard link stores
/// neither a symlink target nor a new inode mode or device number, so none of
/// those facts are representable here.
pub struct InodeLinkContext<'a, N: UserNamespaceView, S: ?Sized, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    source_object: &'a S,
    parent_object: &'a P,
    new_entry_object: &'a E,
}

/// Complete immutable input to one `inode_unlink` policy hook.
///
/// `P` is the embedding kernel's exact parent-directory identity and `E` is
/// its exact existing named-entry identity. The entry payload is deliberately
/// distinct from the parent and is expected to bind the final name to the
/// victim inode snapshot selected by the caller. This mirrors Linux's
/// `dir`/`dentry` hook topology without importing a VFS or permitting this
/// leaf crate to repeat lookup.
///
/// The caller completes writable-mount, parent write/search, sticky-directory,
/// victim-type, backend-support, and other `may_delete`-style admission before
/// dispatch. It must keep the same parent, name, and victim eligible and remove
/// that exact entry under its VFS transaction or locking discipline. Path-level
/// hooks, mountpoint policy, delegation, link-count updates, notifications, and
/// errno mapping remain consumer responsibilities. Directory removal uses the
/// distinct [`InodeRmdirContext`] contract rather than a caller-provided flag.
pub struct InodeUnlinkContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    target_entry_object: &'a E,
}

/// Complete immutable input to one `inode_rmdir` policy hook.
///
/// The opaque parent and existing-entry payloads preserve Linux's distinct
/// `dir` and `dentry` roles. The existing entry is caller-owned and should bind
/// the exact final name to the exact directory snapshot selected for removal;
/// this crate owns no lookup, directory enumeration, or namespace mutation.
///
/// The caller completes writable-mount, parent write/search, sticky-directory,
/// victim-is-directory, backend-support, mountpoint, and other pre-hook
/// admission while keeping the selected objects stable through dispatch and
/// publication. Backend directory-emptiness checks, path-level hooks,
/// delegation, timestamps, notifications, and errno mapping remain outside
/// this context. Non-directory removal uses [`InodeUnlinkContext`].
pub struct InodeRmdirContext<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    parent_object: &'a P,
    target_entry_object: &'a E,
}

/// Complete immutable input to one LSM `inode_rename` leaf hook.
///
/// `OP`, `OE`, `NP`, and `NE` preserve the four distinct object roles in
/// Linux's `old_dir`, `old_dentry`, `new_dir`, and `new_dentry` hook
/// signature. The old entry binds the exact source name and inode selected by
/// the caller. The new entry binds the exact destination name and either its
/// prospective absence or its existing target inode, according to the
/// embedding VFS transaction. All four identities remain opaque so this leaf
/// neither performs lookup nor invents a concrete VFS representation.
///
/// Linux's `security_inode_rename` wrapper receives rename flags, but the
/// actual `inode_rename` LSM leaf does not. Ordinary, `RENAME_NOREPLACE`, and
/// `RENAME_WHITEOUT` operations therefore present the same single forward
/// leaf context. For `RENAME_EXCHANGE`, the embedding dispatcher must first
/// present a separately constructed reverse context and stop on its denial,
/// then present the forward context. Flag decoding, combination validation,
/// path-level hooks, private-inode bypass, and that ordered dispatch remain
/// outside this type rather than being flattened into a boolean or raw mask.
///
/// The caller also owns writable-mount, source/destination DAC and sticky
/// admission, cross-filesystem and ancestry checks, backend support,
/// transaction stability, mutation, notification, and errno mapping.
///
/// The context borrows every frozen input and cannot outlive even one of the
/// four object-role identities:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{
///     Credential, FsCredentialSnapshot, InodeRenameContext, UserNamespaceView,
/// };
///
/// fn cannot_escape_new_entry<'a, N, OP, OE, NP>(
///     actor: &'a Credential<N>,
///     dac: &'a FsCredentialSnapshot,
///     owner: &'a Arc<N>,
///     old_parent: &'a OP,
///     old_entry: &'a OE,
///     new_parent: &'a NP,
/// ) -> InodeRenameContext<'a, N, OP, OE, NP, u8>
/// where
///     N: UserNamespaceView,
/// {
///     let new_entry = 7_u8;
///     InodeRenameContext::new(
///         actor,
///         dac,
///         owner,
///         old_parent,
///         old_entry,
///         new_parent,
///         &new_entry,
///     )
/// }
/// ```
pub struct InodeRenameContext<
    'a,
    N: UserNamespaceView,
    OP: ?Sized,
    OE: ?Sized,
    NP: ?Sized,
    NE: ?Sized,
> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    old_parent_object: &'a OP,
    old_entry_object: &'a OE,
    new_parent_object: &'a NP,
    new_entry_object: &'a NE,
}

impl<'a, N: UserNamespaceView, S: ?Sized, P: ?Sized, E: ?Sized> InodeLinkContext<'a, N, S, P, E> {
    /// Binds one exact actor, source object, and prospective destination.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        source_object: &'a S,
        parent_object: &'a P,
        new_entry_object: &'a E,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            source_object,
            parent_object,
            new_entry_object,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the affected filesystem objects.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact source-inode identity.
    pub const fn source_object(&self) -> &'a S {
        self.source_object
    }

    /// Borrows the embedding-defined exact destination parent identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined prospective named-entry identity.
    pub const fn new_entry_object(&self) -> &'a E {
        self.new_entry_object
    }
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> InodeUnlinkContext<'a, N, P, E> {
    /// Binds one exact actor, parent directory, and existing victim entry.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        target_entry_object: &'a E,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            target_entry_object,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the affected filesystem objects.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined exact existing victim-entry identity.
    pub const fn target_entry_object(&self) -> &'a E {
        self.target_entry_object
    }
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> InodeRmdirContext<'a, N, P, E> {
    /// Binds one exact actor, parent directory, and existing directory entry.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        target_entry_object: &'a E,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            target_entry_object,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the affected filesystem objects.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined exact existing directory-entry identity.
    pub const fn target_entry_object(&self) -> &'a E {
        self.target_entry_object
    }
}

impl<'a, N: UserNamespaceView, OP: ?Sized, OE: ?Sized, NP: ?Sized, NE: ?Sized>
    InodeRenameContext<'a, N, OP, OE, NP, NE>
{
    /// Binds one exact actor and the four ordered rename object roles.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        old_parent_object: &'a OP,
        old_entry_object: &'a OE,
        new_parent_object: &'a NP,
        new_entry_object: &'a NE,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            old_parent_object,
            old_entry_object,
            new_parent_object,
            new_entry_object,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the affected filesystem objects.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact old parent-directory identity.
    pub const fn old_parent_object(&self) -> &'a OP {
        self.old_parent_object
    }

    /// Borrows the embedding-defined exact old source-entry identity.
    pub const fn old_entry_object(&self) -> &'a OE {
        self.old_entry_object
    }

    /// Borrows the embedding-defined exact new parent-directory identity.
    pub const fn new_parent_object(&self) -> &'a NP {
        self.new_parent_object
    }

    /// Borrows the embedding-defined exact new destination-entry identity.
    pub const fn new_entry_object(&self) -> &'a NE {
        self.new_entry_object
    }
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized, T: ?Sized>
    InodeSymlinkContext<'a, N, P, E, T>
{
    /// Binds one exact actor, destination, prospective entry, and target.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        new_entry_object: &'a E,
        symlink_target: &'a T,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            new_entry_object,
            symlink_target,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the destination filesystem.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined prospective named-entry identity.
    pub const fn new_entry_object(&self) -> &'a E {
        self.new_entry_object
    }

    /// Borrows the exact target payload which publication will store.
    pub const fn symlink_target(&self) -> &'a T {
        self.symlink_target
    }
}

impl<'a, N: UserNamespaceView, P: ?Sized, E: ?Sized> InodeMknodContext<'a, N, P, E> {
    /// Binds one exact actor, destination, prospective entry, and operation.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        parent_object: &'a P,
        new_entry_object: &'a E,
        operation: InodeMknodOperation,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            parent_object,
            new_entry_object,
            operation,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for DAC admission.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the destination filesystem.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact parent-directory identity.
    pub const fn parent_object(&self) -> &'a P {
        self.parent_object
    }

    /// Borrows the embedding-defined prospective named-entry identity.
    pub const fn new_entry_object(&self) -> &'a E {
        self.new_entry_object
    }

    /// Returns the normalized special-node creation operation.
    pub const fn operation(&self) -> InodeMknodOperation {
        self.operation
    }
}

/// Maximum number of bytes in one Linux extended-attribute name.
///
/// This excludes the terminating NUL used at the syscall boundary. Attribute
/// names contain no embedded NUL and are otherwise opaque bytes which do not
/// have to be valid UTF-8.
pub const XATTR_NAME_MAX: usize = LINUX_XATTR_NAME_MAX as usize;

/// Validated Linux-compatible flags for one extended-attribute set operation.
///
/// Zero permits either creating a missing attribute or replacing an existing
/// one. Linux v6.18 preserves the [`Self::CREATE`] and [`Self::REPLACE`]
/// combination to the provider: each existence predicate remains observable.
/// Unknown bits are rejected.
///
/// ```compile_fail
/// use thekernel_linux_cred::XattrSetFlags;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = XattrSetFlags(0);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct XattrSetFlags(u32);

impl XattrSetFlags {
    const CREATE_BIT: u32 = 0x1;
    const REPLACE_BIT: u32 = 0x2;

    /// Create a missing attribute or replace an existing one.
    pub const NONE: Self = Self(0);
    /// Require that the named attribute does not already exist.
    pub const CREATE: Self = Self(Self::CREATE_BIT);
    /// Require that the named attribute already exists.
    pub const REPLACE: Self = Self(Self::REPLACE_BIT);
    /// Preserve both Linux existence predicates for the provider's atomic
    /// decision. This is not equivalent to [`Self::NONE`].
    pub const CREATE_AND_REPLACE: Self = Self(Self::CREATE_BIT | Self::REPLACE_BIT);

    /// Constructs flags from the raw Linux-compatible bit domain.
    ///
    /// Accepts every combination of `CREATE` and `REPLACE`. Unknown bits
    /// return `None`.
    pub const fn try_from_bits(bits: u32) -> Option<Self> {
        if bits & !(Self::CREATE_BIT | Self::REPLACE_BIT) == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the validated Linux-compatible bit representation.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether the Linux create-existence predicate is present.
    pub const fn requires_create(self) -> bool {
        self.0 & Self::CREATE_BIT != 0
    }

    /// Returns whether the Linux replace-existence predicate is present.
    pub const fn requires_replace(self) -> bool {
        self.0 & Self::REPLACE_BIT != 0
    }
}

/// Policy-facing classification of one set-xattr value payload.
///
/// The class identifies the `security.capability` wire value without parsing
/// it into [`crate::FileCapabilities`] or exposing a kernel, VFS, xattr-store,
/// or provider type. Parsing and commoncap decisions remain separate steps
/// owned by the consumer and this crate's existing pure parser.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum XattrValueClass {
    /// An opaque value for every name other than `security.capability`.
    Opaque,
    /// The opaque Linux `security.capability` wire value.
    SecurityCapability,
}

impl XattrValueClass {
    fn for_name(name: &[u8]) -> Self {
        if name == crate::SECURITY_CAPABILITY_XATTR_NAME {
            Self::SecurityCapability
        } else {
            Self::Opaque
        }
    }
}

/// Borrowed, operation-specific input to one inode xattr policy hook.
///
/// Names and values remain caller-owned wire payloads. Names are exact bytes,
/// with no UTF-8 requirement. The syscall terminator is not part of the slice,
/// and embedded NUL is rejected. The constructors require a length in
/// `1..=`[`XATTR_NAME_MAX`] and preserve Linux's distinct get, list, set, and
/// remove hook shapes without importing pathname lookup, xattr storage,
/// provider dispatch, or errno mapping. [`Self::set`] derives the value class
/// from the exact name bytes, so a caller cannot label another attribute as
/// `security.capability` or hide that name behind [`XattrValueClass::Opaque`].
///
/// The operation cannot outlive the borrowed name:
///
/// ```compile_fail
/// use thekernel_linux_cred::InodeXattrOperation;
///
/// fn name_cannot_escape() -> InodeXattrOperation<'static> {
///     let name = b"user.example".to_vec();
///     InodeXattrOperation::get(name.as_slice()).unwrap()
/// }
/// ```
///
/// A set operation cannot outlive its value either:
///
/// ```compile_fail
/// use thekernel_linux_cred::{InodeXattrOperation, XattrSetFlags};
///
/// fn value_cannot_escape() -> InodeXattrOperation<'static> {
///     let value = vec![1_u8, 2, 3];
///     InodeXattrOperation::set(b"user.example", value.as_slice(), XattrSetFlags::NONE)
///         .unwrap()
/// }
/// ```
///
/// ```compile_fail
/// use thekernel_linux_cred::{InodeXattrOperation, XattrSetFlags, XattrValueClass};
///
/// // Named variants are non-exhaustive so external code cannot bypass name
/// // validation or forge the name-derived value class.
/// let _ = InodeXattrOperation::Set {
///     name: b"",
///     value: &[],
///     flags: XattrSetFlags::NONE,
///     value_class: XattrValueClass::SecurityCapability,
/// };
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum InodeXattrOperation<'a> {
    /// Read one exact named attribute.
    #[non_exhaustive]
    Get {
        /// Exact bounded attribute-name bytes selected by the consumer.
        name: &'a [u8],
    },
    /// Enumerate the names visible on the exact target object.
    List,
    /// Create or replace one exact named attribute value.
    #[non_exhaustive]
    Set {
        /// Exact bounded attribute-name bytes selected by the consumer.
        name: &'a [u8],
        /// Exact opaque value bytes proposed for publication.
        value: &'a [u8],
        /// Validated create/replace condition.
        flags: XattrSetFlags,
        /// Name-derived policy classification of `value`.
        value_class: XattrValueClass,
    },
    /// Remove one exact named attribute.
    #[non_exhaustive]
    Remove {
        /// Exact bounded attribute-name bytes selected by the consumer.
        name: &'a [u8],
    },
}

impl<'a> InodeXattrOperation<'a> {
    /// Constructs one named-attribute read operation.
    pub fn get(name: &'a [u8]) -> Option<Self> {
        if !valid_xattr_name(name) {
            None
        } else {
            Some(Self::Get { name })
        }
    }

    /// Constructs one attribute-name enumeration operation.
    pub const fn list() -> Self {
        Self::List
    }

    /// Constructs one named-attribute set operation over exact borrowed bytes.
    pub fn set(name: &'a [u8], value: &'a [u8], flags: XattrSetFlags) -> Option<Self> {
        if !valid_xattr_name(name) {
            None
        } else {
            Some(Self::Set {
                name,
                value,
                flags,
                value_class: XattrValueClass::for_name(name),
            })
        }
    }

    /// Constructs one named-attribute removal operation.
    pub fn remove(name: &'a [u8]) -> Option<Self> {
        if !valid_xattr_name(name) {
            None
        } else {
            Some(Self::Remove { name })
        }
    }

    /// Borrows the exact name for a named operation, or `None` for list.
    pub const fn name(self) -> Option<&'a [u8]> {
        match self {
            Self::Get { name } | Self::Set { name, .. } | Self::Remove { name } => Some(name),
            Self::List => None,
        }
    }

    /// Borrows the exact set value, or `None` for operations without a value.
    pub const fn value(self) -> Option<&'a [u8]> {
        match self {
            Self::Set { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Returns the validated set flags, or `None` for non-set operations.
    pub const fn set_flags(self) -> Option<XattrSetFlags> {
        match self {
            Self::Set { flags, .. } => Some(flags),
            _ => None,
        }
    }

    /// Returns the set value's name-derived class, if this is a set operation.
    pub const fn value_class(self) -> Option<XattrValueClass> {
        match self {
            Self::Set { value_class, .. } => Some(value_class),
            _ => None,
        }
    }
}

fn valid_xattr_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= XATTR_NAME_MAX && !name.contains(&0)
}

/// Complete immutable input to one inode xattr policy hook.
///
/// `O` is the embedding kernel's exact pinned target identity. The context
/// retains the immutable actor separately from the DAC snapshot selected for
/// this operation, plus the target filesystem's owner namespace and one typed
/// borrowed xattr operation. It performs no namespace policy, DAC check,
/// lookup, xattr-store mutation, hook dispatch, post-set notification,
/// `security.capability` parsing, or errno mapping.
pub struct InodeXattrContext<'a, N: UserNamespaceView, O: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    target_object: &'a O,
    operation: InodeXattrOperation<'a>,
}

impl<'a, N: UserNamespaceView, O: ?Sized> InodeXattrContext<'a, N, O> {
    /// Binds one exact actor, target object, owner namespace, and xattr input.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
        operation: InodeXattrOperation<'a>,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            operation,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for this operation.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the affected filesystem object.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact target-object identity.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the exact borrowed xattr operation presented to policy.
    pub const fn operation(&self) -> InodeXattrOperation<'a> {
        self.operation
    }
}

/// Normalized persistent access mode for one opened file.
///
/// This enum is deliberately not a raw `O_ACCMODE` value. [`Self::NoData`]
/// preserves Linux's reserved access mode 3 after the ABI adapter performs its
/// read-and-write admission check: the resulting description has neither
/// persistent read nor write data access and is primarily useful to drivers
/// which expose ioctl-only descriptors. This contract is constructed only for
/// opens which reach Linux's `security_file_open` topology. `O_PATH` returns
/// before that hook and is intentionally not representable here.
///
/// ```compile_fail
/// use thekernel_linux_cred::FileOpenAccess;
///
/// // Path-only opens do not enter the file-open hook contract.
/// let _ = FileOpenAccess::Path;
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FileOpenAccess {
    /// A persistently readable file description.
    Read,
    /// A persistently writable file description.
    Write,
    /// A persistently readable and writable file description.
    ReadWrite,
    /// Linux access mode 3: no persistent data access after read/write admission.
    ///
    /// [`Self::reads`] and [`Self::writes`] both return `false` for this
    /// variant.
    NoData,
}

impl FileOpenAccess {
    /// Reports whether the resulting file description is readable.
    pub const fn reads(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// Reports whether the resulting file description is writable.
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    const fn admits_unnamed_create(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite | Self::NoData)
    }
}

/// Normalized, already-validated facts for one file-open policy hook.
///
/// The fields are private and the constructor consumes typed access plus
/// normalized facts, never raw file-descriptor or `open(2)` flags. `created`
/// records that this open transaction created the exact target. `unnamed`
/// narrows that fact to a newly created but currently unnamed VFS object, such
/// as a successful `O_TMPFILE` open; it does not mean an anonymous inode or a
/// non-VFS source. `O_PATH` opens bypass Linux's file-open hook and therefore
/// must not cause a consumer to construct or dispatch this operation.
///
/// `truncate` is kept independent from persistent write access because Linux
/// accepts an already-validated `O_RDONLY | O_TRUNC` request and performs an
/// open-time write-like operation. The same separation lets reserved access
/// mode 3 carry validated truncation or creation facts even though its
/// resulting description has no persistent data access. In contrast, append
/// only has a normalized effect on a persistently writable description and
/// must be cleared before constructing a [`FileOpenAccess::NoData`] operation.
/// Linux's mode-3 `ACC_MODE` still contains `MAY_WRITE`, however, so `NoData`
/// may represent a successfully created unnamed `O_TMPFILE` even though
/// [`FileOpenAccess::writes`] remains false.
///
/// ```compile_fail
/// use thekernel_linux_cred::{FileOpenAccess, FileOpenOperation};
///
/// // External code cannot forge a combination through raw fields.
/// let _ = FileOpenOperation {
///     access: FileOpenAccess::Read,
///     append: false,
///     truncate: false,
///     created: false,
///     unnamed: false,
/// };
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileOpenOperation {
    access: FileOpenAccess,
    append: bool,
    truncate: bool,
    created: bool,
    unnamed: bool,
}

impl FileOpenOperation {
    /// Constructs one normalized file-open operation.
    ///
    /// Returns `None` when append is attached to a non-writable access mode, or
    /// an unnamed object is not newly created under write-admitted `Write`,
    /// `ReadWrite`, or reserved mode-3 `NoData` access.
    pub const fn new(
        access: FileOpenAccess,
        append: bool,
        truncate: bool,
        created: bool,
        unnamed: bool,
    ) -> Option<Self> {
        if (append && !access.writes())
            || (unnamed && (!created || !access.admits_unnamed_create()))
        {
            return None;
        }
        Some(Self {
            access,
            append,
            truncate,
            created,
            unnamed,
        })
    }

    /// Returns the normalized persistent access mode.
    pub const fn access(self) -> FileOpenAccess {
        self.access
    }

    /// Reports whether writes through the opened description append.
    pub const fn append(self) -> bool {
        self.append
    }

    /// Reports whether this request includes an open-time truncation.
    pub const fn truncate(self) -> bool {
        self.truncate
    }

    /// Reports whether this transaction created the exact target object.
    pub const fn created(self) -> bool {
        self.created
    }

    /// Reports whether the created target currently has no directory entry.
    pub const fn unnamed(self) -> bool {
        self.unnamed
    }
}

/// Complete immutable input to one file-open policy hook.
///
/// `O` is the embedding kernel's exact pinned file/location identity. This
/// leaf merely binds that opaque object to the exact actor, owning namespace,
/// selected DAC credential, and normalized operation. The DAC snapshot may
/// intentionally differ from the actor's ordinary filesystem identity. VFS
/// lookup, open transaction ownership, hook registry storage, dispatch, and
/// publication remain consumer responsibilities.
pub struct FileOpenContext<'a, N: UserNamespaceView, O: ?Sized> {
    actor: &'a Credential<N>,
    dac_credential: &'a FsCredentialSnapshot,
    target_owner_user_ns: &'a Arc<N>,
    target_object: &'a O,
    operation: FileOpenOperation,
}

impl<'a, N: UserNamespaceView, O: ?Sized> FileOpenContext<'a, N, O> {
    /// Binds one exact actor, object owner, target object, and open operation.
    pub const fn new(
        actor: &'a Credential<N>,
        dac_credential: &'a FsCredentialSnapshot,
        target_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
        operation: FileOpenOperation,
    ) -> Self {
        Self {
            actor,
            dac_credential,
            target_owner_user_ns,
            target_object,
            operation,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact filesystem identity selected for this open's DAC check.
    pub const fn dac_credential(&self) -> &'a FsCredentialSnapshot {
        self.dac_credential
    }

    /// Borrows the user namespace which owns the exact target object.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_owner_user_ns
    }

    /// Borrows the embedding-defined exact target object identity.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the normalized file-open operation.
    pub const fn operation(&self) -> FileOpenOperation {
        self.operation
    }
}

/// Valid Linux capability number accepted by a security-policy context.
///
/// Keeping the raw value behind this type prevents an invalid capability from
/// reaching namespace-owner shortcuts or stacked policy dispatch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityNumber(u32);

impl CapabilityNumber {
    /// Highest capability supported by the pinned Linux ABI.
    pub const MAX: u32 = CAP_LAST_CAP;

    /// Validates one raw Linux capability number.
    pub const fn try_new(raw: u32) -> Option<Self> {
        if CapabilitySets::cap_mask(raw).is_some() {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// Returns the validated raw Linux capability number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Normalized operation metadata for one Linux `capable` policy hook.
///
/// These variants replace Linux's raw `CAP_OPT_*` bits. Commoncap currently
/// applies the same namespace/effective-set rule to all three, while stacked
/// modules may use the frozen audit and set-ID intent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CapabilitySecurityOperation {
    /// Ordinary capability use corresponding to `CAP_OPT_NONE`.
    Use,
    /// Capability use corresponding to `CAP_OPT_NOAUDIT`.
    UseWithoutAudit,
    /// A set-ID or setgroups check corresponding to `CAP_OPT_INSETID`.
    SetId,
}

/// Operation metadata for a capability decision over one fully prepared,
/// still-unpublished credential.
///
/// This is deliberately separate from [`CapabilitySecurityOperation`]: the
/// live actor remains the source credential while commoncap evaluates the
/// exact proposed credential which will own the new namespace objects.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PreparedCredentialCapabilityOperation {
    /// Authority to create a namespace owned by the proposed credential's
    /// user namespace.
    NamespaceCreate,
}

/// Successful commoncap authorization for one exact capability request.
///
/// The caller supplies the immutable actor credential and target namespace;
/// this type never looks up `current`. Its fields are private, so stacked
/// policy dispatch can only receive a context returned by
/// [`authorize_capability_core`]. A commoncap denial therefore happens before
/// every consumer module, and later modules may only narrow the decision by
/// stopping at their first denial.
///
/// External policy code cannot forge a successful context:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{
///     CapabilityNumber, CapabilitySecurityContext, CapabilitySecurityOperation,
///     Credential, UserNamespaceView,
/// };
///
/// fn forge<'a, N: UserNamespaceView>(
///     actor: &'a Credential<N>,
///     target_user_ns: &'a Arc<N>,
/// ) -> CapabilitySecurityContext<'a, N> {
///     CapabilitySecurityContext {
///         actor,
///         target_user_ns,
///         capability: CapabilityNumber::try_new(0).unwrap(),
///         operation: CapabilitySecurityOperation::Use,
///     }
/// }
/// ```
///
/// The authorization cannot be rebound to a later actor or namespace:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{
///     CapabilityNumber, CapabilitySecurityContext, CapabilitySecurityOperation,
///     Credential, UserNamespaceView, authorize_capability_core,
/// };
///
/// fn cannot_escape<'a, N: UserNamespaceView>(
///     actor: &'a Credential<N>,
///     target: &'a Arc<N>,
/// ) -> CapabilitySecurityContext<'static, N> {
///     authorize_capability_core(
///         actor,
///         target,
///         CapabilityNumber::try_new(0).unwrap(),
///         CapabilitySecurityOperation::Use,
///     )
///     .unwrap()
/// }
/// ```
pub struct CapabilitySecurityContext<'a, N: UserNamespaceView> {
    actor: &'a Credential<N>,
    target_user_ns: &'a Arc<N>,
    capability: CapabilityNumber,
    operation: CapabilitySecurityOperation,
}

impl<'a, N: UserNamespaceView> CapabilitySecurityContext<'a, N> {
    /// Borrows the exact immutable actor credential admitted by commoncap.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact user namespace governing the requested capability.
    pub const fn target_user_ns(&self) -> &'a Arc<N> {
        self.target_user_ns
    }

    /// Returns the validated requested capability.
    pub const fn capability(&self) -> CapabilityNumber {
        self.capability
    }

    /// Returns the frozen capability-check operation.
    pub const fn operation(&self) -> CapabilitySecurityOperation {
        self.operation
    }
}

/// Applies Linux commoncap before stacked capable-policy hooks.
///
/// The returned field-private context is the proof that commoncap admitted the
/// exact actor, target namespace, capability, and operation. An embedding
/// registry passes that same context through modules in declaration order and
/// must stop at the first denial; no module may turn a prior denial into an
/// admission.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] when the actor lacks the
/// requested effective capability over `target_user_ns`.
pub fn authorize_capability_core<'a, N: UserNamespaceView>(
    actor: &'a Credential<N>,
    target_user_ns: &'a Arc<N>,
    capability: CapabilityNumber,
    operation: CapabilitySecurityOperation,
) -> Result<CapabilitySecurityContext<'a, N>, AuthorizationError> {
    if !ns_capable(actor, target_user_ns, capability.get()) {
        return Err(AuthorizationError::NotPermitted);
    }
    Ok(CapabilitySecurityContext {
        actor,
        target_user_ns,
        capability,
        operation,
    })
}

/// Successful commoncap authorization over one exact prepared credential.
///
/// The source is the live actor which initiated the transition. The proposed
/// credential and target namespace are immutable inputs retained by the
/// embedding kernel until publication or rollback. Stacked modules may only
/// narrow this successful commoncap decision.
pub struct PreparedCredentialCapabilityContext<'a, N: UserNamespaceView> {
    source_credential: &'a Credential<N>,
    proposed_credential: &'a Credential<N>,
    target_user_ns: &'a Arc<N>,
    capability: CapabilityNumber,
    operation: PreparedCredentialCapabilityOperation,
}

impl<'a, N: UserNamespaceView> PreparedCredentialCapabilityContext<'a, N> {
    /// Borrows the exact live credential which initiated preparation.
    pub const fn source_credential(&self) -> &'a Credential<N> {
        self.source_credential
    }

    /// Borrows the exact still-unpublished credential admitted by commoncap.
    pub const fn proposed_credential(&self) -> &'a Credential<N> {
        self.proposed_credential
    }

    /// Borrows the namespace governing the requested capability.
    pub const fn target_user_ns(&self) -> &'a Arc<N> {
        self.target_user_ns
    }

    /// Returns the validated raw Linux capability number.
    pub const fn capability(&self) -> CapabilityNumber {
        self.capability
    }

    /// Returns the frozen prepared-credential operation.
    pub const fn operation(&self) -> PreparedCredentialCapabilityOperation {
        self.operation
    }
}

/// Applies commoncap to one exact prepared credential without relabeling it as
/// a live capable actor.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] when `proposed_credential`
/// lacks the requested authority over `target_user_ns`.
pub fn authorize_prepared_credential_capability_core<'a, N: UserNamespaceView>(
    source_credential: &'a Credential<N>,
    proposed_credential: &'a Credential<N>,
    target_user_ns: &'a Arc<N>,
    capability: CapabilityNumber,
    operation: PreparedCredentialCapabilityOperation,
) -> Result<PreparedCredentialCapabilityContext<'a, N>, AuthorizationError> {
    if !ns_capable(proposed_credential, target_user_ns, capability.get()) {
        return Err(AuthorizationError::NotPermitted);
    }
    Ok(PreparedCredentialCapabilityContext {
        source_credential,
        proposed_credential,
        target_user_ns,
        capability,
        operation,
    })
}

/// Credential lifecycle event which has already become externally visible.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CredentialPublicationOperation {
    /// A separately prepared child credential became visible through fork.
    Fork,
    /// A child credential rooted in a newly created user namespace became visible.
    UserNamespace,
}

/// Immutable input to an infallible credential-publication notification.
///
/// `O` is an embedding-defined identity for the exact child or publication
/// target which became visible. The context binds it to the immutable source
/// and published credentials; the target namespace is derived from the latter
/// and cannot be supplied independently. Constructors perform no lookup,
/// allocation, or fallible work.
///
/// An embedding kernel must prepare and authorize module state before making
/// the target visible. Only after successful publication may it construct this
/// context and invoke infallible module callbacks in frozen order. Those
/// callbacks return no error and cannot roll back or retroactively fail the
/// already-visible fork or user-namespace child. Shared-thread clones which do
/// not create a distinct credential publication are not represented here.
///
/// The notification cannot outlive the consumer-owned publication target:
///
/// ```compile_fail
/// use thekernel_linux_cred::{
///     Credential, CredentialPublicationContext, UserNamespaceView,
/// };
///
/// fn target_cannot_escape<N: UserNamespaceView>(
///     source: &'static Credential<N>,
///     published: &'static Credential<N>,
/// ) -> CredentialPublicationContext<'static, N, [u8]> {
///     let target = vec![1_u8, 2, 3];
///     CredentialPublicationContext::fork(source, published, target.as_slice())
/// }
/// ```
#[must_use = "a visible credential publication must be delivered to notification hooks"]
pub struct CredentialPublicationContext<'a, N: UserNamespaceView, O: ?Sized> {
    source_credential: &'a Credential<N>,
    published_credential: &'a Credential<N>,
    target_object: &'a O,
    operation: CredentialPublicationOperation,
}

impl<'a, N: UserNamespaceView, O: ?Sized> CredentialPublicationContext<'a, N, O> {
    /// Binds a successfully published fork child to its exact source and target.
    pub const fn fork(
        source_credential: &'a Credential<N>,
        published_credential: &'a Credential<N>,
        target_object: &'a O,
    ) -> Self {
        Self {
            source_credential,
            published_credential,
            target_object,
            operation: CredentialPublicationOperation::Fork,
        }
    }

    /// Binds a successfully published user-namespace child to its exact source
    /// and target.
    pub const fn user_namespace(
        source_credential: &'a Credential<N>,
        published_credential: &'a Credential<N>,
        target_object: &'a O,
    ) -> Self {
        Self {
            source_credential,
            published_credential,
            target_object,
            operation: CredentialPublicationOperation::UserNamespace,
        }
    }

    /// Borrows the immutable credential from which publication was prepared.
    pub const fn source_credential(&self) -> &'a Credential<N> {
        self.source_credential
    }

    /// Borrows the exact immutable credential which became visible.
    pub const fn published_credential(&self) -> &'a Credential<N> {
        self.published_credential
    }

    /// Borrows the source credential's owning user namespace.
    pub const fn source_user_ns(&self) -> &'a Arc<N> {
        self.source_credential.user_ns()
    }

    /// Borrows the namespace governing the published credential.
    pub const fn target_user_ns(&self) -> &'a Arc<N> {
        self.published_credential.user_ns()
    }

    /// Borrows the embedding-defined exact published target identity.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the frozen publication operation.
    pub const fn operation(&self) -> CredentialPublicationOperation {
        self.operation
    }
}

/// Ptrace operation class presented to security policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtraceAccessKind {
    /// Observe target state without establishing a controlling attachment.
    Read,
    /// Attach to or otherwise control the target.
    Attach,
}

/// Actor capability view selected by a ptrace-style operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtraceCredentialKind {
    /// Use the actor's permitted capabilities, matching a real-credential check.
    Real,
    /// Use the actor's effective capabilities, matching a filesystem check.
    Fs,
}

/// Complete immutable input to one ptrace access hook or commoncap decision.
///
/// `O` is an embedding-defined identity for the exact pinned target object,
/// such as an image-generation token. No trait or ownership assumption is
/// imposed on it; the context only borrows the payload.
pub struct PtraceAccessContext<'a, N: UserNamespaceView, O: ?Sized> {
    actor: &'a Credential<N>,
    target: &'a Credential<N>,
    target_image_owner_user_ns: &'a Arc<N>,
    target_object: &'a O,
    access_kind: PtraceAccessKind,
    credential_kind: PtraceCredentialKind,
}

impl<'a, N: UserNamespaceView, O: ?Sized> PtraceAccessContext<'a, N, O> {
    /// Constructs a context from already-frozen actor, target, image-owner, and
    /// object facts.
    pub const fn new(
        actor: &'a Credential<N>,
        target: &'a Credential<N>,
        target_image_owner_user_ns: &'a Arc<N>,
        target_object: &'a O,
        access_kind: PtraceAccessKind,
        credential_kind: PtraceCredentialKind,
    ) -> Self {
        Self {
            actor,
            target,
            target_image_owner_user_ns,
            target_object,
            access_kind,
            credential_kind,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact immutable target credential.
    pub const fn target(&self) -> &'a Credential<N> {
        self.target
    }

    /// Borrows the user namespace which owns the pinned target image.
    pub const fn target_image_owner_user_ns(&self) -> &'a Arc<N> {
        self.target_image_owner_user_ns
    }

    /// Borrows the embedding-defined exact target object payload.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the requested ptrace operation class.
    pub const fn access_kind(&self) -> PtraceAccessKind {
        self.access_kind
    }

    /// Returns the actor capability view selected by the caller's ABI rule.
    pub const fn credential_kind(&self) -> PtraceCredentialKind {
        self.credential_kind
    }
}

/// Complete immutable input to one `PTRACE_TRACEME` decision.
///
/// The prospective parent tracer is explicitly the actor and the calling child
/// is explicitly the target, preventing the easy-to-miss direction reversal.
pub struct PtraceTracemeContext<'a, N: UserNamespaceView, O: ?Sized> {
    parent_actor: &'a Credential<N>,
    child_target: &'a Credential<N>,
    child_image_owner_user_ns: &'a Arc<N>,
    child_object: &'a O,
}

impl<'a, N: UserNamespaceView, O: ?Sized> PtraceTracemeContext<'a, N, O> {
    /// Constructs a traceme context from an already-frozen parent/child pair and
    /// exact child object.
    pub const fn new(
        parent_actor: &'a Credential<N>,
        child_target: &'a Credential<N>,
        child_image_owner_user_ns: &'a Arc<N>,
        child_object: &'a O,
    ) -> Self {
        Self {
            parent_actor,
            child_target,
            child_image_owner_user_ns,
            child_object,
        }
    }

    /// Borrows the prospective parent tracer credential.
    pub const fn parent_actor(&self) -> &'a Credential<N> {
        self.parent_actor
    }

    /// Borrows the calling child target credential.
    pub const fn child_target(&self) -> &'a Credential<N> {
        self.child_target
    }

    /// Borrows the user namespace which owns the pinned child image.
    pub const fn child_image_owner_user_ns(&self) -> &'a Arc<N> {
        self.child_image_owner_user_ns
    }

    /// Borrows the embedding-defined exact child object payload.
    pub const fn child_object(&self) -> &'a O {
        self.child_object
    }
}

/// One Linux-visible scheduler mutation presented to security policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerSecurityOperation {
    /// Change the target scheduling policy.
    ///
    /// The policy snapshot freezes the old and requested real-time state and
    /// priorities together with the target task's soft `RLIMIT_RTPRIO` value.
    /// This lets commoncap distinguish an unprivileged priority raise, which
    /// Linux may admit through that limit, from a policy change that only
    /// lowers priority or leaves the real-time classes.
    SetPolicy {
        /// Whether the requested scheduling class differs from the current one.
        policy_changed: bool,
        /// Whether the target's current policy is real-time.
        current_realtime: bool,
        /// The target's current real-time priority.
        current_rt_priority: u8,
        /// Whether the requested policy is real-time.
        requested_realtime: bool,
        /// The requested real-time priority.
        requested_rt_priority: u8,
        /// Frozen soft `RLIMIT_RTPRIO` for the target task.
        rlimit_rtprio: u64,
        /// Whether the target's current policy is `SCHED_IDLE`.
        current_idle: bool,
        /// Whether the requested policy is `SCHED_IDLE`.
        requested_idle: bool,
        /// The target's current nice value. `SCHED_IDLE` is represented by
        /// Linux's effective baseline nice value of 20.
        current_nice: i8,
        /// The requested nice value.
        requested_nice: i8,
        /// Frozen soft `RLIMIT_NICE` value for the target task.
        rlimit_nice: u64,
        /// Whether the target policy currently enables reset-on-fork.
        current_reset_on_fork: bool,
        /// Whether the requested policy enables reset-on-fork.
        reset_on_fork: bool,
    },
    /// Change parameters of the target's current scheduling policy.
    SetParam {
        /// Whether the target policy is a real-time class.
        realtime: bool,
        /// RT priority observed in the same scheduler snapshot as the policy.
        old_rt_priority: u8,
        /// Requested RT priority.
        new_rt_priority: u8,
        /// Frozen target task soft `RLIMIT_RTPRIO` value.
        rlimit_rtprio: u64,
    },
    /// Change the target CPU-affinity mask.
    SetAffinity,
    /// Change the target nice value against one frozen caller `RLIMIT_NICE`
    /// value.  The ownership gate is evaluated against the target user
    /// namespace, while the priority-raise exemption is evaluated in the
    /// caller's current user namespace, matching `set_user_nice()`.
    SetNice {
        /// Current nice value.
        current_nice: i8,
        /// Requested nice value.
        requested_nice: i8,
        /// Frozen soft `RLIMIT_NICE` value.
        rlimit_nice: u64,
    },
}

/// Immutable actor/target pair presented to Linux `security_task_getscheduler`.
///
/// Unlike scheduler mutation authorization this hook has no commoncap gate:
/// the caller has already resolved the exact task, and each LSM decides
/// whether its scheduling policy may be observed.
pub struct TaskGetSchedulerContext<'a, N: UserNamespaceView> {
    actor: &'a Credential<N>,
    target: &'a Credential<N>,
}

impl<'a, N: UserNamespaceView> TaskGetSchedulerContext<'a, N> {
    /// Freezes the actor and target credentials for one observation.
    pub const fn new(actor: &'a Credential<N>, target: &'a Credential<N>) -> Self {
        Self { actor, target }
    }

    /// Returns the credential which issued the query.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Returns the credential owned by the resolved task.
    pub const fn target(&self) -> &'a Credential<N> {
        self.target
    }
}

/// Complete immutable input to one scheduler authorization decision.
///
/// Ownership is computed internally from the frozen credentials; callers
/// cannot provide or forge an `owner_match` fact.
pub struct SchedulerSecurityContext<'a, N: UserNamespaceView> {
    actor: &'a Credential<N>,
    target: &'a Credential<N>,
    operation: SchedulerSecurityOperation,
    owner_match: bool,
}

impl<'a, N: UserNamespaceView> SchedulerSecurityContext<'a, N> {
    /// Constructs one context and derives the Linux scheduler ownership relation.
    pub fn new(
        actor: &'a Credential<N>,
        target: &'a Credential<N>,
        operation: SchedulerSecurityOperation,
    ) -> Self {
        Self {
            actor,
            target,
            operation,
            owner_match: scheduler_owner_matches(actor, target),
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact immutable target credential.
    pub const fn target(&self) -> &'a Credential<N> {
        self.target
    }

    /// Returns the frozen scheduler operation.
    pub const fn operation(&self) -> SchedulerSecurityOperation {
        self.operation
    }

    /// Returns the internally derived Linux ownership relation.
    pub const fn owner_match(&self) -> bool {
        self.owner_match
    }
}

fn scheduler_owner_matches<N: UserNamespaceView>(
    actor: &Credential<N>,
    target: &Credential<N>,
) -> bool {
    let actor_euid = actor.ids().euid;
    let target_ids = target.ids();
    actor_euid == target_ids.ruid || actor_euid == target_ids.euid
}

fn selected_actor_capabilities<N: UserNamespaceView>(
    actor: &Credential<N>,
    credential_kind: PtraceCredentialKind,
) -> [u32; CAPABILITY_WORDS] {
    let capabilities = actor.capabilities();
    match credential_kind {
        PtraceCredentialKind::Real => capabilities.permitted(),
        PtraceCredentialKind::Fs => capabilities.effective(),
    }
}

fn commoncap_ptrace_allows<N: UserNamespaceView>(
    actor: &Credential<N>,
    target: &Credential<N>,
    credential_kind: PtraceCredentialKind,
) -> bool {
    let selected_actor = selected_actor_capabilities(actor, credential_kind);
    let target_permitted = target.capabilities().permitted();
    (Arc::ptr_eq(actor.user_ns(), target.user_ns())
        && target_permitted
            .iter()
            .zip(selected_actor.iter())
            .all(|(target, actor)| target & !actor == 0))
        || ns_capable(actor, target.user_ns(), CAP_SYS_PTRACE)
}

/// Applies Linux commoncap's ptrace rule to one frozen access context.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] when the selected actor
/// capability view neither contains the target permitted set in the same
/// namespace nor holds `CAP_SYS_PTRACE` over the target namespace.
pub fn commoncap_ptrace_access<N: UserNamespaceView, O: ?Sized>(
    context: &PtraceAccessContext<'_, N, O>,
) -> Result<(), AuthorizationError> {
    if commoncap_ptrace_allows(context.actor, context.target, context.credential_kind) {
        Ok(())
    } else {
        Err(AuthorizationError::NotPermitted)
    }
}

/// Applies Linux commoncap's `PTRACE_TRACEME` rule.
///
/// The parent actor always uses its permitted capability view.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] when the prospective parent
/// tracer lacks authority over the calling child.
pub fn commoncap_ptrace_traceme<N: UserNamespaceView, O: ?Sized>(
    context: &PtraceTracemeContext<'_, N, O>,
) -> Result<(), AuthorizationError> {
    if commoncap_ptrace_allows(
        context.parent_actor,
        context.child_target,
        PtraceCredentialKind::Real,
    ) {
        Ok(())
    } else {
        Err(AuthorizationError::NotPermitted)
    }
}

/// Origin class for one userspace-requested signal authorization.
///
/// This is intentionally narrower than a raw `siginfo_t`: the embedding ABI
/// adapter validates and resolves userspace records first, then preserves only
/// the source facts a policy module can safely consume without retaining a
/// userspace pointer or repeating usercopy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalSecuritySource {
    /// `kill(2)` and its process-group or broadcast forms.
    Kill,
    /// `tkill(2)` or `tgkill(2)`.
    Thread,
    /// `rt_sigqueueinfo(2)` or `rt_tgsigqueueinfo(2)` with its validated code.
    Queued {
        /// Frozen `si_code` copied from the mandatory userspace record.
        code: i32,
    },
    /// `pidfd_send_signal(2)` with its validated optional `siginfo_t` code.
    PidFd {
        /// Frozen supplied or kernel-synthesized `si_code`.
        code: i32,
    },
}

/// Publication scope after an exact target task passes authorization.
///
/// Linux can authorize a named non-leader task while publishing to that
/// task's shared thread-group pending queue, so this is deliberately separate
/// from source and object identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalDeliveryScope {
    /// Publish to the target task's thread group/shared pending queue.
    ThreadGroup,
    /// Publish only to the exact target task's private pending queue.
    Thread,
}

/// Valid Linux signal number accepted by the typed security contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SignalNumber(NonZeroU32);

impl SignalNumber {
    /// Highest signal number in the Linux 64-signal ABI set.
    pub const MAX: u32 = 64;

    /// Validates a raw Linux signal number.
    pub const fn try_new(raw: u32) -> Option<Self> {
        if raw > Self::MAX {
            return None;
        }
        match NonZeroU32::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the validated raw Linux signal number.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// One already-validated userspace signal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalSecurityOperation {
    signal: Option<SignalNumber>,
    source: SignalSecuritySource,
    delivery_scope: SignalDeliveryScope,
}

impl SignalSecurityOperation {
    /// Constructs a signal-zero existence/permission probe.
    pub const fn probe(source: SignalSecuritySource, delivery_scope: SignalDeliveryScope) -> Self {
        Self {
            signal: None,
            source,
            delivery_scope,
        }
    }

    /// Constructs a request to send one already-validated nonzero signal.
    pub const fn send(
        signal: SignalNumber,
        source: SignalSecuritySource,
        delivery_scope: SignalDeliveryScope,
    ) -> Self {
        Self {
            signal: Some(signal),
            source,
            delivery_scope,
        }
    }

    /// Constructs the corresponding probe or send operation.
    pub const fn from_optional_signal(
        signal: Option<SignalNumber>,
        source: SignalSecuritySource,
        delivery_scope: SignalDeliveryScope,
    ) -> Self {
        match signal {
            Some(signal) => Self::send(signal, source, delivery_scope),
            None => Self::probe(source, delivery_scope),
        }
    }

    /// Returns the requested signal, or `None` for a signal-zero probe.
    pub const fn signal(self) -> Option<SignalNumber> {
        self.signal
    }

    /// Fallibly constructs a send operation from an untrusted raw number.
    /// Embedding adapters which already parsed a signal should prefer
    /// [`Self::send`] and carry [`SignalNumber`] directly.
    pub const fn try_send(
        signal: u32,
        source: SignalSecuritySource,
        delivery_scope: SignalDeliveryScope,
    ) -> Option<Self> {
        match SignalNumber::try_new(signal) {
            Some(signal) => Some(Self::send(signal, source, delivery_scope)),
            None => None,
        }
    }

    /// Returns the frozen userspace request source.
    pub const fn source(self) -> SignalSecuritySource {
        self.source
    }

    /// Returns the frozen publication scope.
    pub const fn delivery_scope(self) -> SignalDeliveryScope {
        self.delivery_scope
    }
}

/// Linux core rule which admitted a signal request before policy hooks run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalCoreAuthorizationReason {
    /// Actor and target belong to the same Linux thread group.
    SameThreadGroup,
    /// A real/effective actor UID matches the target real/saved UID.
    CredentialMatch,
    /// The actor holds `CAP_KILL` over the target credential namespace.
    Capability,
    /// A `SIGCONT` request crosses credentials within the same session.
    SigcontSameSession,
}

/// Opaque proof that Linux's non-LSM signal permission rule admitted one exact
/// immutable actor/target pair.
///
/// The token borrows both credentials and carries the normalized operation, so
/// it cannot be rebound to a later credential snapshot before construction of
/// [`SignalSecurityContext`].
pub struct SignalCoreAuthorization<'a, N: UserNamespaceView> {
    actor: &'a Credential<N>,
    target: &'a Credential<N>,
    operation: SignalSecurityOperation,
    reason: SignalCoreAuthorizationReason,
    same_thread_group: bool,
    sigcont_same_session: bool,
}

impl<'a, N: UserNamespaceView> SignalCoreAuthorization<'a, N> {
    /// Borrows the exact immutable actor credential admitted by the core rule.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the exact immutable target credential admitted by the core rule.
    pub const fn target(&self) -> &'a Credential<N> {
        self.target
    }

    /// Returns the frozen signal operation.
    pub const fn operation(&self) -> SignalSecurityOperation {
        self.operation
    }

    /// Returns the core rule which admitted this request.
    pub const fn reason(&self) -> SignalCoreAuthorizationReason {
        self.reason
    }

    /// Returns the already-resolved same-thread-group fact.
    pub const fn same_thread_group(&self) -> bool {
        self.same_thread_group
    }

    /// Returns whether the request is `SIGCONT` and both processes share one
    /// already-resolved session identity.
    pub const fn sigcont_same_session(&self) -> bool {
        self.sigcont_same_session
    }
}

/// Applies Linux's core signal permission rule before stacked policy hooks.
///
/// Same-thread-group admission is checked first. Otherwise the actor's real or
/// effective UID must match the target's real or saved UID, the actor must hold
/// `CAP_KILL` over the target namespace, or `SIGCONT` must target the same
/// already-resolved session. The session fact is ignored for every other
/// signal and for a signal-zero probe.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] when none of the core rules
/// admits the exact immutable credential pair.
pub fn authorize_signal_core<'a, N: UserNamespaceView>(
    actor: &'a Credential<N>,
    target: &'a Credential<N>,
    operation: SignalSecurityOperation,
    same_thread_group: bool,
    same_session: bool,
) -> Result<SignalCoreAuthorization<'a, N>, AuthorizationError> {
    let actor_ids = actor.ids();
    let target_ids = target.ids();
    let credential_match = [actor_ids.ruid, actor_ids.euid]
        .into_iter()
        .any(|uid| uid == target_ids.ruid || uid == target_ids.suid);
    let sigcont_same_session = operation
        .signal
        .is_some_and(|signal| signal.get() == SIGCONT)
        && same_session;
    let reason = if same_thread_group {
        SignalCoreAuthorizationReason::SameThreadGroup
    } else if credential_match {
        SignalCoreAuthorizationReason::CredentialMatch
    } else if ns_capable(actor, target.user_ns(), CAP_KILL) {
        SignalCoreAuthorizationReason::Capability
    } else if sigcont_same_session {
        SignalCoreAuthorizationReason::SigcontSameSession
    } else {
        return Err(AuthorizationError::NotPermitted);
    };
    Ok(SignalCoreAuthorization {
        actor,
        target,
        operation,
        reason,
        same_thread_group,
        sigcont_same_session,
    })
}

/// Complete immutable input to one stacked signal policy hook.
///
/// `O` is an embedding-owned identity for the exact live thread, process, or
/// durable zombie selected before dispatch. The context can only be built from
/// a successful [`authorize_signal_core`] token, preserving the Linux ordering
/// between core signal permission and policy-module authorization.
pub struct SignalSecurityContext<'a, N: UserNamespaceView, O: ?Sized> {
    authorization: SignalCoreAuthorization<'a, N>,
    target_object: &'a O,
}

impl<'a, N: UserNamespaceView, O: ?Sized> SignalSecurityContext<'a, N, O> {
    /// Binds a successful core authorization to one exact target object.
    pub const fn new(authorization: SignalCoreAuthorization<'a, N>, target_object: &'a O) -> Self {
        Self {
            authorization,
            target_object,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.authorization.actor
    }

    /// Borrows the exact immutable target credential.
    pub const fn target(&self) -> &'a Credential<N> {
        self.authorization.target
    }

    /// Borrows the target credential's owning user namespace.
    pub const fn target_owner_user_ns(&self) -> &'a Arc<N> {
        self.authorization.target.user_ns()
    }

    /// Borrows the embedding-defined exact target object identity.
    pub const fn target_object(&self) -> &'a O {
        self.target_object
    }

    /// Returns the frozen userspace signal operation.
    pub const fn operation(&self) -> SignalSecurityOperation {
        self.authorization.operation
    }

    /// Returns the Linux core rule which admitted the request.
    pub const fn core_reason(&self) -> SignalCoreAuthorizationReason {
        self.authorization.reason
    }

    /// Returns the already-resolved same-thread-group fact.
    pub const fn same_thread_group(&self) -> bool {
        self.authorization.same_thread_group
    }

    /// Returns the already-resolved `SIGCONT` same-session fact.
    pub const fn sigcont_same_session(&self) -> bool {
        self.authorization.sigcont_same_session
    }
}

fn valid_rlimit_nice(rlimit_nice: u64) -> Option<u64> {
    (rlimit_nice <= 40).then_some(rlimit_nice)
}

fn nice_to_rlimit(nice: i8) -> Option<u64> {
    (-20..=19)
        .contains(&nice)
        .then_some((20_i32 - nice as i32) as u64)
}

fn rlimit_allows_nice(rlimit_nice: u64, requested_nice: i8) -> bool {
    let Some(rlimit_nice) = valid_rlimit_nice(rlimit_nice) else {
        return false;
    };
    nice_to_rlimit(requested_nice).is_some_and(|required| required <= rlimit_nice)
}

/// Applies Linux commoncap's scheduler authority, `RLIMIT_RTPRIO`, and
/// `RLIMIT_NICE` rules.
///
/// # Errors
///
/// Returns [`AuthorizationError::NotPermitted`] for an ownership/capability or
/// real-time authority failure. Returns [`AuthorizationError::AccessDenied`]
/// when an owner without `CAP_SYS_NICE` requests a nicer value not admitted by
/// the frozen `RLIMIT_NICE` value.
pub fn commoncap_scheduler<N: UserNamespaceView>(
    context: &SchedulerSecurityContext<'_, N>,
) -> Result<(), AuthorizationError> {
    // Permission to operate on another task is relative to the target's user
    // namespace.  This is deliberately distinct from `can_nice()`: that
    // check asks whether the caller is capable in its *own* current namespace.
    let target_capable = ns_capable(context.actor, context.target.user_ns(), CAP_SYS_NICE);
    let caller_capable = ns_capable(context.actor, context.actor.user_ns(), CAP_SYS_NICE);
    if !context.owner_match && !target_capable {
        return Err(AuthorizationError::NotPermitted);
    }

    match context.operation {
        // Linux lets an unprivileged owner enable reset-on-fork, but only a
        // target-namespace `CAP_SYS_NICE` holder may clear it again.
        SchedulerSecurityOperation::SetPolicy {
            current_reset_on_fork: true,
            reset_on_fork: false,
            ..
        } if !target_capable => Err(AuthorizationError::NotPermitted),
        // A real-time policy-class change needs either `CAP_SYS_NICE` in the
        // target namespace or a nonzero `RLIMIT_RTPRIO`, even when its RT
        // priority remains unchanged (for example, FIFO to RR).
        SchedulerSecurityOperation::SetPolicy {
            policy_changed: true,
            requested_realtime: true,
            rlimit_rtprio: 0,
            ..
        } if !target_capable => Err(AuthorizationError::NotPermitted),
        SchedulerSecurityOperation::SetPolicy {
            current_rt_priority,
            requested_realtime: true,
            requested_rt_priority,
            rlimit_rtprio,
            ..
        } if !target_capable
            && requested_rt_priority > current_rt_priority
            && u64::from(requested_rt_priority) > rlimit_rtprio =>
        {
            Err(AuthorizationError::NotPermitted)
        }
        // sched_setattr() changes policy and nice as one scheduler
        // transaction.  Apply can_nice() as part of that single commoncap
        // decision rather than asking the LSM twice through SetNice followed
        // by SetPolicy.  The limit belongs to the target task snapshot, the
        // capability exception to the caller's current namespace.
        SchedulerSecurityOperation::SetPolicy {
            current_nice,
            requested_nice,
            rlimit_nice,
            ..
        } if requested_nice < current_nice
            && !caller_capable
            && !rlimit_allows_nice(rlimit_nice, requested_nice) =>
        {
            Err(AuthorizationError::NotPermitted)
        }
        // Leaving `SCHED_IDLE` reduces nice from Linux's idle baseline and is
        // therefore an unprivileged priority raise only when `RLIMIT_NICE`
        // admits the requested nice value.
        SchedulerSecurityOperation::SetPolicy {
            current_idle: true,
            requested_idle: false,
            requested_nice,
            rlimit_nice,
            ..
        } if !target_capable && !rlimit_allows_nice(rlimit_nice, requested_nice) => {
            Err(AuthorizationError::NotPermitted)
        }
        // `sched_setparam()` may raise an existing real-time priority only
        // when the caller's current namespace capability or the target's
        // frozen `RLIMIT_RTPRIO` admits the requested value.
        SchedulerSecurityOperation::SetParam {
            realtime: true,
            old_rt_priority,
            new_rt_priority,
            rlimit_rtprio,
        } if new_rt_priority > old_rt_priority
            && !caller_capable
            && rlimit_rtprio != RLIM_INFINITY as i64 as u64
            && new_rt_priority as u64 > rlimit_rtprio =>
        {
            Err(AuthorizationError::NotPermitted)
        }
        SchedulerSecurityOperation::SetNice {
            current_nice,
            requested_nice,
            rlimit_nice,
        } if requested_nice < current_nice
            && !ns_capable(context.actor, context.actor.user_ns(), CAP_SYS_NICE)
            && !rlimit_allows_nice(rlimit_nice, requested_nice) =>
        {
            Err(AuthorizationError::AccessDenied)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{string::ToString, sync::Arc, vec};

    use linux_raw_sys::general::{
        CAP_CHOWN, CAP_KILL, CAP_LAST_CAP, CAP_SYS_NICE, CAP_SYS_PTRACE, SIGTERM,
    };

    use super::*;
    use crate::{
        CAPABILITY_VALID_MASK, CapabilitySets, CredentialIds, CredentialTransitionMode, GroupInfo,
        Kgid, Kuid,
    };

    struct MockNamespace {
        level: u32,
        parent: Option<Arc<Self>>,
        owner: Kuid,
        root: Option<Kuid>,
    }

    impl MockNamespace {
        fn root() -> Arc<Self> {
            Arc::new(Self {
                level: 0,
                parent: None,
                owner: Kuid::INITIAL_ROOT,
                root: Some(Kuid::INITIAL_ROOT),
            })
        }

        fn child(parent: &Arc<Self>, owner: Kuid, root: Option<Kuid>) -> Arc<Self> {
            Arc::new(Self {
                level: parent.level + 1,
                parent: Some(parent.clone()),
                owner,
                root,
            })
        }
    }

    impl UserNamespaceView for MockNamespace {
        fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
            self.parent.clone()
        }

        fn level(&self) -> u32 {
            self.level
        }

        fn owner_kuid(&self) -> Kuid {
            self.owner
        }

        fn root_kuid(&self) -> Option<Kuid> {
            self.root
        }

        fn is_initial(&self) -> bool {
            self.parent.is_none()
        }
    }

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    fn capability_set(capabilities: &[u32]) -> [u32; CAPABILITY_WORDS] {
        let mut result = [0; CAPABILITY_WORDS];
        for capability in capabilities {
            let (word, mask) = CapabilitySets::cap_mask(*capability).unwrap();
            result[word] |= mask;
        }
        result
    }

    fn root_credential(namespace: Arc<MockNamespace>) -> Arc<Credential<MockNamespace>> {
        Credential::try_root(namespace).unwrap()
    }

    fn credential_with_identity_and_caps(
        base: &Arc<Credential<MockNamespace>>,
        uid: u32,
        permitted: &[u32],
        effective: &[u32],
    ) -> Arc<Credential<MockNamespace>> {
        let uid = kuid(uid);
        let gid = kgid(uid.into_raw());
        let ids = CredentialIds {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        let caps = CapabilitySets::try_new(
            capability_set(effective),
            capability_set(permitted),
            [0; CAPABILITY_WORDS],
            CAPABILITY_VALID_MASK,
            [0; CAPABILITY_WORDS],
            base.capabilities().securebits(),
        )
        .unwrap();
        Credential::try_from_transition(
            base,
            ids,
            GroupInfo::try_new(vec![gid]).unwrap(),
            caps,
            base.no_new_privs(),
            base.user_ns().clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap()
    }

    fn credential_with_caps(
        base: &Arc<Credential<MockNamespace>>,
        permitted: &[u32],
        effective: &[u32],
    ) -> Arc<Credential<MockNamespace>> {
        let caps = CapabilitySets::try_new(
            capability_set(effective),
            capability_set(permitted),
            [0; CAPABILITY_WORDS],
            CAPABILITY_VALID_MASK,
            [0; CAPABILITY_WORDS],
            base.capabilities().securebits(),
        )
        .unwrap();
        Credential::try_from_transition(
            base,
            base.ids(),
            base.groups().clone(),
            caps,
            base.no_new_privs(),
            base.user_ns().clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap()
    }

    struct DummyImage {
        generation: u64,
    }

    struct DummyHandle {
        cookie: &'static str,
    }

    #[test]
    fn xattr_set_flags_preserve_combined_and_reject_unknown_bits() {
        assert_eq!(XattrSetFlags::try_from_bits(0), Some(XattrSetFlags::NONE));
        assert_eq!(
            XattrSetFlags::try_from_bits(XattrSetFlags::CREATE.bits()),
            Some(XattrSetFlags::CREATE)
        );
        assert_eq!(
            XattrSetFlags::try_from_bits(XattrSetFlags::REPLACE.bits()),
            Some(XattrSetFlags::REPLACE)
        );
        assert_eq!(
            XattrSetFlags::try_from_bits(
                XattrSetFlags::CREATE.bits() | XattrSetFlags::REPLACE.bits()
            ),
            Some(XattrSetFlags::CREATE_AND_REPLACE)
        );
        assert_eq!(XattrSetFlags::try_from_bits(1 << 7), None);
        assert_eq!(
            XattrSetFlags::try_from_bits(XattrSetFlags::CREATE.bits() | (1 << 7)),
            None
        );
    }

    #[test]
    fn inode_xattr_operations_validate_names_and_classify_set_values() {
        assert_eq!(InodeXattrOperation::get(b""), None);
        assert_eq!(
            InodeXattrOperation::set(b"", b"value", XattrSetFlags::NONE),
            None
        );
        assert_eq!(InodeXattrOperation::remove(b""), None);
        assert_eq!(InodeXattrOperation::get(b"user.\0hidden"), None);
        assert_eq!(
            InodeXattrOperation::set(b"user.\0hidden", b"value", XattrSetFlags::NONE),
            None
        );
        assert_eq!(InodeXattrOperation::remove(b"user.\0hidden"), None);

        let maximum_name = [0xff; XATTR_NAME_MAX];
        assert_eq!(
            InodeXattrOperation::get(maximum_name.as_slice())
                .unwrap()
                .name(),
            Some(maximum_name.as_slice())
        );
        assert!(
            InodeXattrOperation::set(maximum_name.as_slice(), b"value", XattrSetFlags::NONE)
                .is_some()
        );
        assert!(InodeXattrOperation::remove(maximum_name.as_slice()).is_some());

        let oversized_name = [b'x'; XATTR_NAME_MAX + 1];
        assert_eq!(InodeXattrOperation::get(oversized_name.as_slice()), None);
        assert_eq!(
            InodeXattrOperation::set(oversized_name.as_slice(), b"value", XattrSetFlags::NONE),
            None
        );
        assert_eq!(InodeXattrOperation::remove(oversized_name.as_slice()), None);

        let value = [1_u8, 2, 3];
        let capability = InodeXattrOperation::set(
            crate::SECURITY_CAPABILITY_XATTR_NAME,
            value.as_slice(),
            XattrSetFlags::CREATE,
        )
        .unwrap();
        assert_eq!(
            capability.value_class(),
            Some(XattrValueClass::SecurityCapability)
        );
        assert_eq!(capability.value(), Some(value.as_slice()));
        assert_eq!(capability.set_flags(), Some(XattrSetFlags::CREATE));

        let non_utf8_name = b"user.\xff";
        let opaque = InodeXattrOperation::set(non_utf8_name, &[], XattrSetFlags::NONE).unwrap();
        assert_eq!(opaque.value_class(), Some(XattrValueClass::Opaque));
        assert_eq!(opaque.name(), Some(non_utf8_name.as_slice()));
        assert_eq!(opaque.value(), Some([].as_slice()));

        let near_capability = b"security.capability\xff";
        let opaque = InodeXattrOperation::set(near_capability, &[], XattrSetFlags::NONE).unwrap();
        assert_eq!(opaque.value_class(), Some(XattrValueClass::Opaque));
        assert_eq!(InodeXattrOperation::list().name(), None);
    }

    #[test]
    fn inode_create_mode_accepts_only_normalized_permission_and_special_bits() {
        assert_eq!(InodeCreateMode::try_from_bits(0).unwrap().bits(), 0);
        assert_eq!(
            InodeCreateMode::try_from_bits(0o7777).unwrap().bits(),
            0o7777
        );
        assert_eq!(InodeCreateMode::try_from_bits(0o100000 | 0o644), None);
        assert_eq!(InodeCreateMode::try_from_bits(1 << 15), None);
    }

    #[test]
    fn named_inode_contexts_bind_distinct_frozen_roles() {
        let actor_namespace = MockNamespace::root();
        let actor = root_credential(actor_namespace.clone());
        let target_owner_namespace =
            MockNamespace::child(&actor_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let dac_credential = FsCredentialSnapshot::new(
            kuid(1000),
            kgid(1000),
            actor.groups().clone(),
            [0; CAPABILITY_WORDS],
            false,
        );
        let parent = DummyHandle {
            cookie: "create-parent",
        };
        let new_entry = DummyHandle {
            cookie: "prospective-named-entry",
        };
        let file_mode = InodeCreateMode::try_from_bits(0o640).unwrap();
        let create = InodeCreateContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &parent,
            &new_entry,
            file_mode,
        );
        assert!(core::ptr::eq(create.actor(), actor.as_ref()));
        assert!(core::ptr::eq(create.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(
            create.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(core::ptr::eq(create.parent_object(), &parent));
        assert!(core::ptr::eq(create.new_entry_object(), &new_entry));
        assert_eq!(create.parent_object().cookie, "create-parent");
        assert_eq!(create.new_entry_object().cookie, "prospective-named-entry");
        assert_eq!(create.mode(), file_mode);

        let directory_mode = InodeCreateMode::try_from_bits(0o2750).unwrap();
        let mkdir = InodeMkdirContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &parent,
            &new_entry,
            directory_mode,
        );
        assert!(core::ptr::eq(mkdir.parent_object(), &parent));
        assert!(core::ptr::eq(mkdir.new_entry_object(), &new_entry));
        assert_eq!(mkdir.mode(), directory_mode);

        let target = [0xff, b'/', b't', b'a', b'r', b'g', b'e', b't'];
        let symlink = InodeSymlinkContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &parent,
            &new_entry,
            target.as_slice(),
        );
        assert!(core::ptr::eq(symlink.actor(), actor.as_ref()));
        assert!(core::ptr::eq(symlink.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(
            symlink.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(core::ptr::eq(symlink.parent_object(), &parent));
        assert!(core::ptr::eq(symlink.new_entry_object(), &new_entry));
        assert!(core::ptr::eq(symlink.symlink_target(), target.as_slice()));
        assert_eq!(symlink.symlink_target(), target.as_slice());

        let source = DummyHandle {
            cookie: "exact-hard-link-source",
        };
        let link = InodeLinkContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &source,
            &parent,
            &new_entry,
        );
        assert!(core::ptr::eq(link.actor(), actor.as_ref()));
        assert!(core::ptr::eq(link.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(
            link.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(core::ptr::eq(link.source_object(), &source));
        assert!(core::ptr::eq(link.parent_object(), &parent));
        assert!(core::ptr::eq(link.new_entry_object(), &new_entry));
        assert_eq!(link.source_object().cookie, "exact-hard-link-source");

        let target_entry = DummyHandle {
            cookie: "exact-existing-victim-entry",
        };
        let unlink = InodeUnlinkContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &parent,
            &target_entry,
        );
        assert!(core::ptr::eq(unlink.actor(), actor.as_ref()));
        assert!(core::ptr::eq(unlink.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(
            unlink.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(core::ptr::eq(unlink.parent_object(), &parent));
        assert!(core::ptr::eq(unlink.target_entry_object(), &target_entry));
        assert_eq!(
            unlink.target_entry_object().cookie,
            "exact-existing-victim-entry"
        );

        let rmdir = InodeRmdirContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &parent,
            &target_entry,
        );
        assert!(core::ptr::eq(rmdir.actor(), actor.as_ref()));
        assert!(core::ptr::eq(rmdir.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(
            rmdir.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(core::ptr::eq(rmdir.parent_object(), &parent));
        assert!(core::ptr::eq(rmdir.target_entry_object(), &target_entry));
    }

    #[test]
    fn inode_rename_context_binds_four_ordered_roles() {
        let namespace = MockNamespace::root();
        let actor = root_credential(namespace.clone());
        let dac_credential = FsCredentialSnapshot::new(
            kuid(3300),
            kgid(3300),
            actor.groups().clone(),
            [0; CAPABILITY_WORDS],
            false,
        );
        let old_parent = DummyHandle {
            cookie: "exact-old-parent",
        };
        let old_entry = DummyHandle {
            cookie: "exact-old-entry",
        };
        let new_parent = DummyHandle {
            cookie: "exact-new-parent",
        };
        let new_entry = DummyHandle {
            cookie: "exact-new-entry",
        };

        let forward = InodeRenameContext::new(
            &actor,
            &dac_credential,
            &namespace,
            &old_parent,
            &old_entry,
            &new_parent,
            &new_entry,
        );
        assert!(core::ptr::eq(forward.actor(), actor.as_ref()));
        assert!(core::ptr::eq(forward.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(forward.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(forward.old_parent_object(), &old_parent));
        assert!(core::ptr::eq(forward.old_entry_object(), &old_entry));
        assert!(core::ptr::eq(forward.new_parent_object(), &new_parent));
        assert!(core::ptr::eq(forward.new_entry_object(), &new_entry));
        assert_eq!(forward.old_parent_object().cookie, "exact-old-parent");
        assert_eq!(forward.old_entry_object().cookie, "exact-old-entry");
        assert_eq!(forward.new_parent_object().cookie, "exact-new-parent");
        assert_eq!(forward.new_entry_object().cookie, "exact-new-entry");

        // Linux's exchange wrapper dispatches this reverse direction before
        // the forward direction; neither leaf context carries a flag.
        let reverse = InodeRenameContext::new(
            &actor,
            &dac_credential,
            &namespace,
            &new_parent,
            &new_entry,
            &old_parent,
            &old_entry,
        );
        assert!(core::ptr::eq(reverse.old_parent_object(), &new_parent));
        assert!(core::ptr::eq(reverse.old_entry_object(), &new_entry));
        assert!(core::ptr::eq(reverse.new_parent_object(), &old_parent));
        assert!(core::ptr::eq(reverse.new_entry_object(), &old_entry));
    }

    #[test]
    fn inode_mknod_operation_enforces_kind_device_pairing() {
        let mode = InodeCreateMode::try_from_bits(0o600).unwrap();
        for kind in [InodeMknodKind::Fifo, InodeMknodKind::Socket] {
            let operation = InodeMknodOperation::new(kind, mode, None).unwrap();
            assert_eq!(operation.kind(), kind);
            assert_eq!(operation.mode(), mode);
            assert_eq!(operation.rdev(), None);
            assert_eq!(InodeMknodOperation::new(kind, mode, Some(7)), None);
        }
        for kind in [InodeMknodKind::CharacterDevice, InodeMknodKind::BlockDevice] {
            assert_eq!(InodeMknodOperation::new(kind, mode, None), None);
            let operation = InodeMknodOperation::new(kind, mode, Some(0x1234)).unwrap();
            assert_eq!(operation.kind(), kind);
            assert_eq!(operation.mode(), mode);
            assert_eq!(operation.rdev(), Some(0x1234));
        }
    }

    #[test]
    fn inode_mknod_context_binds_operation_and_opaque_entries() {
        let namespace = MockNamespace::root();
        let actor = root_credential(namespace.clone());
        let dac_credential = actor.fs_credential_snapshot();
        let parent = DummyHandle {
            cookie: "mknod-parent",
        };
        let new_entry = DummyHandle {
            cookie: "mknod-entry",
        };
        let operation = InodeMknodOperation::new(
            InodeMknodKind::CharacterDevice,
            InodeCreateMode::try_from_bits(0o620).unwrap(),
            Some(0x0501),
        )
        .unwrap();
        let context = InodeMknodContext::new(
            &actor,
            &dac_credential,
            &namespace,
            &parent,
            &new_entry,
            operation,
        );

        assert!(core::ptr::eq(context.actor(), actor.as_ref()));
        assert!(core::ptr::eq(context.dac_credential(), &dac_credential));
        assert!(Arc::ptr_eq(context.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(context.parent_object(), &parent));
        assert!(core::ptr::eq(context.new_entry_object(), &new_entry));
        assert_eq!(context.operation(), operation);
    }

    #[test]
    fn inode_permission_access_is_nonempty_known_and_composable() {
        assert_eq!(InodePermissionAccess::try_from_bits(0), None);
        assert_eq!(InodePermissionAccess::try_from_bits(1 << 7), None);
        assert_eq!(
            InodePermissionAccess::try_from_bits(InodePermissionAccess::ALL.bits()),
            Some(InodePermissionAccess::ALL)
        );

        let mut access = InodePermissionAccess::READ | InodePermissionAccess::WRITE;
        assert!(access.contains(InodePermissionAccess::READ));
        assert!(access.contains(InodePermissionAccess::WRITE));
        assert!(!access.contains(InodePermissionAccess::EXECUTE));
        assert!(access.intersects(InodePermissionAccess::WRITE));
        assert!(!access.intersects(InodePermissionAccess::EXECUTE));
        access |= InodePermissionAccess::EXECUTE;
        assert_eq!(access, InodePermissionAccess::ALL);
    }

    #[test]
    fn inode_permission_context_binds_actor_owner_object_and_access() {
        let actor_namespace = MockNamespace::root();
        let actor = root_credential(actor_namespace.clone());
        let target_owner_namespace =
            MockNamespace::child(&actor_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let object = DummyHandle {
            cookie: "inode-location",
        };
        let dac_credential = FsCredentialSnapshot::new(
            kuid(1000),
            kgid(1000),
            actor.groups().clone(),
            [0; CAPABILITY_WORDS],
            false,
        );
        let access = InodePermissionAccess::READ | InodePermissionAccess::EXECUTE;
        let context = InodePermissionContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &object,
            access,
        );

        assert!(core::ptr::eq(context.actor(), actor.as_ref()));
        assert!(core::ptr::eq(context.dac_credential(), &dac_credential));
        assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
        assert!(Arc::ptr_eq(
            context.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(!Arc::ptr_eq(
            context.target_owner_user_ns(),
            actor.user_ns()
        ));
        assert!(core::ptr::eq(context.target_object(), &object));
        assert_eq!(context.target_object().cookie, "inode-location");
        assert_eq!(context.access(), access);
    }

    #[test]
    fn file_open_operation_normalizes_creation_and_mutation_facts() {
        let read_truncate = FileOpenOperation::new(FileOpenAccess::Read, false, true, false, false)
            .expect("Linux O_RDONLY|O_TRUNC remains representable");
        assert_eq!(read_truncate.access(), FileOpenAccess::Read);
        assert!(read_truncate.truncate());
        assert_eq!(
            FileOpenOperation::new(FileOpenAccess::Read, true, false, false, false),
            None
        );

        let no_data = FileOpenOperation::new(FileOpenAccess::NoData, false, true, true, false)
            .expect("reserved access mode 3 can retain truncate/create facts");
        assert!(!no_data.access().reads());
        assert!(!no_data.access().writes());
        assert!(no_data.truncate());
        assert!(no_data.created());
        assert_eq!(
            FileOpenOperation::new(FileOpenAccess::NoData, true, false, false, false),
            None
        );
        let no_data_unnamed =
            FileOpenOperation::new(FileOpenAccess::NoData, false, false, true, true)
                .expect("mode-3 ACC_MODE admits O_TMPFILE creation");
        assert!(!no_data_unnamed.access().writes());
        assert!(no_data_unnamed.created());
        assert!(no_data_unnamed.unnamed());
        assert_eq!(
            FileOpenOperation::new(FileOpenAccess::NoData, false, false, false, true),
            None
        );

        let unnamed = FileOpenOperation::new(FileOpenAccess::Write, false, false, true, true)
            .expect("writable created O_TMPFILE target is valid");
        assert!(unnamed.created());
        assert!(unnamed.unnamed());
        assert_eq!(
            FileOpenOperation::new(FileOpenAccess::Write, false, false, false, true),
            None
        );
        assert_eq!(
            FileOpenOperation::new(FileOpenAccess::Read, false, false, true, true),
            None
        );
    }

    #[test]
    fn file_open_context_binds_exact_noncopy_object_and_owner_namespace() {
        let actor_namespace = MockNamespace::root();
        let actor = root_credential(actor_namespace.clone());
        let target_owner_namespace =
            MockNamespace::child(&actor_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let object = DummyHandle {
            cookie: "open-location",
        };
        let dac_credential = FsCredentialSnapshot::new(
            kuid(1000),
            kgid(1000),
            actor.groups().clone(),
            [0; CAPABILITY_WORDS],
            false,
        );
        let operation =
            FileOpenOperation::new(FileOpenAccess::ReadWrite, true, true, true, false).unwrap();
        let context = FileOpenContext::new(
            &actor,
            &dac_credential,
            &target_owner_namespace,
            &object,
            operation,
        );

        assert!(core::ptr::eq(context.actor(), actor.as_ref()));
        assert!(core::ptr::eq(context.dac_credential(), &dac_credential));
        assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
        assert!(Arc::ptr_eq(
            context.target_owner_user_ns(),
            &target_owner_namespace
        ));
        assert!(!Arc::ptr_eq(
            context.target_owner_user_ns(),
            actor.user_ns()
        ));
        assert!(core::ptr::eq(context.target_object(), &object));
        assert_eq!(context.target_object().cookie, "open-location");
        assert_eq!(context.operation(), operation);
        assert!(context.operation().access().reads());
        assert!(context.operation().access().writes());
        assert!(context.operation().append());
        assert!(context.operation().truncate());
        assert!(context.operation().created());
        assert!(!context.operation().unnamed());
    }

    #[test]
    fn ptrace_real_uses_permitted_and_fs_uses_effective() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace.clone());
        let actor = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let target = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        let image = DummyImage { generation: 1 };

        commoncap_ptrace_access(&PtraceAccessContext::new(
            &actor,
            &target,
            &namespace,
            &image,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        ))
        .unwrap();
        assert_eq!(
            commoncap_ptrace_access(&PtraceAccessContext::new(
                &actor,
                &target,
                &namespace,
                &image,
                PtraceAccessKind::Read,
                PtraceCredentialKind::Fs,
            )),
            Err(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn capability_authorization_is_typed_commoncap_first_and_namespace_directed() {
        assert_eq!(CapabilityNumber::MAX, CAP_LAST_CAP);
        assert_eq!(CapabilityNumber::try_new(0).unwrap().get(), 0);
        assert_eq!(
            CapabilityNumber::try_new(CapabilityNumber::MAX)
                .unwrap()
                .get(),
            CapabilityNumber::MAX
        );
        assert_eq!(CapabilityNumber::try_new(CapabilityNumber::MAX + 1), None);
        assert_eq!(CapabilityNumber::try_new(u32::MAX), None);

        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let capability = CapabilityNumber::try_new(CAP_CHOWN).unwrap();
        let admitted = authorize_capability_core(
            &root,
            &root_namespace,
            capability,
            CapabilitySecurityOperation::UseWithoutAudit,
        )
        .unwrap();
        assert!(core::ptr::eq(admitted.actor(), root.as_ref()));
        assert!(Arc::ptr_eq(admitted.target_user_ns(), &root_namespace));
        assert_eq!(admitted.capability(), capability);
        assert_eq!(
            admitted.operation(),
            CapabilitySecurityOperation::UseWithoutAudit
        );

        let restricted = credential_with_caps(&root, &[], &[]);
        assert_eq!(
            authorize_capability_core(
                &restricted,
                &root_namespace,
                capability,
                CapabilitySecurityOperation::Use,
            )
            .err(),
            Some(AuthorizationError::NotPermitted)
        );

        let child_namespace =
            MockNamespace::child(&root_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let child = Credential::try_with_user_namespace(&root, child_namespace.clone()).unwrap();
        let setid = authorize_capability_core(
            &child,
            &child_namespace,
            capability,
            CapabilitySecurityOperation::SetId,
        )
        .unwrap();
        assert_eq!(setid.operation(), CapabilitySecurityOperation::SetId);
        assert_eq!(
            authorize_capability_core(
                &child,
                &root_namespace,
                capability,
                CapabilitySecurityOperation::Use,
            )
            .err(),
            Some(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn credential_publication_contexts_bind_target_and_notification_kind() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let fork_target = DummyHandle {
            cookie: "fork-child",
        };
        let fork = CredentialPublicationContext::fork(&root, &root, &fork_target);
        assert!(core::ptr::eq(fork.source_credential(), root.as_ref()));
        assert!(core::ptr::eq(fork.published_credential(), root.as_ref()));
        assert!(Arc::ptr_eq(fork.source_user_ns(), &root_namespace));
        assert!(Arc::ptr_eq(fork.target_user_ns(), &root_namespace));
        assert!(core::ptr::eq(fork.target_object(), &fork_target));
        assert_eq!(fork.target_object().cookie, "fork-child");
        assert_eq!(fork.operation(), CredentialPublicationOperation::Fork);

        let child_namespace =
            MockNamespace::child(&root_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let child = Credential::try_with_user_namespace(&root, child_namespace.clone()).unwrap();
        let userns_target = DummyHandle {
            cookie: "userns-child",
        };
        let userns = CredentialPublicationContext::user_namespace(&root, &child, &userns_target);
        assert!(core::ptr::eq(userns.source_credential(), root.as_ref()));
        assert!(core::ptr::eq(userns.published_credential(), child.as_ref()));
        assert!(Arc::ptr_eq(userns.source_user_ns(), &root_namespace));
        assert!(Arc::ptr_eq(userns.target_user_ns(), &child_namespace));
        assert!(core::ptr::eq(userns.target_object(), &userns_target));
        assert_eq!(userns.target_object().cookie, "userns-child");
        assert_eq!(
            userns.operation(),
            CredentialPublicationOperation::UserNamespace
        );
    }

    #[test]
    fn ptrace_same_namespace_subset_and_effective_cap_override_are_independent() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace.clone());
        let empty_actor = credential_with_caps(&root, &[], &[]);
        let empty_target = credential_with_caps(&root, &[], &[]);
        let image = DummyImage { generation: 2 };

        commoncap_ptrace_access(&PtraceAccessContext::new(
            &empty_actor,
            &empty_target,
            &namespace,
            &image,
            PtraceAccessKind::Attach,
            PtraceCredentialKind::Fs,
        ))
        .unwrap();

        let capable = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[CAP_SYS_PTRACE]);
        commoncap_ptrace_access(&PtraceAccessContext::new(
            &capable,
            &root,
            &namespace,
            &image,
            PtraceAccessKind::Attach,
            PtraceCredentialKind::Fs,
        ))
        .unwrap();

        let permitted_only = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[]);
        assert_eq!(
            commoncap_ptrace_access(&PtraceAccessContext::new(
                &permitted_only,
                &root,
                &namespace,
                &image,
                PtraceAccessKind::Attach,
                PtraceCredentialKind::Fs,
            )),
            Err(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn ptrace_capability_follows_namespace_direction() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let child_namespace =
            MockNamespace::child(&root_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let child_root =
            Credential::try_with_user_namespace(&root, child_namespace.clone()).unwrap();
        let target = credential_with_identity_and_caps(&child_root, 1000, &[], &[]);
        let actor = credential_with_caps(&root, &[CAP_SYS_PTRACE], &[CAP_SYS_PTRACE]);
        let image = DummyImage { generation: 3 };

        commoncap_ptrace_access(&PtraceAccessContext::new(
            &actor,
            &target,
            &child_namespace,
            &image,
            PtraceAccessKind::Attach,
            PtraceCredentialKind::Real,
        ))
        .unwrap();

        let child_actor = credential_with_identity_and_caps(
            &child_root,
            1000,
            &[CAP_SYS_PTRACE],
            &[CAP_SYS_PTRACE],
        );
        assert_eq!(
            commoncap_ptrace_access(&PtraceAccessContext::new(
                &child_actor,
                &root,
                &root_namespace,
                &image,
                PtraceAccessKind::Attach,
                PtraceCredentialKind::Real,
            )),
            Err(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn traceme_keeps_parent_actor_and_child_target_direction() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace.clone());
        let parent = credential_with_caps(&root, &[], &[]);
        let child = credential_with_identity_and_caps(&root, 1000, &[CAP_CHOWN], &[]);
        let object = DummyImage { generation: 4 };
        let context = PtraceTracemeContext::new(&parent, &child, &namespace, &object);
        assert_eq!(
            commoncap_ptrace_traceme(&context),
            Err(AuthorizationError::NotPermitted)
        );
        assert_eq!(context.parent_actor().ids().euid, Kuid::INITIAL_ROOT);
        assert_eq!(context.child_target().ids().euid, kuid(1000));

        let parent = credential_with_caps(&root, &[CAP_CHOWN], &[]);
        commoncap_ptrace_traceme(&PtraceTracemeContext::new(
            &parent, &child, &namespace, &object,
        ))
        .unwrap();
    }

    #[test]
    fn contexts_borrow_distinct_noncopy_object_payloads_and_image_owner() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let child_namespace =
            MockNamespace::child(&root_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let child = Credential::try_with_user_namespace(&root, child_namespace).unwrap();
        let image = DummyImage { generation: 41 };
        let handle = DummyHandle { cookie: "exact" };

        let access = PtraceAccessContext::new(
            &root,
            &child,
            &root_namespace,
            &image,
            PtraceAccessKind::Read,
            PtraceCredentialKind::Real,
        );
        let traceme = PtraceTracemeContext::new(&root, &child, &root_namespace, &handle);
        assert_eq!(access.target_object().generation, 41);
        assert_eq!(traceme.child_object().cookie, "exact");
        assert!(Arc::ptr_eq(
            access.target_image_owner_user_ns(),
            &root_namespace
        ));
        assert!(Arc::ptr_eq(
            traceme.child_image_owner_user_ns(),
            &root_namespace
        ));
        assert!(!Arc::ptr_eq(
            access.target_image_owner_user_ns(),
            child.user_ns()
        ));
        assert_eq!(access.access_kind(), PtraceAccessKind::Read);
    }

    #[test]
    fn scheduler_owner_relation_is_internal_and_controls_affinity() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let owned = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let other = credential_with_identity_and_caps(&root, 2000, &[], &[]);

        let owned_context =
            SchedulerSecurityContext::new(&actor, &owned, SchedulerSecurityOperation::SetAffinity);
        assert!(owned_context.owner_match());
        commoncap_scheduler(&owned_context).unwrap();

        let other_context =
            SchedulerSecurityContext::new(&actor, &other, SchedulerSecurityOperation::SetAffinity);
        assert!(!other_context.owner_match());
        assert_eq!(
            commoncap_scheduler(&other_context),
            Err(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn scheduler_policy_uses_target_rtprio_for_unprivileged_priority_raises() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        // Entering a real-time class raises priority from the non-RT zero
        // baseline, so an owner may do it only up to `RLIMIT_RTPRIO`.
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetPolicy {
                    policy_changed: true,
                    current_realtime: false,
                    current_rt_priority: 0,
                    requested_realtime: true,
                    requested_rt_priority: 50,
                    rlimit_rtprio: 49,
                    current_idle: false,
                    requested_idle: false,
                    current_nice: 0,
                    requested_nice: 0,
                    rlimit_nice: 0,
                    current_reset_on_fork: false,
                    reset_on_fork: false,
                },
            )),
            Err(AuthorizationError::NotPermitted)
        );
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetPolicy {
                policy_changed: true,
                current_realtime: false,
                current_rt_priority: 0,
                requested_realtime: true,
                requested_rt_priority: 50,
                rlimit_rtprio: 50,
                current_idle: false,
                requested_idle: false,
                current_nice: 0,
                requested_nice: 0,
                rlimit_nice: 0,
                current_reset_on_fork: false,
                reset_on_fork: false,
            },
        ))
        .unwrap();

        // Switching FIFO and RR at the same priority is still an RT policy
        // change, and a zero `RLIMIT_RTPRIO` does not admit it.
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetPolicy {
                    policy_changed: true,
                    current_realtime: true,
                    current_rt_priority: 50,
                    requested_realtime: true,
                    requested_rt_priority: 50,
                    rlimit_rtprio: 0,
                    current_idle: false,
                    requested_idle: false,
                    current_nice: 0,
                    requested_nice: 0,
                    rlimit_nice: 0,
                    current_reset_on_fork: false,
                    reset_on_fork: false,
                },
            )),
            Err(AuthorizationError::NotPermitted)
        );

        // Raising an existing RT priority follows the same limit, while
        // lowering it or leaving RT does not need a real-time raise grant.
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetPolicy {
                    policy_changed: false,
                    current_realtime: true,
                    current_rt_priority: 50,
                    requested_realtime: true,
                    requested_rt_priority: 51,
                    rlimit_rtprio: 50,
                    current_idle: false,
                    requested_idle: false,
                    current_nice: 0,
                    requested_nice: 0,
                    rlimit_nice: 0,
                    current_reset_on_fork: false,
                    reset_on_fork: false,
                },
            )),
            Err(AuthorizationError::NotPermitted)
        );
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetPolicy {
                policy_changed: false,
                current_realtime: true,
                current_rt_priority: 50,
                requested_realtime: true,
                requested_rt_priority: 49,
                rlimit_rtprio: 0,
                current_idle: false,
                requested_idle: false,
                current_nice: 0,
                requested_nice: 0,
                rlimit_nice: 0,
                current_reset_on_fork: false,
                reset_on_fork: true,
            },
        ))
        .unwrap();
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetPolicy {
                    policy_changed: false,
                    current_realtime: true,
                    current_rt_priority: 49,
                    requested_realtime: true,
                    requested_rt_priority: 49,
                    rlimit_rtprio: 0,
                    current_idle: false,
                    requested_idle: false,
                    current_nice: 0,
                    requested_nice: 0,
                    rlimit_nice: 0,
                    current_reset_on_fork: true,
                    reset_on_fork: false,
                },
            )),
            Err(AuthorizationError::NotPermitted)
        );
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetPolicy {
                policy_changed: true,
                current_realtime: true,
                current_rt_priority: 50,
                requested_realtime: false,
                requested_rt_priority: 0,
                rlimit_rtprio: 0,
                current_idle: false,
                requested_idle: false,
                current_nice: 0,
                requested_nice: 0,
                rlimit_nice: 0,
                current_reset_on_fork: false,
                reset_on_fork: false,
            },
        ))
        .unwrap();

        // An unchanged non-RT parameter is always admitted.
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetParam {
                realtime: false,
                old_rt_priority: 0,
                new_rt_priority: 0,
                rlimit_rtprio: 0,
            },
        ))
        .unwrap();

        let actor =
            credential_with_identity_and_caps(&root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetPolicy {
                policy_changed: true,
                current_realtime: false,
                current_rt_priority: 0,
                requested_realtime: true,
                requested_rt_priority: 99,
                rlimit_rtprio: 0,
                current_idle: false,
                requested_idle: false,
                current_nice: 0,
                requested_nice: 0,
                rlimit_nice: 0,
                current_reset_on_fork: true,
                reset_on_fork: false,
            },
        ))
        .unwrap();
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetParam {
                realtime: true,
                old_rt_priority: 1,
                new_rt_priority: 2,
                rlimit_rtprio: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn scheduler_setparam_uses_target_limit_and_callers_namespace_capability() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        // An unprivileged owner may raise only to its target task's frozen
        // soft limit. The context is intentionally scalar: the kernel must
        // source it from the target task while holding the scheduler tuple.
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &actor,
            &target,
            SchedulerSecurityOperation::SetParam {
                realtime: true,
                old_rt_priority: 1,
                new_rt_priority: 2,
                rlimit_rtprio: 2,
            },
        ))
        .unwrap();
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetParam {
                    realtime: true,
                    old_rt_priority: 1,
                    new_rt_priority: 2,
                    rlimit_rtprio: 1,
                },
            )),
            Err(AuthorizationError::NotPermitted)
        );

        let child_namespace = MockNamespace::child(
            &root_namespace,
            Kuid::INITIAL_ROOT,
            Some(Kuid::INITIAL_ROOT),
        );
        let child_root = Credential::try_with_user_namespace(&root, child_namespace).unwrap();
        let child_actor =
            credential_with_identity_and_caps(&child_root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);

        // For an owned target, the rlimit exemption is capability in the
        // caller's current namespace, rather than capability in the target's
        // ancestor namespace.
        commoncap_scheduler(&SchedulerSecurityContext::new(
            &child_actor,
            &target,
            SchedulerSecurityOperation::SetParam {
                realtime: true,
                old_rt_priority: 1,
                new_rt_priority: 2,
                rlimit_rtprio: 0,
            },
        ))
        .unwrap();
    }

    #[test]
    fn scheduler_policy_leaving_idle_uses_rlimit_nice() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        let denied = SchedulerSecurityOperation::SetPolicy {
            policy_changed: true,
            current_realtime: false,
            current_rt_priority: 0,
            requested_realtime: false,
            requested_rt_priority: 0,
            rlimit_rtprio: 0,
            current_idle: true,
            requested_idle: false,
            current_nice: 20,
            requested_nice: 0,
            rlimit_nice: 19,
            current_reset_on_fork: false,
            reset_on_fork: false,
        };
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(&actor, &target, denied)),
            Err(AuthorizationError::NotPermitted)
        );

        let admitted = SchedulerSecurityOperation::SetPolicy {
            policy_changed: true,
            current_realtime: false,
            current_rt_priority: 0,
            requested_realtime: false,
            requested_rt_priority: 0,
            rlimit_rtprio: 0,
            current_idle: true,
            requested_idle: false,
            current_nice: 20,
            requested_nice: 0,
            rlimit_nice: 20,
            current_reset_on_fork: false,
            reset_on_fork: false,
        };
        commoncap_scheduler(&SchedulerSecurityContext::new(&actor, &target, admitted)).unwrap();

        let capable =
            credential_with_identity_and_caps(&root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        commoncap_scheduler(&SchedulerSecurityContext::new(&capable, &target, denied)).unwrap();
    }

    #[test]
    fn scheduler_capability_follows_namespace_direction() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let child_namespace =
            MockNamespace::child(&root_namespace, Kuid::INITIAL_ROOT, Some(kuid(1000)));
        let child_root = Credential::try_with_user_namespace(&root, child_namespace).unwrap();
        let child_target = credential_with_identity_and_caps(&child_root, 1000, &[], &[]);

        commoncap_scheduler(&SchedulerSecurityContext::new(
            &root,
            &child_target,
            SchedulerSecurityOperation::SetParam {
                realtime: true,
                old_rt_priority: 1,
                new_rt_priority: 2,
                rlimit_rtprio: 0,
            },
        ))
        .unwrap();

        let child_actor =
            credential_with_identity_and_caps(&child_root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        assert_eq!(
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &child_actor,
                &root,
                SchedulerSecurityOperation::SetAffinity,
            )),
            Err(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn scheduler_nonroot_capability_crosses_owner_boundary() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor =
            credential_with_identity_and_caps(&root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        commoncap_scheduler(&SchedulerSecurityContext::new(
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
    fn scheduler_nice_uses_owner_and_exact_frozen_rlimit() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        commoncap_scheduler(&SchedulerSecurityContext::new(
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
            commoncap_scheduler(&SchedulerSecurityContext::new(
                &actor,
                &target,
                SchedulerSecurityOperation::SetNice {
                    current_nice: 0,
                    requested_nice: -5,
                    rlimit_nice: 24,
                },
            )),
            Err(AuthorizationError::AccessDenied)
        );
        commoncap_scheduler(&SchedulerSecurityContext::new(
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
    fn scheduler_nice_uses_callers_current_namespace_capability() {
        let root_namespace = MockNamespace::root();
        let root = root_credential(root_namespace.clone());
        let child_namespace = MockNamespace::child(
            &root_namespace,
            Kuid::INITIAL_ROOT,
            Some(Kuid::INITIAL_ROOT),
        );
        let child_root = Credential::try_with_user_namespace(&root, child_namespace).unwrap();
        let actor =
            credential_with_identity_and_caps(&child_root, 1000, &[CAP_SYS_NICE], &[CAP_SYS_NICE]);
        let target = credential_with_identity_and_caps(&root, 1000, &[], &[]);

        // The target namespace capability gate is irrelevant for an owned
        // task; can_nice instead observes the actor's current namespace.
        commoncap_scheduler(&SchedulerSecurityContext::new(
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
    fn signal_core_authorization_covers_probe_identity_capability_and_group_rules() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let unrelated = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        let probe = SignalSecurityOperation::probe(
            SignalSecuritySource::Kill,
            SignalDeliveryScope::ThreadGroup,
        );

        assert_eq!(
            authorize_signal_core(&actor, &unrelated, probe, false, false).err(),
            Some(AuthorizationError::NotPermitted)
        );
        let same_group = authorize_signal_core(&actor, &unrelated, probe, true, false).unwrap();
        assert_eq!(
            same_group.reason(),
            SignalCoreAuthorizationReason::SameThreadGroup
        );
        assert!(same_group.same_thread_group());

        let mut saved_ids = unrelated.ids();
        saved_ids.suid = actor.ids().euid;
        let saved_match = Credential::try_from_transition(
            &unrelated,
            saved_ids,
            unrelated.groups().clone(),
            unrelated.capabilities(),
            unrelated.no_new_privs(),
            unrelated.user_ns().clone(),
            CredentialTransitionMode::Normal,
        )
        .unwrap();
        assert_eq!(
            authorize_signal_core(&actor, &saved_match, probe, false, false)
                .unwrap()
                .reason(),
            SignalCoreAuthorizationReason::CredentialMatch
        );

        let capable = credential_with_identity_and_caps(&root, 3000, &[CAP_KILL], &[CAP_KILL]);
        assert_eq!(
            authorize_signal_core(&capable, &unrelated, probe, false, false)
                .unwrap()
                .reason(),
            SignalCoreAuthorizationReason::Capability
        );
    }

    #[test]
    fn signal_core_uses_same_session_only_for_sigcont() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace);
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);

        let sigcont = SignalSecurityOperation::send(
            SignalNumber::try_new(SIGCONT).unwrap(),
            SignalSecuritySource::Queued { code: -1 },
            SignalDeliveryScope::ThreadGroup,
        );
        let authorization = authorize_signal_core(&actor, &target, sigcont, false, true).unwrap();
        assert_eq!(
            authorization.reason(),
            SignalCoreAuthorizationReason::SigcontSameSession
        );
        assert!(authorization.sigcont_same_session());

        let ordinary = SignalSecurityOperation::send(
            SignalNumber::try_new(SIGTERM).unwrap(),
            SignalSecuritySource::Kill,
            SignalDeliveryScope::ThreadGroup,
        );
        assert_eq!(
            authorize_signal_core(&actor, &target, ordinary, false, true).err(),
            Some(AuthorizationError::NotPermitted)
        );
        assert_eq!(
            authorize_signal_core(
                &actor,
                &target,
                SignalSecurityOperation::probe(
                    SignalSecuritySource::Kill,
                    SignalDeliveryScope::ThreadGroup,
                ),
                false,
                true,
            )
            .err(),
            Some(AuthorizationError::NotPermitted)
        );
    }

    #[test]
    fn signal_context_binds_exact_credentials_operation_owner_and_object() {
        let namespace = MockNamespace::root();
        let root = root_credential(namespace.clone());
        let actor = credential_with_identity_and_caps(&root, 1000, &[], &[]);
        let target = credential_with_identity_and_caps(&root, 2000, &[], &[]);
        let operation = SignalSecurityOperation::send(
            SignalNumber::try_new(SIGTERM).unwrap(),
            SignalSecuritySource::PidFd { code: -6 },
            SignalDeliveryScope::Thread,
        );
        let authorization = authorize_signal_core(&actor, &target, operation, true, false).unwrap();
        let object = DummyHandle {
            cookie: "pidfd-target",
        };
        let context = SignalSecurityContext::new(authorization, &object);

        assert!(core::ptr::eq(context.actor(), actor.as_ref()));
        assert!(core::ptr::eq(context.target(), target.as_ref()));
        assert!(Arc::ptr_eq(context.target_owner_user_ns(), &namespace));
        assert!(core::ptr::eq(context.target_object(), &object));
        assert_eq!(context.target_object().cookie, "pidfd-target");
        assert_eq!(context.operation(), operation);
        assert_eq!(
            context.core_reason(),
            SignalCoreAuthorizationReason::SameThreadGroup
        );
        assert!(context.same_thread_group());
        assert!(!context.sigcont_same_session());
    }

    #[test]
    fn signal_send_construction_rejects_zero_and_preserves_nonzero() {
        assert_eq!(
            SignalSecurityOperation::try_send(
                0,
                SignalSecuritySource::Thread,
                SignalDeliveryScope::Thread,
            ),
            None
        );
        let signal = SignalSecurityOperation::try_send(
            15,
            SignalSecuritySource::Thread,
            SignalDeliveryScope::Thread,
        )
        .unwrap();
        assert_eq!(signal.signal(), SignalNumber::try_new(15));
        assert_eq!(signal.source(), SignalSecuritySource::Thread);
        assert_eq!(signal.delivery_scope(), SignalDeliveryScope::Thread);
    }

    #[test]
    fn signal_number_bounds_are_exact() {
        assert_eq!(SignalNumber::try_new(0), None);
        assert_eq!(SignalNumber::try_new(1).unwrap().get(), 1);
        assert_eq!(SignalNumber::try_new(64).unwrap().get(), 64);
        assert_eq!(SignalNumber::try_new(65), None);
        assert_eq!(SignalNumber::try_new(u32::MAX), None);
        assert_eq!(
            SignalSecurityOperation::try_send(
                65,
                SignalSecuritySource::Kill,
                SignalDeliveryScope::ThreadGroup,
            ),
            None
        );
    }

    #[test]
    fn nice_to_rlimit_boundaries_are_exact_and_bounded() {
        assert_eq!(nice_to_rlimit(-20), Some(40));
        assert_eq!(nice_to_rlimit(19), Some(1));
        assert_eq!(nice_to_rlimit(-21), None);
        assert_eq!(nice_to_rlimit(20), None);
        assert!(!rlimit_allows_nice(0, 19));
        assert!(rlimit_allows_nice(1, 19));
        assert!(rlimit_allows_nice(40, -20));
        assert!(!rlimit_allows_nice(u64::MAX, 19));
    }

    #[test]
    fn authorization_errors_remain_policy_neutral() {
        assert_eq!(
            AuthorizationError::NotPermitted.to_string(),
            "security operation not permitted"
        );
        assert_eq!(
            AuthorizationError::AccessDenied.to_string(),
            "security access denied"
        );
    }
}
