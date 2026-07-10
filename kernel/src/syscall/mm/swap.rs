use core::ffi::c_char;

use axerrno::{AxResult, LinuxError};

pub fn sys_swapon(_specialfile: *const c_char, _swap_flags: i32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_swapoff(_specialfile: *const c_char) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}
