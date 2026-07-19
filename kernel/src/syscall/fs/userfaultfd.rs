use alloc::sync::Arc;

use axerrno::{AxResult, LinuxError};
use linux_raw_sys::general::{O_NONBLOCK, O_RDONLY};
use thekernel_linux_mm::UffdCreateFlags;

use crate::{
    file::{FileDescription, userfaultfd::UserfaultFile},
    mm::{AddrSpace, uffd_policy_error},
};

const fn userfaultfd_status_flags(flags: UffdCreateFlags) -> u32 {
    O_RDONLY | if flags.nonblocking() { O_NONBLOCK } else { 0 }
}

fn checked_userfaultfd_flags(flags: i32) -> AxResult<UffdCreateFlags> {
    UffdCreateFlags::from_bits(flags as u32)
        .and_then(UffdCreateFlags::validate_profile)
        .map_err(uffd_policy_error)
}

/// Unpublished ownership prepared for the eventual syscall visibility commit.
///
/// Keeping fd reservation out of this dormant helper makes it impossible for
/// an internal caller to expose a partially implemented object accidentally.
#[allow(dead_code)]
pub(crate) struct PreparedUserfaultfd {
    pub(crate) description: Arc<FileDescription>,
    pub(crate) close_on_exec: bool,
}

#[allow(dead_code)]
pub(crate) fn prepare_userfaultfd(
    aspace: Arc<axsync::Mutex<AddrSpace>>,
    raw_flags: i32,
) -> AxResult<PreparedUserfaultfd> {
    let flags = checked_userfaultfd_flags(raw_flags)?;
    let file = UserfaultFile::try_new(aspace, flags)?;
    let description = FileDescription::new_with_flags(file, userfaultfd_status_flags(flags))?;
    Ok(PreparedUserfaultfd {
        description,
        close_on_exec: flags.close_on_exec(),
    })
}

pub fn sys_userfaultfd(_flags: i32) -> AxResult<isize> {
    // Do not reserve or publish an fd until REGISTER, resolver ioctls, fault
    // waiting, lifecycle races, and cross-architecture guest contracts close.
    Err(LinuxError::ENOSYS.into())
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;
    use linux_raw_sys::general::O_ACCMODE;
    use thekernel_linux_mm::{UFFD_O_CLOEXEC, UFFD_O_NONBLOCK, UFFD_USER_MODE_ONLY};

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
        assert_eq!(checked_userfaultfd_flags(2), Err(AxError::InvalidInput));

        let flags = checked_userfaultfd_flags(
            (UFFD_USER_MODE_ONLY | UFFD_O_NONBLOCK | UFFD_O_CLOEXEC) as i32,
        )
        .unwrap();
        assert!(flags.user_mode_only());
        assert!(flags.nonblocking());
        assert!(flags.close_on_exec());
    }

    #[test]
    fn public_entry_stays_unconditionally_dormant() {
        let enosys: AxError = LinuxError::ENOSYS.into();
        assert_eq!(sys_userfaultfd(0), Err(enosys));
        assert_eq!(sys_userfaultfd(UFFD_USER_MODE_ONLY as i32), Err(enosys));
    }
}
