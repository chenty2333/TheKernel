use alloc::sync::Arc;
use core::{
    mem::{align_of, offset_of, size_of},
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axpoll::IoEvents;
use bitflags::bitflags;
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLET, EPOLLONESHOT, epoll_event,
    timespec,
};
use thekernel_linux_signal::SignalSet;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use super::{io_to_linux_epoll, linux_epoll_events, wait_io_result};
use crate::{
    file::{
        FileLike,
        epoll::{Epoll, EpollEvent, EpollFlags},
        get_file_description,
    },
    mm::map_usercopy_error,
    syscall::signal::check_sigset_size,
    time::TimeValueLike,
};

// `linux_raw_sys` does not expose bytemuck's object-representation markers for
// these generated ABI structs. The x86_64 Linux layouts are integer-only and
// checked here before the explicit usercopy unchecked path is used.
const _: () = {
    assert!(align_of::<epoll_event>() == 1);
    assert!(size_of::<epoll_event>() == 12);
    assert!(offset_of!(epoll_event, events) == 0);
    assert!(offset_of!(epoll_event, data) == 4);
    assert!(align_of::<timespec>() == 8);
    assert!(size_of::<timespec>() == 16);
    assert!(offset_of!(timespec, tv_sec) == 0);
    assert!(offset_of!(timespec, tv_nsec) == 8);
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

fn epoll_timeout(timeout_ms: i32) -> Option<Duration> {
    (timeout_ms >= 0).then(|| Duration::from_millis(timeout_ms as u64))
}

fn read_epoll_event<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    event: *const epoll_event,
) -> AxResult<epoll_event> {
    let value = unsafe {
        VmPtr::vm_read_uninit(event, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    // SAFETY: the explicit provider initialized every byte, and epoll_event
    // contains only integer fields in the checked packed x86_64 ABI.
    Ok(value)
}

pub fn sys_epoll_create1(flags: u32) -> AxResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");
    Epoll::new()?
        .add_to_fd_table(flags.contains(EpollCreateFlags::CLOEXEC))
        .map(|fd| fd as isize)
}

pub fn sys_epoll_ctl<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    epfd: i32,
    op: u32,
    fd: i32,
    event: *const epoll_event,
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

    let mut parse_event = || -> AxResult<(EpollEvent, EpollFlags)> {
        let event = read_epoll_event(memory, event)?;
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

fn do_epoll_wait<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: Option<&mut UserContext>,
    epfd: i32,
    events: *mut epoll_event,
    maxevents: i32,
    timeout: Option<Duration>,
    sigmask: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    let sigmask = if sigmask.is_null() {
        None
    } else {
        // Linux ignores sigsetsize when no temporary mask is supplied. For a
        // present mask, reject a bad size before touching the user pointer so
        // EINVAL wins over a possible EFAULT from the copyin.
        check_sigset_size(sigsetsize)?;
        let value = unsafe {
            VmPtr::vm_read_uninit(sigmask, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        // SAFETY: the explicit provider initialized the complete signal-set
        // representation; SignalSet is an integer-backed mask.
        Some(value)
    };
    debug!("sys_epoll_wait <= epfd: {epfd}, maxevents: {maxevents}, timeout: {timeout:?}");

    let epoll = Epoll::from_fd(epfd)?;

    if maxevents <= 0 {
        return Err(AxError::InvalidInput);
    }
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        crate::readiness::block_on_poll_io_until(
            epoll.as_ref(),
            IoEvents::READABLE,
            false,
            deadline,
            || {
                let batch = epoll.prepare_events(maxevents as usize)?;
                let events_base = events as usize;
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
                    let copy_result =
                        checked_epoll_event_ptr(events_base, copied).and_then(|destination| {
                            // SAFETY: epoll_event has no padding in the
                            // checked packed x86_64 ABI, and all fields are
                            // initialized before this copyout.
                            unsafe { VmMutPtr::vm_write_unchecked(destination, memory, event) }
                                .map_err(map_usercopy_error)
                        });
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

pub fn sys_epoll_pwait<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    epfd: i32,
    events: *mut epoll_event,
    maxevents: i32,
    timeout: i32,
    sigmask: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    let timeout = epoll_timeout(timeout);
    do_epoll_wait(
        memory,
        Some(uctx),
        epfd,
        events,
        maxevents,
        timeout,
        sigmask,
        sigsetsize,
    )
}

#[cfg(target_arch = "x86_64")]
pub fn sys_epoll_wait<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    epfd: i32,
    events: *mut epoll_event,
    maxevents: i32,
    timeout: i32,
) -> AxResult<isize> {
    let timeout = epoll_timeout(timeout);
    do_epoll_wait(
        memory,
        None,
        epfd,
        events,
        maxevents,
        timeout,
        core::ptr::null(),
        0,
    )
}

pub fn sys_epoll_pwait2<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    uctx: &mut UserContext,
    epfd: i32,
    events: *mut epoll_event,
    maxevents: i32,
    timeout: *const timespec,
    sigmask: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    let timeout = if timeout.is_null() {
        None
    } else {
        let value = unsafe {
            VmPtr::vm_read_uninit(timeout, memory)
                .map_err(map_usercopy_error)?
                .assume_init()
        };
        // SAFETY: the explicit provider initialized the complete timespec;
        // its two integer fields are valid for all copied bit patterns.
        Some(value.try_into_time_value()?)
    };
    do_epoll_wait(
        memory,
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

    #[test]
    fn every_negative_millisecond_timeout_means_infinite_wait() {
        assert_eq!(epoll_timeout(-1), None);
        assert_eq!(epoll_timeout(-2), None);
        assert_eq!(epoll_timeout(i32::MIN), None);
        assert_eq!(epoll_timeout(0), Some(Duration::ZERO));
    }
}
