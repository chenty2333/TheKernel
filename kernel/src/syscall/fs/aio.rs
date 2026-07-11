use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::{
    future::poll_fn,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    task::Poll,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::PollSet;
use axsync::Mutex;
use axtask::{
    current,
    future::{self, block_on, interruptible},
};
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::__kernel_off_t;
use starry_process::Pid;
use starry_signal::SignalSet;
use starry_vm::{VmMutPtr, VmPtr};

use super::{sys_fdatasync, sys_fsync, sys_pread64, sys_preadv, sys_pwrite64, sys_pwritev};
use crate::{
    file::{FileHandle, event::EventFd, get_typed_file},
    mm::IoVec,
    task::{AsThread, with_blocked_signals},
};

const AIO_MAX_NR_DEFAULT: usize = 0x10000;
const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;
const IOCB_CMD_POLL: u16 = 5;
const IOCB_CMD_NOOP: u16 = 6;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
const IOCB_FLAG_RESFD: u32 = 1 << 0;
const IOCB_FLAG_IOPRIO: u32 = 1 << 1;
const KIOCB_KEY: u32 = 0;
const RWF_HIPRI: u32 = 0x00000001;
const RWF_DSYNC: u32 = 0x00000002;
const RWF_SYNC: u32 = 0x00000004;
const RWF_NOWAIT: u32 = 0x00000008;
const RWF_APPEND: u32 = 0x00000010;
const RWF_SUPPORTED: u32 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_NOWAIT | RWF_APPEND;
const AIO_HARD_MAX_EVENTS: usize = 0x10000000 / size_of::<IoEvent>();

static NEXT_AIO_CTX: AtomicU64 = AtomicU64::new(1);
static AIO_NR: AtomicUsize = AtomicUsize::new(0);
static AIO_MAX_NR: AtomicUsize = AtomicUsize::new(AIO_MAX_NR_DEFAULT);
static AIO_CONTEXTS: Mutex<AioManager> = Mutex::new(AioManager::new());

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
pub struct IoEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct Iocb {
    aio_data: u64,
    aio_key: u32,
    aio_rw_flags: u32,
    aio_lio_opcode: u16,
    aio_reqprio: i16,
    aio_fildes: u32,
    aio_buf: u64,
    aio_nbytes: u64,
    aio_offset: i64,
    aio_reserved2: u64,
    aio_flags: u32,
    aio_resfd: u32,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct KernelTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub struct AioSigset {
    sigmask: *const u8,
    sigsetsize: usize,
}

struct AioContext {
    owner: Pid,
    max_events: usize,
    state: Mutex<AioContextState>,
    waiters: PollSet,
}

struct AioContextState {
    events: VecDeque<IoEvent>,
    in_flight: usize,
    accepting: bool,
}

struct AioManager {
    contexts: BTreeMap<u64, Arc<AioContext>>,
}

impl AioManager {
    const fn new() -> Self {
        Self {
            contexts: BTreeMap::new(),
        }
    }
}

pub fn aio_nr() -> usize {
    AIO_NR.load(Ordering::Acquire)
}

pub fn aio_max_nr() -> usize {
    AIO_MAX_NR.load(Ordering::Acquire)
}

pub fn set_aio_max_nr(value: usize) {
    AIO_MAX_NR.store(value, Ordering::Release);
}

fn next_aio_context_id() -> u64 {
    loop {
        let id = NEXT_AIO_CTX.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

fn try_reserve_aio_events(count: usize) -> AxResult {
    let max = aio_max_nr();
    if count > max {
        return Err(LinuxError::EAGAIN.into());
    }

    let mut current = AIO_NR.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(count) else {
            return Err(LinuxError::EAGAIN.into());
        };
        if next > max {
            return Err(LinuxError::EAGAIN.into());
        }
        match AIO_NR.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

fn release_aio_events(count: usize) {
    AIO_NR.fetch_sub(count, Ordering::AcqRel);
}

fn current_aio_owner() -> Pid {
    current().as_thread().proc_data.proc.pid()
}

fn context_for_current(ctx: u64) -> AxResult<Arc<AioContext>> {
    let context = AIO_CONTEXTS
        .lock()
        .contexts
        .get(&ctx)
        .cloned()
        .ok_or(AxError::InvalidInput)?;
    if context.owner != current_aio_owner() {
        return Err(AxError::InvalidInput);
    }
    Ok(context)
}

fn reserve_submission_slots(context: &AioContext, requested: usize) -> AxResult<usize> {
    let mut state = context.state.lock();
    if !state.accepting {
        return Err(AxError::InvalidInput);
    }
    let used = state.events.len().saturating_add(state.in_flight);
    let available = context.max_events.saturating_sub(used);
    if available == 0 && requested != 0 {
        return Err(LinuxError::EAGAIN.into());
    }
    let reserved = requested.min(available);
    state.in_flight += reserved;
    Ok(reserved)
}

fn read_iocb_ptr(iocbpp: *const *const Iocb, index: usize) -> AxResult<*const Iocb> {
    Ok(unsafe { iocbpp.wrapping_add(index).vm_read_uninit()?.assume_init() })
}

fn write_iocb_key(iocb: *const Iocb) -> AxResult {
    let key = unsafe { core::ptr::addr_of_mut!((*iocb.cast_mut()).aio_key) };
    key.vm_write(KIOCB_KEY)?;
    Ok(())
}

fn read_optional_timespec(timeout: *const KernelTimespec) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }

    let timeout = timeout.vm_read()?;
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(AxError::InvalidInput);
    }
    Ok(Some(Duration::new(
        timeout.tv_sec as u64,
        timeout.tv_nsec as u32,
    )))
}

fn read_optional_sigset(sigset: *const AioSigset) -> AxResult<Option<SignalSet>> {
    if sigset.is_null() {
        return Ok(None);
    }

    let sigset = sigset.vm_read()?;
    if sigset.sigmask.is_null() {
        return Ok(None);
    }
    if sigset.sigsetsize != size_of::<SignalSet>() {
        return Err(AxError::InvalidInput);
    }
    Ok(Some(unsafe {
        sigset
            .sigmask
            .cast::<SignalSet>()
            .vm_read_uninit()?
            .assume_init()
    }))
}

fn resfd_file(iocb: &Iocb) -> AxResult<Option<FileHandle<EventFd>>> {
    if iocb.aio_flags & IOCB_FLAG_RESFD == 0 {
        return Ok(None);
    }

    Ok(Some(get_typed_file::<EventFd>(iocb.aio_resfd as i32)?))
}

fn validate_iocb_common(iocb: &Iocb) -> AxResult {
    if iocb.aio_reserved2 != 0 || iocb.aio_nbytes > isize::MAX as u64 || iocb.aio_reqprio != 0 {
        return Err(AxError::InvalidInput);
    }
    if iocb.aio_flags & !(IOCB_FLAG_RESFD | IOCB_FLAG_IOPRIO) != 0 {
        return Err(AxError::InvalidInput);
    }
    if iocb.aio_flags & IOCB_FLAG_IOPRIO != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    Ok(())
}

fn validate_aio_rw_flags(flags: u32) -> AxResult {
    if flags & !RWF_SUPPORTED != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    if flags & (RWF_HIPRI | RWF_NOWAIT | RWF_APPEND) != 0 {
        return Err(LinuxError::EOPNOTSUPP.into());
    }
    Ok(())
}

fn maybe_sync_after_write(fd: i32, flags: u32) -> AxResult {
    if flags & RWF_SYNC != 0 {
        sys_fsync(fd)?;
    } else if flags & RWF_DSYNC != 0 {
        sys_fdatasync(fd)?;
    }
    Ok(())
}

fn execute_iocb(iocb: &Iocb) -> AxResult<isize> {
    validate_iocb_common(iocb)?;

    let fd = iocb.aio_fildes as i32;
    let offset = iocb.aio_offset as __kernel_off_t;
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            sys_pread64(
                fd,
                iocb.aio_buf as *mut u8,
                iocb.aio_nbytes as usize,
                offset,
            )
        }
        IOCB_CMD_PWRITE => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            let res = sys_pwrite64(
                fd,
                iocb.aio_buf as *const u8,
                iocb.aio_nbytes as usize,
                offset,
            )?;
            maybe_sync_after_write(fd, iocb.aio_rw_flags)?;
            Ok(res)
        }
        IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => {
            if iocb.aio_buf != 0
                || iocb.aio_offset != 0
                || iocb.aio_nbytes != 0
                || iocb.aio_rw_flags != 0
            {
                return Err(AxError::InvalidInput);
            }
            if iocb.aio_lio_opcode == IOCB_CMD_FSYNC {
                sys_fsync(fd)
            } else {
                sys_fdatasync(fd)
            }
        }
        IOCB_CMD_NOOP => Ok(0),
        IOCB_CMD_PREADV => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            sys_preadv(
                fd,
                iocb.aio_buf as *const IoVec,
                iocb.aio_nbytes as usize,
                offset,
            )
        }
        IOCB_CMD_PWRITEV => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            let res = sys_pwritev(
                fd,
                iocb.aio_buf as *const IoVec,
                iocb.aio_nbytes as usize,
                offset,
            )?;
            maybe_sync_after_write(fd, iocb.aio_rw_flags)?;
            Ok(res)
        }
        IOCB_CMD_POLL => {
            if iocb.aio_buf > u16::MAX as u64
                || iocb.aio_offset != 0
                || iocb.aio_nbytes != 0
                || iocb.aio_rw_flags != 0
            {
                return Err(AxError::InvalidInput);
            }
            Err(AxError::Unsupported)
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn finish_io_submit(
    context: &AioContext,
    reserved: usize,
    completions: VecDeque<IoEvent>,
    submitted: isize,
) -> AxResult<isize> {
    let mut state = context.state.lock();
    state.in_flight = state.in_flight.saturating_sub(reserved);
    if state.accepting {
        state.events.extend(completions);
    }
    drop(state);
    context.waiters.wake();
    Ok(submitted)
}

fn fail_io_submit(
    context: &AioContext,
    reserved: usize,
    completions: VecDeque<IoEvent>,
    submitted: isize,
    error: AxError,
) -> AxResult<isize> {
    finish_io_submit(context, reserved, completions, submitted)?;
    if submitted == 0 {
        Err(error)
    } else {
        Ok(submitted)
    }
}

pub fn sys_io_setup(nr_events: u32, ctxp: *mut u64) -> AxResult<isize> {
    let current = ctxp.vm_read()?;
    if current != 0 || nr_events == 0 {
        return Err(AxError::InvalidInput);
    }

    let nr_events = nr_events as usize;
    if nr_events > AIO_HARD_MAX_EVENTS {
        return Err(AxError::InvalidInput);
    }
    try_reserve_aio_events(nr_events)?;

    let id = next_aio_context_id();
    AIO_CONTEXTS.lock().contexts.insert(
        id,
        Arc::new(AioContext {
            owner: current_aio_owner(),
            max_events: nr_events,
            state: Mutex::new(AioContextState {
                events: VecDeque::new(),
                in_flight: 0,
                accepting: true,
            }),
            waiters: PollSet::new(),
        }),
    );
    if let Err(err) = ctxp.vm_write(id) {
        AIO_CONTEXTS.lock().contexts.remove(&id);
        release_aio_events(nr_events);
        return Err(err.into());
    }
    Ok(0)
}

pub fn sys_io_destroy(ctx: u64) -> AxResult<isize> {
    let context = {
        let mut manager = AIO_CONTEXTS.lock();
        let context = manager
            .contexts
            .get(&ctx)
            .cloned()
            .ok_or(AxError::InvalidInput)?;
        if context.owner != current_aio_owner() {
            return Err(AxError::InvalidInput);
        }
        context.state.lock().accepting = false;
        manager.contexts.remove(&ctx);
        context
    };
    block_on(poll_fn(|cx| {
        if context.state.lock().in_flight == 0 {
            return Poll::Ready(());
        }
        context.waiters.register(cx.waker());
        if context.state.lock().in_flight == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }));
    context.state.lock().events.clear();
    release_aio_events(context.max_events);
    Ok(0)
}

pub fn sys_io_submit(ctx: u64, nr: isize, iocbpp: *const *const Iocb) -> AxResult<isize> {
    if nr < 0 {
        return Err(AxError::InvalidInput);
    }

    let context = context_for_current(ctx)?;
    if nr == 0 {
        return Ok(0);
    }
    let reserved = reserve_submission_slots(&context, nr as usize)?;

    let mut submitted = 0isize;
    let mut completions = VecDeque::new();

    for index in 0..reserved {
        let ptr = match read_iocb_ptr(iocbpp, index) {
            Ok(ptr) if !ptr.is_null() => ptr,
            Ok(_) => {
                return fail_io_submit(
                    &context,
                    reserved,
                    completions,
                    submitted,
                    AxError::BadAddress,
                );
            }
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };
        let iocb = match ptr.vm_read() {
            Ok(iocb) => iocb,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err.into());
            }
        };

        let resfd = match resfd_file(&iocb) {
            Ok(resfd) => resfd,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };
        if let Err(err) = write_iocb_key(ptr) {
            return fail_io_submit(&context, reserved, completions, submitted, err);
        }
        let res = match execute_iocb(&iocb) {
            Ok(res) => res,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };
        if let Some(event) = resfd {
            if let Err(err) = event.signal(1) {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        }
        completions.push_back(IoEvent {
            data: iocb.aio_data,
            obj: ptr as u64,
            res: res as i64,
            res2: 0,
        });
        submitted += 1;
    }

    finish_io_submit(&context, reserved, completions, submitted)
}

