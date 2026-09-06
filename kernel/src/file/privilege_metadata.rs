//! Inode privilege-metadata provider routing for setattr operations.
//!
//! Syscall glue must not select a concrete filesystem backend. This adapter is
//! the current integration point between typed Linux setattr cleanup intent and
//! the providers that can discover/remove executable privilege metadata. Only
//! tmpfs exposes `security.capability` today; additional providers and
//! module-aggregated need-killpriv policy can join here without leaking into
//! syscall entry code or the generic VFS contract.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, Metadata, MetadataUpdate, NodePermission, NodeType};
use linux_raw_sys::general::CAP_FSETID;
use thekernel_linux_cred::{
    ContentWriteMode, ContentWriteSetIdAuthority,
    InodeSetattrPrivilegeCleanup as CredentialPrivilegeCleanup, plan_content_write_setid_cleanup,
};

use super::{
    executable::{self, ContentPrivilegeMetadataMutationGuard},
    xattr_provider::{read_security_capability, remove_security_capability_if_present},
};
use crate::task::{Cred, UserNamespace, ns_capable_for_setid};

/// Exact actor and filesystem-owner namespace frozen for one content mutation.
///
/// Filesystems are not implicitly owned by the actor's current user namespace.
/// Keeping both values explicit prevents a child-namespace `CAP_FSETID` from
/// preserving set-ID bits on an inode owned by an ancestor namespace.
pub(crate) struct ContentWriteCredentialView<'a> {
    actor: &'a Cred,
    filesystem_owner_user_ns: &'a Arc<UserNamespace>,
}

impl<'a> ContentWriteCredentialView<'a> {
    pub(crate) const fn new(
        actor: &'a Cred,
        filesystem_owner_user_ns: &'a Arc<UserNamespace>,
    ) -> Self {
        Self {
            actor,
            filesystem_owner_user_ns,
        }
    }

    fn setid_authority(&self) -> ContentWriteSetIdAuthority {
        if ns_capable_for_setid(self.actor, self.filesystem_owner_user_ns, CAP_FSETID) {
            ContentWriteSetIdAuthority::CAP_FSETID
        } else {
            ContentWriteSetIdAuthority::UNPRIVILEGED
        }
    }
}

/// Opaque proof that privilege metadata remains excluded through data commit.
#[must_use = "the privilege cleanup guard must remain alive through data commit"]
pub(crate) struct ContentWritePrivilegeGuard {
    _metadata: ContentPrivilegeMetadataMutationGuard,
}

/// Cleans executable privilege metadata before a content mutation and returns
/// the exclusion which must remain owned until that mutation commits.
///
/// Mode and xattr failures are returned before the caller receives a guard, so
/// a caller using `?` cannot reach its backend mutation. Cleanup is deliberately
/// conservative: if mode publication succeeds and later xattr removal fails,
/// the cleared mode is not rolled back.
pub(crate) fn begin_content_write_privilege_cleanup(
    location: &Location,
    credentials: ContentWriteCredentialView<'_>,
) -> AxResult<ContentWritePrivilegeGuard> {
    begin_content_write_privilege_cleanup_with_authority(location, credentials.setid_authority())
}

/// Begins cleanup for a content write whose exact actor is unavailable at this
/// compatibility boundary.
///
/// Callers with an operation-scoped [`ContentWriteCredentialView`] must use
/// [`begin_content_write_privilege_cleanup`]. This conservative entry point is
/// limited to legacy generic `FileLike` dispatch and must never infer authority
/// from the current thread: treating the writer as unprivileged is safe across
/// inherited handles and future non-task callers.
pub(crate) fn begin_conservative_content_write_privilege_cleanup(
    location: &Location,
) -> AxResult<ContentWritePrivilegeGuard> {
    begin_content_write_privilege_cleanup_with_authority(
        location,
        ContentWriteSetIdAuthority::UNPRIVILEGED,
    )
}

/// Begins conservative privilege cleanup for a shared-writable mapping.
///
/// The mapping can outlive its creator and be written by inherited processes
/// with different credentials. Until the MM has a page-write hook carrying the
/// exact writer, activation must never use the creator's `CAP_FSETID` result.
pub(crate) fn begin_shared_writable_mapping_privilege_cleanup(
    location: &Location,
) -> AxResult<ContentWritePrivilegeGuard> {
    begin_conservative_content_write_privilege_cleanup(location)
}

