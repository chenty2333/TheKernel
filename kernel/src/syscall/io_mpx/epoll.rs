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
    EPOLL_CLOEXEC, EPOLLET, EPOLLEXCLUSIVE as RAW_EPOLLEXCLUSIVE, EPOLLONESHOT, epoll_event,
    timespec,
};
use tk_linux_fd::{
    EPOLLEXCLUSIVE, EpollControl, ExclusiveAdmission, exclusive_admission, strip_epollwakeup,
};
use tk_linux_signal::SignalSet;
use tk_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

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
    // The exclusive-admission rule is a `tk-linux-fd` policy contract, so it
    // owns its own copy of the flag values; pin them to the generated ABI
    // constants used by the rest of this file.
    assert!(EPOLLEXCLUSIVE == RAW_EPOLLEXCLUSIVE);
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

/// One fully parsed `epoll_ctl` event argument.
struct EpollCtlRequest {
    /// Interest and user data in the kernel's readiness domain.
    event: EpollEvent,
    /// `EPOLLET` / `EPOLLONESHOT` trigger flags.
    flags: EpollFlags,
    /// The raw mask after Linux's `ep_take_care_of_epollwakeup()` strip. The
    /// `EPOLLEXCLUSIVE` admission rule inspects this value, so it must keep
    /// bits the readiness translation rejects.
    raw_events: u32,
}

