use alloc::string::String;
use core::marker::PhantomData;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    CreateDisposition, DeviceId, Location, NamedCreateOptions, NamespaceGeneration, NodePermission,
    NodeType,
};
use linux_raw_sys::general::CAP_MKNOD;
use linux_vfs::{MutationBackend, MutationTransaction};

use super::permission::{
    VfsSecurityContext, authorize_hardlink_create, authorize_hardlink_source,
    authorize_inode_rename, authorize_inode_rmdir, authorize_inode_unlink,
    authorize_named_inode_create, authorize_symlink_create,
    check_create_permissions_with_frozen_metadata,
    check_cross_parent_rename_source_permissions_with_security,
    check_remove_permissions_with_security, check_rename_parent_permissions_with_security,
    check_writable_mount, initial_named_create_owner_mode_with_security,
};
use crate::mounts::NamespaceOperationGuard;

const GENERATION_SNAPSHOT_RETRIES: usize = 4;

fn try_owned(value: &str) -> AxResult<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| AxError::NoMemory)?;
    owned.push_str(value);
    Ok(owned)
}

fn lookup_optional(parent: &Location, name: &str) -> AxResult<Option<Location>> {
    match parent.lookup_no_follow_in_mount(name) {
        Ok(location) => Ok(Some(location)),
        Err(AxError::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

fn stable_lookup(
    parent: &Location,
    name: &str,
) -> AxResult<(Option<Location>, NamespaceGeneration)> {
    for _ in 0..GENERATION_SNAPSHOT_RETRIES {
        let before = parent.namespace_generation()?;
        let current = lookup_optional(parent, name)?;
        let after = parent.namespace_generation()?;
        if before == after {
            return Ok((current, after));
        }
    }
    Err(AxError::ResourceBusy)
}

fn validate_expected_identity(
    current: Option<&Location>,
    expected: Option<&Location>,
) -> AxResult<()> {
    match (current, expected) {
        (None, None) => Ok(()),
        (Some(current), Some(expected)) if current.same_node(expected) => Ok(()),
        (Some(_), None) => Err(AxError::AlreadyExists),
        (None, Some(_)) | (Some(_), Some(_)) => Err(AxError::NotFound),
    }
}

/// Owned name snapshot retained by one prepared namespace mutation.
struct PreparedName {
    parent: Location,
    name: String,
    expected: Option<Location>,
    generation: NamespaceGeneration,
}

impl PreparedName {
    fn reserve(parent: &Location, name: &str, expected: Option<&Location>) -> AxResult<Self> {
        let name = try_owned(name)?;
        let (current, generation) = stable_lookup(parent, &name)?;
        validate_expected_identity(current.as_ref(), expected)?;
        Ok(Self {
            parent: parent.clone(),
            name,
            expected: expected.cloned(),
            generation,
        })
    }

    fn revalidate(&self) -> AxResult<()> {
        if self
            .parent
            .namespace_generation_is_current(self.generation)?
        {
            return Ok(());
        }
        let (current, _) = stable_lookup(&self.parent, &self.name)?;
        validate_expected_identity(current.as_ref(), self.expected.as_ref())
    }
}

trait KernelMutationRequest: Sized {
    type Reservation;
    type Output;

    fn reserve(self) -> AxResult<Self::Reservation>;
    fn revalidate(reservation: &Self::Reservation) -> AxResult<()>;
    fn admit(_reservation: &mut Self::Reservation) -> AxResult<()> {
        Ok(())
    }
    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output>;

    fn rollback(_reservation: &mut Self::Reservation) {}
}

struct KernelMutationBackend<M>(PhantomData<fn() -> M>);

impl<M> KernelMutationBackend<M> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M: KernelMutationRequest> MutationBackend for KernelMutationBackend<M> {
    type Request = M;
    type Reservation = M::Reservation;
    type Output = M::Output;
    type Error = AxError;

    fn reserve(&self, request: Self::Request) -> Result<Self::Reservation, Self::Error> {
        request.reserve()
    }

    fn revalidate(&self, reservation: &Self::Reservation) -> Result<(), Self::Error> {
        M::revalidate(reservation)
    }

    fn admit(&self, reservation: &mut Self::Reservation) -> Result<(), Self::Error> {
        M::admit(reservation)
    }

    fn publish(&self, reservation: &mut Self::Reservation) -> Result<Self::Output, Self::Error> {
        M::publish(reservation)
    }

    fn rollback(&self, reservation: &mut Self::Reservation) {
        M::rollback(reservation);
    }
}

fn commit<M: KernelMutationRequest>(request: M) -> AxResult<M::Output> {
    let backend = KernelMutationBackend::<M>::new();
    MutationTransaction::prepare(&backend, request)?.commit()
}

struct CreateRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    security: &'a VfsSecurityContext,
}

struct PreparedCreate {
    name: PreparedName,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    security: VfsSecurityContext,
    attributes: Option<PreparedCreateAttributes>,
}

struct PreparedCreateAttributes {
    permission: NodePermission,
    owner: (u32, u32),
}

fn check_named_create_capability(
    node_type: NodeType,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
        && !security.has_capability(CAP_MKNOD)
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

fn check_named_create_mechanism(parent: &Location, node_type: NodeType) -> AxResult<()> {
    if parent.supports_named_create(node_type) {
        return Ok(());
    }
    if node_type == NodeType::RegularFile {
        Err(AxError::PermissionDenied)
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

impl KernelMutationRequest for CreateRequest<'_> {
    type Reservation = PreparedCreate;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        Ok(PreparedCreate {
            name: PreparedName::reserve(self.parent, self.name, None)?,
            node_type: self.node_type,
            requested_mode: self.requested_mode,
            umask: self.umask,
            rdev: self.rdev,
            security: self.security.clone(),
            attributes: None,
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()
    }

    fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
        if reservation.attributes.is_some() {
            return Err(AxError::BadState);
        }
        let parent_metadata = reservation.name.parent.metadata()?;
        check_create_permissions_with_frozen_metadata(
            &reservation.name.parent,
            &parent_metadata,
            &reservation.security,
        )?;
        check_named_create_capability(reservation.node_type, &reservation.security)?;
        check_named_create_mechanism(&reservation.name.parent, reservation.node_type)?;
        let (permission, owner) = initial_named_create_owner_mode_with_security(
            &parent_metadata,
            &reservation.security,
            reservation.node_type,
            reservation.requested_mode,
            reservation.umask,
        );
        authorize_named_inode_create(
            &reservation.name.parent,
            &parent_metadata,
            &reservation.name.name,
            reservation.node_type,
            permission,
            reservation.rdev,
            &reservation.security,
        )?;
        reservation.attributes = Some(PreparedCreateAttributes { permission, owner });
        Ok(())
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let PreparedCreateAttributes { permission, owner } =
            reservation.attributes.take().ok_or(AxError::BadState)?;
        reservation
            .name
            .parent
            .create_named(
                &reservation.name.name,
                &NamedCreateOptions {
                    node_type: reservation.node_type,
                    permission,
                    owner: Some(owner),
                    rdev: reservation.rdev,
                    initial_data: None,
                },
                CreateDisposition::Exclusive,
            )
            .map(|outcome| outcome.entry)
    }
}

pub(crate) fn create_named(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
    rdev: Option<DeviceId>,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    commit(CreateRequest {
        parent,
        name,
        node_type,
        requested_mode,
        umask,
        rdev,
        security,
    })
}

struct SymlinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    target: &'a str,
    security: &'a VfsSecurityContext,
}

struct PreparedSymlink {
    name: PreparedName,
    target: String,
    security: VfsSecurityContext,
    attributes: Option<PreparedSymlinkAttributes>,
}

struct PreparedSymlinkAttributes {
    owner: (u32, u32),
}

impl KernelMutationRequest for SymlinkRequest<'_> {
    type Reservation = PreparedSymlink;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        let name = PreparedName::reserve(self.parent, self.name, None)?;
        Ok(PreparedSymlink {
            name,
            target: try_owned(self.target)?,
            security: self.security.clone(),
            attributes: None,
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()
    }

    fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
        if reservation.attributes.is_some() {
            return Err(AxError::BadState);
        }
        let parent_metadata = reservation.name.parent.metadata()?;
        check_create_permissions_with_frozen_metadata(
            &reservation.name.parent,
            &parent_metadata,
            &reservation.security,
        )?;
        if !reservation.name.parent.supports_symlink() {
            return Err(AxError::OperationNotPermitted);
        }
        let owner_gid = if parent_metadata.mode.contains(NodePermission::SET_GID) {
            parent_metadata.gid
        } else {
            reservation.security.credentials().gid().into_raw()
        };
        authorize_symlink_create(
            &reservation.name.parent,
            &parent_metadata,
            &reservation.name.name,
            &reservation.target,
            &reservation.security,
        )?;
        reservation.attributes = Some(PreparedSymlinkAttributes {
            owner: (
                reservation.security.credentials().uid().into_raw(),
                owner_gid,
            ),
        });
        Ok(())
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let PreparedSymlinkAttributes { owner } =
            reservation.attributes.take().ok_or(AxError::BadState)?;
        reservation.name.parent.create_symlink(
            &reservation.name.name,
            &reservation.target,
            NodePermission::from_bits_truncate(0o777),
            Some(owner),
        )
    }
}

pub(crate) fn create_symlink(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    target: &str,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    commit(SymlinkRequest {
        parent,
        name,
        target,
        security,
    })
}

struct LinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    source: LinkRequestSource<'a>,
    security: &'a VfsSecurityContext,
}

enum LinkRequestSource<'a> {
    Named(&'a Location),
    /// A valid fd-backed object whose private anonymous mount can never be
    /// named in the caller's destination mount. Destination exact lookup and
    /// writable-mount admission must still precede the eventual `EXDEV`.
    Unnameable,
}

struct PreparedLink {
    name: PreparedName,
    source: Location,
    security: VfsSecurityContext,
    admitted: bool,
}

// TheKernel currently has no writable fs.protected_hardlinks sysctl. Keep the
// modern safe policy enabled explicitly until that control is extracted.
const PROTECTED_HARDLINKS: bool = true;

fn admit_supported_hardlink(
    supports_hard_links: bool,
    source_node_type: NodeType,
    final_authorize: impl FnOnce() -> AxResult<()>,
) -> AxResult<()> {
    if !supports_hard_links || source_node_type == NodeType::Directory {
        return Err(AxError::OperationNotPermitted);
    }
    final_authorize()
}

impl KernelMutationRequest for LinkRequest<'_> {
    type Reservation = PreparedLink;
    type Output = Location;

    fn reserve(self) -> AxResult<Self::Reservation> {
        let name = PreparedName::reserve(self.parent, self.name, None)?;
        check_writable_mount(&name.parent)?;
        let source = match self.source {
            LinkRequestSource::Named(source) => source,
            LinkRequestSource::Unnameable => return Err(LinuxError::EXDEV.into()),
        };
        if !name.parent.same_mount(source) {
            return Err(LinuxError::EXDEV.into());
        }
        Ok(PreparedLink {
            name,
            source: source.clone(),
            security: self.security.clone(),
            admitted: false,
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()
    }

    fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
        if reservation.admitted {
            return Err(AxError::BadState);
        }

        let source_metadata = reservation.source.metadata()?;
        authorize_hardlink_source(
            &reservation.source,
            &source_metadata,
            &reservation.security,
            PROTECTED_HARDLINKS,
        )?;
        let parent_metadata = reservation.name.parent.metadata()?;
        check_create_permissions_with_frozen_metadata(
            &reservation.name.parent,
            &parent_metadata,
            &reservation.security,
        )?;
        admit_supported_hardlink(
            reservation.name.parent.supports_hard_links(),
            source_metadata.node_type,
            || {
                authorize_hardlink_create(
                    &reservation.source,
                    &source_metadata,
                    &reservation.name.parent,
                    &parent_metadata,
                    &reservation.name.name,
                    &reservation.security,
                )
            },
        )?;
        reservation.admitted = true;
        Ok(())
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        if !core::mem::replace(&mut reservation.admitted, false) {
            return Err(AxError::BadState);
        }
        reservation
            .name
            .parent
            .link(&reservation.name.name, &reservation.source)
    }
}

pub(crate) fn link(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    source: &Location,
    security: &VfsSecurityContext,
) -> AxResult<Location> {
    commit(LinkRequest {
        parent,
        name,
        source: LinkRequestSource::Named(source),
        security,
    })
}

/// Completes destination-side hard-link preflight for a valid but unnameable
/// anonymous source, then reports the unavoidable cross-mount result.
///
/// Linux resolves the exact destination and obtains mount write access before
/// comparing the old and new mounts. Keeping this in the shared transaction
/// adapter ensures `EEXIST`/`EROFS` can precede `EXDEV` without duplicating
/// final-name lookup in syscall glue.
pub(crate) fn reject_unnameable_link_source(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    security: &VfsSecurityContext,
) -> AxResult<()> {
    commit(LinkRequest {
        parent,
        name,
        source: LinkRequestSource::Unnameable,
        security,
    })
    .and(Err(AxError::BadState))
}

pub(crate) struct UnlinkOutcome {
    pub(crate) is_dir: bool,
    pub(crate) loses_last_link: bool,
}

struct UnlinkRequest<'a> {
    parent: &'a Location,
    name: &'a str,
    target: &'a Location,
    remove_dir: bool,
    security: &'a VfsSecurityContext,
}

