use axerrno::{AxResult, LinuxError};

mod uapi;

pub fn sys_io_uring_setup(_entries: u32, _params: *mut ()) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_io_uring_register(
    _fd: i32,
    _opcode: u32,
    _arg: usize,
    _nr_args: u32,
) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}

pub fn sys_io_uring_enter(
    _fd: i32,
    _to_submit: u32,
    _min_complete: u32,
    _flags: u32,
    _sig: usize,
    _argsz: usize,
) -> AxResult<isize> {
    Err(LinuxError::ENOSYS.into())
}
