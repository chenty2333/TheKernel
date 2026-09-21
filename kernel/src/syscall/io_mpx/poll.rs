use alloc::{sync::Arc, vec::Vec};
use core::mem::{MaybeUninit, offset_of, size_of};

use axerrno::{AxError, AxResult};
use axhal::{
    time::{TimeValue, wall_time},
    uspace::UserContext,
};
use axpoll::IoEvents;
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{POLLNVAL, RLIMIT_NOFILE, pollfd, timespec};
use tk_linux_signal::SignalSet;

use super::{
    FdPollSet, io_to_linux_poll, linux_poll_events, select::finish_timeout, wait_io_result,
    wait_signal_only,
};
use crate::{
    file::get_file_like,
    mm::{AddrSpace, UserConstPtr, UserMemoryCapability, UserPtr, map_usercopy_error},
    syscall::signal::check_sigset_size,
    task::{AsThread, PollRestart, RestartBlock},
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

fn read_user_value<T>(caller: &UserMemoryCapability, address: usize) -> AxResult<T> {
    let value = caller
        .read_value_uninit(address as *const T)
        .map_err(map_usercopy_error)?;
    // SAFETY: the explicit usercopy initialized the complete value before it
    // is exposed to the kernel. The syscall mirror types used here contain
    // only integer fields, so every byte representation is valid.
    Ok(unsafe { value.assume_init() })
}

fn snapshot_pollfds(
    caller: &UserMemoryCapability,
    fds: UserPtr<pollfd>,
    nfds: usize,
) -> AxResult<Vec<pollfd>> {
    if nfds == 0 {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    values
        .try_reserve_exact(nfds)
        .map_err(|_| AxError::NoMemory)?;
    values.resize_with(nfds, MaybeUninit::uninit);
    caller
        .read_slice(fds.address().as_usize() as *const pollfd, &mut values)
        .map_err(map_usercopy_error)?;

    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(nfds)
        .map_err(|_| AxError::NoMemory)?;
    for value in values {
        // SAFETY: `read_slice` initialized every byte of this pollfd mirror.
        snapshot.push(unsafe { value.assume_init() });
    }
    Ok(snapshot)
}

fn copy_poll_results(
    caller: &UserMemoryCapability,
    user_fds: UserPtr<pollfd>,
    poll_fds: &[pollfd],
) -> AxResult<()> {
    let base = user_fds.address().as_usize();
    for (index, fd) in poll_fds.iter().enumerate() {
        let offset = index
            .checked_mul(size_of::<pollfd>())
            .and_then(|offset| offset.checked_add(offset_of!(pollfd, revents)))
            .ok_or(AxError::BadAddress)?;
        let address = base.checked_add(offset).ok_or(AxError::BadAddress)?;
        // Copy only revents. This deliberately avoids copying pollfd padding
        // or treating a user pointer as a Rust reference.
        caller
            .write_bytes(address, &fd.revents.to_ne_bytes())
            .map_err(map_usercopy_error)?;
    }
    Ok(())
}

fn do_poll(
    uctx: Option<&mut UserContext>,
    poll_fds: &mut [pollfd],
    timeout: Option<TimeValue>,
    sigmask: Option<SignalSet>,
    caller: &UserMemoryCapability,
    user_fds: UserPtr<pollfd>,
) -> AxResult<isize> {
    debug!("do_poll fds={poll_fds:?} timeout={timeout:?}");

    let mut invalid_count = 0usize;
    let mut fds = FdPollSet::try_with_capacity(poll_fds.len())?;
    let mut revent_indices = Vec::new();
    revent_indices
        .try_reserve_exact(poll_fds.len())
        .map_err(|_| AxError::NoMemory)?;
    for (index, fd) in poll_fds.iter_mut().enumerate() {
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
                revent_indices.push(index);
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
        let result = if invalid_count != 0 {
            Ok(invalid_count as isize)
        } else {
            wait_signal_only(uctx, timeout, sigmask)
        };
        return match result {
            Ok(result) => {
                copy_poll_results(caller, user_fds, poll_fds)?;
                Ok(result)
            }
            Err(error) => Err(error),
        };
    }
    let deadline = timeout.map(|dur| wall_time().saturating_add(dur));
    let mut poll_once = || {
        let mut res = invalid_count;
        for entry in fds.entries() {
            let fd_index = revent_indices[entry.output_index];
            res += usize::from(write_poll_result(
                &mut poll_fds[fd_index].revents,
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
            false,
            false,
            deadline,
            &mut poll_once,
        )
    };

    match wait_io_result(uctx, sigmask, &mut wait_once) {
        Ok(result) => {
            copy_poll_results(caller, user_fds, poll_fds)?;
            Ok(result)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_arch = "x86_64")]
pub fn sys_poll(
    caller: UserMemoryCapability,
    fds: UserPtr<pollfd>,
    nfds: u32,
    timeout: i32,
) -> AxResult<isize> {
    let nfds = checked_nfds(nfds as usize)?;
    let mut poll_fds = snapshot_pollfds(&caller, fds, nfds)?;
    let timeout = if timeout < 0 {
        None
    } else {
        Some(TimeValue::from_millis(timeout as u64))
    };
    // Linux `SYSCALL_DEFINE3(poll)` converts `-ERESTARTNOHAND` into
    // `set_restart_fn(restart_block, do_restart_poll)`, i.e.
    // `-ERESTART_RESTARTBLOCK` with the *absolute* expiry and the original
    // descriptor array recorded: the transparent restart re-enters
    // `restart_syscall` and resumes in `do_restart_poll()`, so a signal
    // interruption never recomputes the relative timeout from zero. `ppoll`
    // deliberately does not do this: it returns `-ERESTARTNOHAND` and relies
    // on the remaining-time write-back to its `tsp`, so a replay resumes with
    // the shortened timeout.
    let deadline = timeout.map(|dur| wall_time().saturating_add(dur));
    let result = do_poll(None, &mut poll_fds, timeout, None, &caller, fds);
    if matches!(result, Err(AxError::Interrupted)) {
        // `install_restart_block()` rewrites the pending restart action to
        // `restart_syscall`, mirroring `-ERESTART_RESTARTBLOCK`: both the
        // no-handler transparent restart and the return from a handler resume
        // in `restart_poll()` with the recorded absolute `deadline`, which
        // also keeps the block reachable for a handler's explicit
        // `restart_syscall()`.
        current()
            .as_thread()
            .install_restart_block(RestartBlock::Poll(PollRestart {
                fds: fds.address().as_usize(),
                nfds: nfds as u32,
                deadline,
            }));
    }
    result
}

/// Resumes a `poll` restart block.
///
/// This is Linux `do_restart_poll()`: re-run `do_sys_poll()` with the recorded
/// user array, descriptor count, and absolute `end_time`. The array is read
/// again from userspace, so a concurrent change to it is observed on the
/// restarted invocation exactly as in Linux.
pub(crate) fn restart_poll(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    block: PollRestart,
) -> AxResult<isize> {
    let caller = UserMemoryCapability::new(caller_aspace);
    let nfds = checked_nfds(block.nfds as usize)?;
    let fds = UserPtr::from(block.fds);
    let mut poll_fds = snapshot_pollfds(&caller, fds, nfds)?;
    let timeout = block
        .deadline
        .map(|deadline| deadline.saturating_sub(wall_time()));
    let result = do_poll(None, &mut poll_fds, timeout, None, &caller, fds);
    if matches!(result, Err(AxError::Interrupted)) {
        // Linux re-arms the same block from `do_restart_poll()` whenever
        // `do_sys_poll()` reports `-ERESTARTNOHAND` again.
        current()
            .as_thread()
            .install_restart_block(RestartBlock::Poll(block));
    }
    result
}

pub fn sys_ppoll(
    caller: UserMemoryCapability,
    uctx: &mut UserContext,
    fds: UserPtr<pollfd>,
    nfds: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> AxResult<isize> {
    let sigmask = if sigmask.is_null() {
        None
    } else {
        // Validate the scalar size before copying the non-NULL mask. This is
        // Linux's EINVAL-before-EFAULT ordering for malformed ppoll calls.
        check_sigset_size(sigsetsize)?;
        Some(read_user_value(&caller, sigmask.address().as_usize())?)
    };
    let nfds = checked_nfds(nfds.try_into().map_err(|_| AxError::InvalidInput)?)?;
    let mut poll_fds = snapshot_pollfds(&caller, fds, nfds)?;
    let timeout_value = if timeout.is_null() {
        None
    } else {
        Some(
            read_user_value::<timespec>(&caller, timeout.address().as_usize())?
                .try_into_time_value()?,
        )
    };
    let started = axhal::time::monotonic_time();
    let result = do_poll(
        Some(uctx),
        &mut poll_fds,
        timeout_value,
        sigmask,
        &caller,
        fds,
    );
    // Linux `SYSCALL_DEFINE5(ppoll)` ends in
    // `poll_select_finish(&end_time, tsp, PT_TIMESPEC, ret)`, which writes the
    // remaining time back to the caller's `tsp` on every return path --
    // including `-ERESTARTNOHAND`. That write-back is what lets ppoll replay
    // the syscall instead of carrying a restart block, so it has to happen for
    // the interrupted case too.
    finish_timeout(
        &caller,
        timeout.address().as_usize(),
        timeout_value,
        started,
        false,
    );
    result
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
