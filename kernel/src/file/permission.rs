use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, NodePermission, NodeType};
use axtask::current_may_uninit;
use linux_raw_sys::general::{W_OK, X_OK};

use crate::task::AsThread;

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
        proc_data.euid(),
        proc_data.egid(),
        &proc_data.supplementary_groups(),
    )
}
