use core::mem::size_of;

use axerrno::{AxError, AxResult};
use bitflags::bitflags;
use linux_raw_sys::general::{O_CLOEXEC, O_NONBLOCK, O_RDWR};
use thekernel_linux_signal::SignalSet;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmPtr};

use crate::{
    file::{FileLike, add_file_like_with_flags, signalfd::Signalfd},
    mm::map_usercopy_error,
};

// SFD flag definitions (if not available in linux_raw_sys)
const SFD_CLOEXEC: u32 = O_CLOEXEC;
const SFD_NONBLOCK: u32 = O_NONBLOCK;

fn check_signalfd_sigset_size(size: usize) -> AxResult<()> {
    if size != size_of::<SignalSet>() {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

bitflags! {
    /// Flags for the `signalfd4` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct SignalfdFlags: u32 {
        /// Create a file descriptor that is closed on `exec`.
        const CLOEXEC = SFD_CLOEXEC;
        /// Create a non-blocking signalfd.
        const NONBLOCK = SFD_NONBLOCK;
    }
}

/// signalfd4 system call
///
/// Creates a file descriptor that can be used to accept signals targeted at
/// the caller. This provides an alternative to the use of a signal handler or
/// sigwaitinfo(2), and has the advantage that the file descriptor may be
/// monitored by select(2), poll(2), and epoll(7).
///
/// # Arguments
/// * `fd` - If `fd` is -1, then a new file descriptor is created. Otherwise,
///   `fd` must specify a valid existing signalfd file descriptor.
/// * `mask` - Pointer to a signal set (sigset_t).
/// * `sigsetsize` - The size (in bytes) of the mask pointed to by `mask`.
/// * `flags` - Flags to control the operation.
pub fn sys_signalfd4<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    mask: *const SignalSet,
    sigsetsize: usize,
    flags: u32,
) -> AxResult<isize> {
    check_signalfd_sigset_size(sigsetsize)?;

    // Read the signal mask from user space before handling the request mode.
    let mask = unsafe {
        VmPtr::vm_read_uninit(mask, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };

    let flags = SignalfdFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;

    // If fd is not -1, we should modify the existing signalfd
    if fd != -1 {
        let signalfd = Signalfd::from_fd(fd)?;
        signalfd.update_mask(mask);
        // Linux applies creation flags only when allocating a new descriptor.
        return Ok(fd as _);
    }

    // Create a new Signalfd
    let signalfd = Signalfd::new(mask);
    signalfd.set_nonblocking(flags.contains(SignalfdFlags::NONBLOCK))?;

    // Add to file descriptor table
    let status_flags = O_RDWR | (flags.bits() & O_NONBLOCK);
    add_file_like_with_flags(
        signalfd as _,
        flags.contains(SignalfdFlags::CLOEXEC),
        status_flags,
    )
    .map(|fd| fd as _)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigset_size_must_match_kernel_signal_set() {
        assert!(check_signalfd_sigset_size(0).is_err());
        assert!(check_signalfd_sigset_size(size_of::<SignalSet>()).is_ok());
        assert!(check_signalfd_sigset_size(size_of::<SignalSet>() + 1).is_err());
    }
}
