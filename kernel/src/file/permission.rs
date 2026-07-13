use alloc::borrow::Cow;

use axerrno::{AxError, AxResult};
use axfs::FsContext;
use axfs_ng_vfs::{Location, Metadata, NodePermission, NodeType, path::Path};
use linux_raw_sys::general::{
    CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, R_OK, W_OK, X_OK,
};
use linux_vfs::{
    Access, DacCapability, DacCredentials, DacError, NodeKind as LinuxNodeKind,
    NodeMetadata as LinuxNodeMetadata, check_dac as check_linux_dac,
    check_sticky_mutation as check_linux_sticky_mutation,
    initial_create_attributes as linux_initial_create_attributes,
};

use crate::task::{DacCredentialView, Kgid};

static INITIAL_USER_NAMESPACE_DAC_DOMAIN: () = ();

struct KernelDacCredentials<'a>(&'a DacCredentialView);

impl DacCredentials for KernelDacCredentials<'_> {
    type UserId = u32;
    type GroupId = u32;
    type UserNamespace = ();

    fn fs_user_id(&self) -> Self::UserId {
        self.0.uid().into_raw()
    }

    fn fs_group_id(&self) -> Self::GroupId {
        self.0.gid().into_raw()
    }

    fn is_in_group(&self, group: Self::GroupId) -> bool {
        Kgid::from_raw(group).is_some_and(|group| self.0.supplementary_groups().contains(&group))
    }

    fn has_capability(&self, _owner: &Self::UserNamespace, capability: DacCapability) -> bool {
        self.0.has_capability(match capability {
            DacCapability::Override => CAP_DAC_OVERRIDE,
            DacCapability::ReadSearch => CAP_DAC_READ_SEARCH,
            DacCapability::Fowner => CAP_FOWNER,
            DacCapability::Fsetid => CAP_FSETID,
            _ => return false,
        })
    }
}

fn linux_node_kind(node_type: NodeType) -> LinuxNodeKind {
    match node_type {
        NodeType::Unknown => LinuxNodeKind::Unknown,
        NodeType::Fifo => LinuxNodeKind::Fifo,
        NodeType::CharacterDevice => LinuxNodeKind::CharacterDevice,
        NodeType::Directory => LinuxNodeKind::Directory,
        NodeType::BlockDevice => LinuxNodeKind::BlockDevice,
        NodeType::RegularFile => LinuxNodeKind::Regular,
        NodeType::Symlink => LinuxNodeKind::Symlink,
        NodeType::Socket => LinuxNodeKind::Socket,
    }
}

fn linux_access(requested: u32) -> Access {
    let mut access = Access::NONE;
    if requested & R_OK != 0 {
        access |= Access::READ;
    }
    if requested & W_OK != 0 {
        access |= Access::WRITE;
    }
    if requested & X_OK != 0 {
        access |= Access::EXECUTE;
    }
    access
}

fn linux_node_metadata(
    mode: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
) -> LinuxNodeMetadata<'static, u32, u32, ()> {
    LinuxNodeMetadata {
        mode: mode as u16,
        owner_user: owner_uid,
        owner_group: owner_gid,
        kind: linux_node_kind(node_type),
        owner_user_namespace: &INITIAL_USER_NAMESPACE_DAC_DOMAIN,
        ids_mapped: true,
    }
}

fn map_dac_error(error: DacError) -> AxError {
    match error {
        DacError::AccessDenied => AxError::PermissionDenied,
        DacError::StickyDenied => AxError::OperationNotPermitted,
        _ => AxError::PermissionDenied,
    }
}

pub(crate) fn check_writable_mount(dir: &Location) -> AxResult {
    if crate::mounts::is_readonly(dir)? {
        Err(AxError::ReadOnlyFilesystem)
    } else {
        Ok(())
    }
}

fn dac_access_allowed(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    credentials: &DacCredentialView,
) -> bool {
    check_linux_dac(
        &linux_node_metadata(perm, owner_uid, owner_gid, node_type),
        linux_access(requested),
        &KernelDacCredentials(credentials),
    )
    .is_ok()
}

pub(crate) fn check_dac_permissions(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    node_type: NodeType,
    requested: u32,
    credentials: &DacCredentialView,
) -> AxResult {
    if dac_access_allowed(
        perm,
        owner_uid,
        owner_gid,
        node_type,
        requested,
        credentials,
    ) {
        Ok(())
    } else {
        Err(AxError::PermissionDenied)
    }
}

pub(crate) fn check_pathwalk_search_permission(
    dir: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    let stat = dir.metadata()?;
    check_dac_permissions(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        stat.node_type,
        X_OK,
        credentials,
    )
}

