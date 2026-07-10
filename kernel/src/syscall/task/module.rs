use core::ffi::c_char;

use axerrno::{AxResult, LinuxError};

pub fn sys_delete_module(_name: *const c_char, _flags: u32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_init_module(
    _module_image: *const u8,
    _len: usize,
    _args: *const c_char,
) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_finit_module(_fd: i32, _args: *const c_char, _flags: u32) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}
