//! Policy-neutral typed contexts for Linux memory-mapping security hooks.
//!
//! These values describe the immutable inputs visible at the Linux v6.18
//! `mmap_file`, `mmap_addr`, and `file_mprotect` LSM leaves. They do not look
//! up a current task, resolve a file descriptor, choose an address, inspect or
//! mutate a VMA, dispatch a security-module registry, or map policy failures
//! into errno values.
//!
//! The embedding kernel validates raw protection bits, freezes the exact file,
//! image, or pre-change VMA object, and owns every lock and mutation
//! transaction. Opaque objects are borrowed only for one policy call, so none
//! of their lifetimes can escape through these contexts.

use alloc::sync::Arc;
use core::ops::{BitOr, BitOrAssign};

use crate::{Credential, UserNamespaceView};

/// Validated read, write, and execute protection visible to an MM hook.
///
/// The low three bits deliberately match Linux's architecture-independent
/// `PROT_READ`, `PROT_WRITE`, and `PROT_EXEC` values. `PROT_NONE` is represented
/// by [`Self::NONE`]. Architecture-specific flags, growth selectors, and every
/// other unknown bit must be handled by the embedding ABI adapter and are
/// rejected by [`Self::try_from_bits`].
///
/// ```compile_fail
/// use thekernel_linux_cred::MemoryProtection;
///
/// // Raw tuple construction is not part of the public contract.
/// let _ = MemoryProtection(1);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MemoryProtection(usize);

impl MemoryProtection {
    const READ_BIT: usize = 0x1;
    const WRITE_BIT: usize = 0x2;
    const EXECUTE_BIT: usize = 0x4;
    const ALL_BITS: usize = Self::READ_BIT | Self::WRITE_BIT | Self::EXECUTE_BIT;

    /// No read, write, or execute access (`PROT_NONE`).
    pub const NONE: Self = Self(0);
    /// Read access (`PROT_READ`).
    pub const READ: Self = Self(Self::READ_BIT);
    /// Write access (`PROT_WRITE`).
    pub const WRITE: Self = Self(Self::WRITE_BIT);
    /// Execute access (`PROT_EXEC`).
    pub const EXECUTE: Self = Self(Self::EXECUTE_BIT);
    /// Every protection represented by this contract.
    pub const ALL: Self = Self(Self::ALL_BITS);

    /// Strictly decodes the architecture-independent protection bit domain.
    ///
    /// Zero and every read/write/execute combination are accepted. Any other
    /// bit returns `None`; unknown values are never silently truncated.
    pub const fn try_from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::ALL_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the validated protection bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Reports whether no read, write, or execute access is present.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Reports whether every protection in `other` is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Reports whether any protection in `other` is present.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Combines two validated protection sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOr for MemoryProtection {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for MemoryProtection {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// Lossless raw mapping flags observed by the `mmap_file` leaf.
///
/// Unlike [`MemoryProtection`], mapping flags are not normalized or filtered:
/// Linux passes the full `unsigned long` flags word to the security leaf. The
/// embedding syscall/MM adapter remains responsible for validating flag
/// combinations before preserving that exact word here.
///
/// ```compile_fail
/// use thekernel_linux_cred::MmapFileFlags;
///
/// // The private representation cannot be forged without `from_raw`.
/// let _ = MmapFileFlags(usize::MAX);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MmapFileFlags(usize);

impl MmapFileFlags {
    /// Preserves one complete raw Linux mapping-flags word without truncation.
    pub const fn from_raw(raw: usize) -> Self {
        Self(raw)
    }

    /// Returns the exact raw word supplied at construction.
    pub const fn raw(self) -> usize {
        self.0
    }
}

/// Normalized protection and lossless flags passed to `mmap_file` policy.
///
/// `requested` is the protection requested at the wrapper boundary;
/// `effective` is the protection after the consumer has applied Linux's
/// `READ_IMPLIES_EXEC` and executable-mount rule. Their relationship is not
/// recomputed here because the actor personality and mount state belong to the
/// embedding kernel.
///
/// ```compile_fail
/// use thekernel_linux_cred::{MemoryProtection, MmapFileFlags, MmapFileOperation};
///
/// // Field-private construction prevents replacing a frozen operation fact.
/// let _ = MmapFileOperation {
///     requested: MemoryProtection::READ,
///     effective: MemoryProtection::READ,
///     flags: MmapFileFlags::from_raw(0),
/// };
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MmapFileOperation {
    requested: MemoryProtection,
    effective: MemoryProtection,
    flags: MmapFileFlags,
}

impl MmapFileOperation {
    /// Freezes one consumer-prepared `mmap_file` operation.
    pub const fn new(
        requested: MemoryProtection,
        effective: MemoryProtection,
        flags: MmapFileFlags,
    ) -> Self {
        Self {
            requested,
            effective,
            flags,
        }
    }

    /// Returns the protection requested at the Linux security wrapper.
    pub const fn requested(self) -> MemoryProtection {
        self.requested
    }

    /// Returns the protection presented as effective to the hook leaf.
    pub const fn effective(self) -> MemoryProtection {
        self.effective
    }

    /// Returns the lossless raw mapping flags.
    pub const fn flags(self) -> MmapFileFlags {
        self.flags
    }
}

/// Exact file object and filesystem-owner namespace for a file mapping.
///
/// Fields are private so the two facts cannot be separated or replaced after
/// construction. `F` remains an opaque consumer-owned identity; this crate
/// neither requires nor exposes a concrete VFS or file-description type.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{MmapFileSecurityRef, UserNamespaceView};
///
/// fn forge<'a, N: UserNamespaceView, F: ?Sized>(
///     namespace: &'a Arc<N>,
///     file: &'a F,
/// ) -> MmapFileSecurityRef<'a, N, F> {
///     MmapFileSecurityRef {
///         filesystem_owner_user_ns: namespace,
///         file_object: file,
///     }
/// }
/// ```
pub struct MmapFileSecurityRef<'a, N: UserNamespaceView, F: ?Sized> {
    filesystem_owner_user_ns: &'a Arc<N>,
    file_object: &'a F,
}

impl<'a, N: UserNamespaceView, F: ?Sized> MmapFileSecurityRef<'a, N, F> {
    /// Binds one exact file object to the namespace owning its filesystem.
    pub const fn new(filesystem_owner_user_ns: &'a Arc<N>, file_object: &'a F) -> Self {
        Self {
            filesystem_owner_user_ns,
            file_object,
        }
    }

    /// Borrows the user namespace which owns the mapped filesystem.
    pub const fn filesystem_owner_user_ns(&self) -> &'a Arc<N> {
        self.filesystem_owner_user_ns
    }

    /// Borrows the exact opaque file object selected for the mapping.
    pub const fn file_object(&self) -> &'a F {
        self.file_object
    }
}

