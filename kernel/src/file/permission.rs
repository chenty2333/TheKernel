use alloc::borrow::Cow;

use axerrno::{AxError, AxResult};
use axfs::FsContext;
use axfs_ng_vfs::{Location, NodePermission, NodeType, path::Path};
use axtask::current_may_uninit;
use linux_raw_sys::general::{CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, R_OK, W_OK, X_OK};

use crate::task::{AsThread, DacCredentialView};

const STICKY_MODE_BIT: u32 = 0o1000;

pub(crate) fn check_writable_mount(dir: &Location) -> AxResult {
    let path = dir.absolute_path().map_err(|_| AxError::InvalidInput)?;
    if crate::mounts::is_readonly(path.as_ref()) {
        Err(AxError::ReadOnlyFilesystem)
    } else {
        Ok(())
    }
}

pub(crate) fn granted_access_bits(
    perm: u32,
    owner_uid: u32,
    owner_gid: u32,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> u32 {
    if uid == owner_uid {
        (perm >> 6) & 0o7
    } else if gid == owner_gid || supplementary_groups.contains(&owner_gid) {
        (perm >> 3) & 0o7
    } else {
        perm & 0o7
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
    let requested = requested & 0o7;
    if requested == 0 {
        return true;
    }

    let granted = granted_access_bits(
        perm,
        owner_uid,
        owner_gid,
        credentials.uid(),
        credentials.gid(),
        credentials.supplementary_groups(),
    );
    if granted & requested == requested {
        return true;
    }

    if node_type == NodeType::Directory {
        if requested & W_OK == 0 && credentials.has_capability(CAP_DAC_READ_SEARCH) {
            return true;
        }
        return credentials.has_capability(CAP_DAC_OVERRIDE);
    }

    if requested == R_OK && credentials.has_capability(CAP_DAC_READ_SEARCH) {
        return true;
    }

    (requested & X_OK == 0 || perm & 0o111 != 0) && credentials.has_capability(CAP_DAC_OVERRIDE)
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

    fn resolve_no_follow_dac(
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

    fn resolve_no_follow_dac(
        &self,
        path: impl AsRef<Path>,
        credentials: &DacCredentialView,
    ) -> AxResult<Location> {
        self.resolve_no_follow_with_admission(path, &mut |dir| {
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
    if dir_stat.mode.bits() as u32 & STICKY_MODE_BIT == 0 {
        return Ok(());
    }

    let target_stat = target.metadata()?;
    if credentials.uid() == dir_stat.uid
        || credentials.uid() == target_stat.uid
        || credentials.has_capability(CAP_FOWNER)
    {
        return Ok(());
    }

    Err(AxError::OperationNotPermitted)
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
    let path = loc.absolute_path().map_err(|_| AxError::InvalidInput)?;
    if crate::mounts::is_noexec(path.as_ref()) {
        return Err(AxError::PermissionDenied);
    }

    let stat = loc.metadata()?;
    if stat.node_type != NodeType::RegularFile {
        return Err(AxError::PermissionDenied);
    }

    let perm = stat.mode.bits() as u32 & NodePermission::all().bits() as u32;
    check_dac_permissions(perm, stat.uid, stat.gid, stat.node_type, X_OK, credentials)
}

pub(crate) fn check_current_execute_permissions(loc: &Location) -> AxResult {
    let Some(curr) = current_may_uninit() else {
        return Ok(());
    };
    let Some(thr) = curr.try_as_thread() else {
        return Ok(());
    };

    let proc_data = &thr.proc_data;
    let credentials = proc_data.fs_dac_credentials();
    check_execute_permissions(loc, &credentials)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(uid: u32, gid: u32, groups: &[u32], capabilities: &[u32]) -> DacCredentialView {
        let mut effective = [0; 2];
        for &capability in capabilities {
            let word = capability as usize / u32::BITS as usize;
            effective[word] |= 1 << (capability % u32::BITS);
        }
        DacCredentialView::new(uid, gid, groups.to_vec(), effective)
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
}
