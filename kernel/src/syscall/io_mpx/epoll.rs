use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use axtask::{
    current,
    future::{self, block_on, poll_io},
};
use bitflags::bitflags;
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, epoll_event, timespec,
};
use starry_signal::SignalSet;

use crate::{
    file::{
        FileLike,
        epoll::{Epoll, EpollEvent, EpollFlags},
    },
    mm::{UserConstPtr, UserPtr, nullable},
    syscall::signal::check_sigset_size,
    task::{AsThread, check_signals},
    time::TimeValueLike,
};

bitflags! {
    /// Flags for the `epoll_create` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollCreateFlags: u32 {
        const CLOEXEC = EPOLL_CLOEXEC;
    }
}

pub fn sys_epoll_create1(flags: u32) -> AxResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");
    Epoll::new()
        .add_to_fd_table(flags.contains(EpollCreateFlags::CLOEXEC))
        .map(|fd| fd as isize)
}

pub fn sys_epoll_ctl(
    epfd: i32,
    op: u32,
    fd: i32,
    event: UserConstPtr<epoll_event>,
) -> AxResult<isize> {
    let epoll = Epoll::from_fd(epfd)?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");

    let parse_event = || -> AxResult<(EpollEvent, EpollFlags)> {
        let event = event.get_as_ref()?;
        let events = IoEvents::from_bits_truncate(event.events);
        let flags =
            EpollFlags::from_bits(event.events & !events.bits()).ok_or(AxError::InvalidInput)?;
        Ok((
            EpollEvent {
                events,
                user_data: event.data,
            },
            flags,
        ))
    };
    match op {
        EPOLL_CTL_ADD => {
            let (event, flags) = parse_event()?;
            epoll.add(fd, event, flags)?;
        }
        EPOLL_CTL_MOD => {
            let (event, flags) = parse_event()?;
            epoll.modify(fd, event, flags)?;
        }
        EPOLL_CTL_DEL => {
            epoll.delete(fd)?;
        }
        _ => return Err(AxError::InvalidInput),
    }
    Ok(0)
}

fn do_epoll_wait(
    uctx: Option<&mut UserContext>,
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: Option<Duration>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> AxResult<isize> {
    check_sigset_size(sigsetsize)?;
    debug!("sys_epoll_wait <= epfd: {epfd}, maxevents: {maxevents}, timeout: {timeout:?}");

    let epoll = Epoll::from_fd(epfd)?;

    if maxevents <= 0 {
        return Err(AxError::InvalidInput);
    }
    let events = events.get_as_mut_slice(maxevents as usize)?;
    let sigmask = nullable!(sigmask.get_as_ref())?.copied();
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        block_on(future::timeout(
            deadline.map(|end| end.saturating_sub(axhal::time::wall_time())),
            poll_io(epoll.as_ref(), IoEvents::IN, false, || {
                epoll.poll_events(events)
            }),
        ))
    };

    let Some(sigmask) = sigmask else {
        return match wait_once() {
            Ok(r) => r.map(|n| n as _),
            Err(_) => Ok(0),
        };
    };
    let Some(uctx) = uctx else {
        return Err(AxError::InvalidInput);
    };

    let curr = current();
    let thr = curr.as_thread();
    let old_blocked = thr.signal.set_blocked(sigmask);
    // epoll_pwait() also resumes through a signal frame when a handler runs;
    // pre-install -EINTR so interrupted waits cannot return stale register
    // contents after sigreturn.
    uctx.set_retval(-LinuxError::EINTR.code() as usize);
    let result = loop {
        match wait_once() {
            Ok(Ok(res)) => break Ok(res as _),
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

pub fn sys_epoll_pwait(
    uctx: &mut UserContext,
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: i32,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> AxResult<isize> {
    let timeout = match timeout {
        -1 => None,
        t if t >= 0 => Some(Duration::from_millis(t as u64)),
        _ => return Err(AxError::InvalidInput),
    };
    do_epoll_wait(
        Some(uctx),
        epfd,
        events,
        maxevents,
        timeout,
        sigmask,
        sigsetsize,
    )
}

pub fn sys_epoll_pwait2(
    uctx: &mut UserContext,
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> AxResult<isize> {
    let timeout = nullable!(timeout.get_as_ref())?
        .map(|ts| ts.try_into_time_value())
        .transpose()?;
    do_epoll_wait(
        Some(uctx),
        epfd,
        events,
        maxevents,
        timeout,
        sigmask,
        sigsetsize,
    )
}
