//! Linux v6.18 `file_getattr(2)` and `file_setattr(2)`.

use alloc::vec::Vec;
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{FileAttr as VfsFileAttr, FsPath, Location};
use axtask::current;
use linux_raw_sys::general::AT_EMPTY_PATH;
use linux_vfs::{
    FileAttr, LinuxVfsError, StructCopyPlan, file_getattr_copy_plan, file_setattr_copy_plan,
    validate_file_at_flags, validate_file_setattr_xflags,
};
use thekernel_linux_usercopy::{
    UserCopyError, UserMemory, UserMemoryContext, vm_load_until_nul_bounded,
};

use crate::{
    file::{
        Directory, File, ResolveAtResult, get_file_description, inode_flags,
        permission::VfsSecurityContext, pipe::NamedPipe, resolve_at_with_security,
        validate_pathname,
    },
    mm::{copy_struct_from_user, copy_struct_to_user, map_usercopy_error},
    task::AsThread,
};

const PATH_MAX: usize = 4096;

fn map_vfs_error(error: LinuxVfsError) -> AxError {
    match error {
        LinuxVfsError::StructTooLarge => LinuxError::E2BIG.into(),
        LinuxVfsError::StructTooSmall | LinuxVfsError::InvalidFlags => LinuxError::EINVAL.into(),
        _ => AxError::InvalidInput,
    }
}

fn plan(plan: Result<StructCopyPlan, LinuxVfsError>) -> AxResult<StructCopyPlan> {
    plan.map_err(map_vfs_error)
}

fn current_security() -> VfsSecurityContext {
    VfsSecurityContext::new(current().as_thread().current_cred())
}

/// Imports a pathname as opaque VFS bytes.  This intentionally has no UTF-8
/// conversion: ext4 and tmpfs names may contain every non-NUL byte.
fn read_path<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
) -> AxResult<Vec<u8>> {
    // Linux accepts at most PATH_MAX - 1 pathname bytes.  Bound the copy
    // itself so a missing terminator at that boundary is ENAMETOOLONG rather
    // than an unbounded faulting probe.
    let bytes = vm_load_until_nul_bounded(memory, path.cast::<u8>(), PATH_MAX).map_err(
        |error| match error {
            UserCopyError::TooLong => AxError::NameTooLong,
            other => map_usercopy_error(other),
        },
    )?;
    let path = FsPath::new(&bytes);
    validate_pathname(path)?;
    Ok(bytes)
}

fn empty_path_security(
    dfd: i32,
) -> AxResult<(
    VfsSecurityContext,
    Option<alloc::sync::Arc<crate::file::FileDescription>>,
)> {
    let snapshot = current().as_thread().namespace_credential_fs_snapshot();
    let description = if dfd == linux_raw_sys::general::AT_FDCWD {
        None
    } else {
        Some(get_file_description(dfd)?)
    };
    let topology = description
        .as_ref()
        .and_then(|description| description.vfs_mount_topology())
        .unwrap_or_else(|| snapshot.mount_topology.clone());
    Ok((
        VfsSecurityContext::with_execution_authority(
            snapshot.credential,
            topology,
            snapshot.landlock_domain,
        ),
        description,
    ))
}

fn resolve_file_attr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    at_flags: u32,
) -> AxResult<(VfsSecurityContext, Location)> {
    let bytes = if path.is_null() {
        if at_flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::BadAddress);
        }
        None
    } else {
        Some(read_path(memory, path)?)
    };
    let empty = bytes.as_ref().is_none_or(Vec::is_empty);
    let (security, description) = if empty && at_flags & AT_EMPTY_PATH != 0 {
        empty_path_security(dfd)?
    } else {
        (current_security(), None)
    };
    if let Some(description) = description {
        let file = description.file_handle();
        let location = if let Some(file) = file.downcast_ref::<File>() {
            Some(file.inner().location().clone())
        } else if let Some(directory) = file.downcast_ref::<Directory>() {
            Some(directory.inner().clone())
        } else if let Some(pipe) = file.downcast_ref::<NamedPipe>() {
            Some(pipe.location().clone())
        } else {
            None
        };
        return location
            .map(|location| (security, location))
            .ok_or(LinuxError::EOPNOTSUPP.into());
    }
    let resolved = resolve_at_with_security(
        dfd,
        bytes
            .as_deref()
            .map(FsPath::new)
            .filter(|path| !path.as_bytes().is_empty()),
        at_flags,
        &security,
    )?;
    match resolved {
        ResolveAtResult::File(location) => Ok((security, location)),
        ResolveAtResult::Other(_) => Err(LinuxError::EOPNOTSUPP.into()),
    }
}

fn as_uapi(attr: VfsFileAttr) -> FileAttr {
    FileAttr {
        fa_xflags: attr.xflags & inode_flags::FS_XFLAGS_MASK as u64,
        fa_extsize: attr.extsize,
        fa_nextents: attr.nextents,
        fa_projid: attr.project_id,
        fa_cowextsize: attr.cowextsize,
    }
}

fn as_vfs(attr: FileAttr) -> AxResult<VfsFileAttr> {
    validate_file_setattr_xflags(attr.fa_xflags).map_err(map_vfs_error)?;
    Ok(VfsFileAttr {
        xflags: attr.fa_xflags,
        extsize: attr.fa_extsize,
        // Generic fileattr treats this output-only field as an observation.
        // The shared setter will replace it with the provider's current value.
        nextents: attr.fa_nextents,
        project_id: attr.fa_projid,
        cowextsize: attr.fa_cowextsize,
    })
}

/// Linux v6.18 syscall 468.
pub fn sys_file_getattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    user_output: *mut FileAttr,
    usize: usize,
    at_flags: u32,
) -> AxResult<isize> {
    validate_file_at_flags(at_flags).map_err(map_vfs_error)?;
    let plan = plan(file_getattr_copy_plan(usize))?;
    let (security, location) = resolve_file_attr(memory, dfd, path, at_flags)?;
    let attr = inode_flags::get_file_attr(&location, &security)?;
    let output = as_uapi(attr);
    // `copy_struct_to_user` performs the mandatory zeroing of a larger user
    // extension and returns whether a short output hid nonzero fields.  The
    // v6.18 structure has no optional tail, so that result is informational.
    let _ = copy_struct_to_user(memory, user_output.cast(), plan.user_size, &output)?;
    Ok(0)
}

/// Linux v6.18 syscall 469.
pub fn sys_file_setattr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    dfd: i32,
    path: *const c_char,
    input: *const FileAttr,
    usize: usize,
    at_flags: u32,
) -> AxResult<isize> {
    validate_file_at_flags(at_flags).map_err(map_vfs_error)?;
    let plan = plan(file_setattr_copy_plan(usize))?;
    let input: FileAttr = copy_struct_from_user(memory, input.cast(), plan.user_size)?;
    let requested = as_vfs(input)?;
    let (security, location) = resolve_file_attr(memory, dfd, path, at_flags)?;
    let initial_ns = security.actor().user_ns().is_initial();
    let topology = security.mount_topology().ok_or(AxError::BadState)?;
    inode_flags::set_file_attr(&location, requested, &security, initial_ns, &topology)?;
    Ok(0)
}