fn begin_content_write_privilege_cleanup_with_authority(
    location: &Location,
    authority: ContentWriteSetIdAuthority,
) -> AxResult<ContentWritePrivilegeGuard> {
    let metadata_guard = executable::begin_content_privilege_metadata_mutation(location)?;
    let metadata = location.metadata()?;
    if metadata.node_type == NodeType::RegularFile {
        let current_mode =
            ContentWriteMode::try_from_bits(metadata.mode.bits()).ok_or(AxError::BadState)?;
        let plan = plan_content_write_setid_cleanup(current_mode, authority);
        if plan.changes_mode() {
            let mode =
                NodePermission::from_bits(plan.next_mode().bits()).ok_or(AxError::BadState)?;
            location.update_metadata(MetadataUpdate {
                mode: Some(mode),
                ..Default::default()
            })?;
        }
        remove_security_capability_if_present(location)?;
    }
    Ok(ContentWritePrivilegeGuard {
        _metadata: metadata_guard,
    })
}

/// Move-only privilege-cleanup decision bound to one exact VFS location.
///
/// The public syscall layer may obtain and forward this token, but only the
/// file-policy layer can inspect or apply it. In particular, a copied cleanup
/// enum cannot be paired with another inode between the pre-hook proposal and
/// backend publication.
#[must_use = "a probed inode privilege cleanup must be consumed by setattr policy"]
pub(crate) struct InodePrivilegeCleanup<'location> {
    location: &'location Location,
    intent: CredentialPrivilegeCleanup,
}

impl InodePrivilegeCleanup<'_> {
    pub(super) const fn intent(&self) -> CredentialPrivilegeCleanup {
        self.intent
    }

    /// Requires the policy and provider decision to name the same exact mount
    /// and dentry. Device/inode numbers alone are insufficient across mounts,
    /// filesystem instances, and recycled inode generations.
    pub(super) fn validate_location(&self, location: &Location) -> AxResult<()> {
        if self.location.ptr_eq(location) {
            Ok(())
        } else {
            Err(AxError::BadState)
        }
    }

    /// Applies this token to the location captured by the probe. No caller can
    /// substitute a second location at the cleanup boundary.
    pub(super) fn apply(self) -> AxResult<()> {
        match self.intent {
            CredentialPrivilegeCleanup::Preserve => Ok(()),
            CredentialPrivilegeCleanup::Kill => {
                remove_security_capability_if_present(self.location)
            }
            _ => Err(AxError::BadState),
        }
    }
}

/// Probes the active inode provider for privilege metadata which must be
/// removed before a successful non-directory chown publication.
pub(crate) fn probe_inode_setattr_privilege_cleanup<'location>(
    location: &'location Location,
    metadata: &Metadata,
) -> AxResult<InodePrivilegeCleanup<'location>> {
    let intent = if metadata.node_type != NodeType::Directory
        && read_security_capability(location)?.is_some()
    {
        CredentialPrivilegeCleanup::Kill
    } else {
        CredentialPrivilegeCleanup::Preserve
    };
    Ok(InodePrivilegeCleanup { location, intent })
}

#[cfg(test)]
mod tests {
    use axerrno::LinuxError;
    use axfs_ng_vfs::{Mountpoint, NodePermission, XattrSetMode};

    use super::*;
    use crate::{
        pseudofs::tmp,
        task::{Kgid, Kuid},
    };

    #[test]
    fn cleanup_token_rejects_an_alias_from_a_distinct_mount() {
        let fs = tmp::MemoryFs::new().unwrap();
        let first_mount = Mountpoint::new_root(&fs);
        let second_mount = Mountpoint::new_root(&fs);
        let first = first_mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"setattr-token-mount"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let second = second_mount
            .root_location()
            .lookup_no_follow(axfs_ng_vfs::FsName::new(b"setattr-token-mount"))
            .unwrap();
        let metadata = first.metadata().unwrap();
        let second_metadata = second.metadata().unwrap();

        assert_eq!(
            (metadata.device, metadata.inode),
            (second_metadata.device, second_metadata.inode)
        );
        assert!(!first.ptr_eq(&second));
        let cleanup = probe_inode_setattr_privilege_cleanup(&first, &metadata).unwrap();
        assert_eq!(cleanup.validate_location(&first), Ok(()));
        assert_eq!(cleanup.validate_location(&second), Err(AxError::BadState));
    }

