use alloc::vec::Vec;
use core::time::Duration;

use axerrno::{AxError, AxResult};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::UserContext,
};
use axpoll::IoEvents;
use axtask::future::{self, block_on, poll_io};
use linux_raw_sys::general::{POLLNVAL, pollfd, timespec};
use starry_signal::SignalSet;

use super::{FdPollSet, wait_io_result, wait_signal_only};
use crate::{
    file::get_file_like,
    mm::{UserConstPtr, UserPtr, nullable},
    syscall::signal::check_sigset_size,
    time::TimeValueLike,
};

fn do_poll(
    uctx: Option<&mut UserContext>,
    poll_fds: &mut [pollfd],
    timeout: Option<TimeValue>,
    sigmask: Option<SignalSet>,
) -> AxResult<isize> {
    debug!("do_poll fds={poll_fds:?} timeout={timeout:?}");

    let mut res = 0isize;
    let mut fds = Vec::with_capacity(poll_fds.len());
    let mut revents = Vec::with_capacity(poll_fds.len());
    for fd in poll_fds.iter_mut() {
        if fd.fd == -1 {
            // Skip -1
            continue;
        }
        match get_file_like(fd.fd) {
            Ok(f) => {
                fds.push((
                    f,
                    IoEvents::from_bits(fd.events as _).ok_or(AxError::InvalidInput)?
                        | IoEvents::ALWAYS_POLL,
                ));
                revents.push(&mut fd.revents);
            }
            Err(_) => {
                // If the fd is invalid, set revents to POLLNVAL
                fd.revents = POLLNVAL as _;
                res += 1;
            }
        }
    }
    if res > 0 {
        return Ok(res);
    }
    let fds = FdPollSet(fds);
    if fds.0.is_empty() {
        return wait_signal_only(uctx, timeout.map(Duration::from), sigmask);
    }
    let deadline = timeout.map(|dur| wall_time().saturating_add(dur));
    let mut poll_once = || {
        let mut res = 0usize;
        for ((fd, events), revents) in fds.0.iter().zip(revents.iter_mut()) {
            let mut result = fd.poll();
            if result.contains(IoEvents::IN) {
                result |= IoEvents::RDNORM;
            }
            if result.contains(IoEvents::OUT) {
                result |= IoEvents::WRNORM;
            }
            result &= *events;

            **revents = result.bits() as _;
            if **revents != 0 {
                res += 1;
            }
        }

        if res > 0 {
            Ok(res as isize)
        } else {
            Err(AxError::WouldBlock)
        }
    };

    let mut wait_once = || {
        block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(wall_time())),
            poll_io(&fds, IoEvents::empty(), false, &mut poll_once),
        ))
    };

    wait_io_result(uctx, sigmask, &mut wait_once)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_poll(fds: UserPtr<pollfd>, nfds: u32, timeout: i32) -> AxResult<isize> {
    let fds: &mut [pollfd] = if nfds == 0 {
        &mut []
    } else {
        fds.get_as_mut_slice(nfds as usize)?
    };
    let timeout = if timeout < 0 {
        None
    } else {
        Some(TimeValue::from_millis(timeout as u64))
    };
    do_poll(None, fds, timeout, None)
}

pub fn sys_ppoll(
    uctx: &mut UserContext,
    fds: UserPtr<pollfd>,
    nfds: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> AxResult<isize> {
    let sigmask = if let Some(sigmask) = nullable!(sigmask.get_as_ref())? {
        check_sigset_size(sigsetsize)?;
        Some(*sigmask)
    } else {
        None
    };
    let fds: &mut [pollfd] = if nfds == 0 {
        &mut []
    } else {
        fds.get_as_mut_slice(nfds.try_into().map_err(|_| AxError::InvalidInput)?)?
    };
    let timeout = nullable!(timeout.get_as_ref())?
        .map(|ts| ts.try_into_time_value())
        .transpose()?;
    do_poll(Some(uctx), fds, timeout, sigmask)
}
