use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::{Location, NodePermission};
use axtask::current;
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EACCESS, AT_EMPTY_PATH, R_OK, W_OK, X_OK, stat, statfs, statx,
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{
        File, FileLike,
        permission::{check_parent_search_permissions, granted_access_bits},
        resolve_at,
    },
    mm::vm_load_string,
    task::AsThread,
};

/// Get the file metadata by `path` and write into `statbuf`.
///
/// Return 0 if success.
#[cfg(target_arch = "x86_64")]
pub fn sys_stat(path: *const c_char, statbuf: *mut stat) -> AxResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_fstatat(AT_FDCWD, path, statbuf, 0)
}

/// Get file metadata by `fd` and write into `statbuf`.
///
/// Return 0 if success.
pub fn sys_fstat(fd: i32, statbuf: *mut stat) -> AxResult<isize> {
    sys_fstatat(fd, core::ptr::null(), statbuf, AT_EMPTY_PATH)
}

/// Get the metadata of the symbolic link and write into `buf`.
///
/// Return 0 if success.
#[cfg(target_arch = "x86_64")]
pub fn sys_lstat(path: *const c_char, statbuf: *mut stat) -> AxResult<isize> {
    use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

    sys_fstatat(AT_FDCWD, path, statbuf, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fstatat(
    dirfd: i32,
    path: *const c_char,
    statbuf: *mut stat,
    flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;

    debug!("sys_fstatat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    statbuf.vm_write(loc.stat()?.into())?;

    Ok(0)
}

pub fn sys_statx(
    dirfd: c_int,
    path: *const c_char,
    flags: u32,
    _mask: u32,
    statxbuf: *mut statx,
) -> AxResult<isize> {
    // `statx()` uses pathname, dirfd, and flags to identify the target
    // file in one of the following ways:

    // An absolute pathname(situation 1)
    //        If pathname begins with a slash, then it is an absolute
    //        pathname that identifies the target file.  In this case,
    //        dirfd is ignored.

    // A relative pathname(situation 2)
    //        If pathname is a string that begins with a character other
    //        than a slash and dirfd is AT_FDCWD, then pathname is a
    //        relative pathname that is interpreted relative to the
    //        process's current working directory.

    // A directory-relative pathname(situation 3)
    //        If pathname is a string that begins with a character other
    //        than a slash and dirfd is a file descriptor that refers to
    //        a directory, then pathname is a relative pathname that is
    //        interpreted relative to the directory referred to by dirfd.
    //        (See openat(2) for an explanation of why this is useful.)

    // By file descriptor(situation 4)
    //        If pathname is an empty string (or NULL since Linux 6.11)
    //        and the AT_EMPTY_PATH flag is specified in flags (see
    //        below), then the target file is the one referred to by the
    //        file descriptor dirfd.

    let path = path.nullable().map(vm_load_string).transpose()?;
    debug!("sys_statx <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    statxbuf.vm_write(resolve_at(dirfd, path.as_deref(), flags)?.stat()?.into())?;

    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_access(path: *const c_char, mode: u32) -> AxResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_faccessat2(AT_FDCWD, path, mode, 0)
}

fn check_readonly_write_access(loc: &Location) -> AxResult {
    let path = loc.absolute_path().map_err(|_| AxError::InvalidInput)?;
    if crate::mounts::is_readonly(path.as_ref()) {
        Err(AxError::ReadOnlyFilesystem)
    } else {
        Ok(())
    }
}

pub fn sys_faccessat2(dirfd: c_int, path: *const c_char, mode: u32, flags: u32) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    debug!("sys_faccessat2 <= dirfd: {dirfd}, path: {path:?}, mode: {mode}, flags: {flags}");

    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let supplementary_groups = proc_data.supplementary_groups();
    let (uid, gid) = if flags & AT_EACCESS as u32 != 0 {
        (proc_data.euid(), proc_data.egid())
    } else {
        (proc_data.uid(), proc_data.gid())
    };

    let file = resolve_at(dirfd, path.as_deref(), flags)?;
    let stat = file.stat()?;
    let perm = stat.mode & NodePermission::all().bits() as u32;

    if let Some(loc) = match &file {
        crate::file::ResolveAtResult::File(loc) => Some(loc),
        crate::file::ResolveAtResult::Other(_) => None,
    } {
        if uid != 0 {
            check_parent_search_permissions(loc, uid, gid, &supplementary_groups)?;
        }
        if mode & W_OK != 0 {
            check_readonly_write_access(loc)?;
        }
    }

    if mode == 0 {
        return Ok(0);
    }

    if uid == 0 {
        if mode & X_OK != 0 && perm & 0o111 == 0 {
            return Err(AxError::PermissionDenied);
        }
        return Ok(0);
    }

    let granted = granted_access_bits(perm, stat.uid, stat.gid, uid, gid, &supplementary_groups);

    let requested = mode & 0o7;
    if requested & granted != requested {
        return Err(AxError::PermissionDenied);
    }

    Ok(0)
}

fn statfs(loc: &Location) -> AxResult<statfs> {
    let stat = loc.filesystem().stat()?;
    // FIXME: Zeroable
    let mut result: statfs = unsafe { core::mem::zeroed() };
    result.f_type = stat.fs_type as _;
    result.f_bsize = stat.block_size as _;
    result.f_blocks = stat.blocks as _;
    result.f_bfree = stat.blocks_free as _;
    result.f_bavail = stat.blocks_available as _;
    result.f_files = stat.file_count as _;
    result.f_ffree = stat.free_file_count as _;
    // TODO: fsid
    result.f_fsid = __kernel_fsid_t {
        val: [0, loc.mountpoint().device() as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = stat.mount_flags as _;
    Ok(result)
}

pub fn sys_statfs(path: *const c_char, buf: *mut statfs) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    debug!("sys_statfs <= path: {path:?}");

    buf.vm_write(statfs(
        &FS_CONTEXT
            .lock()
            .resolve(path)?
            .mountpoint()
            .root_location(),
    )?)?;
    Ok(0)
}

pub fn sys_fstatfs(fd: i32, buf: *mut statfs) -> AxResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    buf.vm_write(statfs(File::from_fd(fd)?.inner().location())?)?;
    Ok(0)
}
