use alloc::sync::Arc;

use axerrno::AxResult;
use axtask::current;
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK, O_RDONLY};
use thekernel_linux_mm::{UFFD_O_CLOEXEC, UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY, UffdCreateFlags};

use crate::{
    file::{FileDescription, reserve_fd, userfaultfd::UserfaultFile},
    mm::{AddrSpace, uffd_policy_error},
    task::AsThread,
};

const _: () = {
    assert!(UFFD_O_NONBLOCK == O_NONBLOCK);
    assert!(UFFD_O_CLOEXEC == O_CLOEXEC);
};

const fn userfaultfd_status_flags(flags: UffdCreateFlags) -> u32 {
    O_RDONLY | if flags.nonblocking() { O_NONBLOCK } else { 0 }
}

fn checked_userfaultfd_flags(flags: i32) -> AxResult<UffdCreateFlags> {
    let flags = flags as u32;
    // Linux performs the unprivileged USER_MODE_ONLY permission gate before
    // validating the remaining namespace. Under this bounded unprivileged
    // profile, an unknown bit without USER_MODE_ONLY is therefore EPERM, while
    // USER_MODE_ONLY combined with an unknown bit is EINVAL.
    if flags & UFFD_USER_MODE_ONLY == 0 {
        return Err(axerrno::AxError::OperationNotPermitted);
    }
    UffdCreateFlags::from_bits(flags)
        .and_then(UffdCreateFlags::validate_profile)
        .map_err(uffd_policy_error)
}

fn prepare_userfaultfd_description(
    aspace: Arc<axsync::Mutex<AddrSpace>>,
    flags: UffdCreateFlags,
) -> AxResult<Arc<FileDescription>> {
    let file = UserfaultFile::try_new(aspace, flags)?;
    FileDescription::new_with_flags(file, userfaultfd_status_flags(flags))
}

pub fn sys_userfaultfd(raw_flags: i32) -> AxResult<isize> {
    let flags = checked_userfaultfd_flags(raw_flags)?;

    // Reserve the process-visible name before attaching a handler to the
    // address space. EMFILE therefore has no userfaultfd/MM side effect. Every
    // later fallible owner is unpublished and rolls back through Drop.
    let reservation = reserve_fd(flags.close_on_exec())?;
    let aspace = current().as_thread().proc_data.aspace();
    let description = prepare_userfaultfd_description(aspace, flags)?;
    let publication = reservation.prepare_publication(description)?;

    // Descriptor accounting and exact-table admission are complete. This is
    // the only visibility transition and is infallible.
    Ok(publication.commit() as isize)
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::O_ACCMODE;

    use super::*;

    #[test]
    fn status_flags_match_linux_read_only_ofd() {
        let blocking = UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY).unwrap();
        assert_eq!(userfaultfd_status_flags(blocking) & O_ACCMODE, O_RDONLY);
        assert_eq!(userfaultfd_status_flags(blocking) & O_NONBLOCK, 0);

        let nonblocking =
            UffdCreateFlags::from_bits(UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK).unwrap();
        assert_eq!(userfaultfd_status_flags(nonblocking), O_RDONLY | O_NONBLOCK);
    }

    #[test]
    fn profile_gate_distinguishes_permission_from_unknown_bits() {
        assert_eq!(
            checked_userfaultfd_flags(0),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            checked_userfaultfd_flags(2),
            Err(AxError::OperationNotPermitted)
        );
        assert_eq!(
            checked_userfaultfd_flags((UFFD_USER_MODE_ONLY | 2) as i32),
            Err(AxError::InvalidInput)
        );

        let flags = checked_userfaultfd_flags(
            (UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK | UFFD_O_CLOEXEC) as i32,
        )
        .unwrap();
        assert!(flags.user_mode_only());
        assert!(flags.nonblocking());
        assert!(flags.close_on_exec());
    }
}