/// Anonymous or exact file-backed target passed to `mmap_file` policy.
///
/// Linux invokes the leaf with a null file for anonymous mappings. The
/// separate variants preserve that topology without accepting a forgeable
/// `is_anonymous` boolean. A file variant can contain only the field-private,
/// paired [`MmapFileSecurityRef`].
pub enum MmapFileTarget<'a, N: UserNamespaceView, F: ?Sized = ()> {
    /// An anonymous mapping with no file or filesystem-owner namespace.
    Anonymous,
    /// A mapping of one exact file owned by one exact filesystem namespace.
    File(MmapFileSecurityRef<'a, N, F>),
}

impl<'a, N: UserNamespaceView, F: ?Sized> MmapFileTarget<'a, N, F> {
    /// Reports whether this leaf target is anonymous.
    pub const fn is_anonymous(&self) -> bool {
        matches!(self, Self::Anonymous)
    }

    /// Borrows the paired file security facts, if this mapping is file-backed.
    pub const fn file_security_ref(&self) -> Option<&MmapFileSecurityRef<'a, N, F>> {
        match self {
            Self::Anonymous => None,
            Self::File(file) => Some(file),
        }
    }

    /// Borrows the exact mapped file object, if present.
    pub const fn file_object(&self) -> Option<&'a F> {
        match self {
            Self::Anonymous => None,
            Self::File(file) => Some(file.file_object()),
        }
    }

    /// Borrows the mapped filesystem's owner namespace, if present.
    pub const fn filesystem_owner_user_ns(&self) -> Option<&'a Arc<N>> {
        match self {
            Self::Anonymous => None,
            Self::File(file) => Some(file.filesystem_owner_user_ns()),
        }
    }
}

/// Complete immutable input to one `mmap_file` policy leaf.
///
/// The context binds one exact actor to an anonymous/file target and the
/// wrapper-prepared operation. It intentionally carries no address, length,
/// offset, descriptor number, or mapping transaction because none appears in
/// the Linux v6.18 leaf signature.
///
/// ```compile_fail
/// use thekernel_linux_cred::{MmapFileContext, UserNamespaceView};
///
/// fn inspect_private_fields<N: UserNamespaceView>(context: MmapFileContext<'_, N>) {
///     let MmapFileContext { actor, target, operation } = context;
///     let _ = (actor, target, operation);
/// }
/// ```
pub struct MmapFileContext<'a, N: UserNamespaceView, F: ?Sized = ()> {
    actor: &'a Credential<N>,
    target: MmapFileTarget<'a, N, F>,
    operation: MmapFileOperation,
}

