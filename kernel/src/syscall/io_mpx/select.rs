use alloc::vec::Vec;
use core::{fmt, mem::MaybeUninit, slice, time::Duration};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use bitmaps::Bitmap;
use linux_raw_sys::{
    general::*,
    select_macros::{FD_ISSET, FD_SET, FD_ZERO},
};
use thekernel_linux_signal::SignalSet;

use super::{FdPollSet, wait_io_result, wait_signal_only};
use crate::{
    file::get_file_like,
    mm::{UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
    syscall::signal::check_sigset_size,
    time::TimeValueLike,
};

struct FdSet(Bitmap<{ __FD_SETSIZE as usize }>);

impl FdSet {
    fn new(nfds: usize, fds: Option<&__kernel_fd_set>) -> Self {
        let mut bitmap = Bitmap::new();
        if let Some(fds) = fds {
            for i in 0..nfds {
                if unsafe { FD_ISSET(i as _, fds) } {
                    bitmap.set(i, true);
                }
            }
        }
        Self(bitmap)
    }
}

impl fmt::Debug for FdSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.0).finish()
    }
}

fn read_user_value<T>(caller: &UserMemoryCapability, address: usize) -> AxResult<T> {
    let value = caller
        .read_value_uninit(address as *const T)
        .map_err(map_usercopy_error)?;
    // SAFETY: the explicit usercopy initialized the complete value before it
    // is exposed to the kernel. The syscall mirror types used here contain
    // only integer fields or opaque user pointers.
    Ok(unsafe { value.assume_init() })
}

fn snapshot_fd_set(
    caller: &UserMemoryCapability,
    fds: UserPtr<__kernel_fd_set>,
    nfds: u32,
) -> AxResult<Option<__kernel_fd_set>> {
    if nfds == 0 || fds.is_null() {
        return Ok(None);
    }
    Ok(Some(read_user_value(caller, fds.address().as_usize())?))
}

fn copy_fd_set(
    caller: &UserMemoryCapability,
    destination: UserPtr<__kernel_fd_set>,
    bitmap: Bitmap<{ __FD_SETSIZE as usize }>,
) -> AxResult<()> {
    if destination.is_null() {
        return Ok(());
    }

    // Build a fully initialized kernel-owned mirror, then copy its bytes. No
    // user pointer is ever converted to a Rust reference and fd_set padding
    // is deterministically zeroed rather than copied from uninitialized data.
    let mut set = unsafe { MaybeUninit::<__kernel_fd_set>::zeroed().assume_init() };
    unsafe { FD_ZERO(&mut set) };
    for fd in &bitmap {
        unsafe { FD_SET(fd as _, &mut set) };
    }
    let bytes = unsafe {
        slice::from_raw_parts(
            (&set as *const __kernel_fd_set).cast::<u8>(),
            core::mem::size_of::<__kernel_fd_set>(),
        )
    };
    caller
        .write_bytes(destination.address().as_usize(), bytes)
        .map_err(map_usercopy_error)
}

fn select_ready_events(
    path_only: bool,
    interested: IoEvents,
    poll: impl FnOnce() -> IoEvents,
) -> IoEvents {
    if path_only {
        // Linux select is intentionally unlike poll here: O_PATH reports
        // every requested read/write/exception class ready. In particular,
        // do not call through to the pathname handle's inner poll callback.
        interested
    } else {
        poll() & interested
    }
}

