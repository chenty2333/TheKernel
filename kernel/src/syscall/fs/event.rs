use axerrno::{AxError, AxResult};
use bitflags::bitflags;
use linux_raw_sys::general::{EFD_CLOEXEC, EFD_NONBLOCK, EFD_SEMAPHORE, O_NONBLOCK, O_RDWR};

use crate::file::{FileLike, add_file_like_with_flags, event::EventFd};

bitflags! {
    /// Flags for the `eventfd2` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EventFdFlags: u32 {
        /// Create a file descriptor that is closed on `exec`.
        const CLOEXEC = EFD_CLOEXEC;
        /// Create a non-blocking eventfd.
        const NONBLOCK = EFD_NONBLOCK;
        /// Create a semaphore eventfd.
        const SEMAPHORE = EFD_SEMAPHORE;
    }
}

pub fn sys_eventfd2(initval: u32, flags: u32) -> AxResult<isize> {
    debug!("sys_eventfd2 <= initval: {initval}, flags: {flags}");

    let flags = EventFdFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    let event_fd = EventFd::new(initval as _, flags.contains(EventFdFlags::SEMAPHORE));
    event_fd.set_nonblocking(flags.contains(EventFdFlags::NONBLOCK))?;
    // Generic read/write dispatch snapshots the OFD flags, so publishing only
    // the backend's nonblocking bit would turn an empty nonblocking read into
    // a blocking wait.
    let status_flags = O_RDWR
        | if flags.contains(EventFdFlags::NONBLOCK) {
            O_NONBLOCK
        } else {
            0
        };
    add_file_like_with_flags(
        event_fd as _,
        flags.contains(EventFdFlags::CLOEXEC),
        status_flags,
    )
    .map(|fd| fd as _)
}
