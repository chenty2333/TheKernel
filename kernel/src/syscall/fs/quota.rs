use core::ffi::c_char;

use axerrno::{AxResult, LinuxError};

pub fn sys_quotactl(_cmd: u32, _special: *const c_char, _id: u32, _addr: usize) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_quotactl_fd(_fd: i32, _cmd: u32, _id: u32, _addr: usize) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}
