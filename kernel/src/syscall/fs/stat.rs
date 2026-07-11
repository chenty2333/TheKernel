use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, Location, NodePermission, NodeType, path::Path};
use axtask::current;
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EACCESS, AT_EMPTY_PATH, AT_FDCWD, AT_NO_AUTOMOUNT, AT_STATX_SYNC_TYPE,
    AT_SYMLINK_NOFOLLOW, R_OK, S_IFBLK, S_IFMT, S_IFREG, STATX__RESERVED, STATX_ALL, STATX_ATIME,
    STATX_BASIC_STATS, STATX_BLOCKS, STATX_BTIME, STATX_CTIME, STATX_DIOALIGN, STATX_GID,
    STATX_INO, STATX_MNT_ID, STATX_MNT_ID_UNIQUE, STATX_MODE, STATX_MTIME, STATX_NLINK, STATX_SIZE,
    STATX_SUBVOL, STATX_TYPE, STATX_UID, STATX_WRITE_ATOMIC, W_OK, X_OK, stat, statfs, statx,
    statx_timestamp,
};
use starry_vm::{VmMutPtr, VmPtr};

use super::ctl::validate_pathname;
use crate::{
    file::{
        Directory, File, FileLike, Pipe, Socket, get_file_like,
        permission::{DacFsContextExt, check_dac_permissions},
        resolve_at, resolve_at_with_credentials, with_path_fs,
    },
    mm::vm_load_string,
    mounts,
    task::AsThread,
};

const SUPPORTED_FACCESSAT_FLAGS: u32 = AT_EACCESS as u32 | AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_FSTATAT_FLAGS: u32 = AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW;
const SUPPORTED_STATX_FLAGS: u32 =
    AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE;
const VALID_STATX_MASK: u32 = STATX_TYPE
    | STATX_MODE
    | STATX_NLINK
    | STATX_UID
    | STATX_GID
    | STATX_ATIME
    | STATX_MTIME
    | STATX_CTIME
    | STATX_INO
    | STATX_SIZE
    | STATX_BLOCKS
    | STATX_BTIME
    | STATX_MNT_ID
    | STATX_DIOALIGN
    | STATX_MNT_ID_UNIQUE
    | STATX_SUBVOL
    | STATX_WRITE_ATOMIC
    | STATX_ALL;
const STATX_CHANGE_COOKIE: u32 = 0x4000_0000;
// Match Linux's regular-file O_DIRECT floor: logical sector alignment, not
// the filesystem's preferred st_blksize.
const REGULAR_FILE_DIO_ALIGNMENT: u32 = 512;
const PIPEFS_MAGIC: i64 = 0x5049_5045;
const SOCKFS_MAGIC: i64 = 0x534f_434b;

fn node_type_from_mode(mode: u32) -> NodeType {
    NodeType::from(((mode & S_IFMT) >> 12) as u8)
}

#[cfg(test)]
fn uses_empty_path_fd(path: Option<&str>, flags: u32) -> bool {
    flags & AT_EMPTY_PATH != 0 && path.is_none_or(str::is_empty)
}

fn readonly_access_check_applies(node_type: NodeType) -> bool {
    !matches!(
        node_type,
        NodeType::CharacterDevice | NodeType::BlockDevice | NodeType::Fifo | NodeType::Socket
    )
}

fn statx_timestamp_from_duration(time: core::time::Duration) -> statx_timestamp {
    statx_timestamp {
        tv_sec: time.as_secs() as _,
        tv_nsec: time.subsec_nanos() as _,
        __reserved: 0,
    }
}

fn statx_from_kstat(value: crate::file::Kstat, request_mask: u32) -> statx {
    let mut result: statx = unsafe { core::mem::zeroed() };
    result.stx_mask = STATX_BASIC_STATS | STATX_BTIME;
    result.stx_blksize = value.blksize as _;
    result.stx_attributes = value.attributes;
    result.stx_attributes_mask = value.attributes_mask;
    result.stx_nlink = value.nlink as _;
    result.stx_uid = value.uid as _;
    result.stx_gid = value.gid as _;
    result.stx_mode = value.mode as _;
    result.stx_ino = value.ino as _;
    result.stx_size = value.size as _;
    result.stx_blocks = value.blocks as _;
    result.stx_atime = statx_timestamp_from_duration(value.atime);
    result.stx_btime = statx_timestamp_from_duration(value.btime);
    result.stx_ctime = statx_timestamp_from_duration(value.ctime);
    result.stx_mtime = statx_timestamp_from_duration(value.mtime);
    result.stx_rdev_major = value.rdev.major();
    result.stx_rdev_minor = value.rdev.minor();
    let dev = DeviceId(value.dev);
    result.stx_dev_major = dev.major();
    result.stx_dev_minor = dev.minor();
    if value.mnt_id != 0 {
        result.stx_mask |= STATX_MNT_ID;
        result.stx_mnt_id = value.mnt_id;
    }

    let request_mask = request_mask & !STATX_CHANGE_COOKIE;
    let file_type = value.mode & S_IFMT;
    if request_mask & STATX_DIOALIGN != 0 && file_type == S_IFBLK {
        result.stx_mask |= STATX_DIOALIGN;
        result.stx_dio_mem_align = 1;
        result.stx_dio_offset_align = value.blksize.max(512);
    } else if request_mask & STATX_DIOALIGN != 0 && file_type == S_IFREG {
        result.stx_mask |= STATX_DIOALIGN;
        result.stx_dio_mem_align = REGULAR_FILE_DIO_ALIGNMENT;
        result.stx_dio_offset_align = REGULAR_FILE_DIO_ALIGNMENT;
    }

    result
}

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
    if flags & !SUPPORTED_FSTATAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_null() && flags & AT_EMPTY_PATH == 0 {
        return Err(AxError::BadAddress);
    }
    let path = path.nullable().map(vm_load_string).transpose()?;
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    debug!("sys_fstatat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    statbuf.vm_write(loc.stat()?.into())?;

    Ok(0)
}