struct PreparedUnlink {
    name: PreparedName,
    target: Location,
    remove_dir: bool,
    security: VfsSecurityContext,
    admission: Option<PreparedUnlinkAdmission>,
}

struct PreparedUnlinkAdmission {
    is_dir: bool,
    loses_last_link: bool,
}

fn validate_unlink_type(remove_dir: bool, node_type: NodeType) -> AxResult<()> {
    match (remove_dir, node_type == NodeType::Directory) {
        (false, true) => Err(AxError::IsADirectory),
        (true, false) => Err(AxError::NotADirectory),
        _ => Ok(()),
    }
}

fn admit_unlink_mechanism(
    remove_dir: bool,
    node_type: NodeType,
    supported: bool,
    same_mount: bool,
    is_mountpoint: bool,
    final_authorize: impl FnOnce() -> AxResult<()>,
) -> AxResult<()> {
    validate_unlink_type(remove_dir, node_type)?;
    if !supported {
        return Err(AxError::OperationNotPermitted);
    }
    if !same_mount || is_mountpoint {
        return Err(AxError::ResourceBusy);
    }
    final_authorize()
}

impl KernelMutationRequest for UnlinkRequest<'_> {
    type Reservation = PreparedUnlink;
    type Output = UnlinkOutcome;

    fn reserve(self) -> AxResult<Self::Reservation> {
        Ok(PreparedUnlink {
            name: PreparedName::reserve(self.parent, self.name, Some(self.target))?,
            target: self.target.clone(),
            remove_dir: self.remove_dir,
            security: self.security.clone(),
            admission: None,
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.name.revalidate()
    }

    fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
        if reservation.admission.is_some() {
            return Err(AxError::BadState);
        }

        let parent_metadata = reservation.name.parent.metadata()?;
        let target_metadata = reservation.target.metadata()?;
        check_remove_permissions_with_security(
            &reservation.name.parent,
            &parent_metadata,
            &target_metadata,
            &reservation.security,
        )?;
        let supported = if reservation.remove_dir {
            reservation.name.parent.supports_rmdir()
        } else {
            reservation.name.parent.supports_unlink()
        };
        admit_unlink_mechanism(
            reservation.remove_dir,
            target_metadata.node_type,
            supported,
            reservation.name.parent.same_mount(&reservation.target),
            reservation.target.is_mountpoint(),
            || {
                if reservation.remove_dir {
                    authorize_inode_rmdir(
                        &reservation.name.parent,
                        &parent_metadata,
                        &reservation.target,
                        &target_metadata,
                        &reservation.name.name,
                        &reservation.security,
                    )
                } else {
                    authorize_inode_unlink(
                        &reservation.name.parent,
                        &parent_metadata,
                        &reservation.target,
                        &target_metadata,
                        &reservation.name.name,
                        &reservation.security,
                    )
                }
            },
        )?;
        reservation.admission = Some(PreparedUnlinkAdmission {
            is_dir: target_metadata.node_type == NodeType::Directory,
            loses_last_link: reservation.remove_dir || target_metadata.nlink <= 1,
        });
        Ok(())
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let PreparedUnlinkAdmission {
            is_dir,
            loses_last_link,
        } = reservation.admission.take().ok_or(AxError::BadState)?;
        reservation.name.parent.unlink_checked(
            &reservation.name.name,
            reservation.remove_dir,
            &reservation.target,
        )?;
        Ok(UnlinkOutcome {
            is_dir,
            loses_last_link,
        })
    }
}

pub(crate) fn unlink(
    _operation: &NamespaceOperationGuard,
    parent: &Location,
    name: &str,
    target: &Location,
    remove_dir: bool,
    security: &VfsSecurityContext,
) -> AxResult<UnlinkOutcome> {
    commit(UnlinkRequest {
        parent,
        name,
        target,
        remove_dir,
        security,
    })
}

pub(crate) struct RenameOutcome {
    pub(crate) replaced: Option<Location>,
    pub(crate) replaced_loses_last_link: bool,
}

struct RenameRequest<'a> {
    old_parent: &'a Location,
    old_name: &'a str,
    source: &'a Location,
    new_parent: &'a Location,
    new_name: &'a str,
    replaced: Option<&'a Location>,
    no_replace: bool,
    security: &'a VfsSecurityContext,
}

struct PreparedRename {
    source_name: PreparedName,
    destination_name: PreparedName,
    source: Location,
    replaced: Option<Location>,
    no_replace: bool,
    security: VfsSecurityContext,
    admission: Option<PreparedRenameAdmission>,
}

enum PreparedRenameAdmission {
    Noop,
    Mutate { replaced_loses_last_link: bool },
}

fn validate_rename_types(
    source: &axfs_ng_vfs::Metadata,
    replaced: Option<&axfs_ng_vfs::Metadata>,
) -> AxResult<()> {
    if let Some(replaced) = replaced {
        match (
            source.node_type == NodeType::Directory,
            replaced.node_type == NodeType::Directory,
        ) {
            (true, false) => Err(AxError::NotADirectory),
            (false, true) => Err(AxError::IsADirectory),
            _ => Ok(()),
        }
    } else {
        Ok(())
    }
}

