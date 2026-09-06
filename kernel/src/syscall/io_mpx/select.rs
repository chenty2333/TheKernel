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
    task::AsThread,
    time::TimeValueLike,
};

struct FdSet(Bitmap<{ __FD_SETSIZE as usize }>);
const STICKY_TIMEOUTS: u32 = 0x0400_0000;

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
    let mut set = MaybeUninit::<__kernel_fd_set>::zeroed();
    // select copies only the native-long words covered by nfds, even when
    // libc's fd_set is larger or crosses an inaccessible page boundary.
    let bytes = unsafe {
        slice::from_raw_parts_mut(
            set.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            fd_set_bytes(nfds),
        )
    };
    caller
        .read_bytes(fds.address().as_usize(), bytes)
        .map_err(map_usercopy_error)?;
    Ok(Some(unsafe { set.assume_init() }))
}

fn fd_set_bytes(nfds: u32) -> usize {
    (nfds as usize).div_ceil(usize::BITS as usize) * core::mem::size_of::<usize>()
}

fn copy_fd_set(
    caller: &UserMemoryCapability,
    destination: UserPtr<__kernel_fd_set>,
    bitmap: Bitmap<{ __FD_SETSIZE as usize }>,
    nfds: u32,
) -> AxResult<()> {
    if destination.is_null() || nfds == 0 {
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
            fd_set_bytes(nfds),
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
        let mut ready = poll();
        if ready.intersects(IoEvents::HANGUP | IoEvents::ERROR) {
            ready |= IoEvents::READABLE;
        }
        if ready.contains(IoEvents::ERROR) {
            ready |= IoEvents::WRITABLE;
        }
        ready & interested
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
    sigmask: Option<SignalSet>,
) -> AxResult<isize> {
    if nfds > i32::MAX as u32 {
        return Err(AxError::InvalidInput);
    }
    // Linux bounds a nonnegative nfds by the allocated file-table capacity.
    // This profile's table owns AX_FILE_LIMIT slots; it cannot contain a
    // descriptor above that bound, even if the caller supplies a larger n.
    const _: () = assert!(crate::task::AX_FILE_LIMIT <= __FD_SETSIZE as usize);
    let nfds = nfds.min(crate::task::AX_FILE_LIMIT as u32);
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
        events.set(IoEvents::PRIORITY, except_set.0.get(fd));
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
            if events.contains(IoEvents::PRIORITY) && !exceptfds.is_null() {
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
            false,
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
            copy_fd_set(caller, readfds, ready_read, nfds)?;
            copy_fd_set(caller, writefds, ready_write, nfds)?;
            copy_fd_set(caller, exceptfds, ready_except, nfds)?;
            Ok(result)
        }
        Err(error) => Err(error),
    }
}

fn select_timeval_duration(value: timeval) -> AxResult<Duration> {
    if value.tv_sec < 0 || value.tv_usec < 0 {
        return Err(AxError::InvalidInput);
    }
    let seconds = value
        .tv_sec
        .checked_add(value.tv_usec / 1_000_000)
        .ok_or(AxError::InvalidInput)?;
    Ok(Duration::new(
        seconds as u64,
        (value.tv_usec % 1_000_000) as u32 * 1000,
    ))
}

fn finish_timeout(
    caller: &UserMemoryCapability,
    address: usize,
    timeout: Option<Duration>,
    started: Duration,
    microseconds: bool,
) {
    let Some(timeout) = timeout.filter(|duration| !duration.is_zero()) else {
        return;
    };
    if axtask::current().as_thread().personality() & STICKY_TIMEOUTS != 0 {
        return;
    }
    let elapsed = axhal::time::monotonic_time().saturating_sub(started);
    let remaining = timeout.saturating_sub(elapsed);
    // Both native x86_64 timeout structures contain exactly two signed longs.
    // Linux deliberately ignores a read-only timeout's final copyout fault.
    let value = [
        remaining.as_secs() as i64,
        if microseconds {
            remaining.subsec_micros() as i64
        } else {
            remaining.subsec_nanos() as i64
        },
    ];
    let _ = caller.write_value(address as *mut [i64; 2], value);
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
    let duration = if timeout.is_null() {
        None
    } else {
        Some(select_timeval_duration(read_user_value(
            &caller,
            timeout.address().as_usize(),
        )?)?)
    };
    let started = axhal::time::monotonic_time();
    let result = do_select(
        &caller, None, nfds, readfds, writefds, exceptfds, duration, None,
    );
    finish_timeout(
        &caller,
        timeout.address().as_usize(),
        duration,
        started,
        true,
    );
    result
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
    // The syscall wrapper copies the sigset argument header before the
    // timeout; the actual mask is admitted after timeout validation.
    let sigmask = if sigmask.is_null() {
        None
    } else {
        Some(read_user_value::<SignalSetWithSize>(
            &caller,
            sigmask.address().as_usize(),
        )?)
    };
    let duration = if timeout.is_null() {
        None
    } else {
        Some(
            read_user_value::<timespec>(&caller, timeout.address().as_usize())?
                .try_into_time_value()?,
        )
    };
    let started = axhal::time::monotonic_time();
    let sigmask = match sigmask {
        Some(mask) if !mask.set.is_null() => {
            check_sigset_size(mask.sigsetsize)?;
            Some(read_user_value(&caller, mask.set.address().as_usize())?)
        }
        _ => None,
    };
    let result = do_select(
        &caller,
        Some(uctx),
        nfds,
        readfds,
        writefds,
        exceptfds,
        duration,
        sigmask,
    );
    finish_timeout(
        &caller,
        timeout.address().as_usize(),
        duration,
        started,
        false,
    );
    result
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn select_error_and_hangup_are_not_priority_data() {
        let interests = IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::PRIORITY;
        assert_eq!(
            select_ready_events(false, interests, || IoEvents::ERROR),
            IoEvents::READABLE | IoEvents::WRITABLE
        );
        assert_eq!(
            select_ready_events(false, interests, || IoEvents::HANGUP),
            IoEvents::READABLE
        );
        assert_eq!(
            select_ready_events(false, interests, || IoEvents::PRIORITY),
            IoEvents::PRIORITY
        );
    }

    #[test]
    fn select_timeval_normalizes_positive_microseconds() {
        assert_eq!(
            select_timeval_duration(timeval {
                tv_sec: 1,
                tv_usec: 1_500_001
            }),
            Ok(Duration::new(2, 500_001_000))
        );
        assert_eq!(
            select_timeval_duration(timeval {
                tv_sec: -1,
                tv_usec: 2_000_000
            }),
            Err(AxError::InvalidInput)
        );
        assert_eq!(
            select_timeval_duration(timeval {
                tv_sec: 0,
                tv_usec: -1
            }),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn opath_select_keeps_all_requested_classes_ready() {
        let interested = IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::PRIORITY;
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
                IoEvents::READABLE | IoEvents::PRIORITY
            }),
            IoEvents::READABLE | IoEvents::PRIORITY
        );
        assert_eq!(poll_calls.get(), 1);
    }
}
