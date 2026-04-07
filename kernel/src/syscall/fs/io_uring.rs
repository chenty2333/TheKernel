use axerrno::{AxError, AxResult};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{add_file_like, get_typed_file, io_uring::{IoUringFile, IoUringParams}},
    mm::IoVec,
};

pub fn sys_io_uring_setup(entries: u32, params: *mut IoUringParams) -> AxResult<isize> {
    let ring = IoUringFile::new(entries)?;
    let out = ring.params();
    if let Some(params) = params.nullable() {
        params.vm_write(out)?;
    }
    add_file_like(ring, false).map(|fd| fd as isize)
}

pub fn sys_io_uring_register(fd: i32, opcode: u32, arg: usize, nr_args: u32) -> AxResult<isize> {
    let ring = get_typed_file::<IoUringFile>(fd)?;
    match opcode {
        0 => ring.register_buffers(arg as *const IoVec, nr_args),
        1 => ring.unregister_buffers(),
        _ => Err(AxError::Unsupported),
    }
}

pub fn sys_io_uring_enter(
    fd: i32,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    _sig: usize,
) -> AxResult<isize> {
    let ring = get_typed_file::<IoUringFile>(fd)?;
    ring.enter(to_submit, min_complete, flags)
}
