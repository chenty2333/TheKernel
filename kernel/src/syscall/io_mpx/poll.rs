use alloc::vec::Vec;
use core::time::Duration;

use axerrno::{AxError, AxResult};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::UserContext,
};
use axpoll::IoEvents;
use axtask::current;
use linux_raw_sys::general::{POLLNVAL, RLIMIT_NOFILE, pollfd, timespec};
use starry_signal::SignalSet;

use super::{FdPollSet, io_to_linux_poll, linux_poll_events, wait_io_result, wait_signal_only};
use crate::{
    file::get_file_like,
    mm::{UserConstPtr, UserPtr, nullable},
    syscall::signal::check_sigset_size,
    task::AsThread,
    time::TimeValueLike,
};

fn checked_nfds(nfds: usize) -> AxResult<usize> {
    let limit = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE].current as usize;
    if nfds > limit {
        Err(AxError::InvalidInput)
    } else {
        Ok(nfds)
    }
}

fn reset_pollfd(fd: &mut pollfd) -> Option<i32> {
    fd.revents = 0;
    (fd.fd >= 0).then_some(fd.fd)
}

fn write_poll_result(revents: &mut i16, mut result: IoEvents, interested: IoEvents) -> bool {
    if result.contains(IoEvents::READABLE) {
        result |= IoEvents::READ_NORMAL;
    }
    if result.contains(IoEvents::WRITABLE) {
        result |= IoEvents::WRITE_NORMAL;
    }
    result &= interested;

    *revents = io_to_linux_poll(result) as i16;
    *revents != 0
}

fn do_poll(
    uctx: Option<&mut UserContext>,
    poll_fds: &mut [pollfd],
    timeout: Option<TimeValue>,
    sigmask: Option<SignalSet>,
) -> AxResult<isize> {
    debug!("do_poll fds={poll_fds:?} timeout={timeout:?}");

    let mut invalid_count = 0usize;
    let mut fds = FdPollSet::try_with_capacity(poll_fds.len())?;
    let mut revents = Vec::new();
    revents
        .try_reserve_exact(poll_fds.len())
        .map_err(|_| AxError::NoMemory)?;
    for fd in poll_fds.iter_mut() {
        let Some(raw_fd) = reset_pollfd(fd) else {
            // Linux ignores every negative descriptor for this invocation.
            continue;
        };
        match get_file_like(raw_fd) {
            Ok(f) if f.is_path_only() => {
                // Linux poll treats a live O_PATH description like an invalid
                // poll source without invalidating the descriptor itself.
                fd.revents = POLLNVAL as _;
                invalid_count += 1;
            }
            Ok(f) => {
                fds.push(
                    f,
                    linux_poll_events(fd.events as u16 as u32) | IoEvents::ALWAYS,
                );
                revents.push(&mut fd.revents);
            }
            Err(_) => {
                // If the fd is invalid, set revents to POLLNVAL
                fd.revents = POLLNVAL as _;
                invalid_count += 1;
            }
        }
    }
    let fds = fds.finish();
    if fds.is_empty() {
        if invalid_count != 0 {
            return Ok(invalid_count as isize);
        }
        return wait_signal_only(uctx, timeout.map(Duration::from), sigmask);
    }
    let deadline = timeout.map(|dur| wall_time().saturating_add(dur));
    let mut poll_once = || {
        let mut res = invalid_count;
        for entry in fds.entries() {
            res += usize::from(write_poll_result(
                &mut *revents[entry.output_index],
                entry.file.poll_events_for_poll(),
                entry.events,
            ));
        }

        if res > 0 {
            Ok(res as isize)
        } else {
            Err(AxError::WouldBlock)
        }
    };

    let mut wait_once = || {
        crate::readiness::block_on_poll_io_until(
            &fds,
            IoEvents::empty(),
            false,
            deadline,
            &mut poll_once,
        )
    };

    wait_io_result(uctx, sigmask, &mut wait_once)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_poll(fds: UserPtr<pollfd>, nfds: u32, timeout: i32) -> AxResult<isize> {
    let nfds = checked_nfds(nfds as usize)?;
    let fds: &mut [pollfd] = if nfds == 0 {
        &mut []
    } else {
        fds.get_as_mut_slice(nfds)?
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
    let nfds = checked_nfds(nfds.try_into().map_err(|_| AxError::InvalidInput)?)?;
    let fds: &mut [pollfd] = if nfds == 0 {
        &mut []
    } else {
        fds.get_as_mut_slice(nfds)?
    };
    let timeout = nullable!(timeout.get_as_ref())?
        .map(|ts| ts.try_into_time_value())
        .transpose()?;
    do_poll(Some(uctx), fds, timeout, sigmask)
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::{POLLIN, POLLOUT};

    use super::*;

    #[test]
    fn every_negative_fd_is_ignored_and_cleared() {
        for raw_fd in [-1, -2, i32::MIN] {
            let mut fd = pollfd {
                fd: raw_fd,
                events: POLLIN as i16,
                revents: POLLNVAL as i16,
            };
            assert_eq!(reset_pollfd(&mut fd), None);
            assert_eq!(fd.revents, 0);
        }
    }

    #[test]
    fn unknown_input_event_bits_are_ignored() {
        let translated = linux_poll_events(POLLIN | 0x4000 | 0x8000);
        assert_eq!(translated, IoEvents::READABLE);
    }

    #[test]
    fn invalid_and_ready_descriptors_are_counted_together() {
        let mut ready_revents = POLLNVAL as i16;
        let count = 1 + usize::from(write_poll_result(
            &mut ready_revents,
            IoEvents::WRITABLE,
            IoEvents::WRITABLE | IoEvents::ALWAYS,
        ));

        assert_eq!(count, 2);
        assert_eq!(ready_revents, POLLOUT as i16);
    }
}