/// Parses the `epoll_event` argument the way Linux `SYSCALL_DEFINE4(epoll_ctl)`
/// and `do_epoll_ctl_file()` do: copy in, strip `EPOLLWAKEUP`, then translate.
///
/// `EPOLLEXCLUSIVE` is a private control bit rather than a readiness interest,
/// so it never contributes an interest bit here; the caller decides its fate
/// through [`exclusive_admission`] before this parser runs for an admitted
/// request.
fn parse_epoll_ctl_event<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    event: *const epoll_event,
) -> AxResult<EpollCtlRequest> {
    let raw = read_epoll_event(memory, event)?;
    let raw_events = strip_epollwakeup(raw.events);
    let flag_bits = raw_events & (EPOLLET | EPOLLONESHOT);
    let events = linux_epoll_events(raw_events & !(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE));
    let flags = EpollFlags::from_bits(flag_bits).ok_or(AxError::InvalidInput)?;
    Ok(EpollCtlRequest {
        event: EpollEvent {
            events,
            user_data: raw.data,
        },
        flags,
        raw_events,
    })
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
    // Linux `SYSCALL_DEFINE4(epoll_ctl)` copies the event for every operation
    // except DEL (`ep_op_has_event()`) *before* it resolves either descriptor
    // (fs/eventpoll.c:2753-2755):
    //     if (ep_op_has_event(op) &&
    //         copy_from_user(&epds, event, sizeof(struct epoll_event)))
    //             return -EFAULT;
    //     return do_epoll_ctl(epfd, op, fd, &epds, false);
    // so an unusable `event` pointer is EFAULT even when `epfd` or `fd` is
    // itself invalid.  This kernel previously resolved both descriptors
    // first, which answered EBADF for the same call.
    let control = EpollControl::from_raw(op);
    let request = if control.carries_event() {
        Some(parse_epoll_ctl_event(memory, event)?)
    } else {
        None
    };

    let epoll_description = get_file_description(epfd)?;
    // `do_epoll_ctl()` resolves the eventpoll descriptor with `CLASS(fd)`,
    // whose `fdget()` rejects `FMODE_PATH`, so an O_PATH epfd is EBADF before
    // anything else is examined.
    check_epoll_target(epoll_description.is_path_only())?;
    let target = get_file_description(fd)?;
    // The target lookup also precedes every `do_epoll_ctl_file()` test, and
    // its `fdget()` likewise rejects FMODE_PATH: an O_PATH target is EBADF,
    // *not* the EPERM a missing `->poll()` would produce.
    check_epoll_target(target.is_path_only())?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");
    // `do_epoll_ctl_file()` tests pollability before the self/epoll-type test:
    // any file whose `->poll()` is absent (regular files and directories)
    // answers -EPERM, for ADD, MOD, DEL, and even an unknown op alike.
    Epoll::validate_target(&target)?;
    if Arc::ptr_eq(&epoll_description, &target) {
        return Err(AxError::InvalidInput);
    }
    let epoll = epoll_description
        .inner
        .clone()
        .downcast_arc::<Epoll>()
        .map_err(|_| AxError::InvalidInput)?;

    // `do_epoll_ctl_file()` then applies `ep_take_care_of_epollwakeup()` and
    // the EPOLLEXCLUSIVE admission rule *before* the operation switch and any
    // interest lookup. Keeping that order keeps EINVAL, EEXIST, and ENOENT
    // precedence intact.
    if let Some(request) = &request {
        // `ep_take_care_of_epollwakeup()` clears the bit in place. With no
        // suspend blocker to attach, the cleared mask is also what this kernel
        // implements, so the strip is unconditional.
        let admitted = exclusive_admission(
            control,
            strip_epollwakeup(request.raw_events),
            target.inner.downcast_ref::<Epoll>().is_some(),
        );
        match admitted {
            ExclusiveAdmission::Rejected => return Err(AxError::InvalidInput),
            // Linux accepts the request here. This kernel's readiness adapter
            // registers a plain waker per interest and `PollSet::wake()` drains
            // every registration on the source, so there is no
            // WQ_FLAG_EXCLUSIVE equivalent and no per-event round in which
            // `ep_poll_callback()` could hand the single exclusive slot to one
            // waiter (`__wake_up_common()` with `nr_exclusive == 1`). Accepting
            // the flag would silently wake every waiter: a thundering herd the
            // caller explicitly asked to avoid. Report it the way Linux reports
            // a target that cannot participate in the epoll wakeup contract,
            // `do_epoll_ctl_file()`'s `!file_can_poll(tf->file) -> -EPERM`.
            //
            // The refusal is confined to ADD because that is the only
            // operation Linux routes into an exclusive registration: the
            // EPOLLEXCLUSIVE block matches `op == EPOLL_CTL_MOD` and
            // `op == EPOLL_CTL_ADD` alone (fs/eventpoll.c:2668-2676), DEL
            // never carries an event (`ep_op_has_event()`), and any other
            // operation falls through to the switch below, whose default arm
            // answers -EINVAL (fs/eventpoll.c:2690).  Answering EPERM
            // for the unknown operation instead would hide that EINVAL.
            ExclusiveAdmission::Admitted if control == EpollControl::Add => {
                return Err(AxError::OperationNotPermitted);
            }
            ExclusiveAdmission::Admitted | ExclusiveAdmission::Absent => {}
        }
    }

    match control {
        EpollControl::Add => {
            let EpollCtlRequest {
                mut event, flags, ..
            } = request.expect("ADD carries an event");
            event.events |= IoEvents::ALWAYS;
            epoll.add(fd, event, flags)?;
        }
        EpollControl::Modify => {
            let EpollCtlRequest {
                mut event, flags, ..
            } = request.expect("MOD carries an event");
            event.events |= IoEvents::ALWAYS;
            epoll.modify(fd, event, flags)?;
        }
        EpollControl::Delete => {
            epoll.delete(fd)?;
        }
        EpollControl::Unknown => return Err(AxError::InvalidInput),
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

    // Linux `do_epoll_wait()` resolves the descriptor with `CLASS(fd)`
    // (`fdget()` rejects `FMODE_PATH`, so a bad or O_PATH epfd is EBADF), and
    // only then runs `ep_check_params()`: maxevents EINVAL, `access_ok`
    // EFAULT, and finally the `is_file_epoll` EINVAL.
    let epoll_description = get_file_description(epfd)?;
    check_epoll_target(epoll_description.is_path_only())?;

    // access_ok() checks the address extent, not whether pages are mapped or
    // writable. An empty wait may return zero with an unmapped user pointer;
    // actual write permissions are checked only when copying ready events.
    const EP_MAX_EVENTS: i32 = i32::MAX / size_of::<epoll_event>() as i32;
    if maxevents <= 0 || maxevents > EP_MAX_EVENTS {
        return Err(AxError::InvalidInput);
    }
    let end = (events as usize)
        .checked_add(maxevents as usize * size_of::<epoll_event>())
        .ok_or(AxError::BadAddress)?;
    if end > crate::config::TASK_SIZE_MAX {
        return Err(AxError::BadAddress);
    }
    let epoll = epoll_description
        .inner
        .clone()
        .downcast_arc::<Epoll>()
        .map_err(|_| AxError::InvalidInput)?;
    let deadline = timeout.map(|dur| axhal::time::wall_time().saturating_add(dur));
    let mut wait_once = || {
        crate::readiness::block_on_poll_io_until(
            epoll.as_ref(),
            IoEvents::READABLE,
            false,
            false,
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
    use core::mem::MaybeUninit;

    use linux_raw_sys::general::{EPOLLIN, EPOLLMSG, EPOLLWAKEUP};
    use tk_linux_usercopy::{UserCopyError, VmResult};

    use super::*;

    /// Byte-addressed user memory whose first 12 bytes hold one
    /// `epoll_event`.
    struct EventMemory {
        bytes: [u8; size_of::<epoll_event>()],
    }

    impl EventMemory {
        fn new(events: u32, data: u64) -> Self {
            let mut bytes = [0u8; size_of::<epoll_event>()];
            bytes[..4].copy_from_slice(&events.to_ne_bytes());
            bytes[4..].copy_from_slice(&data.to_ne_bytes());
            Self { bytes }
        }
    }

    // SAFETY: EventMemory bounds-checks the opaque user address and
    // initializes every destination byte before returning a successful read.
    unsafe impl UserMemory for EventMemory {
        fn read(&mut self, start: usize, dst: &mut [MaybeUninit<u8>]) -> VmResult {
            let end = start
                .checked_add(dst.len())
                .ok_or(UserCopyError::BadAddress)?;
            let source = self
                .bytes
                .get(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            for (output, input) in dst.iter_mut().zip(source) {
                output.write(*input);
            }
            Ok(())
        }

        fn write(&mut self, start: usize, src: &[u8]) -> VmResult {
            let end = start
                .checked_add(src.len())
                .ok_or(UserCopyError::BadAddress)?;
            let destination = self
                .bytes
                .get_mut(start..end)
                .ok_or(UserCopyError::BadAddress)?;
            destination.copy_from_slice(src);
            Ok(())
        }
    }

    fn parse(events: u32) -> AxResult<EpollCtlRequest> {
        let mut memory = EventMemory::new(events, 7);
        let mut context = UserMemoryContext::new(&mut memory);
        parse_epoll_ctl_event(&mut context, core::ptr::null())
    }

    #[test]
    fn epollmsg_is_stored_in_the_interest_mask_like_linux() {
        // Linux never validates the caller's mask: `epi->event.events` keeps
        // `EPOLLMSG` verbatim and only ever intersects it with what `->poll()`
        // reports, so a target that never raises `POLLMSG` simply never sees
        // the bit again.
        let request = parse(EPOLLMSG).unwrap();
        assert_eq!(request.event.events, IoEvents::MESSAGE);
        assert_eq!(request.raw_events, EPOLLMSG);
        assert_eq!(request.event.user_data, 7);
    }

    #[test]
    fn epollwakeup_is_accepted_and_stripped_from_the_stored_mask() {
        let request = parse(EPOLLWAKEUP).unwrap();
        assert_eq!(request.event.events, IoEvents::empty());
        assert_eq!(request.raw_events, 0);
        let request = parse(EPOLLWAKEUP | EPOLLIN).unwrap();
        assert_eq!(request.event.events, IoEvents::READABLE);
        assert_eq!(request.raw_events, EPOLLIN);
    }

    #[test]
    fn trigger_flags_and_the_exclusive_control_bit_are_not_interests() {
        let request = parse(EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLIN).unwrap();
        assert_eq!(request.event.events, IoEvents::READABLE);
        assert!(request.flags.contains(EpollFlags::EDGE_TRIGGER));
        assert!(request.flags.contains(EpollFlags::ONESHOT));
        assert_eq!(
            request.raw_events,
            EPOLLET | EPOLLONESHOT | EPOLLEXCLUSIVE | EPOLLIN
        );
    }

    #[test]
    fn unknown_event_bits_are_stored_verbatim_like_linux() {
        // Linux never validates the caller's mask: `epoll_ctl(ADD, EPOLLIN |
        // 0x1000)` succeeds, keeps the unknown bit in `epi->event.events`,
        // and the bit simply never fires because no `->poll()` raises it.
        let request = parse(EPOLLIN | 0x0000_1000).unwrap();
        assert_eq!(request.event.events, IoEvents::READABLE);
        assert_eq!(request.raw_events, EPOLLIN | 0x0000_1000);
    }

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