    #[test]
    fn cleanup_token_applies_only_to_its_captured_location() {
        let fs = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let file = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"setattr-token-apply"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let capability = crate::task::SECURITY_CAPABILITY_XATTR_NAME;
        file.set_xattr(capability, &[1, 2, 3], XattrSetMode::Upsert)
            .unwrap();

        let metadata = file.metadata().unwrap();
        let cleanup = probe_inode_setattr_privilege_cleanup(&file, &metadata).unwrap();
        assert_eq!(cleanup.intent(), CredentialPrivilegeCleanup::Kill);
        cleanup.apply().unwrap();
        assert_eq!(read_security_capability(&file).unwrap(), None);
    }

    #[test]
    fn child_namespace_fsetid_cannot_preserve_initial_filesystem_bits() {
        executable::init().unwrap();
        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child = Cred::try_with_user_namespace(&root, child_namespace.clone()).unwrap();

        assert_eq!(
            ContentWriteCredentialView::new(&root, &root_namespace).setid_authority(),
            ContentWriteSetIdAuthority::CAP_FSETID
        );
        assert_eq!(
            ContentWriteCredentialView::new(&child, &child_namespace).setid_authority(),
            ContentWriteSetIdAuthority::CAP_FSETID
        );
        assert_eq!(
            ContentWriteCredentialView::new(&child, &root_namespace).setid_authority(),
            ContentWriteSetIdAuthority::UNPRIVILEGED
        );
    }

    #[test]
    fn shared_mapping_cleanup_never_inherits_creator_cap_fsetid() {
        executable::init().unwrap();
        let filesystem = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let file = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"shared-mapping-conservative-killpriv"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6755),
            )
            .unwrap();
        file.set_xattr(
            crate::task::SECURITY_CAPABILITY_XATTR_NAME,
            b"capability-record",
            XattrSetMode::Upsert,
        )
        .unwrap();

        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        drop(
            begin_content_write_privilege_cleanup(
                &file,
                ContentWriteCredentialView::new(&root, &root_namespace),
            )
            .unwrap(),
        );
        assert_eq!(file.metadata().unwrap().mode.bits(), 0o6755);
        file.set_xattr(
            crate::task::SECURITY_CAPABILITY_XATTR_NAME,
            b"capability-record",
            XattrSetMode::Upsert,
        )
        .unwrap();

        drop(begin_shared_writable_mapping_privilege_cleanup(&file).unwrap());
        assert_eq!(file.metadata().unwrap().mode.bits(), 0o0755);
        assert_eq!(read_security_capability(&file).unwrap(), None);
    }

    #[test]
    fn concurrent_content_guards_share_exclusion_through_commit() {
        executable::init().unwrap();
        let filesystem = tmp::MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let file = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(b"content-write-killpriv"),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o6755),
            )
            .unwrap();
        file.set_xattr(
            crate::task::SECURITY_CAPABILITY_XATTR_NAME,
            b"capability-record",
            XattrSetMode::Upsert,
        )
        .unwrap();

        let root_namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(root_namespace.clone()).unwrap();
        let child_namespace = root_namespace
            .try_fork(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT, true)
            .unwrap();
        let child = Cred::try_with_user_namespace(&root, child_namespace).unwrap();
        let guard = begin_content_write_privilege_cleanup(
            &file,
            ContentWriteCredentialView::new(&child, &root_namespace),
        )
        .unwrap();
        let concurrent_guard = begin_content_write_privilege_cleanup(
            &file,
            ContentWriteCredentialView::new(&child, &root_namespace),
        )
        .unwrap();

        assert_eq!(file.metadata().unwrap().mode.bits(), 0o0755);
        assert_eq!(read_security_capability(&file).unwrap(), None);
        assert_eq!(
            executable::CredentialReadLease::acquire(&file).err(),
            Some(AxError::from(LinuxError::ETXTBSY))
        );
        assert_eq!(
            executable::with_credential_metadata_unpinned(&file, || Ok(())),
            Err(AxError::from(LinuxError::ETXTBSY))
        );
        drop(guard);
        assert_eq!(
            executable::CredentialReadLease::acquire(&file).err(),
            Some(AxError::from(LinuxError::ETXTBSY))
        );
        assert_eq!(
            executable::with_credential_metadata_unpinned(&file, || Ok(())),
            Err(AxError::from(LinuxError::ETXTBSY))
        );
        drop(concurrent_guard);
        drop(executable::CredentialReadLease::acquire(&file).unwrap());
        assert_eq!(
            executable::with_credential_metadata_unpinned(&file, || Ok(())),
            Ok(())
        );
    }
}