pub fn sys_statx(
    dirfd: c_int,
    path: *const c_char,
    flags: u32,
    mask: u32,
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
    if flags & !SUPPORTED_STATX_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE {
        return Err(AxError::InvalidInput);
    }
    if mask & STATX__RESERVED != 0 {
        return Err(AxError::InvalidInput);
    }
    if mask & !(VALID_STATX_MASK | STATX__RESERVED | STATX_CHANGE_COOKIE) != 0 {
        return Err(AxError::InvalidInput);
    }
    if path.is_none() && flags & AT_EMPTY_PATH == 0 {
        return Err(AxError::BadAddress);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    statxbuf.vm_write(statx_from_kstat(loc.stat()?, mask))?;

    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_access(path: *const c_char, mode: u32) -> AxResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_faccessat2(AT_FDCWD, path, mode, 0)
}

pub fn sys_faccessat(dirfd: c_int, path: *const c_char, mode: u32) -> AxResult<isize> {
    sys_faccessat2(dirfd, path, mode, 0)
}

fn check_readonly_write_access(loc: &Location) -> AxResult {
    if crate::mounts::is_readonly(loc)? {
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
    if flags & !SUPPORTED_FACCESSAT_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if let Some(path) = path.as_deref() {
        if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
            return Err(AxError::NotFound);
        }
        validate_pathname(Path::new(path))?;
    }

    let curr = current();
    let credentials = curr
        .as_thread()
        .access_dac_credentials(flags & AT_EACCESS as u32 != 0);

    let file = resolve_at_with_credentials(dirfd, path.as_deref(), flags, &credentials)?;
    let stat = file.stat()?;
    let perm = stat.mode & NodePermission::all().bits() as u32;
    let node_type = node_type_from_mode(stat.mode);

    let loc = match &file {
        crate::file::ResolveAtResult::File(loc) => Some(loc),
        crate::file::ResolveAtResult::Other(_) => None,
    };
    if let Some(loc) = loc {
        if mode & X_OK != 0 && node_type == NodeType::RegularFile {
            if crate::mounts::is_noexec(loc)? {
                return Err(AxError::PermissionDenied);
            }
        }
    }

    if mode == 0 {
        return Ok(0);
    }

    check_dac_permissions(perm, stat.uid, stat.gid, node_type, mode, &credentials)?;
    if mode & W_OK != 0
        && readonly_access_check_applies(node_type)
        && let Some(loc) = loc
    {
        check_readonly_write_access(loc)?;
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
    let device = mounts::linux_device_id(loc.mountpoint().device()).0;
    result.f_fsid = __kernel_fsid_t {
        val: [device as _, (device >> 32) as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = crate::mounts::statfs_mount_flags(loc, stat.mount_flags)? as _;
    Ok(result)
}

fn special_fd_statfs(fd: &dyn FileLike) -> Option<AxResult<statfs>> {
    let mut result: statfs = unsafe { core::mem::zeroed() };

    if fd.downcast_ref::<Pipe>().is_some() {
        result.f_type = PIPEFS_MAGIC;
        result.f_bsize = 4096;
        result.f_namelen = 255;
        result.f_frsize = 4096;
        return Some(Ok(result));
    }

    if fd.downcast_ref::<Socket>().is_some() {
        result.f_type = SOCKFS_MAGIC;
        result.f_bsize = 4096;
        result.f_namelen = 255;
        result.f_frsize = 4096;
        return Some(Ok(result));
    }

    None
}

pub fn sys_statfs(path: *const c_char, buf: *mut statfs) -> AxResult<isize> {
    let path = vm_load_string(path)?;
    debug!("sys_statfs <= path: {path:?}");
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    let path_ref = Path::new(&path);
    validate_pathname(path_ref)?;

    let curr = current();
    let credentials = curr.as_thread().fs_dac_credentials();
    let loc = with_path_fs(AT_FDCWD, path_ref, |fs| {
        fs.resolve_dac(path_ref, &credentials)
    })?;

    buf.vm_write(statfs(&loc.mountpoint().root_location())?)?;
    Ok(0)
}

pub fn sys_fstatfs(fd: i32, buf: *mut statfs) -> AxResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    let file = get_file_like(fd)?;
    if let Some(file) = file.downcast_ref::<File>() {
        buf.vm_write(statfs(file.inner().location())?)?;
    } else if let Some(dir) = file.downcast_ref::<Directory>() {
        buf.vm_write(statfs(dir.inner())?)?;
    } else if let Some(result) = special_fd_statfs(file.as_ref()) {
        buf.vm_write(result?)?;
    } else {
        return Err(AxError::InvalidInput);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_fd_detection_requires_the_flag_and_an_empty_path() {
        assert!(uses_empty_path_fd(None, AT_EMPTY_PATH));
        assert!(uses_empty_path_fd(Some(""), AT_EMPTY_PATH));
        assert!(!uses_empty_path_fd(Some("file"), AT_EMPTY_PATH));
        assert!(!uses_empty_path_fd(None, 0));
    }

    #[test]
    fn readonly_access_check_excludes_linux_special_files() {
        for node_type in [
            NodeType::CharacterDevice,
            NodeType::BlockDevice,
            NodeType::Fifo,
            NodeType::Socket,
        ] {
            assert!(!readonly_access_check_applies(node_type));
        }
        assert!(readonly_access_check_applies(NodeType::RegularFile));
        assert!(readonly_access_check_applies(NodeType::Directory));
        assert!(readonly_access_check_applies(NodeType::Symlink));
    }
}
