use axerrno::{AxError, AxResult};
use axfs::FsContext;
use axfs_ng_vfs::{
    Location, NodePermission, NodeType,
    path::{Component, Path},
};
use axtask::current_may_uninit;
use linux_raw_sys::general::{W_OK, X_OK};

use crate::task::AsThread;

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
    let mut granted = perm & 0o7;
    if gid == owner_gid || supplementary_groups.contains(&owner_gid) {
        granted |= (perm >> 3) & 0o7;
    }
    if uid == owner_uid {
        granted |= (perm >> 6) & 0o7;
    }
    granted
}

pub(crate) fn check_parent_search_permissions(
    loc: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    let mut parent = loc.parent();
    while let Some(dir) = parent {
        let stat = dir.metadata()?;
        if granted_access_bits(
            stat.mode.bits() as u32,
            stat.uid,
            stat.gid,
            uid,
            gid,
            supplementary_groups,
        ) & X_OK
            == 0
        {
            return Err(AxError::PermissionDenied);
        }
        parent = dir.parent();
    }
    Ok(())
}

pub(crate) fn check_path_prefix_search_permissions(
    fs: &FsContext,
    path: &Path,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    if uid == 0 {
        return Ok(());
    }

    let mut current = if path.is_absolute() {
        fs.root_dir().clone()
    } else {
        fs.current_dir().clone()
    };
    let mut components = path.components().peekable();

    while let Some(comp) = components.next() {
        match comp {
            Component::CurDir => {}
            Component::RootDir => current = fs.root_dir().clone(),
            Component::ParentDir => {
                check_search_permissions(&current, uid, gid, supplementary_groups)?;
                current = current.parent().unwrap_or_else(|| fs.root_dir().clone());
            }
            Component::Normal(name) => {
                check_search_permissions(&current, uid, gid, supplementary_groups)?;
                if components.peek().is_none() {
                    break;
                }
                current = fs.with_current_dir(current.clone())?.resolve(name)?;
                current.check_is_dir()?;
            }
        }
    }

    Ok(())
}

pub(crate) fn check_search_permissions(
    loc: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    if uid == 0 {
        return Ok(());
    }

    check_parent_search_permissions(loc, uid, gid, supplementary_groups)?;

    let stat = loc.metadata()?;
    if granted_access_bits(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        uid,
        gid,
        supplementary_groups,
    ) & X_OK
        == 0
    {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

pub(crate) fn check_create_permissions(
    dir: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    check_writable_mount(dir)?;

    if uid == 0 {
        return Ok(());
    }

    check_search_permissions(dir, uid, gid, supplementary_groups)?;

    let stat = dir.metadata()?;
    if granted_access_bits(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        uid,
        gid,
        supplementary_groups,
    ) & W_OK
        == 0
    {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

fn check_directory_write_search_permissions(
    dir: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    check_writable_mount(dir)?;

    if uid == 0 {
        return Ok(());
    }

    check_search_permissions(dir, uid, gid, supplementary_groups)?;

    let stat = dir.metadata()?;
    if granted_access_bits(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        uid,
        gid,
        supplementary_groups,
    ) & W_OK
        == 0
    {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

fn check_sticky_delete_permissions(dir: &Location, target: &Location, uid: u32) -> AxResult {
    if uid == 0 {
        return Ok(());
    }

    let dir_stat = dir.metadata()?;
    if dir_stat.mode.bits() as u32 & STICKY_MODE_BIT == 0 {
        return Ok(());
    }

    let target_stat = target.metadata()?;
    if uid == dir_stat.uid || uid == target_stat.uid {
        return Ok(());
    }

    Err(AxError::OperationNotPermitted)
}

pub(crate) fn check_remove_permissions(
    dir: &Location,
    target: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    check_directory_write_search_permissions(dir, uid, gid, supplementary_groups)?;
    check_sticky_delete_permissions(dir, target, uid)
}

pub(crate) fn check_rename_permissions(
    old_dir: &Location,
    source: &Location,
    new_dir: &Location,
    replaced: Option<&Location>,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    check_remove_permissions(old_dir, source, uid, gid, supplementary_groups)?;
    check_directory_write_search_permissions(new_dir, uid, gid, supplementary_groups)?;
    if let Some(replaced) = replaced {
        check_sticky_delete_permissions(new_dir, replaced, uid)?;
    }
    Ok(())
}

pub(crate) fn check_open_permissions(
    loc: &Location,
    mask: u32,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    if uid == 0 || mask == 0 {
        return Ok(());
    }

    check_parent_search_permissions(loc, uid, gid, supplementary_groups)?;

    let stat = loc.metadata()?;
    let granted = granted_access_bits(
        stat.mode.bits() as u32,
        stat.uid,
        stat.gid,
        uid,
        gid,
        supplementary_groups,
    );
    if granted & mask != mask {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

pub(crate) fn check_execute_permissions(
    loc: &Location,
    uid: u32,
    gid: u32,
    supplementary_groups: &[u32],
) -> AxResult {
    if uid != 0 {
        check_parent_search_permissions(loc, uid, gid, supplementary_groups)?;
    }

    let stat = loc.metadata()?;
    if stat.node_type != NodeType::RegularFile {
        return Err(AxError::PermissionDenied);
    }

    let perm = stat.mode.bits() as u32 & NodePermission::all().bits() as u32;
    if uid == 0 {
        if perm & 0o111 == 0 {
            return Err(AxError::PermissionDenied);
        }
        return Ok(());
    }

    if granted_access_bits(perm, stat.uid, stat.gid, uid, gid, supplementary_groups) & X_OK == 0 {
        return Err(AxError::PermissionDenied);
    }

    Ok(())
}

pub(crate) fn check_current_execute_permissions(loc: &Location) -> AxResult {
    let Some(curr) = current_may_uninit() else {
        return Ok(());
    };
    let Some(thr) = curr.try_as_thread() else {
        return Ok(());
    };

    let proc_data = &thr.proc_data;
    check_execute_permissions(
        loc,
        proc_data.fsuid(),
        proc_data.fsgid(),
        &proc_data.supplementary_groups(),
    )
}
