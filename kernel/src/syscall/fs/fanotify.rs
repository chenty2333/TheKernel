use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW};

use crate::{
    file::{
        FileLike, ResolveAtResult, add_file_like, fanotify::*, get_file_like,
        inotify::location_for_fd, resolve_at,
    },
    mm::vm_load_string,
};

pub fn sys_fanotify_init(flags: u32, event_f_flags: u32) -> AxResult<isize> {
    validate_init_flags(flags, event_f_flags)?;

    add_file_like(
        FanotifyFile::new(flags, event_f_flags)?,
        flags & FAN_CLOEXEC != 0,
    )
    .map(|fd| fd as isize)
}

pub fn sys_fanotify_mark(
    fanotify_fd: c_int,
    flags: u32,
    mask: u64,
    dirfd: c_int,
    pathname: *const c_char,
) -> AxResult<isize> {
    let fanotify = FanotifyFile::from_fd(fanotify_fd)?;
    let loc = if flags & FAN_MARK_FLUSH != 0 {
        None
    } else if pathname.is_null() {
        let file = get_file_like(dirfd)?;
        if flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0 {
            return Err(AxError::InvalidInput);
        }
        file.stat()?;
        Some(location_for_fd(dirfd).ok_or(AxError::InvalidInput)?)
    } else {
        let pathname = vm_load_string(pathname)?;
        let mut resolve_flags = 0;
        if flags & FAN_MARK_DONT_FOLLOW != 0 {
            resolve_flags |= AT_SYMLINK_NOFOLLOW;
        }
        if pathname.is_empty() {
            resolve_flags |= AT_EMPTY_PATH;
        }
        let ResolveAtResult::File(loc) = resolve_at(dirfd, Some(&pathname), resolve_flags)? else {
            return Err(AxError::InvalidInput);
        };
        Some(loc)
    };

    fanotify.mark(flags, mask, loc.as_ref())?;
    Ok(0)
}
