use axerrno::{AxResult, LinuxError};

pub fn sys_userfaultfd(_flags: i32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}
