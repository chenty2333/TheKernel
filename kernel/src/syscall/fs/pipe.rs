use alloc::sync::Arc;
use core::ffi::c_int;

use axerrno::{AxError, AxResult, LinuxError};
use bitflags::bitflags;
use linux_raw_sys::general::{O_CLOEXEC, O_DIRECT, O_EXCL, O_NONBLOCK, O_RDONLY, O_WRONLY};
use tk_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr};

use crate::{
    file::{FileLike, Pipe, add_file_like_with_flags, close_file_like},
    mm::map_usercopy_error,
};

bitflags! {
    /// Flags for the `pipe2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct PipeFlags: u32 {
        /// Create a pipe with close-on-exec flag.
        const CLOEXEC = O_CLOEXEC;
        /// Create a non-blocking pipe.
        const NONBLOCK = O_NONBLOCK;
        /// Request packet mode: writes through the write end become
        /// `PIPE_BUF_FLAG_PACKET` buffers (`fs/pipe.c:507-510`, `:631-634`).
        const DIRECT = O_DIRECT;
        /// `O_NOTIFICATION_PIPE` is an alias of `O_EXCL`
        /// (include/uapi/linux/watch_queue.h), not a distinct bit.
        const NOTIFICATION = O_EXCL;
    }
}

pub fn sys_pipe2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fds: *mut [c_int; 2],
    flags: u32,
) -> AxResult<isize> {
    // fs/pipe.c `__do_pipe_flags()` validates the flag word before anything
    // else happens:
    //     if (flags & ~(O_CLOEXEC | O_NONBLOCK | O_DIRECT |
    //                   O_NOTIFICATION_PIPE))
    //             return -EINVAL;
    let flags = PipeFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;

    // `create_pipe_files()` calls `watch_queue_init()` for O_NOTIFICATION_PIPE;
    // the CONFIG_WATCH_QUEUE=n stub in include/linux/watch_queue.h is
    //     static inline int watch_queue_init(struct pipe_inode_info *pipe)
    //     {
    //             return -ENOPKG;
    //     }
    // so -ENOPKG is the exact verdict for the selected build.
    if flags.contains(PipeFlags::NOTIFICATION) {
        return Err(LinuxError::ENOPKG.into());
    }

    let cloexec = flags.contains(PipeFlags::CLOEXEC);
    let (read_end, write_end) = Pipe::new();
    if flags.contains(PipeFlags::NONBLOCK) {
        read_end.set_nonblocking(true)?;
        write_end.set_nonblocking(true)?;
    }
    // `create_pipe_files()` gives the write end `O_WRONLY | (flags &
    // (O_NONBLOCK | O_DIRECT))` and clones the read end with
    // `O_RDONLY | (flags & O_NONBLOCK)` (fs/pipe.c:1042-1056), so O_DIRECT
    // packetizes writes through the write end only and `F_GETFL` never reports
    // it on the read end.
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
        .inspect_err(|_| {
            let _ = close_file_like(read_fd);
        })?;

    if let Err(err) = VmMutPtr::vm_write(fds, memory, [read_fd, write_fd]) {
        let _ = close_file_like(read_fd);
        let _ = close_file_like(write_fd);
        return Err(map_usercopy_error(err));
    }

    debug!(
        "sys_pipe2 <= fds: {:?}, flags: {:?}",
        [read_fd, write_fd],
        flags
    );
    Ok(0)
}