impl<'a, N: UserNamespaceView, F: ?Sized> MmapFileContext<'a, N, F> {
    /// Binds one exact actor, target, and prepared operation.
    pub const fn new(
        actor: &'a Credential<N>,
        target: MmapFileTarget<'a, N, F>,
        operation: MmapFileOperation,
    ) -> Self {
        Self {
            actor,
            target,
            operation,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the anonymous or exact file-backed mapping target.
    pub const fn target(&self) -> &MmapFileTarget<'a, N, F> {
        &self.target
    }

    /// Returns the frozen protection and flags operation.
    pub const fn operation(&self) -> MmapFileOperation {
        self.operation
    }
}

/// Complete immutable input to one `mmap_addr` policy leaf.
///
/// `I` is the exact consumer-owned image/address-space identity used for the
/// selection. The context retains only the final candidate address returned by
/// address selection, matching Linux's leaf: it deliberately has no requested
/// hint, length, offset, file, or mapping flags.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{
///     Credential, MmapAddressContext, UserNamespaceView,
/// };
///
/// fn image_cannot_escape<N: UserNamespaceView + 'static>(
///     actor: &'static Credential<N>,
///     owner: &'static Arc<N>,
/// ) -> MmapAddressContext<'static, N, [u8]> {
///     let image = [1_u8, 2, 3];
///     MmapAddressContext::new(actor, owner, image.as_slice(), 0x4000)
/// }
/// ```
pub struct MmapAddressContext<'a, N: UserNamespaceView, I: ?Sized> {
    actor: &'a Credential<N>,
    image_owner_user_ns: &'a Arc<N>,
    image: &'a I,
    final_address: usize,
}

impl<'a, N: UserNamespaceView, I: ?Sized> MmapAddressContext<'a, N, I> {
    /// Binds one actor and exact image to the final selected address.
    pub const fn new(
        actor: &'a Credential<N>,
        image_owner_user_ns: &'a Arc<N>,
        image: &'a I,
        final_address: usize,
    ) -> Self {
        Self {
            actor,
            image_owner_user_ns,
            image,
            final_address,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the user namespace which owns the selected image.
    pub const fn image_owner_user_ns(&self) -> &'a Arc<N> {
        self.image_owner_user_ns
    }

    /// Borrows the exact opaque image/address-space identity.
    pub const fn image(&self) -> &'a I {
        self.image
    }

    /// Returns the final address produced by consumer address selection.
    pub const fn final_address(&self) -> usize {
        self.final_address
    }
}

/// Complete immutable input to one `file_mprotect` policy leaf.
///
/// `V` is the exact pre-change VMA snapshot selected while the embedding MM
/// transaction is still able to abort. The context does not expose a mutable
/// VMA and does not own splitting, protection-key handling, page-table updates,
/// rollback, or publication.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use thekernel_linux_cred::{
///     Credential, FileMprotectContext, MemoryProtection, UserNamespaceView,
/// };
///
/// fn vma_cannot_escape<N: UserNamespaceView + 'static>(
///     actor: &'static Credential<N>,
///     owner: &'static Arc<N>,
/// ) -> FileMprotectContext<'static, N, [u8]> {
///     let vma = [1_u8, 2, 3];
///     FileMprotectContext::new(
///         actor,
///         owner,
///         vma.as_slice(),
///         MemoryProtection::READ,
///         MemoryProtection::READ | MemoryProtection::EXECUTE,
///     )
/// }
/// ```
pub struct FileMprotectContext<'a, N: UserNamespaceView, V: ?Sized> {
    actor: &'a Credential<N>,
    image_owner_user_ns: &'a Arc<N>,
    pre_change_vma: &'a V,
    requested: MemoryProtection,
    effective: MemoryProtection,
}

impl<'a, N: UserNamespaceView, V: ?Sized> FileMprotectContext<'a, N, V> {
    /// Binds one actor and image owner to an exact pre-change VMA and request.
    pub const fn new(
        actor: &'a Credential<N>,
        image_owner_user_ns: &'a Arc<N>,
        pre_change_vma: &'a V,
        requested: MemoryProtection,
        effective: MemoryProtection,
    ) -> Self {
        Self {
            actor,
            image_owner_user_ns,
            pre_change_vma,
            requested,
            effective,
        }
    }