/// Linux DAC admission over the generic axfs path resolver.
///
/// The generic resolver owns component and symlink traversal. This adapter
/// injects one immutable-per-operation credential view without teaching axfs
/// about Linux identities or capabilities.
pub(crate) trait DacFsContextExt {
    fn resolve_dac(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;

    fn resolve_dac_unobserved(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;

    fn resolve_no_follow_dac(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;

    fn resolve_no_follow_dac_unobserved(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location>;

    fn resolve_parent_dac<'a>(
        &self,
        path: &'a Path,
        credentials: &DacCredentialView,
    ) -> AxResult<(Location, Cow<'a, str>)>;

    fn resolve_nonexistent_dac<'a>(
        &self,
        path: &'a Path,
        credentials: &DacCredentialView,
    ) -> AxResult<(Location, &'a str)>;
}

impl DacFsContextExt for FsContext {
    fn resolve_dac(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_dac_unobserved(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_with_admission_unobserved(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_no_follow_dac(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_no_follow_dac_unobserved(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission_unobserved(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_parent_dac<'a>(
        &self,
        path: &'a Path,
        credentials: &DacCredentialView,
    ) -> AxResult<(Location, Cow<'a, str>)> {
        self.resolve_parent_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }

    fn resolve_nonexistent_dac<'a>(
        &self,
        path: &'a Path,
        credentials: &DacCredentialView,
    ) -> AxResult<(Location, &'a str)> {
        self.resolve_nonexistent_with_admission(path, &mut |dir| {
            check_pathwalk_search_permission(dir, credentials)
        })
    }
}

pub(crate) fn check_search_permissions(
    loc: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    check_pathwalk_search_permission(loc, credentials)
}

pub(crate) fn check_create_permissions(
    dir: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    check_directory_write_search_permissions(dir, credentials)
}

/// Computes Linux `vfs_prepare_mode()` plus `inode_init_owner()` attributes
/// for one named inode before the generic VFS publishes its name.
///
/// The SGID permission check intentionally precedes umask, as it does in
/// Linux. Directories under an SGID parent always inherit that bit; regular
/// and special files only lose an executable SGID request when the caller is
/// neither in the parent group nor capable of preserving set-id bits.
pub(crate) fn initial_named_create_owner_mode(
    parent: &Metadata,
    credentials: &DacCredentialView,
    node_type: NodeType,
    requested_mode: NodePermission,
    umask: u32,
) -> (NodePermission, (u32, u32)) {
    let attributes = linux_initial_create_attributes(
        &linux_node_metadata(
            parent.mode.bits() as u32,
            parent.uid,
            parent.gid,
            parent.node_type,
        ),
        linux_node_kind(node_type),
        requested_mode.bits(),
        umask as u16,
        &KernelDacCredentials(credentials),
    );
    (
        NodePermission::from_bits_truncate(attributes.mode),
        (attributes.user, attributes.group),
    )
}

fn check_directory_write_search_permissions(
    dir: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    check_writable_mount(dir)?;

    let stat = dir.metadata()?;
    check_dac_permissions(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        stat.node_type,
        W_OK | X_OK,
        credentials,
    )
}

fn check_sticky_delete_permissions(
    dir: &Location,
    target: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    let dir_stat = dir.metadata()?;
    let target_stat = target.metadata()?;
    check_linux_sticky_mutation(
        &linux_node_metadata(
            dir_stat.mode.bits() as u32,
            dir_stat.uid,
            dir_stat.gid,
            dir_stat.node_type,
        ),
        &linux_node_metadata(
            target_stat.mode.bits() as u32,
            target_stat.uid,
            target_stat.gid,
            target_stat.node_type,
        ),
        &KernelDacCredentials(credentials),
    )
    .map_err(map_dac_error)
}

pub(crate) fn check_remove_permissions(
    dir: &Location,
    target: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    check_directory_write_search_permissions(dir, credentials)?;
    check_sticky_delete_permissions(dir, target, credentials)
}

pub(crate) fn check_rename_permissions(
    old_dir: &Location,
    source: &Location,
    new_dir: &Location,
    replaced: Option<&Location>,
    credentials: &DacCredentialView,
) -> AxResult {
    check_remove_permissions(old_dir, source, credentials)?;
    check_directory_write_search_permissions(new_dir, credentials)?;
    if let Some(replaced) = replaced {
        check_sticky_delete_permissions(new_dir, replaced, credentials)?;
    }
    Ok(())
}

pub(crate) fn check_open_permissions(
    loc: &Location,
    mask: u32,
    credentials: &DacCredentialView,
) -> AxResult {
    if mask == 0 {
        return Ok(());
    }

    let stat = loc.metadata()?;
    check_dac_permissions(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        stat.node_type,
        mask,
        credentials,
    )
}

pub(crate) fn check_execute_permissions(
    loc: &Location,
    credentials: &DacCredentialView,
) -> AxResult {
    if crate::mounts::is_noexec(loc)? {
        return Err(AxError::PermissionDenied);
    }

    let stat = loc.metadata()?;
    if stat.node_type != NodeType::RegularFile {
        return Err(AxError::PermissionDenied);
    }

    let perm = stat.mode.bits() as u32 & NodePermission::all().bits() as u32;
    check_dac_permissions(perm, stat.uid, stat.gid, stat.node_type, X_OK, credentials)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use thekernel_linux_cred::{FsCredentialSnapshot, GroupInfo, Kgid, Kuid};

    use super::*;

    fn credentials(uid: u32, gid: u32, groups: &[u32], capabilities: &[u32]) -> DacCredentialView {
        let mut effective = [0; 2];
        for &capability in capabilities {
            let word = capability as usize / u32::BITS as usize;
            effective[word] |= 1 << (capability % u32::BITS);
        }
        let mut supplementary_groups = Vec::new();
        supplementary_groups
            .try_reserve_exact(groups.len())
            .unwrap();
        for &group in groups {
            supplementary_groups.push(Kgid::from_raw(group).unwrap());
        }
        FsCredentialSnapshot::new(
            Kuid::from_raw(uid).unwrap(),
            Kgid::from_raw(gid).unwrap(),
            GroupInfo::try_new(supplementary_groups).unwrap(),
            effective,
            true,
        )
    }

    fn directory_metadata(mode: u16, uid: u32, gid: u32) -> Metadata {
        Metadata {
            device: 1,
            inode: 1,
            nlink: 1,
            mode: NodePermission::from_bits_truncate(mode),
            node_type: NodeType::Directory,
            uid,
            gid,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: axfs_ng_vfs::DeviceId(0),
            atime: core::time::Duration::ZERO,
            btime: core::time::Duration::ZERO,
            mtime: core::time::Duration::ZERO,
            ctime: core::time::Duration::ZERO,
        }
    }

    #[test]
    fn owner_class_does_not_inherit_other_permissions() {
        let credentials = credentials(1000, 100, &[], &[]);
        assert!(!dac_access_allowed(
            0o004,
            1000,
            200,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
    }

    #[test]
    fn group_class_does_not_inherit_other_permissions() {
        let credentials = credentials(1000, 200, &[], &[]);
        assert!(!dac_access_allowed(
            0o004,
            3000,
            200,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
    }

    #[test]
    fn uid_zero_without_effective_dac_capabilities_is_not_privileged() {
        let credentials = credentials(0, 0, &[], &[]);
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::Directory,
            X_OK,
            &credentials,
        ));
    }

    #[test]
    fn read_search_does_not_override_write_permissions() {
        let credentials = credentials(0, 0, &[], &[CAP_DAC_READ_SEARCH]);
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK,
            &credentials,
        ));
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::Directory,
            X_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            W_OK,
            &credentials,
        ));
    }

    #[test]
    fn override_requires_an_execute_bit_for_regular_files() {
        let credentials = credentials(0, 0, &[], &[CAP_DAC_OVERRIDE]);
        assert!(dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            R_OK | W_OK,
            &credentials,
        ));
        assert!(!dac_access_allowed(
            0,
            1000,
            100,
            NodeType::RegularFile,
            X_OK,
            &credentials,
        ));
        assert!(dac_access_allowed(
            0o001,
            1000,
            100,
            NodeType::RegularFile,
            X_OK,
            &credentials,
        ));
    }

    #[test]
    fn sgid_parent_attributes_follow_linux_prepare_then_owner_order() {
        let parent = directory_metadata(0o2770, 10, 200);
        let unprivileged = credentials(1000, 100, &[], &[]);

        let (regular_mode, regular_owner) = initial_named_create_owner_mode(
            &parent,
            &unprivileged,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o2670),
            0o020,
        );
        assert_eq!(regular_owner, (1000, 200));
        assert_eq!(regular_mode.bits(), 0o650);

        let (directory_mode, directory_owner) = initial_named_create_owner_mode(
            &parent,
            &unprivileged,
            NodeType::Directory,
            NodePermission::from_bits_truncate(0o770),
            0o027,
        );
        assert_eq!(directory_owner, (1000, 200));
        assert_eq!(directory_mode.bits(), 0o2750);
    }

    #[test]
    fn cap_fsetid_preserves_executable_sgid_request() {
        let parent = directory_metadata(0o2770, 10, 200);
        let capable = credentials(1000, 100, &[], &[CAP_FSETID]);
        let (mode, owner) = initial_named_create_owner_mode(
            &parent,
            &capable,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o2670),
            0,
        );
        assert_eq!(owner, (1000, 200));
        assert_eq!(mode.bits(), 0o2670);
    }

    #[test]
    fn unix_socket_mode_uses_umask_and_sgid_parent_group() {
        let parent = directory_metadata(0o2770, 10, 200);
        let caller = credentials(1000, 100, &[300], &[]);
        let (mode, owner) = initial_named_create_owner_mode(
            &parent,
            &caller,
            NodeType::Socket,
            NodePermission::from_bits_truncate(0o777),
            0o027,
        );
        assert_eq!(mode.bits(), 0o750);
        assert_eq!(owner, (1000, 200));
    }
}
