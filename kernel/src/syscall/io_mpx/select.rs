use alloc::vec::Vec;
use core::{fmt, time::Duration};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use axtask::future::{self, block_on};
use bitmaps::Bitmap;
use linux_raw_sys::{
    general::*,
    select_macros::{FD_ISSET, FD_SET, FD_ZERO},
};
use starry_signal::SignalSet;

use super::{FdPollSet, flatten_blocked_timeout, wait_io_result, wait_signal_only};
use crate::{
    file::get_file_like,
    mm::{UserConstPtr, UserPtr, nullable},
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
    let sigmask = if let Some(sigmask) = nullable!(sigmask.get_as_ref())? {
        let set = sigmask.set;
        if let Some(set) = nullable!(set.get_as_ref())? {
            check_sigset_size(sigmask.sigsetsize)?;
            Some(set)
        } else {
            None
        }
    } else {
        None
    };

    let mut readfds = if nfds == 0 {
        None
    } else {
        nullable!(readfds.get_as_mut())?
    };
    let mut writefds = if nfds == 0 {
        None
    } else {
        nullable!(writefds.get_as_mut())?
    };
    let mut exceptfds = if nfds == 0 {
        None
    } else {
        nullable!(exceptfds.get_as_mut())?
    };

    let read_set = FdSet::new(nfds as _, readfds.as_deref());
    let write_set = FdSet::new(nfds as _, writefds.as_deref());
    let except_set = FdSet::new(nfds as _, exceptfds.as_deref());

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
    if fds.is_empty() {
        return wait_signal_only(uctx, timeout, sigmask.copied());
    }

    if let Some(readfds) = readfds.as_deref_mut() {
        unsafe { FD_ZERO(readfds) };
    }
    if let Some(writefds) = writefds.as_deref_mut() {
        unsafe { FD_ZERO(writefds) };
    }
    if let Some(exceptfds) = exceptfds.as_deref_mut() {
        unsafe { FD_ZERO(exceptfds) };
    }
    let mut poll_once = || {
        let mut res = 0usize;
        for entry in fds.entries() {
            let index = fd_indices[entry.output_index];
            let events = select_ready_events(entry.file.is_path_only(), entry.events, || {
                entry.file.poll()
            });
            if events.contains(IoEvents::READABLE)
                && let Some(set) = readfds.as_deref_mut()
            {
                res += 1;
                unsafe { FD_SET(index as _, set) };
            }
            if events.contains(IoEvents::WRITABLE)
                && let Some(set) = writefds.as_deref_mut()
            {
                res += 1;
                unsafe { FD_SET(index as _, set) };
            }
            if events.contains(IoEvents::ERROR)
                && let Some(set) = exceptfds.as_deref_mut()
            {
                res += 1;
                unsafe { FD_SET(index as _, set) };
            }
        }
        if res > 0 {
            return Ok(res as _);
        }

        Err(AxError::WouldBlock)
    };

    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut select_once = || {
        flatten_blocked_timeout(block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(axhal::time::wall_time())),
            async {
                crate::readiness::interruptible_poll_io(
                    &fds,
                    IoEvents::empty(),
                    false,
                    &mut poll_once,
                )
                .await
            },
        )))
    };

    wait_io_result(uctx, sigmask.copied(), &mut select_once)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_select(
    nfds: u32,
    readfds: UserPtr<__kernel_fd_set>,
    writefds: UserPtr<__kernel_fd_set>,
    exceptfds: UserPtr<__kernel_fd_set>,
    timeout: UserConstPtr<timeval>,
) -> AxResult<isize> {
    do_select(
        None,
        nfds,
        readfds,
        writefds,
        exceptfds,
        nullable!(timeout.get_as_ref())?
            .map(|it| it.try_into_time_value())
            .transpose()?,
        0.into(),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SignalSetWithSize {
    set: UserConstPtr<SignalSet>,
    sigsetsize: usize,
}

pub fn sys_pselect6(
    uctx: &mut UserContext,
    nfds: u32,
    readfds: UserPtr<__kernel_fd_set>,
    writefds: UserPtr<__kernel_fd_set>,
    exceptfds: UserPtr<__kernel_fd_set>,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> AxResult<isize> {
    do_select(
        Some(uctx),
        nfds,
        readfds,
        writefds,
        exceptfds,
        nullable!(timeout.get_as_ref())?
            .map(|ts| ts.try_into_time_value())
            .transpose()?,
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
