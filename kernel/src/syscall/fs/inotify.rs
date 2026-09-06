use core::ffi::c_char;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::FsPathBuf;
use linux_raw_sys::general::{
    AT_FDCWD, AT_SYMLINK_NOFOLLOW, IN_ACCESS, IN_ATTRIB, IN_CLOEXEC, IN_CLOSE_NOWRITE,
    IN_CLOSE_WRITE, IN_CREATE, IN_DELETE, IN_DELETE_SELF, IN_DONT_FOLLOW, IN_EXCL_UNLINK,
    IN_IGNORED, IN_ISDIR, IN_MASK_ADD, IN_MASK_CREATE, IN_MODIFY, IN_MOVE_SELF, IN_MOVED_FROM,
    IN_MOVED_TO, IN_NONBLOCK, IN_ONESHOT, IN_ONLYDIR, IN_OPEN, IN_Q_OVERFLOW, IN_UNMOUNT,
    O_NONBLOCK, O_RDONLY,
};

use crate::{
    file::{FileLike, ResolveAtResult, add_file_like_with_flags, inotify::InotifyFile, resolve_at},
    mm::{UserMemoryCapability, map_usercopy_error},
};

const ALL_INOTIFY_BITS: u32 = IN_ACCESS
    | IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_CLOSE_NOWRITE
    | IN_OPEN
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_UNMOUNT
    | IN_Q_OVERFLOW
    | IN_IGNORED
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_ADD
    | IN_MASK_CREATE
    | IN_ISDIR
    | IN_ONESHOT;

pub fn sys_inotify_init1(flags: i32) -> AxResult<isize> {
    let flags = flags as u32;
    if flags & !(IN_CLOEXEC | IN_NONBLOCK) != 0 {
        return Err(AxError::InvalidInput);
    }

    add_file_like_with_flags(
        InotifyFile::new(flags & IN_NONBLOCK != 0)?,
        flags & IN_CLOEXEC != 0,
        O_RDONLY | (flags & O_NONBLOCK),
    )
    .map(|fd| fd as isize)
}

pub fn sys_inotify_add_watch(
    memory: UserMemoryCapability,
    fd: i32,
    pathname: *const c_char,
    mask: u32,
) -> AxResult<isize> {
    if mask & !ALL_INOTIFY_BITS != 0 || mask & ALL_INOTIFY_BITS == 0 {
        return Err(AxError::InvalidInput);
    }
    let inotify = crate::file::inotify::InotifyFile::from_fd(fd)?;
    if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
        return Err(AxError::InvalidInput);
    }

    let pathname = FsPathBuf::from_vec(
        memory
            .load_until_nul(pathname.cast::<u8>())
            .map_err(map_usercopy_error)?,
    );
    let resolve_flags = if mask & IN_DONT_FOLLOW != 0 {
        AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    let ResolveAtResult::File(loc) = resolve_at(AT_FDCWD, Some(&pathname), resolve_flags)? else {
        return Err(AxError::InvalidInput);
    };
    if mask & IN_ONLYDIR != 0 && !loc.is_dir() {
        return Err(AxError::NotADirectory);
    }

    inotify.add_watch(&loc, mask).map(|wd| wd as isize)
}

pub fn sys_inotify_rm_watch(fd: i32, wd: i32) -> AxResult<isize> {
    let inotify = crate::file::inotify::InotifyFile::from_fd(fd)?;
    inotify.remove_watch(wd)?;
    Ok(0)
}