pub fn sys_io_getevents(
    ctx: u64,
    min_nr: isize,
    nr: isize,
    events: *mut IoEvent,
    timeout: *const KernelTimespec,
) -> AxResult<isize> {
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(AxError::InvalidInput);
    }
    let timeout = read_optional_timespec(timeout)?;
    let context = context_for_current(ctx)?;
    let min_nr = min_nr as usize;
    let nr = nr as usize;

    enum WaitResult {
        Ready,
        TimedOut,
        Interrupted,
    }

    let enough_events = || context.state.lock().events.len() >= min_nr;
    let wait_result = if min_nr == 0 || enough_events() {
        WaitResult::Ready
    } else {
        let wait = poll_fn(|cx| {
            let state = context.state.lock();
            if state.events.len() >= min_nr || !state.accepting {
                return Poll::Ready(());
            }
            drop(state);
            context.waiters.register(cx.waker());
            let state = context.state.lock();
            if state.events.len() >= min_nr || !state.accepting {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        match block_on(future::timeout(timeout, interruptible(wait))) {
            Ok(Ok(())) => WaitResult::Ready,
            Ok(Err(_)) => WaitResult::Interrupted,
            Err(_) => WaitResult::TimedOut,
        }
    };

    let mut state = context.state.lock();
    let count = nr.min(state.events.len());
    if count == 0 {
        return match wait_result {
            WaitResult::Interrupted => Err(AxError::Interrupted),
            WaitResult::Ready if !state.accepting => Err(AxError::InvalidInput),
            WaitResult::Ready | WaitResult::TimedOut => Ok(0),
        };
    }

    for index in 0..count {
        let event = *state.events.get(index).ok_or(AxError::InvalidInput)?;
        events.wrapping_add(index).vm_write(event)?;
    }
    for _ in 0..count {
        state.events.pop_front();
    }
    Ok(count as isize)
}

pub fn sys_io_pgetevents(
    ctx: u64,
    min_nr: isize,
    nr: isize,
    events: *mut IoEvent,
    timeout: *const KernelTimespec,
    sigset: *const AioSigset,
) -> AxResult<isize> {
    let sigset = read_optional_sigset(sigset)?;
    with_blocked_signals(sigset, || {
        sys_io_getevents(ctx, min_nr, nr, events, timeout)
    })
}

pub fn sys_io_cancel(ctx: u64, iocb: *const Iocb, result: *mut IoEvent) -> AxResult<isize> {
    let _ = iocb.vm_read()?;
    if result.is_null() {
        return Err(AxError::BadAddress);
    }
    context_for_current(ctx)?;
    Err(AxError::InvalidInput)
}

pub fn cleanup_process_aio(owner: Pid) {
    loop {
        // Exec and process exit call this after their irreversible lifecycle
        // transition has begun. Detach one context at a time so cleanup never
        // allocates a temporary ID/context vector after that commit point.
        let context = {
            let mut manager = AIO_CONTEXTS.lock();
            let id = manager
                .contexts
                .iter()
                .find_map(|(id, context)| (context.owner == owner).then_some(*id));
            id.and_then(|id| manager.contexts.remove(&id))
        };
        let Some(context) = context else {
            break;
        };

        let mut state = context.state.lock();
        state.accepting = false;
        state.events.clear();
        drop(state);
        context.waiters.wake();
        release_aio_events(context.max_events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(max_events: usize) -> AioContext {
        AioContext {
            owner: 1,
            max_events,
            state: Mutex::new(AioContextState {
                events: VecDeque::new(),
                in_flight: 0,
                accepting: true,
            }),
            waiters: PollSet::new(),
        }
    }

    #[test]
    fn submission_reservations_share_the_hard_capacity() {
        let context = context(4);
        assert_eq!(reserve_submission_slots(&context, 3).unwrap(), 3);
        assert_eq!(reserve_submission_slots(&context, 3).unwrap(), 1);
        assert_eq!(
            reserve_submission_slots(&context, 1).unwrap_err(),
            LinuxError::EAGAIN.into()
        );
    }

    #[test]
    fn completions_release_in_flight_slots_but_remain_accounted() {
        let context = context(2);
        let reserved = reserve_submission_slots(&context, 2).unwrap();
        let mut completions = VecDeque::new();
        completions.push_back(IoEvent::default());
        finish_io_submit(&context, reserved, completions, 1).unwrap();

        let state = context.state.lock();
        assert_eq!(state.in_flight, 0);
        assert_eq!(state.events.len(), 1);
        drop(state);
        assert_eq!(reserve_submission_slots(&context, 2).unwrap(), 1);
    }

    #[test]
    fn closed_context_rejects_new_submissions() {
        let context = context(1);
        context.state.lock().accepting = false;
        assert_eq!(
            reserve_submission_slots(&context, 1).unwrap_err(),
            AxError::InvalidInput
        );
    }
}