fn do_select(
    caller: &UserMemoryCapability,
    uctx: Option<&mut UserContext>,
    nfds: u32,
    readfds: UserPtr<__kernel_fd_set>,
    writefds: UserPtr<__kernel_fd_set>,
    exceptfds: UserPtr<__kernel_fd_set>,
    timeout: Option<Duration>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> AxResult<isize> {
    if nfds > __FD_SETSIZE {
        return Err(AxError::InvalidInput);
    }
    let sigmask = if sigmask.is_null() {
        None
    } else {
        let sigmask: SignalSetWithSize = read_user_value(caller, sigmask.address().as_usize())?;
        if sigmask.set.is_null() {
            None
        } else {
            // As with ppoll, size validation must precede copying the actual
            // mask so an invalid size takes precedence over EFAULT.
            check_sigset_size(sigmask.sigsetsize)?;
            Some(read_user_value(caller, sigmask.set.address().as_usize())?)
        }
    };

    let readfds_snapshot = snapshot_fd_set(caller, readfds, nfds)?;
    let writefds_snapshot = snapshot_fd_set(caller, writefds, nfds)?;
    let exceptfds_snapshot = snapshot_fd_set(caller, exceptfds, nfds)?;

    let read_set = FdSet::new(nfds as _, readfds_snapshot.as_ref());
    let write_set = FdSet::new(nfds as _, writefds_snapshot.as_ref());
    let except_set = FdSet::new(nfds as _, exceptfds_snapshot.as_ref());

    debug!(
        "sys_select <= nfds: {nfds} sets: [read: {read_set:?}, write: {write_set:?}, except: \
         {except_set:?}] timeout: {timeout:?}"
    );

    let fd_bitmap = read_set.0 | write_set.0 | except_set.0;
    let fd_count = fd_bitmap.len();
    let mut fds = FdPollSet::try_with_capacity(fd_count)?;
    let mut fd_indices = Vec::new();
    fd_indices
        .try_reserve_exact(fd_count)
        .map_err(|_| AxError::NoMemory)?;
    for fd in fd_bitmap.into_iter() {
        let f = get_file_like(fd as i32)?;
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, read_set.0.get(fd));
        events.set(IoEvents::WRITABLE, write_set.0.get(fd));
        events.set(IoEvents::ERROR, except_set.0.get(fd));
        if !events.is_empty() {
            fds.push(f, events);
            fd_indices.push(fd);
        }
    }

    let fds = fds.finish();
    let mut ready_read = Bitmap::new();
    let mut ready_write = Bitmap::new();
    let mut ready_except = Bitmap::new();
    let mut poll_once = || {
        ready_read = Bitmap::new();
        ready_write = Bitmap::new();
        ready_except = Bitmap::new();
        let mut res = 0usize;
        for entry in fds.entries() {
            let index = fd_indices[entry.output_index];
            let events = select_ready_events(entry.file.is_path_only(), entry.events, || {
                entry.file.poll()
            });
            if events.contains(IoEvents::READABLE) && !readfds.is_null() {
                res += 1;
                ready_read.set(index, true);
            }
            if events.contains(IoEvents::WRITABLE) && !writefds.is_null() {
                res += 1;
                ready_write.set(index, true);
            }
            if events.contains(IoEvents::ERROR) && !exceptfds.is_null() {
                res += 1;
                ready_except.set(index, true);
            }
        }
        if res > 0 {
            return Ok(res as _);
        }

        Err(AxError::WouldBlock)
    };

    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut select_once = || {
        crate::readiness::block_on_poll_io_until(
            &fds,
            IoEvents::empty(),
            false,
            deadline,
            &mut poll_once,
        )
    };

    let result = if fds.is_empty() {
        wait_signal_only(uctx, timeout, sigmask)
    } else {
        wait_io_result(uctx, sigmask, &mut select_once)
    };
    match result {
        Ok(result) => {
            copy_fd_set(caller, readfds, ready_read)?;
            copy_fd_set(caller, writefds, ready_write)?;
            copy_fd_set(caller, exceptfds, ready_except)?;
            Ok(result)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_arch = "x86_64")]
pub fn sys_select(
    caller: UserMemoryCapability,
    nfds: u32,
    readfds: UserPtr<__kernel_fd_set>,
    writefds: UserPtr<__kernel_fd_set>,
    exceptfds: UserPtr<__kernel_fd_set>,
    timeout: UserConstPtr<timeval>,
) -> AxResult<isize> {
    do_select(
        &caller,
        None,
        nfds,
        readfds,
        writefds,
        exceptfds,
        if timeout.is_null() {
            None
        } else {
            Some(
                read_user_value::<timeval>(&caller, timeout.address().as_usize())?
                    .try_into_time_value()?,
            )
        },
        UserConstPtr::default(),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SignalSetWithSize {
    set: UserConstPtr<SignalSet>,
    sigsetsize: usize,
}

pub fn sys_pselect6(
    caller: UserMemoryCapability,
    uctx: &mut UserContext,
    nfds: u32,
    readfds: UserPtr<__kernel_fd_set>,
    writefds: UserPtr<__kernel_fd_set>,
    exceptfds: UserPtr<__kernel_fd_set>,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> AxResult<isize> {
    do_select(
        &caller,
        Some(uctx),
        nfds,
        readfds,
        writefds,
        exceptfds,
        if timeout.is_null() {
            None
        } else {
            Some(
                read_user_value::<timespec>(&caller, timeout.address().as_usize())?
                    .try_into_time_value()?,
            )
        },
        sigmask,
    )
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn opath_select_keeps_all_requested_classes_ready() {
        let interested = IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR;
        let poll_calls = Cell::new(0);
        assert_eq!(
            select_ready_events(true, interested, || {
                poll_calls.set(poll_calls.get() + 1);
                IoEvents::empty()
            }),
            interested
        );
        assert_eq!(poll_calls.get(), 0);

        assert_eq!(
            select_ready_events(false, interested, || {
                poll_calls.set(poll_calls.get() + 1);
                IoEvents::READABLE | IoEvents::ERROR
            }),
            IoEvents::READABLE | IoEvents::ERROR
        );
        assert_eq!(poll_calls.get(), 1);
    }
}