    /// Borrows the exact immutable actor credential.
    pub const fn actor(&self) -> &'a Credential<N> {
        self.actor
    }

    /// Borrows the user namespace which owns the affected image.
    pub const fn image_owner_user_ns(&self) -> &'a Arc<N> {
        self.image_owner_user_ns
    }

    /// Borrows the exact VMA snapshot from before any protection change.
    pub const fn pre_change_vma(&self) -> &'a V {
        self.pre_change_vma
    }

    /// Returns the protection requested at the Linux hook wrapper.
    pub const fn requested(&self) -> MemoryProtection {
        self.requested
    }

    /// Returns the protection prepared for application to this VMA.
    pub const fn effective(&self) -> MemoryProtection {
        self.effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kuid;

    struct TestNamespace;

    impl UserNamespaceView for TestNamespace {
        fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
            None
        }

        fn level(&self) -> u32 {
            0
        }

        fn owner_kuid(&self) -> Kuid {
            Kuid::INITIAL_ROOT
        }

        fn root_kuid(&self) -> Option<Kuid> {
            Some(Kuid::INITIAL_ROOT)
        }

        fn is_initial(&self) -> bool {
            true
        }
    }

    #[test]
    fn protection_decode_is_strict_and_none_is_valid() {
        assert_eq!(
            MemoryProtection::try_from_bits(0),
            Some(MemoryProtection::NONE)
        );
        assert!(MemoryProtection::NONE.is_none());

        let rwx = MemoryProtection::READ | MemoryProtection::WRITE | MemoryProtection::EXECUTE;
        assert_eq!(rwx, MemoryProtection::ALL);
        assert_eq!(MemoryProtection::try_from_bits(0x7), Some(rwx));
        assert!(rwx.contains(MemoryProtection::WRITE));
        assert!(rwx.intersects(MemoryProtection::EXECUTE));
        assert_eq!(MemoryProtection::try_from_bits(0x8), None);
        assert_eq!(MemoryProtection::try_from_bits(usize::MAX), None);
    }

    #[test]
    fn mmap_operation_preserves_effective_protection_and_full_flags_word() {
        let requested = MemoryProtection::READ;
        let effective = requested | MemoryProtection::EXECUTE;
        let flags = MmapFileFlags::from_raw(usize::MAX);
        let operation = MmapFileOperation::new(requested, effective, flags);

        assert_eq!(operation.requested(), requested);
        assert_eq!(operation.effective(), effective);
        assert_eq!(operation.flags().raw(), usize::MAX);
    }

    #[test]
    fn contexts_retain_exact_borrowed_targets_and_final_leaf_facts() {
        struct File(&'static str);
        struct Image(u64);
        struct PreChangeVma {
            start: usize,
            end: usize,
        }

        let actor_namespace = Arc::new(TestNamespace);
        let filesystem_namespace = Arc::new(TestNamespace);
        let image_namespace = Arc::new(TestNamespace);
        let actor = Credential::try_root(actor_namespace).unwrap();
        let file = File("exact-file");
        let image = Image(17);
        let vma = PreChangeVma {
            start: 0x4000,
            end: 0x8000,
        };
        let operation = MmapFileOperation::new(
            MemoryProtection::READ,
            MemoryProtection::READ,
            MmapFileFlags::from_raw(0x100022),
        );

        let anonymous: MmapFileContext<'_, TestNamespace, File> =
            MmapFileContext::new(&actor, MmapFileTarget::Anonymous, operation);
        assert!(anonymous.target().is_anonymous());
        assert!(anonymous.target().file_object().is_none());
        assert!(anonymous.target().filesystem_owner_user_ns().is_none());

        let file_target =
            MmapFileTarget::File(MmapFileSecurityRef::new(&filesystem_namespace, &file));
        let file_context = MmapFileContext::new(&actor, file_target, operation);
        assert!(core::ptr::eq(file_context.actor(), actor.as_ref()));
        assert!(core::ptr::eq(
            file_context.target().file_object().unwrap(),
            &file
        ));
        assert!(Arc::ptr_eq(
            file_context.target().filesystem_owner_user_ns().unwrap(),
            &filesystem_namespace
        ));
        assert_eq!(file_context.target().file_object().unwrap().0, "exact-file");

        let address = MmapAddressContext::new(&actor, &image_namespace, &image, 0x20_0000);
        assert!(core::ptr::eq(address.image(), &image));
        assert_eq!(address.image().0, 17);
        assert_eq!(address.final_address(), 0x20_0000);

        let mprotect = FileMprotectContext::new(
            &actor,
            &image_namespace,
            &vma,
            MemoryProtection::READ,
            MemoryProtection::READ | MemoryProtection::EXECUTE,
        );
        assert!(core::ptr::eq(mprotect.pre_change_vma(), &vma));
        assert_eq!(mprotect.pre_change_vma().start, 0x4000);
        assert_eq!(mprotect.pre_change_vma().end, 0x8000);
        assert_eq!(mprotect.requested(), MemoryProtection::READ);
        assert_eq!(
            mprotect.effective(),
            MemoryProtection::READ | MemoryProtection::EXECUTE
        );
    }
}
