use alloc::sync::Arc;
use core::{mem::size_of, time::Duration};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use bitflags::bitflags;
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLET, EPOLLONESHOT, epoll_event,
    timespec,
};
use starry_signal::SignalSet;
use starry_vm::VmMutPtr;

use super::{io_to_linux_epoll, linux_epoll_events, wait_io_result};
use crate::{
    file::{
        FileLike,
        epoll::{Epoll, EpollEvent, EpollFlags},
        get_file_description,
    },
    mm::{UserConstPtr, UserPtr, nullable},
    syscall::signal::check_sigset_size,
    time::TimeValueLike,
};

bitflags! {
    /// Flags for the `epoll_create` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollCreateFlags: u32 {
        const CLOEXEC = EPOLL_CLOEXEC;
    }
}

fn checked_epoll_event_ptr(base: usize, index: usize) -> AxResult<*mut epoll_event> {
    let offset = index
        .checked_mul(size_of::<epoll_event>())
        .ok_or(AxError::BadAddress)?;
    base.checked_add(offset)
        .map(|address| address as *mut epoll_event)
        .ok_or(AxError::BadAddress)
}

fn check_epoll_target(path_only: bool) -> AxResult<()> {
    if path_only {
        Err(AxError::BadFileDescriptor)
    } else {
        Ok(())
    }
}

pub fn sys_epoll_create1(flags: u32) -> AxResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");
    Epoll::new()?
        .add_to_fd_table(flags.contains(EpollCreateFlags::CLOEXEC))
        .map(|fd| fd as isize)
}

pub fn sys_epoll_ctl(
    epfd: i32,
    op: u32,
    fd: i32,
    event: UserConstPtr<epoll_event>,
) -> AxResult<isize> {
    let epoll_description = get_file_description(epfd)?;
    let target = get_file_description(fd)?;
    if Arc::ptr_eq(&epoll_description, &target) {
        return Err(AxError::InvalidInput);
    }
    let epoll = epoll_description
        .inner
        .clone()
        .downcast_arc::<Epoll>()
        .map_err(|_| AxError::InvalidInput)?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");
    // Linux rejects O_PATH as a non-pollable target before interpreting the
    // control operation; ADD, MOD, DEL, and even an unknown op report EBADF.
    check_epoll_target(target.is_path_only())?;

    let parse_event = || -> AxResult<(EpollEvent, EpollFlags)> {
        let event = event.get_as_ref()?;
        let flag_bits = event.events & (EPOLLET | EPOLLONESHOT);
        let events = linux_epoll_events(event.events & !flag_bits)?;
        let flags = EpollFlags::from_bits(flag_bits).ok_or(AxError::InvalidInput)?;
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
            let (mut event, flags) = parse_event()?;
            event.events |= IoEvents::ALWAYS;
            epoll.add(fd, event, flags)?;
        }
        EPOLL_CTL_MOD => {
            let (mut event, flags) = parse_event()?;
            event.events |= IoEvents::ALWAYS;
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
    let sigmask = nullable!(sigmask.get_as_ref())?.copied();
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        crate::readiness::block_on_poll_io_until(
            epoll.as_ref(),
            IoEvents::READABLE,
            false,
            deadline,
            || {
                let batch = epoll.prepare_events(maxevents as usize)?;
                let events_base = events.address().as_usize();
                let mut copied = 0;
                while copied < batch.len() {
                    let Some(source) = batch.event(copied) else {
                        return if copied == 0 {
                            Err(AxError::BadState)
                        } else {
                            Ok(batch.complete_prefix(copied))
                        };
                    };
                    let event = epoll_event {
                        events: io_to_linux_epoll(source.events),
                        data: source.user_data,
                    };
                    let copy_result = checked_epoll_event_ptr(events_base, copied)
                        .and_then(|destination| destination.vm_write(event).map_err(AxError::from));
                    if let Err(error) = copy_result {
                        return if copied == 0 {
                            let _ = batch.complete_prefix(0);
                            Err(error)
                        } else {
                            Ok(batch.complete_prefix(copied))
                        };
                    }
                    copied += 1;
                }
                Ok(batch.complete_prefix(copied))
            },
        )
        .map(|result| result.map(|count| count as isize))
    };

    wait_io_result(uctx, sigmask, &mut wait_once)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoll_copyout_address_arithmetic_never_wraps() {
        let event_size = size_of::<epoll_event>();
        assert_eq!(
            checked_epoll_event_ptr(usize::MAX - event_size, 1).unwrap() as usize,
            usize::MAX
        );
        assert_eq!(
            checked_epoll_event_ptr(usize::MAX - event_size + 1, 1),
            Err(AxError::BadAddress)
        );
        assert_eq!(
            checked_epoll_event_ptr(0, usize::MAX),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn epoll_all_control_operations_reject_opath_targets() {
        assert_eq!(check_epoll_target(true), Err(AxError::BadFileDescriptor));
        assert_eq!(check_epoll_target(false), Ok(()));
    }
}
