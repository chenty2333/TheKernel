//! Inode privilege-metadata provider routing for setattr operations.
//!
//! Syscall glue must not select a concrete filesystem backend. This adapter is
//! the current integration point between typed Linux setattr cleanup intent and
//! the providers that can discover/remove executable privilege metadata. Only
//! tmpfs exposes `security.capability` today; additional providers and
//! module-aggregated need-killpriv policy can join here without leaking into
//! syscall entry code or the generic VFS contract.

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, Metadata, NodeType};
use thekernel_linux_cred::InodeSetattrPrivilegeCleanup as CredentialPrivilegeCleanup;

use super::xattr_provider::{read_security_capability, remove_security_capability_if_present};

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
    use axfs_ng_vfs::{Mountpoint, NodePermission, XattrSetMode};

    use super::*;
    use crate::pseudofs::tmp;

    #[test]
    fn cleanup_token_rejects_an_alias_from_a_distinct_mount() {
        let fs = tmp::MemoryFs::new().unwrap();
        let first_mount = Mountpoint::new_root(&fs);
        let second_mount = Mountpoint::new_root(&fs);
        let first = first_mount
            .root_location()
            .create(
                "setattr-token-mount",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        let second = second_mount
            .root_location()
            .lookup_no_follow("setattr-token-mount")
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
                "setattr-token-apply",
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
}
