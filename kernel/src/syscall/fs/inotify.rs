use core::ffi::c_char;

use axerrno::AxResult;
use linux_raw_sys::general::{IN_CLOEXEC, IN_NONBLOCK};

use crate::{
    file::{FileLike, add_file_like, inotify::InotifyFile},
    mm::vm_load_string,
};

pub fn sys_inotify_init1(flags: i32) -> AxResult<isize> {
    let flags = flags as u32;
    if flags & !(IN_CLOEXEC | IN_NONBLOCK) != 0 {
        return Err(axerrno::AxError::InvalidInput);
    }

    add_file_like(
        InotifyFile::new(flags & IN_NONBLOCK != 0),
        flags & IN_CLOEXEC != 0,
    )
    .map(|fd| fd as isize)
}

pub fn sys_inotify_add_watch(fd: i32, pathname: *const c_char, mask: u32) -> AxResult<isize> {
    let pathname = vm_load_string(pathname)?;
    let loc = axfs::FS_CONTEXT.lock().resolve_no_follow(&pathname)?;
    let inotify = crate::file::inotify::InotifyFile::from_fd(fd)?;
    inotify.add_watch(&loc, mask).map(|wd| wd as isize)
}

pub fn sys_inotify_rm_watch(fd: i32, wd: i32) -> AxResult<isize> {
    let inotify = crate::file::inotify::InotifyFile::from_fd(fd)?;
    inotify.remove_watch(wd)?;
    Ok(0)
}
