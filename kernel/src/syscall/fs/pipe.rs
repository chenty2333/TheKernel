use alloc::sync::Arc;
use core::ffi::c_int;

use axerrno::AxResult;
use bitflags::bitflags;
use linux_raw_sys::general::{O_CLOEXEC, O_DIRECT, O_NONBLOCK, O_RDONLY, O_WRONLY};
use starry_vm::VmMutPtr;

use crate::file::{FileLike, Pipe, add_file_like_with_flags, close_file_like};

bitflags! {
    /// Flags for the `pipe2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct PipeFlags: u32 {
        /// Create a pipe with close-on-exec flag.
        const CLOEXEC = O_CLOEXEC;
        /// Create a non-blocking pipe.
        const NONBLOCK = O_NONBLOCK;
        /// Enable packet mode for the pipe. We currently expose the flag via F_GETFL.
        const DIRECT = O_DIRECT;
    }
}

pub fn sys_pipe2(fds: *mut [c_int; 2], flags: u32) -> AxResult<isize> {
    let flags = {
        let new_flags = PipeFlags::from_bits_truncate(flags);
        if new_flags.bits() != flags {
            warn!("sys_pipe2 <= unrecognized flags: {flags}");
        }
        new_flags
    };

    let cloexec = flags.contains(PipeFlags::CLOEXEC);
    let (read_end, write_end) = Pipe::new();
    if flags.contains(PipeFlags::NONBLOCK) {
        read_end.set_nonblocking(true)?;
        write_end.set_nonblocking(true)?;
    }
    let mut read_status = O_RDONLY;
    let mut write_status = O_WRONLY;
    if flags.contains(PipeFlags::NONBLOCK) {
        read_status |= O_NONBLOCK;
        write_status |= O_NONBLOCK;
    }
    if flags.contains(PipeFlags::DIRECT) {
        write_status |= O_DIRECT;
    }

    let read_fd = add_file_like_with_flags(Arc::new(read_end), cloexec, read_status)?;
    let write_fd = add_file_like_with_flags(Arc::new(write_end), cloexec, write_status)
        .inspect_err(|_| close_file_like(read_fd).unwrap())?;

    fds.vm_write([read_fd, write_fd])?;

    debug!(
        "sys_pipe2 <= fds: {:?}, flags: {:?}",
        [read_fd, write_fd],
        flags
    );
    Ok(0)
}
