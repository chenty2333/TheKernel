use alloc::vec::Vec;
use core::{fmt, time::Duration};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use axtask::{
    current,
    future::{self, block_on, poll_io},
};
use bitmaps::Bitmap;
use linux_raw_sys::{
    general::*,
    select_macros::{FD_ISSET, FD_SET, FD_ZERO},
};
use starry_signal::SignalSet;

use super::FdPollSet;
use crate::{
    file::get_file_like,
    mm::{UserConstPtr, UserPtr, nullable},
    syscall::signal::check_sigset_size,
    task::{AsThread, check_signals},
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
        check_sigset_size(sigmask.sigsetsize)?;
        let set = sigmask.set;
        nullable!(set.get_as_ref())?
    } else {
        None
    };

    let mut readfds = nullable!(readfds.get_as_mut())?;
    let mut writefds = nullable!(writefds.get_as_mut())?;
    let mut exceptfds = nullable!(exceptfds.get_as_mut())?;

    let read_set = FdSet::new(nfds as _, readfds.as_deref());
    let write_set = FdSet::new(nfds as _, writefds.as_deref());
    let except_set = FdSet::new(nfds as _, exceptfds.as_deref());

    debug!(
        "sys_select <= nfds: {nfds} sets: [read: {read_set:?}, write: {write_set:?}, except: \
         {except_set:?}] timeout: {timeout:?}"
    );

    let fd_bitmap = read_set.0 | write_set.0 | except_set.0;
    let fd_count = fd_bitmap.len();
    let mut fds = Vec::with_capacity(fd_count);
    let mut fd_indices = Vec::with_capacity(fd_count);
    for fd in fd_bitmap.into_iter() {
        let f = get_file_like(fd as i32)?;
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, read_set.0.get(fd));
        events.set(IoEvents::OUT, write_set.0.get(fd));
        events.set(IoEvents::ERR, except_set.0.get(fd));
        if !events.is_empty() {
            fds.push((f, events));
            fd_indices.push(fd);
        }
    }

    let fds = FdPollSet(fds);

    if let Some(readfds) = readfds.as_deref_mut() {
        unsafe { FD_ZERO(readfds) };
    }
    if let Some(writefds) = writefds.as_deref_mut() {
        unsafe { FD_ZERO(writefds) };
    }
    if let Some(exceptfds) = exceptfds.as_deref_mut() {
        unsafe { FD_ZERO(exceptfds) };
    }
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut select_once = || {
        block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(axhal::time::wall_time())),
            poll_io(&fds, IoEvents::empty(), false, || {
                let mut res = 0usize;
                for ((fd, interested), index) in fds.0.iter().zip(fd_indices.iter().copied()) {
                    let events = fd.poll() & *interested;
                    if events.contains(IoEvents::IN)
                        && let Some(set) = readfds.as_deref_mut()
                    {
                        res += 1;
                        unsafe { FD_SET(index as _, set) };
                    }
                    if events.contains(IoEvents::OUT)
                        && let Some(set) = writefds.as_deref_mut()
                    {
                        res += 1;
                        unsafe { FD_SET(index as _, set) };
                    }
                    if events.contains(IoEvents::ERR)
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
            }),
        ))
    };

    let Some(sigmask) = sigmask.copied() else {
        return match select_once() {
            Ok(r) => r,
            Err(_) => Ok(0),
        };
    };
    let Some(uctx) = uctx else {
        return Err(AxError::InvalidInput);
    };

    let curr = current();
    let thr = curr.as_thread();
    let old_blocked = thr.signal.set_blocked(sigmask);
    // pselect6() shares ppoll()/sigsuspend() semantics: if a handler runs,
    // the saved userspace return register must already contain -EINTR.
    uctx.set_retval(-LinuxError::EINTR.code() as usize);
    let result = loop {
        match select_once() {
            Ok(Ok(res)) => break Ok(res),
            Ok(Err(AxError::Interrupted)) => {
                let handler_depth = thr.signal_handler_depth();
                if check_signals(thr, uctx, Some(old_blocked)) {
                    if thr.signal_handler_depth() == handler_depth {
                        thr.signal.set_blocked(old_blocked);
                    }
                    break Err(AxError::Interrupted);
                }
            }
            Ok(Err(err)) => break Err(err),
            Err(_) => break Ok(0),
        }
    };
    if !matches!(result, Err(AxError::Interrupted)) {
        thr.signal.set_blocked(old_blocked);
    }
    result
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
