use core::ffi::{c_char, c_int};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::FsPathBuf;
use linux_raw_sys::general::{
    AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW, CAP_SYS_ADMIN, O_NONBLOCK, O_RDWR,
};

use crate::{
    file::{
        FileLike, ResolveAtResult, add_file_like_with_flags, fanotify::*, get_file_like,
        inotify::location_for_fd, resolve_at,
    },
    mm::{UserMemoryCapability, map_usercopy_error},
    task::AsThread,
};

use crate::file::fanotify::map_mark_reject;
use tk_linux_fsnotify::FAN_MARK_ONLYDIR;

pub fn sys_fanotify_init(flags: u32, event_f_flags: u32) -> AxResult<isize> {
    let thread = axtask::current();
    let thread = thread.as_thread();
    // SYSCALL_DEFINE2(fanotify_init) opens with the capability decision, before
    // any flag is validated:
    //   if (((flags & FANOTIFY_ADMIN_INIT_FLAGS) ||
    //        !(flags & (FANOTIFY_FID_BITS | FAN_REPORT_MNT))) &&
    //       !capable(CAP_SYS_ADMIN))
    //           return -EPERM;
    let may_admin = thread.has_effective_capability(CAP_SYS_ADMIN);
    if !may_admin
        && (flags & tk_linux_fsnotify::FANOTIFY_ADMIN_INIT_FLAGS != 0
            || flags & (tk_linux_fsnotify::FANOTIFY_FID_BITS | tk_linux_fsnotify::FAN_REPORT_MNT)
                == 0)
    {
        return Err(AxError::OperationNotPermitted);
    }
    // The flag table, the FAN_REPORT_MNT combinations, `event_f_flags` and the
    // class switch all report EINVAL.
    tk_linux_fsnotify::fanotify_init_grammar(flags, event_f_flags).map_err(|reject| match reject {
        tk_linux_fsnotify::FanotifyInitReject::Invalid => AxError::InvalidInput,
        tk_linux_fsnotify::FanotifyInitReject::Unsupported => AxError::OperationNotSupported,
    })?;
    // The audit decision is the last check Linux performs:
    //   if (flags & FAN_ENABLE_AUDIT) { if (!capable(CAP_AUDIT_WRITE)) return -EPERM; }
    if flags & tk_linux_fsnotify::FAN_ENABLE_AUDIT != 0
        && !thread.has_effective_capability(linux_raw_sys::general::CAP_AUDIT_WRITE)
    {
        return Err(AxError::OperationNotPermitted);
    }
    // Facilities with no provider in this kernel are well-formed Linux
    // requests and are reported as unsupported rather than invalid.
    if tk_linux_fsnotify::fanotify_init_unsupported(flags) {
        return Err(AxError::OperationNotSupported);
    }
    // An unprivileged listener is admitted, but Linux marks the group
    // FANOTIFY_UNPRIV:
    //   if (!ns_capable_noaudit(&init_user_ns, CAP_SYS_ADMIN)) {
    //           /* Setting the internal flag FANOTIFY_UNPRIV on the group
    //            * prevents setting mount/filesystem marks on this group and
    //            * prevents reporting pid and open fd in events. */
    //           internal_flags |= FANOTIFY_UNPRIV;
    //   }
    // The group's user namespace is the caller's current one and decides the
    // mark-scope gate, while `FANOTIFY_UNPRIV` decides the pid masking; the
    // EPERM rule above already guarantees such a group requested FID or mount
    // reporting, and mount reporting is the unimplemented case refused here.
    let unprivileged = !may_admin;
    let user_ns = thread.current_cred().user_ns().clone();

    add_file_like_with_flags(
        FanotifyFile::new(flags, event_f_flags, user_ns, unprivileged)?,
        flags & FAN_CLOEXEC != 0,
        O_RDWR
            | if flags & FAN_NONBLOCK != 0 {
                O_NONBLOCK
            } else {
                0
            },
    )
    .map(|fd| fd as isize)
}

pub fn sys_fanotify_mark(
    memory: UserMemoryCapability,
    fanotify_fd: c_int,
    flags: u32,
    mask: u64,
    dirfd: c_int,
    pathname: *const c_char,
) -> AxResult<isize> {
    // do_fanotify_mark() rejects every scalar-only violation - upper mask bits,
    // unknown flags, an unknown command, an empty or out-of-range mask and the
    // two conflicting ignore forms - before it looks at the fanotify
    // descriptor, so a bad group fd cannot hide an EINVAL.
    tk_linux_fsnotify::fanotify_mark_scalars(flags, mask).map_err(map_mark_reject)?;
    // fd_empty(f) -> EBADF; f_op != &fanotify_fops -> EINVAL.
    let fanotify = FanotifyFile::from_fd(fanotify_fd)?;
    // Group-dependent admission, and FAN_MARK_FLUSH, which Linux completes
    // before it resolves any path:
    //   if (mark_cmd == FAN_MARK_FLUSH) {
    //           fsnotify_clear_marks_by_group(group, obj_type); return 0; }
    if fanotify.precheck(flags, mask)? {
        fanotify.flush(flags);
        return Ok(0);
    }
    let loc = if pathname.is_null() {
        // fanotify_find_path() with a NULL pathname marks the object its own
        // descriptor refers to.  FAN_MARK_ONLYDIR is checked here, and a
        // non-descriptor such as AT_FDCWD is EBADF because no AT_* handling
        // happens on this path.
        get_file_like(dirfd)?;
        let loc = location_for_fd(dirfd).ok_or(AxError::OperationNotSupported)?;
        if flags & FAN_MARK_ONLYDIR != 0 && !loc.is_dir() {
            return Err(AxError::NotADirectory);
        }
        loc
    } else {
        let pathname = FsPathBuf::from_vec(
            memory
                .load_until_nul(pathname.cast::<u8>())
                .map_err(map_usercopy_error)?,
        );
        let mut resolve_flags = 0;
        if flags & FAN_MARK_DONT_FOLLOW != 0 {
            resolve_flags |= AT_SYMLINK_NOFOLLOW;
        }
        if pathname.as_bytes().is_empty() {
            resolve_flags |= AT_EMPTY_PATH;
        }
        let ResolveAtResult::File(loc) = resolve_at(dirfd, Some(&pathname), resolve_flags)? else {
            return Err(AxError::InvalidInput);
        };
        loc
    };

    fanotify.mark(flags, mask, &loc)?;
    Ok(0)
}