impl KernelMutationRequest for RenameRequest<'_> {
    type Reservation = PreparedRename;
    type Output = RenameOutcome;

    fn reserve(self) -> AxResult<Self::Reservation> {
        if self.no_replace && self.replaced.is_some() {
            return Err(AxError::AlreadyExists);
        }
        Ok(PreparedRename {
            source_name: PreparedName::reserve(self.old_parent, self.old_name, Some(self.source))?,
            destination_name: PreparedName::reserve(self.new_parent, self.new_name, self.replaced)?,
            source: self.source.clone(),
            replaced: self.replaced.cloned(),
            no_replace: self.no_replace,
            security: self.security.clone(),
            admission: None,
        })
    }

    fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
        reservation.source_name.revalidate()?;
        reservation.destination_name.revalidate()?;
        if reservation.no_replace && reservation.replaced.is_some() {
            return Err(AxError::AlreadyExists);
        }
        Ok(())
    }

    fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
        if reservation.admission.is_some() {
            return Err(AxError::BadState);
        }

        // vfs_rename() treats two names for the same inode as a complete
        // no-op before may_delete, mechanism admission, or inode hooks.
        if reservation
            .replaced
            .as_ref()
            .is_some_and(|replaced| replaced.same_node(&reservation.source))
        {
            reservation.admission = Some(PreparedRenameAdmission::Noop);
            return Ok(());
        }

        // Freeze every object snapshot once after exact-name revalidation.
        // A shared old/new parent reuses one snapshot rather than resampling
        // mutable metadata between the two Linux admission roles.
        let old_parent_metadata = reservation.source_name.parent.metadata()?;
        let source_metadata = reservation.source.metadata()?;
        let new_parent_metadata = if reservation
            .source_name
            .parent
            .same_node(&reservation.destination_name.parent)
        {
            old_parent_metadata.clone()
        } else {
            reservation.destination_name.parent.metadata()?
        };
        let replaced_metadata = reservation
            .replaced
            .as_ref()
            .map(Location::metadata)
            .transpose()?;

        check_rename_parent_permissions_with_security(
            &reservation.source_name.parent,
            &old_parent_metadata,
            &source_metadata,
            &reservation.destination_name.parent,
            &new_parent_metadata,
            replaced_metadata.as_ref(),
            &reservation.security,
        )?;

        // Linux performs destination may_delete/may_create before the final
        // source/destination type matrix, then verifies that the filesystem
        // actually supplies an ordinary rename operation before invoking the
        // moved-directory permission check or inode_rename hook.
        validate_rename_types(&source_metadata, replaced_metadata.as_ref())?;
        if !reservation.source_name.parent.supports_rename() {
            return Err(AxError::OperationNotPermitted);
        }
        check_cross_parent_rename_source_permissions_with_security(
            &reservation.source_name.parent,
            &reservation.destination_name.parent,
            &reservation.source,
            &source_metadata,
            &reservation.security,
        )?;
        authorize_inode_rename(
            &reservation.source_name.parent,
            &old_parent_metadata,
            &reservation.source,
            &source_metadata,
            &reservation.source_name.name,
            &reservation.destination_name.parent,
            &new_parent_metadata,
            reservation
                .replaced
                .as_ref()
                .zip(replaced_metadata.as_ref()),
            &reservation.destination_name.name,
            &reservation.security,
        )?;

        reservation.admission = Some(PreparedRenameAdmission::Mutate {
            replaced_loses_last_link: replaced_metadata.as_ref().is_some_and(|metadata| {
                metadata.node_type == NodeType::RegularFile && metadata.nlink == 1
            }),
        });
        Ok(())
    }

    fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
        let admission = reservation.admission.take().ok_or(AxError::BadState)?;
        let PreparedRenameAdmission::Mutate {
            replaced_loses_last_link,
        } = admission
        else {
            return Ok(RenameOutcome {
                replaced: None,
                replaced_loses_last_link: false,
            });
        };
        reservation.source_name.parent.rename_checked(
            &reservation.source_name.name,
            &reservation.source,
            &reservation.destination_name.parent,
            &reservation.destination_name.name,
            reservation.replaced.as_ref(),
        )?;
        Ok(RenameOutcome {
            replaced: reservation.replaced.clone(),
            replaced_loses_last_link,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn rename(
    _operation: &NamespaceOperationGuard,
    old_parent: &Location,
    old_name: &str,
    source: &Location,
    new_parent: &Location,
    new_name: &str,
    replaced: Option<&Location>,
    no_replace: bool,
    security: &VfsSecurityContext,
) -> AxResult<RenameOutcome> {
    if no_replace && replaced.is_some() {
        return Err(AxError::AlreadyExists);
    }
    if replaced.is_some_and(|destination| destination.same_node(source)) {
        return Ok(RenameOutcome {
            replaced: None,
            replaced_loses_last_link: false,
        });
    }
    commit(RenameRequest {
        old_parent,
        old_name,
        source,
        new_parent,
        new_name,
        replaced,
        no_replace,
        security,
    })
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use axfs_ng_vfs::{
        CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, Filesystem, FilesystemOps,
        Metadata, MetadataUpdate, Mountpoint, NodeOps, Reference,
        RenameRequest as VfsRenameRequest, StatFs, UnlinkRequest as VfsUnlinkRequest, VfsError,
        VfsResult,
    };
    use linux_raw_sys::general::{CAP_DAC_OVERRIDE, CAP_FOWNER, CAP_MKNOD, CAP_SYS_ADMIN};
    use thekernel_linux_cred::{InodeMknodKind, Kgid, Kuid};

    use super::*;
    use crate::{
        pseudofs::tmp::MemoryFs,
        task::{
            Cred, CredentialSlot, UserNamespace,
            security::{
                NamedCreateSecurityTestExpectation, NamedCreateSecurityTestLeaf,
                NamedCreateSecurityTestProbe, RenameSecurityTestExpectation,
                RenameSecurityTestProbe, malformed_rename_security_test_credential,
                named_create_security_test_credential,
                named_create_security_test_unprivileged_credential,
                rename_security_test_credential, rename_security_test_unprivileged_credential,
            },
        },
    };

    fn memory_root() -> Location {
        let fs = MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
        let mount = Mountpoint::new_root(&fs);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        mount.root_location()
    }

    fn create_file(parent: &Location, name: &str) -> Location {
        parent
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap()
    }

    fn create_dir(parent: &Location, name: &str) -> Location {
        parent
            .create(
                name,
                NodeType::Directory,
                NodePermission::from_bits_truncate(0o777),
            )
            .unwrap()
    }

    struct UnsupportedTestFilesystem {
        root: spin::Once<DirEntry>,
    }

    impl UnsupportedTestFilesystem {
        fn new() -> Arc<Self> {
            let filesystem = Arc::new(Self {
                root: spin::Once::new(),
            });
            let root = DirEntry::new_dir(
                {
                    let filesystem = filesystem.clone();
                    move |_this| {
                        DirNode::new(Arc::new(UnsupportedTestDirectory {
                            filesystem: filesystem.clone(),
                        }))
                    }
                },
                Reference::root(),
            );
            filesystem.root.call_once(|| root);
            filesystem
        }
    }

    impl FilesystemOps for UnsupportedTestFilesystem {
        fn name(&self) -> &str {
            "unsupported-test"
        }

        fn root_dir(&self) -> DirEntry {
            self.root.get().unwrap().clone()
        }

        fn stat(&self) -> VfsResult<StatFs> {
            Ok(StatFs {
                fs_type: 0x756e_7375,
                block_size: 4096,
                blocks: 0,
                blocks_free: 0,
                blocks_available: 0,
                file_count: 1,
                free_file_count: 0,
                name_length: 255,
                fragment_size: 4096,
                mount_flags: 0,
            })
        }
    }

    struct UnsupportedTestDirectory {
        filesystem: Arc<UnsupportedTestFilesystem>,
    }

    impl NodeOps for UnsupportedTestDirectory {
        fn inode(&self) -> u64 {
            1
        }

        fn metadata(&self) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 1,
                nlink: 2,
                mode: NodePermission::from_bits_truncate(0o777),
                node_type: NodeType::Directory,
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 4096,
                blocks: 0,
                rdev: DeviceId::default(),
                atime: Duration::ZERO,
                btime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            })
        }

        fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
            Ok(())
        }

        fn filesystem(&self) -> &dyn FilesystemOps {
            &*self.filesystem
        }

        fn sync(&self, _data_only: bool) -> VfsResult<()> {
            Ok(())
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
    }

    impl DirNodeOps for UnsupportedTestDirectory {
        fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
            Ok(0)
        }

        fn lookup(&self, _name: &str) -> VfsResult<DirEntry> {
            Err(VfsError::NotFound)
        }

        fn create_named(
            &self,
            _name: &str,
            _options: &NamedCreateOptions,
            _disposition: CreateDisposition,
        ) -> VfsResult<CreateOutcome<DirEntry>> {
            Err(VfsError::Unsupported)
        }

        fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
            Err(VfsError::Unsupported)
        }

        fn unlink(&self, _request: VfsUnlinkRequest<'_>) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }

        fn rename(&self, _request: VfsRenameRequest<'_>) -> VfsResult<()> {
            Err(VfsError::Unsupported)
        }
    }

    fn unsupported_root() -> Location {
        let filesystem = Filesystem::new(UnsupportedTestFilesystem::new());
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        mount.root_location()
    }

    fn assert_metadata_preserved(before: &Metadata, location: &Location) {
        let after = location.metadata().unwrap();
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

    fn set_mode_owner(location: &Location, mode: u16, uid: u32, gid: u32) {
        location
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(mode)),
                owner: Some((uid, gid)),
                ..Default::default()
            })
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn rename_probe_security(
        old_parent: &Location,
        source: &Location,
        old_name: &str,
        new_parent: &Location,
        replaced: Option<&Location>,
        new_name: &str,
        deny: bool,
        with_second_module: bool,
        fs_ids: Option<(u32, u32)>,
    ) -> (
        VfsSecurityContext,
        Arc<RenameSecurityTestProbe>,
        Option<Arc<RenameSecurityTestProbe>>,
    ) {
        let first = RenameSecurityTestProbe::new(
            RenameSecurityTestExpectation::new(
                old_parent, source, old_name, new_parent, replaced, new_name,
            )
            .unwrap(),
            deny,
        );
        let second = with_second_module.then(|| {
            RenameSecurityTestProbe::new(
                RenameSecurityTestExpectation::new(
                    old_parent, source, old_name, new_parent, replaced, new_name,
                )
                .unwrap(),
                false,
            )
        });
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = if let Some((uid, gid)) = fs_ids {
            let credential = rename_security_test_unprivileged_credential(
                namespace,
                first.clone(),
                second.as_ref().map(Arc::clone),
                uid,
                gid,
            );
            assert!(!credential.capabilities().has_effective(CAP_DAC_OVERRIDE));
            assert!(!credential.capabilities().has_effective(CAP_FOWNER));
            credential
        } else {
            rename_security_test_credential(
                namespace,
                first.clone(),
                second.as_ref().map(Arc::clone),
            )
        };
        (VfsSecurityContext::new(credential), first, second)
    }

    fn named_create_probe_security(
        parent: &Location,
        name: &str,
        leaf: NamedCreateSecurityTestLeaf,
        deny_permission: bool,
        deny_leaf: bool,
        fs_ids: Option<(u32, u32)>,
    ) -> (VfsSecurityContext, Arc<NamedCreateSecurityTestProbe>) {
        let probe = NamedCreateSecurityTestProbe::new(
            NamedCreateSecurityTestExpectation::new(parent, name, leaf).unwrap(),
            deny_permission,
            deny_leaf,
        );
        let namespace = UserNamespace::try_new_root().unwrap();
        let credential = if let Some((uid, gid)) = fs_ids {
            let credential = named_create_security_test_unprivileged_credential(
                namespace,
                probe.clone(),
                uid,
                gid,
            );
            assert!(!credential.capabilities().has_effective(CAP_DAC_OVERRIDE));
            assert!(!credential.capabilities().has_effective(CAP_MKNOD));
            credential
        } else {
            named_create_security_test_credential(namespace, probe.clone())
        };
        (VfsSecurityContext::new(credential), probe)
    }

    #[derive(Clone, Copy)]
    enum Failure {
        None,
        Revalidate,
        Admit,
        Publish,
    }

    struct ProbeRequest {
        failure: Failure,
        reserved: Arc<AtomicUsize>,
        published: Arc<AtomicUsize>,
        rolled_back: Arc<AtomicUsize>,
    }

    struct ProbeReservation {
        failure: Failure,
        published: Arc<AtomicUsize>,
        rolled_back: Arc<AtomicUsize>,
        rollback_complete: bool,
    }

    impl KernelMutationRequest for ProbeRequest {
        type Reservation = ProbeReservation;
        type Output = ();

        fn reserve(self) -> AxResult<Self::Reservation> {
            self.reserved.fetch_add(1, Ordering::SeqCst);
            Ok(ProbeReservation {
                failure: self.failure,
                published: self.published,
                rolled_back: self.rolled_back,
                rollback_complete: false,
            })
        }

        fn revalidate(reservation: &Self::Reservation) -> AxResult<()> {
            if matches!(reservation.failure, Failure::Revalidate) {
                Err(AxError::ResourceBusy)
            } else {
                Ok(())
            }
        }

        fn admit(reservation: &mut Self::Reservation) -> AxResult<()> {
            if matches!(reservation.failure, Failure::Admit) {
                Err(AxError::PermissionDenied)
            } else {
                Ok(())
            }
        }

        fn publish(reservation: &mut Self::Reservation) -> AxResult<Self::Output> {
            if matches!(reservation.failure, Failure::Publish) {
                return Err(AxError::Io);
            }
            reservation.published.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn rollback(reservation: &mut Self::Reservation) {
            if !reservation.rollback_complete {
                reservation.rolled_back.fetch_add(1, Ordering::SeqCst);
                reservation.rollback_complete = true;
            }
        }
    }

    fn probe(failure: Failure) -> (AxResult<()>, usize, usize, usize) {
        let reserved = Arc::new(AtomicUsize::new(0));
        let published = Arc::new(AtomicUsize::new(0));
        let rolled_back = Arc::new(AtomicUsize::new(0));
        let result = commit(ProbeRequest {
            failure,
            reserved: reserved.clone(),
            published: published.clone(),
            rolled_back: rolled_back.clone(),
        });
        (
            result,
            reserved.load(Ordering::SeqCst),
            published.load(Ordering::SeqCst),
            rolled_back.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn revalidation_failure_rolls_back_once_without_publication() {
        let (result, reserved, published, rolled_back) = probe(Failure::Revalidate);
        assert_eq!(result, Err(AxError::ResourceBusy));
        assert_eq!((reserved, published, rolled_back), (1, 0, 1));
    }

    #[test]
    fn final_admission_failure_rolls_back_once_without_publication() {
        let (result, reserved, published, rolled_back) = probe(Failure::Admit);
        assert_eq!(result, Err(AxError::PermissionDenied));
        assert_eq!((reserved, published, rolled_back), (1, 0, 1));
    }

    #[test]
    fn publication_failure_rolls_back_once() {
        let (result, reserved, published, rolled_back) = probe(Failure::Publish);
        assert_eq!(result, Err(AxError::Io));
        assert_eq!((reserved, published, rolled_back), (1, 0, 1));
    }

    #[test]
    fn successful_publication_does_not_run_rollback() {
        let (result, reserved, published, rolled_back) = probe(Failure::None);
        assert_eq!(result, Ok(()));
        assert_eq!((reserved, published, rolled_back), (1, 1, 0));
    }

    #[test]
    fn named_create_existing_and_covered_destinations_win_before_security_admission() {
        {
            let parent = memory_root();
            let existing = create_file(&parent, "occupied");
            let parent_before = parent.metadata().unwrap();
            let existing_before = existing.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) = named_create_probe_security(
                &parent,
                "occupied",
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                false,
                false,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "occupied",
                    NodeType::RegularFile,
                    NodePermission::from_bits_truncate(0o666),
                    0o022,
                    None,
                    &security,
                ),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert_metadata_preserved(&existing_before, &existing);
            assert!(
                parent
                    .lookup_no_follow_in_mount("occupied")
                    .unwrap()
                    .same_node(&existing)
            );

            let (symlink_security, symlink_probe) = named_create_probe_security(
                &parent,
                "occupied",
                NamedCreateSecurityTestLeaf::Symlink {
                    target: "target".into(),
                },
                false,
                false,
                None,
            );
            assert!(matches!(
                create_symlink(&operation, &parent, "occupied", "target", &symlink_security,),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!(
                (symlink_probe.permission_calls(), symlink_probe.leaf_calls(),),
                (0, 0)
            );
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert_metadata_preserved(&existing_before, &existing);
        }

        {
            let parent = memory_root();
            let covered = create_dir(&parent, "covered");
            let covered_before = covered.metadata().unwrap();
            let child_filesystem = MemoryFs::new().unwrap();
            let child_mount = covered.mount(&child_filesystem).unwrap();
            assert!(covered.is_mountpoint());
            assert!(
                parent
                    .lookup_no_follow_in_mount("covered")
                    .unwrap()
                    .same_node(&covered)
            );
            assert!(
                parent
                    .lookup_no_follow("covered")
                    .unwrap()
                    .same_node(&child_mount.root_location())
            );
            let parent_before = parent.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) = named_create_probe_security(
                &parent,
                "covered",
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                false,
                false,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "covered",
                    NodeType::RegularFile,
                    NodePermission::from_bits_truncate(0o666),
                    0o022,
                    None,
                    &security,
                ),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert_metadata_preserved(&covered_before, &covered);
            assert!(
                parent
                    .lookup_no_follow_in_mount("covered")
                    .unwrap()
                    .same_node(&covered)
            );

            let (symlink_security, symlink_probe) = named_create_probe_security(
                &parent,
                "covered",
                NamedCreateSecurityTestLeaf::Symlink {
                    target: "target".into(),
                },
                false,
                false,
                None,
            );
            assert!(matches!(
                create_symlink(&operation, &parent, "covered", "target", &symlink_security,),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!(
                (symlink_probe.permission_calls(), symlink_probe.leaf_calls(),),
                (0, 0)
            );
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert_metadata_preserved(&covered_before, &covered);
        }
    }

    #[test]
    fn successful_named_creates_dispatch_one_parent_permission_and_one_typed_leaf() {
        let cases = [
            (
                "regular",
                NodeType::RegularFile,
                0o666,
                0o022,
                None,
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
            ),
            (
                "directory",
                NodeType::Directory,
                0o777,
                0o022,
                None,
                NamedCreateSecurityTestLeaf::Directory { mode: 0o755 },
            ),
            (
                "fifo",
                NodeType::Fifo,
                0o660,
                0o027,
                None,
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::Fifo,
                    mode: 0o640,
                    rdev: None,
                },
            ),
            (
                "character",
                NodeType::CharacterDevice,
                0o660,
                0o027,
                Some(DeviceId(0x1234)),
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x1234),
                },
            ),
            (
                "block",
                NodeType::BlockDevice,
                0o660,
                0o027,
                Some(DeviceId(0x5678)),
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::BlockDevice,
                    mode: 0o640,
                    rdev: Some(0x5678),
                },
            ),
            (
                "socket",
                NodeType::Socket,
                0o660,
                0o027,
                None,
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::Socket,
                    mode: 0o640,
                    rdev: None,
                },
            ),
        ];

        for (name, node_type, requested_mode, umask, rdev, leaf) in cases {
            let parent = memory_root();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) =
                named_create_probe_security(&parent, name, leaf, false, false, None);
            let operation = crate::mounts::namespace_operation();

            let created = create_named(
                &operation,
                &parent,
                name,
                node_type,
                NodePermission::from_bits_truncate(requested_mode),
                umask,
                rdev,
                &security,
            )
            .unwrap();

            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (1, 1));
            assert_ne!(parent.namespace_generation().unwrap(), generation);
            assert!(
                parent
                    .lookup_no_follow_in_mount(name)
                    .unwrap()
                    .same_node(&created)
            );
            let metadata = created.metadata().unwrap();
            assert_eq!(metadata.node_type, node_type);
            assert_eq!(metadata.rdev, rdev.unwrap_or_default());
        }

        let parent = memory_root();
        let target = "../target-before-publication";
        let (security, probe) = named_create_probe_security(
            &parent,
            "symlink",
            NamedCreateSecurityTestLeaf::Symlink {
                target: target.into(),
            },
            false,
            false,
            None,
        );
        let operation = crate::mounts::namespace_operation();
        let created = create_symlink(&operation, &parent, "symlink", target, &security).unwrap();
        assert_eq!((probe.permission_calls(), probe.leaf_calls()), (1, 1));
        assert_eq!(created.read_link().unwrap(), target);
    }

    #[test]
    fn named_create_security_denials_leave_name_generation_and_metadata_unchanged() {
        for (deny_permission, deny_leaf, expected_calls) in
            [(true, false, (1, 0)), (false, true, (1, 1))]
        {
            let parent = memory_root();
            set_mode_owner(&parent, 0o2777, 4100, 4200);
            let parent_before = parent.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let name = if deny_permission {
                "permission-denied"
            } else {
                "leaf-denied"
            };
            let (security, probe) = named_create_probe_security(
                &parent,
                name,
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o2640 },
                deny_permission,
                deny_leaf,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    name,
                    NodeType::RegularFile,
                    NodePermission::from_bits_truncate(0o2660),
                    0o027,
                    None,
                    &security,
                ),
                Err(AxError::PermissionDenied)
            ));
            assert_eq!(
                (probe.permission_calls(), probe.leaf_calls()),
                expected_calls
            );
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert!(matches!(
                parent.lookup_no_follow_in_mount(name),
                Err(AxError::NotFound)
            ));
        }
    }

    #[test]
    fn no_cap_mknod_observes_exact_mount_and_dac_precedence_before_leaf_dispatch() {
        let rdev = Some(DeviceId(0x1234));
        let requested_mode = NodePermission::from_bits_truncate(0o660);

        {
            let parent = memory_root();
            create_file(&parent, "existing-device");
            let (security, probe) = named_create_probe_security(
                &parent,
                "existing-device",
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x1234),
                },
                false,
                false,
                Some((1200, 1300)),
            );
            let operation = crate::mounts::namespace_operation();
            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "existing-device",
                    NodeType::CharacterDevice,
                    requested_mode,
                    0o027,
                    rdev,
                    &security,
                ),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
        }

        {
            let filesystem =
                MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
            let mount = Mountpoint::new_root(&filesystem);
            crate::mounts::initialize_test_mount(&mount, 1).unwrap();
            let parent = mount.root_location();
            let (security, probe) = named_create_probe_security(
                &parent,
                "readonly-device",
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x1234),
                },
                false,
                false,
                Some((1200, 1300)),
            );
            let operation = crate::mounts::namespace_operation();
            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "readonly-device",
                    NodeType::CharacterDevice,
                    requested_mode,
                    0o027,
                    rdev,
                    &security,
                ),
                Err(AxError::ReadOnlyFilesystem)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
        }

        {
            let parent = memory_root();
            set_mode_owner(&parent, 0o555, 0, 0);
            let (security, probe) = named_create_probe_security(
                &parent,
                "dac-denied-device",
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x1234),
                },
                false,
                false,
                Some((1200, 1300)),
            );
            let operation = crate::mounts::namespace_operation();
            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "dac-denied-device",
                    NodeType::CharacterDevice,
                    requested_mode,
                    0o027,
                    rdev,
                    &security,
                ),
                Err(AxError::PermissionDenied)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
        }

        {
            let parent = memory_root();
            let (security, probe) = named_create_probe_security(
                &parent,
                "cap-denied-device",
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x1234),
                },
                false,
                false,
                Some((1200, 1300)),
            );
            let operation = crate::mounts::namespace_operation();
            assert!(matches!(
                create_named(
                    &operation,
                    &parent,
                    "cap-denied-device",
                    NodeType::CharacterDevice,
                    requested_mode,
                    0o027,
                    rdev,
                    &security,
                ),
                Err(AxError::OperationNotPermitted)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (1, 0));
        }
    }

    #[test]
    fn unsupported_named_create_backends_map_errno_after_permission_before_leaf() {
        let cases = [
            (
                "regular",
                NodeType::RegularFile,
                0o644,
                None,
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                AxError::PermissionDenied,
            ),
            (
                "directory",
                NodeType::Directory,
                0o755,
                None,
                NamedCreateSecurityTestLeaf::Directory { mode: 0o755 },
                AxError::OperationNotPermitted,
            ),
            (
                "device",
                NodeType::CharacterDevice,
                0o640,
                Some(DeviceId(0x4321)),
                NamedCreateSecurityTestLeaf::Mknod {
                    kind: InodeMknodKind::CharacterDevice,
                    mode: 0o640,
                    rdev: Some(0x4321),
                },
                AxError::OperationNotPermitted,
            ),
        ];

        for (name, node_type, mode, rdev, leaf, expected_error) in cases {
            let parent = unsupported_root();
            let parent_before = parent.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) =
                named_create_probe_security(&parent, name, leaf, false, false, None);
            let operation = crate::mounts::namespace_operation();
            assert_eq!(
                create_named(
                    &operation,
                    &parent,
                    name,
                    node_type,
                    NodePermission::from_bits_truncate(mode),
                    0,
                    rdev,
                    &security,
                )
                .err(),
                Some(expected_error)
            );
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (1, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert!(matches!(
                parent.lookup_no_follow_in_mount(name),
                Err(AxError::NotFound)
            ));
        }

        let parent = unsupported_root();
        let parent_before = parent.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let (security, probe) = named_create_probe_security(
            &parent,
            "symlink",
            NamedCreateSecurityTestLeaf::Symlink {
                target: "target".into(),
            },
            false,
            false,
            None,
        );
        let operation = crate::mounts::namespace_operation();
        assert!(matches!(
            create_symlink(&operation, &parent, "symlink", "target", &security),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!((probe.permission_calls(), probe.leaf_calls()), (1, 0));
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_metadata_preserved(&parent_before, &parent);
        assert!(matches!(
            parent.lookup_no_follow_in_mount("symlink"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn unnameable_hardlink_source_preserves_destination_first_errno_order_without_hooks() {
        {
            let parent = memory_root();
            let existing = create_file(&parent, "existing-link");
            let parent_before = parent.metadata().unwrap();
            let existing_before = existing.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) = named_create_probe_security(
                &parent,
                "existing-link",
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                false,
                false,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                reject_unnameable_link_source(&operation, &parent, "existing-link", &security,),
                Err(AxError::AlreadyExists)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert_metadata_preserved(&existing_before, &existing);
            assert!(
                parent
                    .lookup_no_follow_in_mount("existing-link")
                    .unwrap()
                    .same_node(&existing)
            );
        }

        {
            let filesystem =
                MemoryFs::new_with_permission(NodePermission::from_bits_truncate(0o777)).unwrap();
            let mount = Mountpoint::new_root(&filesystem);
            crate::mounts::initialize_test_mount(&mount, 1).unwrap();
            let parent = mount.root_location();
            let parent_before = parent.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) = named_create_probe_security(
                &parent,
                "readonly-link",
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                false,
                false,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                reject_unnameable_link_source(&operation, &parent, "readonly-link", &security,),
                Err(AxError::ReadOnlyFilesystem)
            ));
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert!(matches!(
                parent.lookup_no_follow_in_mount("readonly-link"),
                Err(AxError::NotFound)
            ));
        }

        {
            let parent = memory_root();
            let parent_before = parent.metadata().unwrap();
            let generation = parent.namespace_generation().unwrap();
            let (security, probe) = named_create_probe_security(
                &parent,
                "cross-device-link",
                NamedCreateSecurityTestLeaf::RegularFile { mode: 0o644 },
                false,
                false,
                None,
            );
            let operation = crate::mounts::namespace_operation();

            let error =
                reject_unnameable_link_source(&operation, &parent, "cross-device-link", &security)
                    .unwrap_err();
            assert_eq!(error.canonicalize(), AxError::CrossesDevices);
            assert_eq!((probe.permission_calls(), probe.leaf_calls()), (0, 0));
            assert_eq!(parent.namespace_generation().unwrap(), generation);
            assert_metadata_preserved(&parent_before, &parent);
            assert!(matches!(
                parent.lookup_no_follow_in_mount("cross-device-link"),
                Err(AxError::NotFound)
            ));
        }
    }

    #[test]
    fn hardlink_backend_and_type_preflight_precedes_final_authorization() {
        for (supports_hard_links, source_node_type) in [
            (false, NodeType::RegularFile),
            (true, NodeType::Directory),
            (false, NodeType::Directory),
        ] {
            let final_authorizations = AtomicUsize::new(0);

            assert_eq!(
                admit_supported_hardlink(supports_hard_links, source_node_type, || {
                    final_authorizations.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                Err(AxError::OperationNotPermitted)
            );
            assert_eq!(final_authorizations.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn supported_non_directory_hardlink_reaches_final_authorization_once() {
        let final_authorizations = AtomicUsize::new(0);

        admit_supported_hardlink(true, NodeType::RegularFile, || {
            final_authorizations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

        assert_eq!(final_authorizations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn remove_type_backend_and_mount_preflight_precede_inode_hook_dispatch() {
        let cases = [
            (
                false,
                NodeType::Directory,
                true,
                true,
                false,
                AxError::IsADirectory,
            ),
            (
                true,
                NodeType::RegularFile,
                true,
                true,
                false,
                AxError::NotADirectory,
            ),
            (
                false,
                NodeType::RegularFile,
                false,
                true,
                false,
                AxError::OperationNotPermitted,
            ),
            (
                true,
                NodeType::Directory,
                true,
                false,
                false,
                AxError::ResourceBusy,
            ),
            (
                true,
                NodeType::Directory,
                true,
                true,
                true,
                AxError::ResourceBusy,
            ),
        ];
        for (remove_dir, node_type, supported, same_mount, is_mountpoint, expected) in cases {
            let final_authorizations = AtomicUsize::new(0);
            assert_eq!(
                admit_unlink_mechanism(
                    remove_dir,
                    node_type,
                    supported,
                    same_mount,
                    is_mountpoint,
                    || {
                        final_authorizations.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                ),
                Err(expected)
            );
            assert_eq!(final_authorizations.load(Ordering::SeqCst), 0);
        }

        for (remove_dir, node_type) in [(false, NodeType::RegularFile), (true, NodeType::Directory)]
        {
            let final_authorizations = AtomicUsize::new(0);
            admit_unlink_mechanism(remove_dir, node_type, true, true, false, || {
                final_authorizations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
            assert_eq!(final_authorizations.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn symlink_admission_freezes_target_mode_and_sgid_owner_before_publication() {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::new(Cred::try_root(namespace).unwrap());
        let fsuid = Kuid::from_raw(1200).unwrap();
        let fsgid = Kgid::from_raw(1300).unwrap();
        let credential = slot.replace_fs_ids_for_test(fsuid, fsgid).unwrap();
        let security = VfsSecurityContext::new(credential);

        for (index, (parent_mode, expected_gid)) in [(0o777, fsgid.into_raw()), (0o2777, 4242)]
            .into_iter()
            .enumerate()
        {
            let filesystem =
                MemoryFs::new_with_permission(NodePermission::from_bits_truncate(parent_mode))
                    .unwrap();
            let mount = Mountpoint::new_root(&filesystem);
            crate::mounts::initialize_test_mount(&mount, 0).unwrap();
            let parent = mount.root_location();
            parent
                .update_metadata(MetadataUpdate {
                    mode: Some(NodePermission::from_bits_truncate(parent_mode)),
                    owner: Some((1234, 4242)),
                    ..Default::default()
                })
                .unwrap();
            let name = if index == 0 { "ordinary" } else { "sgid" };
            let target = if index == 0 {
                "../ordinary-target"
            } else {
                "/absolute/sgid-target"
            };
            let generation = parent.namespace_generation().unwrap();
            let operation = crate::mounts::namespace_operation();

            let link = create_symlink(&operation, &parent, name, target, &security).unwrap();
            let metadata = link.metadata().unwrap();
            assert_eq!(metadata.node_type, NodeType::Symlink);
            assert_eq!(metadata.mode.bits() & 0o7777, 0o777);
            assert_eq!(
                (metadata.uid, metadata.gid),
                (fsuid.into_raw(), expected_gid)
            );
            assert_eq!(link.read_link().unwrap(), target);
            assert_ne!(parent.namespace_generation().unwrap(), generation);
            assert!(parent.lookup_no_follow(name).unwrap().same_node(&link));
        }
    }

    fn root_security() -> VfsSecurityContext {
        let namespace = UserNamespace::try_new_root().unwrap();
        VfsSecurityContext::new(Cred::try_root(namespace).unwrap())
    }

    #[test]
    fn unlink_transaction_preserves_other_names_and_reports_exact_link_outcome() {
        let root = memory_root();
        let victim = create_file(&root, "victim");
        let alias = root.link("alias", &victim).unwrap();
        assert_eq!(victim.metadata().unwrap().nlink, 2);
        let security = root_security();
        let operation = crate::mounts::namespace_operation();

        let first = unlink(&operation, &root, "victim", &victim, false, &security).unwrap();
        assert!(!first.is_dir);
        assert!(!first.loses_last_link);
        assert!(matches!(
            root.lookup_no_follow("victim"),
            Err(AxError::NotFound)
        ));
        assert!(root.lookup_no_follow("alias").unwrap().same_node(&alias));
        assert_eq!(alias.metadata().unwrap().nlink, 1);

        let last = unlink(&operation, &root, "alias", &alias, false, &security).unwrap();
        assert!(!last.is_dir);
        assert!(last.loses_last_link);
        assert!(matches!(
            root.lookup_no_follow("alias"),
            Err(AxError::NotFound)
        ));
        assert_eq!(alias.metadata().unwrap().nlink, 0);
    }

    #[test]
    fn rmdir_hook_admission_leaves_backend_emptiness_after_the_hook_boundary() {
        let root = memory_root();
        let directory = create_dir(&root, "directory");
        create_file(&directory, "child");
        let security = root_security();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            unlink(&operation, &root, "directory", &directory, true, &security,),
            Err(AxError::DirectoryNotEmpty)
        ));
        assert!(
            root.lookup_no_follow("directory")
                .unwrap()
                .same_node(&directory)
        );
        assert!(directory.lookup_no_follow("child").is_ok());
    }

    #[test]
    fn wrong_remove_type_fails_without_namespace_or_link_count_change() {
        let root = memory_root();
        let file = create_file(&root, "file");
        let directory = create_dir(&root, "directory");
        let file_nlink = file.metadata().unwrap().nlink;
        let directory_nlink = directory.metadata().unwrap().nlink;
        let generation = root.namespace_generation().unwrap();
        let security = root_security();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            unlink(&operation, &root, "file", &file, true, &security),
            Err(AxError::NotADirectory)
        ));
        assert!(matches!(
            unlink(&operation, &root, "directory", &directory, false, &security,),
            Err(AxError::IsADirectory)
        ));
        assert_eq!(root.namespace_generation().unwrap(), generation);
        assert_eq!(file.metadata().unwrap().nlink, file_nlink);
        assert_eq!(directory.metadata().unwrap().nlink, directory_nlink);
        assert!(root.lookup_no_follow("file").unwrap().same_node(&file));
        assert!(
            root.lookup_no_follow("directory")
                .unwrap()
                .same_node(&directory)
        );
    }

    #[test]
    fn unrelated_generation_drift_revalidates_the_exact_expected_identity() {
        let root = memory_root();
        let victim = create_file(&root, "victim");
        let prepared = PreparedName::reserve(&root, "victim", Some(&victim)).unwrap();

        create_file(&root, "unrelated");

        let result = prepared.revalidate();
        assert!(result.is_ok(), "revalidation failed: {result:?}");
        assert!(root.lookup_no_follow("victim").unwrap().same_node(&victim));
    }

    #[test]
    fn replacement_snapshot_is_rejected_without_touching_the_new_object() {
        let root = memory_root();
        let old = create_file(&root, "slot");
        let prepared = PreparedName::reserve(&root, "slot", Some(&old)).unwrap();
        let replacement = create_file(&root, "replacement");

        assert!(matches!(
            validate_expected_identity(Some(&replacement), prepared.expected.as_ref()),
            Err(AxError::NotFound)
        ));
        assert!(
            root.lookup_no_follow("replacement")
                .unwrap()
                .same_node(&replacement)
        );
        assert!(root.lookup_no_follow("slot").unwrap().same_node(&old));
    }

    #[test]
    fn rename_revalidates_both_parent_generations_and_exact_identities() {
        let root = memory_root();
        let old_parent = create_dir(&root, "old");
        let new_parent = create_dir(&root, "new");
        let source = create_file(&old_parent, "source");
        let security = root_security();
        let destination = create_file(&new_parent, "destination");
        let mut prepared = RenameRequest {
            old_parent: &old_parent,
            old_name: "source",
            source: &source,
            new_parent: &new_parent,
            new_name: "destination",
            replaced: Some(&destination),
            no_replace: false,
            security: &security,
        }
        .reserve()
        .unwrap();

        create_file(&old_parent, "old-noise");
        create_file(&new_parent, "new-noise");
        RenameRequest::revalidate(&prepared).unwrap();
        assert!(
            old_parent
                .lookup_no_follow("source")
                .unwrap()
                .same_node(&source)
        );
        assert!(
            new_parent
                .lookup_no_follow("destination")
                .unwrap()
                .same_node(&destination)
        );

        let replacement = create_file(&new_parent, "replacement");
        prepared.destination_name.expected = Some(replacement.clone());
        create_file(&new_parent, "force-destination-revalidation");
        assert!(matches!(
            RenameRequest::revalidate(&prepared),
            Err(AxError::NotFound)
        ));
        assert!(
            new_parent
                .lookup_no_follow("replacement")
                .unwrap()
                .same_node(&replacement)
        );
    }

    #[test]
    fn rename_denial_is_once_short_circuits_and_preserves_absent_destination_transaction() {
        let root = memory_root();
        let old_parent = create_dir(&root, "old-parent");
        let new_parent = create_dir(&root, "new-parent");
        let source = create_file(&old_parent, "old-name");
        source
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o640)),
                owner: Some((1201, 1202)),
                atime: Some(Duration::from_secs(11)),
                mtime: Some(Duration::from_secs(12)),
                ctime: Some(Duration::from_secs(13)),
                ..Default::default()
            })
            .unwrap();
        let (security, first, second) = rename_probe_security(
            &old_parent,
            &source,
            "old-name",
            &new_parent,
            None,
            "new-name",
            true,
            true,
            None,
        );
        let second = second.unwrap();
        let old_parent_before = old_parent.metadata().unwrap();
        let new_parent_before = new_parent.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let old_generation = old_parent.namespace_generation().unwrap();
        let new_generation = new_parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            rename(
                &operation,
                &old_parent,
                "old-name",
                &source,
                &new_parent,
                "new-name",
                None,
                false,
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 0);
        assert_eq!(old_parent.namespace_generation().unwrap(), old_generation);
        assert_eq!(new_parent.namespace_generation().unwrap(), new_generation);
        assert_metadata_preserved(&old_parent_before, &old_parent);
        assert_metadata_preserved(&new_parent_before, &new_parent);
        assert_metadata_preserved(&source_before, &source);
        assert!(
            old_parent
                .lookup_no_follow_in_mount("old-name")
                .unwrap()
                .same_node(&source)
        );
        assert!(matches!(
            new_parent.lookup_no_follow_in_mount("new-name"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn rename_existing_destination_context_binds_victim_and_denial_preserves_every_snapshot() {
        let root = memory_root();
        let old_parent = create_dir(&root, "existing-old-parent");
        let new_parent = create_dir(&root, "existing-new-parent");
        let source = create_file(&old_parent, "source-entry");
        let victim = create_file(&new_parent, "victim-entry");
        victim
            .update_metadata(MetadataUpdate {
                mode: Some(NodePermission::from_bits_truncate(0o654)),
                owner: Some((2301, 2302)),
                atime: Some(Duration::from_secs(21)),
                mtime: Some(Duration::from_secs(22)),
                ctime: Some(Duration::from_secs(23)),
                ..Default::default()
            })
            .unwrap();
        let (security, probe, _) = rename_probe_security(
            &old_parent,
            &source,
            "source-entry",
            &new_parent,
            Some(&victim),
            "victim-entry",
            true,
            false,
            None,
        );
        let old_parent_before = old_parent.metadata().unwrap();
        let new_parent_before = new_parent.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let victim_before = victim.metadata().unwrap();
        let old_generation = old_parent.namespace_generation().unwrap();
        let new_generation = new_parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            rename(
                &operation,
                &old_parent,
                "source-entry",
                &source,
                &new_parent,
                "victim-entry",
                Some(&victim),
                false,
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(probe.calls(), 1);
        assert_eq!(old_parent.namespace_generation().unwrap(), old_generation);
        assert_eq!(new_parent.namespace_generation().unwrap(), new_generation);
        assert_metadata_preserved(&old_parent_before, &old_parent);
        assert_metadata_preserved(&new_parent_before, &new_parent);
        assert_metadata_preserved(&source_before, &source);
        assert_metadata_preserved(&victim_before, &victim);
        assert!(
            old_parent
                .lookup_no_follow_in_mount("source-entry")
                .unwrap()
                .same_node(&source)
        );
        assert!(
            new_parent
                .lookup_no_follow_in_mount("victim-entry")
                .unwrap()
                .same_node(&victim)
        );
    }

    #[test]
    fn rename_unsupported_backend_fails_before_inode_hook() {
        let filesystem = crate::pseudofs::cgroup::new_cgroup_v2().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        crate::mounts::initialize_test_mount(&mount, 0).unwrap();
        let parent = mount.root_location();
        let source = create_dir(&parent, "unsupported-source");
        assert!(!parent.supports_rename());
        let (security, probe, _) = rename_probe_security(
            &parent,
            &source,
            "unsupported-source",
            &parent,
            None,
            "unsupported-target",
            false,
            false,
            None,
        );
        let parent_before = parent.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let generation = parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            rename(
                &operation,
                &parent,
                "unsupported-source",
                &source,
                &parent,
                "unsupported-target",
                None,
                false,
                &security,
            ),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!(probe.calls(), 0);
        assert_eq!(parent.namespace_generation().unwrap(), generation);
        assert_metadata_preserved(&parent_before, &parent);
        assert_metadata_preserved(&source_before, &source);
        assert!(
            parent
                .lookup_no_follow_in_mount("unsupported-source")
                .unwrap()
                .same_node(&source)
        );
        assert!(matches!(
            parent.lookup_no_follow_in_mount("unsupported-target"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn rename_type_mismatch_runs_destination_dac_and_sticky_admission_before_type_error() {
        for (case, new_parent_mode, new_parent_owner, victim_owner, expected) in [
            (
                "dac",
                0o555,
                (2401, 2401),
                (2401, 2401),
                AxError::PermissionDenied,
            ),
            (
                "sticky",
                0o1777,
                (3401, 3401),
                (3402, 3402),
                AxError::OperationNotPermitted,
            ),
            (
                "type",
                0o777,
                (2401, 2401),
                (2401, 2401),
                AxError::IsADirectory,
            ),
        ] {
            let root = memory_root();
            let old_parent = create_dir(&root, &alloc::format!("{case}-old-parent"));
            let new_parent = create_dir(&root, &alloc::format!("{case}-new-parent"));
            let source = create_file(&old_parent, "type-source");
            let victim = create_dir(&new_parent, "type-victim");
            set_mode_owner(&old_parent, 0o777, 2401, 2401);
            set_mode_owner(
                &new_parent,
                new_parent_mode,
                new_parent_owner.0,
                new_parent_owner.1,
            );
            set_mode_owner(&source, 0o600, 2401, 2401);
            set_mode_owner(&victim, 0o700, victim_owner.0, victim_owner.1);
            let (security, probe, _) = rename_probe_security(
                &old_parent,
                &source,
                "type-source",
                &new_parent,
                Some(&victim),
                "type-victim",
                false,
                false,
                Some((2401, 2401)),
            );
            let old_generation = old_parent.namespace_generation().unwrap();
            let new_generation = new_parent.namespace_generation().unwrap();
            let source_before = source.metadata().unwrap();
            let victim_before = victim.metadata().unwrap();
            let operation = crate::mounts::namespace_operation();

            assert!(matches!(
                rename(
                    &operation,
                    &old_parent,
                    "type-source",
                    &source,
                    &new_parent,
                    "type-victim",
                    Some(&victim),
                    false,
                    &security,
                ),
                Err(error) if error == expected
            ));
            assert_eq!(probe.calls(), 0);
            assert_eq!(old_parent.namespace_generation().unwrap(), old_generation);
            assert_eq!(new_parent.namespace_generation().unwrap(), new_generation);
            assert_metadata_preserved(&source_before, &source);
            assert_metadata_preserved(&victim_before, &victim);
        }
    }

    #[test]
    fn cross_parent_directory_source_write_denial_precedes_inode_rename_hook() {
        let root = memory_root();
        let old_parent = create_dir(&root, "move-old-parent");
        let new_parent = create_dir(&root, "move-new-parent");
        let source = create_dir(&old_parent, "moved-directory");
        set_mode_owner(&old_parent, 0o777, 2501, 2501);
        set_mode_owner(&new_parent, 0o777, 2501, 2501);
        set_mode_owner(&source, 0o555, 2501, 2501);
        let (security, probe, _) = rename_probe_security(
            &old_parent,
            &source,
            "moved-directory",
            &new_parent,
            None,
            "moved-target",
            false,
            false,
            Some((2501, 2501)),
        );
        let old_generation = old_parent.namespace_generation().unwrap();
        let new_generation = new_parent.namespace_generation().unwrap();
        let source_before = source.metadata().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            rename(
                &operation,
                &old_parent,
                "moved-directory",
                &source,
                &new_parent,
                "moved-target",
                None,
                false,
                &security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(probe.calls(), 0);
        assert_eq!(old_parent.namespace_generation().unwrap(), old_generation);
        assert_eq!(new_parent.namespace_generation().unwrap(), new_generation);
        assert_metadata_preserved(&source_before, &source);
        assert!(
            old_parent
                .lookup_no_follow_in_mount("moved-directory")
                .unwrap()
                .same_node(&source)
        );
        assert!(matches!(
            new_parent.lookup_no_follow_in_mount("moved-target"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn rename_mountpoint_allow_reaches_hook_then_busy_and_denial_overrides_busy_on_covered_inode() {
        let root = memory_root();
        let source = create_dir(&root, "mount-source");
        let covered = create_dir(&root, "mount-target");
        set_mode_owner(&covered, 0o751, 2601, 2602);
        let child_filesystem = MemoryFs::new().unwrap();
        let child_mount = covered.mount(&child_filesystem).unwrap();
        assert!(covered.is_mountpoint());
        assert!(
            root.lookup_no_follow("mount-target")
                .unwrap()
                .is_root_of_mount()
        );
        assert!(
            root.lookup_no_follow_in_mount("mount-target")
                .unwrap()
                .same_node(&covered)
        );
        let root_before = root.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let covered_before = covered.metadata().unwrap();
        let operation = crate::mounts::namespace_operation();

        let (allow_security, allow_probe, _) = rename_probe_security(
            &root,
            &source,
            "mount-source",
            &root,
            Some(&covered),
            "mount-target",
            false,
            false,
            None,
        );
        assert!(matches!(
            rename(
                &operation,
                &root,
                "mount-source",
                &source,
                &root,
                "mount-target",
                Some(&covered),
                false,
                &allow_security,
            ),
            Err(AxError::ResourceBusy)
        ));
        assert_eq!(allow_probe.calls(), 1);
        let generation_after_backend_rejection = root.namespace_generation().unwrap();

        let (deny_security, deny_probe, later_probe) = rename_probe_security(
            &root,
            &source,
            "mount-source",
            &root,
            Some(&covered),
            "mount-target",
            true,
            true,
            None,
        );
        assert!(matches!(
            rename(
                &operation,
                &root,
                "mount-source",
                &source,
                &root,
                "mount-target",
                Some(&covered),
                false,
                &deny_security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(deny_probe.calls(), 1);
        assert_eq!(later_probe.unwrap().calls(), 0);
        assert_eq!(
            root.namespace_generation().unwrap(),
            generation_after_backend_rejection
        );
        assert_metadata_preserved(&root_before, &root);
        assert_metadata_preserved(&source_before, &source);
        assert_metadata_preserved(&covered_before, &covered);
        assert!(
            root.lookup_no_follow_in_mount("mount-source")
                .unwrap()
                .same_node(&source)
        );
        assert!(
            root.lookup_no_follow_in_mount("mount-target")
                .unwrap()
                .same_node(&covered)
        );
        assert!(Arc::ptr_eq(
            root.lookup_no_follow("mount-target").unwrap().mountpoint(),
            &child_mount
        ));
    }

    #[test]
    fn rename_nonempty_destination_allow_reaches_hook_then_notempty_and_denial_overrides_it() {
        let root = memory_root();
        let source = create_dir(&root, "nonempty-source");
        let victim = create_dir(&root, "nonempty-target");
        let child = create_file(&victim, "child");
        let root_before = root.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let victim_before = victim.metadata().unwrap();
        let child_before = child.metadata().unwrap();
        let operation = crate::mounts::namespace_operation();

        let (allow_security, allow_probe, _) = rename_probe_security(
            &root,
            &source,
            "nonempty-source",
            &root,
            Some(&victim),
            "nonempty-target",
            false,
            false,
            None,
        );
        assert!(matches!(
            rename(
                &operation,
                &root,
                "nonempty-source",
                &source,
                &root,
                "nonempty-target",
                Some(&victim),
                false,
                &allow_security,
            ),
            Err(AxError::DirectoryNotEmpty)
        ));
        assert_eq!(allow_probe.calls(), 1);
        let generation_after_backend_rejection = root.namespace_generation().unwrap();

        let (deny_security, deny_probe, _) = rename_probe_security(
            &root,
            &source,
            "nonempty-source",
            &root,
            Some(&victim),
            "nonempty-target",
            true,
            false,
            None,
        );
        assert!(matches!(
            rename(
                &operation,
                &root,
                "nonempty-source",
                &source,
                &root,
                "nonempty-target",
                Some(&victim),
                false,
                &deny_security,
            ),
            Err(AxError::PermissionDenied)
        ));
        assert_eq!(deny_probe.calls(), 1);
        assert_eq!(
            root.namespace_generation().unwrap(),
            generation_after_backend_rejection
        );
        assert_metadata_preserved(&root_before, &root);
        assert_metadata_preserved(&source_before, &source);
        assert_metadata_preserved(&victim_before, &victim);
        assert_metadata_preserved(&child_before, &child);
        assert!(
            victim
                .lookup_no_follow_in_mount("child")
                .unwrap()
                .same_node(&child)
        );
    }

    #[test]
    fn rename_same_inode_is_a_zero_effect_noop_before_hooks_or_backend_publication() {
        let root = memory_root();
        let source = create_file(&root, "same-source");
        let alias = root.link("same-alias", &source).unwrap();
        assert_eq!(source.metadata().unwrap().nlink, 2);
        let (security, probe, _) = rename_probe_security(
            &root,
            &source,
            "same-source",
            &root,
            Some(&alias),
            "same-alias",
            false,
            false,
            None,
        );
        let root_before = root.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let alias_before = alias.metadata().unwrap();
        let generation = root.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        let outcome = rename(
            &operation,
            &root,
            "same-source",
            &source,
            &root,
            "same-alias",
            Some(&alias),
            false,
            &security,
        )
        .unwrap();
        assert!(outcome.replaced.is_none());
        assert!(!outcome.replaced_loses_last_link);
        assert_eq!(probe.calls(), 0);
        assert_eq!(root.namespace_generation().unwrap(), generation);
        assert_metadata_preserved(&root_before, &root);
        assert_metadata_preserved(&source_before, &source);
        assert_metadata_preserved(&alias_before, &alias);
        assert!(
            root.lookup_no_follow_in_mount("same-source")
                .unwrap()
                .same_node(&source)
        );
        assert!(
            root.lookup_no_follow_in_mount("same-alias")
                .unwrap()
                .same_node(&alias)
        );
        // Fsnotify/fanotify publication belongs to the ctl layer. At this
        // transaction boundary, unchanged generation plus complete metadata,
        // nlink, and timestamp equality is the observable zero-effect proof.
    }

    #[test]
    fn malformed_rename_module_state_fails_before_callback_and_preserves_transaction() {
        let root = memory_root();
        let old_parent = create_dir(&root, "malformed-old-parent");
        let new_parent = create_dir(&root, "malformed-new-parent");
        let source = create_file(&old_parent, "malformed-source");
        let expectation = RenameSecurityTestExpectation::new(
            &old_parent,
            &source,
            "malformed-source",
            &new_parent,
            None,
            "malformed-target",
        )
        .unwrap();
        let probe = RenameSecurityTestProbe::new(expectation, false);
        let namespace = UserNamespace::try_new_root().unwrap();
        let malformed = malformed_rename_security_test_credential(namespace, probe.clone());
        let security = VfsSecurityContext::new(malformed);
        let old_parent_before = old_parent.metadata().unwrap();
        let new_parent_before = new_parent.metadata().unwrap();
        let source_before = source.metadata().unwrap();
        let old_generation = old_parent.namespace_generation().unwrap();
        let new_generation = new_parent.namespace_generation().unwrap();
        let operation = crate::mounts::namespace_operation();

        assert!(matches!(
            rename(
                &operation,
                &old_parent,
                "malformed-source",
                &source,
                &new_parent,
                "malformed-target",
                None,
                false,
                &security,
            ),
            Err(AxError::OperationNotPermitted)
        ));
        assert_eq!(probe.calls(), 0);
        assert_eq!(old_parent.namespace_generation().unwrap(), old_generation);
        assert_eq!(new_parent.namespace_generation().unwrap(), new_generation);
        assert_metadata_preserved(&old_parent_before, &old_parent);
        assert_metadata_preserved(&new_parent_before, &new_parent);
        assert_metadata_preserved(&source_before, &source);
        assert!(
            old_parent
                .lookup_no_follow_in_mount("malformed-source")
                .unwrap()
                .same_node(&source)
        );
        assert!(matches!(
            new_parent.lookup_no_follow_in_mount("malformed-target"),
            Err(AxError::NotFound)
        ));
    }

    #[test]
    fn malformed_module_state_denies_pinned_capability_consumers() {
        let root = memory_root();
        let source = create_file(&root, "capable-source");
        let expectation = RenameSecurityTestExpectation::new(
            &root,
            &source,
            "capable-source",
            &root,
            None,
            "capable-target",
        )
        .unwrap();
        let probe = RenameSecurityTestProbe::new(expectation, false);
        let namespace = UserNamespace::try_new_root().unwrap();
        let malformed = malformed_rename_security_test_credential(namespace, probe.clone());
        let security = VfsSecurityContext::new(malformed);

        for capability in [CAP_MKNOD, CAP_SYS_ADMIN, CAP_FOWNER] {
            assert!(security.credentials().has_capability(capability));
            assert!(!security.has_capability(capability));
        }
        assert_eq!(
            check_named_create_capability(NodeType::CharacterDevice, &security),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(probe.calls(), 0);
    }

    #[test]
    // Spelling each signature out in full is the assertion: this test freezes
    // the public mutation surface, so collapsing the types into aliases would
    // remove exactly what is being checked.
    #[allow(clippy::type_complexity)]
    fn public_namespace_mutations_require_the_shared_operation_capability() {
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            NodeType,
            NodePermission,
            u32,
            Option<DeviceId>,
            &VfsSecurityContext,
        ) -> AxResult<Location> = create_named;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &str,
            &VfsSecurityContext,
        ) -> AxResult<Location> = create_symlink;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            &VfsSecurityContext,
        ) -> AxResult<Location> = link;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            bool,
            &VfsSecurityContext,
        ) -> AxResult<UnlinkOutcome> = unlink;
        let _: fn(
            &NamespaceOperationGuard,
            &Location,
            &str,
            &Location,
            &Location,
            &str,
            Option<&Location>,
            bool,
            &VfsSecurityContext,
        ) -> AxResult<RenameOutcome> = rename;
    }
}
