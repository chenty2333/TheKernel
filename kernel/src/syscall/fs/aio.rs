use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};
use core::{
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axhal::time::wall_time;
use axpoll::PollSet;
#[cfg(not(test))]
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::__kernel_off_t;
// These accounting tests do not initialize a scheduler/current task. Runtime
// sleeping-lock and wake behavior is exercised by guest tests; host tests use
// the same critical sections with a spin mutex.
#[cfg(test)]
use spin::Mutex;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::SignalSet;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use super::{sys_fdatasync, sys_fsync, sys_pread64, sys_preadv, sys_pwrite64, sys_pwritev};
use crate::{
    file::{FileHandle, event::EventFd, get_typed_file},
    mm::{IoVec, UserMemoryCapability, map_usercopy_error},
    readiness::{block_on_poll_set_uninterruptible, block_on_poll_set_until},
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

// These records are integer/pointer-only Linux UAPI mirrors.  Keep the
// x86_64 layouts executable before using the one audited unchecked copyout
// for `io_event`; no user-memory context is retained in an AIO object.
const _: () = {
    assert!(align_of::<IoEvent>() == 8);
    assert!(size_of::<IoEvent>() == 32);
    assert!(offset_of!(IoEvent, data) == 0);
    assert!(offset_of!(IoEvent, obj) == 8);
    assert!(offset_of!(IoEvent, res) == 16);
    assert!(offset_of!(IoEvent, res2) == 24);
    assert!(align_of::<Iocb>() == 8);
    assert!(size_of::<Iocb>() == 64);
    assert!(offset_of!(Iocb, aio_data) == 0);
    assert!(offset_of!(Iocb, aio_key) == 8);
    assert!(offset_of!(Iocb, aio_rw_flags) == 12);
    assert!(offset_of!(Iocb, aio_lio_opcode) == 16);
    assert!(offset_of!(Iocb, aio_reqprio) == 18);
    assert!(offset_of!(Iocb, aio_fildes) == 20);
    assert!(offset_of!(Iocb, aio_buf) == 24);
    assert!(offset_of!(Iocb, aio_nbytes) == 32);
    assert!(offset_of!(Iocb, aio_offset) == 40);
    assert!(offset_of!(Iocb, aio_reserved2) == 48);
    assert!(offset_of!(Iocb, aio_flags) == 56);
    assert!(offset_of!(Iocb, aio_resfd) == 60);
    assert!(align_of::<KernelTimespec>() == 8);
    assert!(size_of::<KernelTimespec>() == 16);
    assert!(align_of::<AioSigset>() == 8);
    assert!(size_of::<AioSigset>() == 16);
};

fn write_io_event<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *mut IoEvent,
    value: IoEvent,
) -> AxResult<()> {
    // SAFETY: `IoEvent` is four initialized 64-bit words with no padding on
    // x86_64; the complete layout is asserted above.
    unsafe { VmMutPtr::vm_write_unchecked(ptr, memory, value) }.map_err(map_usercopy_error)
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

    /// Call while holding the manager lock and insert the returned ID before
    /// releasing it. This makes allocation and publication one transaction.
    fn allocate_context_id(&self) -> AxResult<u64> {
        self.allocate_context_id_in_range(u64::MAX)
    }

    fn allocate_context_id_in_range(&self, maximum: u64) -> AxResult<u64> {
        if maximum == 0 {
            return Err(LinuxError::EAGAIN.into());
        }
        let probes = (self.contexts.len() as u64).saturating_add(1).min(maximum);
        for _ in 0..probes {
            let previous = NEXT_AIO_CTX
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    let candidate = if current == 0 || current > maximum {
                        1
                    } else {
                        current
                    };
                    Some(if candidate == maximum {
                        1
                    } else {
                        candidate + 1
                    })
                })
                .unwrap_or(1);
            let id = if previous == 0 || previous > maximum {
                1
            } else {
                previous
            };
            if id != 0 && id <= maximum && !self.contexts.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(LinuxError::EAGAIN.into())
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

fn read_iocb_ptr<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    iocbpp: *const *const Iocb,
    index: usize,
) -> AxResult<*const Iocb> {
    let ptr = unsafe {
        VmPtr::vm_read_uninit(iocbpp.wrapping_add(index), memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    Ok(ptr)
}

fn write_iocb_key<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    iocb: *const Iocb,
) -> AxResult {
    let key = unsafe { core::ptr::addr_of_mut!((*iocb.cast_mut()).aio_key) };
    VmMutPtr::vm_write(key, memory, KIOCB_KEY).map_err(map_usercopy_error)?;
    Ok(())
}

fn read_optional_timespec<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    timeout: *const KernelTimespec,
) -> AxResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }

    let timeout = VmPtr::vm_read(timeout, memory).map_err(map_usercopy_error)?;
    if timeout.tv_sec < 0 || !(0..1_000_000_000).contains(&timeout.tv_nsec) {
        return Err(AxError::InvalidInput);
    }
    Ok(Some(Duration::new(
        timeout.tv_sec as u64,
        timeout.tv_nsec as u32,
    )))
}

fn read_optional_sigset<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    sigset: *const AioSigset,
) -> AxResult<Option<SignalSet>> {
    if sigset.is_null() {
        return Ok(None);
    }

    let sigset = VmPtr::vm_read(sigset, memory).map_err(map_usercopy_error)?;
    if sigset.sigmask.is_null() {
        return Ok(None);
    }
    if sigset.sigsetsize != size_of::<SignalSet>() {
        return Err(AxError::InvalidInput);
    }
    let signal_set = unsafe {
        VmPtr::vm_read_uninit(sigset.sigmask.cast::<SignalSet>(), memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
    Ok(Some(signal_set))
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

fn execute_iocb(capability: &UserMemoryCapability, iocb: &Iocb) -> AxResult<isize> {
    validate_iocb_common(iocb)?;

    let fd = iocb.aio_fildes as i32;
    let offset = iocb.aio_offset as __kernel_off_t;
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            sys_pread64(
                capability.clone(),
                fd,
                iocb.aio_buf as *mut u8,
                iocb.aio_nbytes as usize,
                offset,
            )
        }
        IOCB_CMD_PWRITE => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            let res = sys_pwrite64(
                capability.clone(),
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
                capability.clone(),
                fd,
                iocb.aio_buf as *const IoVec,
                iocb.aio_nbytes as usize,
                offset,
            )
        }
        IOCB_CMD_PWRITEV => {
            validate_aio_rw_flags(iocb.aio_rw_flags)?;
            let res = sys_pwritev(
                capability.clone(),
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

pub fn sys_io_setup<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    nr_events: u32,
    ctxp: *mut u64,
) -> AxResult<isize> {
    let current = VmPtr::vm_read(ctxp, memory).map_err(map_usercopy_error)?;
    if current != 0 || nr_events == 0 {
        return Err(AxError::InvalidInput);
    }

    let nr_events = nr_events as usize;
    if nr_events > AIO_HARD_MAX_EVENTS {
        return Err(AxError::InvalidInput);
    }
    try_reserve_aio_events(nr_events)?;

    let context = Arc::new(AioContext {
        owner: current_aio_owner(),
        max_events: nr_events,
        state: Mutex::new(AioContextState {
            events: VecDeque::new(),
            in_flight: 0,
            accepting: true,
        }),
        waiters: PollSet::new(),
    });
    let id = {
        let mut manager = AIO_CONTEXTS.lock();
        let id = match manager.allocate_context_id() {
            Ok(id) => id,
            Err(error) => {
                release_aio_events(nr_events);
                return Err(error);
            }
        };
        debug_assert!(!manager.contexts.contains_key(&id));
        manager.contexts.insert(id, context);
        id
    };
    if let Err(err) = VmMutPtr::vm_write(ctxp, memory, id) {
        AIO_CONTEXTS.lock().contexts.remove(&id);
        release_aio_events(nr_events);
        return Err(map_usercopy_error(err));
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
    block_on_poll_set_uninterruptible(&context.waiters, || {
        if context.state.lock().in_flight == 0 {
            Ok(())
        } else {
            Err(AxError::WouldBlock)
        }
    })?;
    context.state.lock().events.clear();
    release_aio_events(context.max_events);
    Ok(0)
}

pub fn sys_io_submit<M: UserMemory + ?Sized>(
    capability: UserMemoryCapability,
    memory: &mut UserMemoryContext<'_, M>,
    ctx: u64,
    nr: isize,
    iocbpp: *const *const Iocb,
) -> AxResult<isize> {
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
        let ptr = match read_iocb_ptr(memory, iocbpp, index) {
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
        let iocb = match VmPtr::vm_read(ptr, memory).map_err(map_usercopy_error) {
            Ok(iocb) => iocb,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };

        let resfd = match resfd_file(&iocb) {
            Ok(resfd) => resfd,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };
        if let Err(err) = write_iocb_key(memory, ptr) {
            return fail_io_submit(&context, reserved, completions, submitted, err);
        }
        let res = match execute_iocb(&capability, &iocb) {
            Ok(res) => res,
            Err(err) => {
                return fail_io_submit(&context, reserved, completions, submitted, err);
            }
        };
        if let Some(event) = resfd
            && let Err(err) = event.signal(1)
        {
            return fail_io_submit(&context, reserved, completions, submitted, err);
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

pub fn sys_io_getevents<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ctx: u64,
    min_nr: isize,
    nr: isize,
    events: *mut IoEvent,
    timeout: *const KernelTimespec,
) -> AxResult<isize> {
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(AxError::InvalidInput);
    }
    let timeout = read_optional_timespec(memory, timeout)?;
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
        let deadline = timeout
            .map(|duration| wall_time().checked_add(duration).ok_or(AxError::OutOfRange))
            .transpose()?;
        match block_on_poll_set_until(&context.waiters, deadline, || {
            let state = context.state.lock();
            if state.events.len() >= min_nr || !state.accepting {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        }) {
            Ok(Ok(())) => WaitResult::Ready,
            Ok(Err(AxError::Interrupted)) => WaitResult::Interrupted,
            Ok(Err(error)) => return Err(error),
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
        write_io_event(memory, events.wrapping_add(index), event)?;
    }
    for _ in 0..count {
        state.events.pop_front();
    }
    Ok(count as isize)
}

pub fn sys_io_pgetevents<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ctx: u64,
    min_nr: isize,
    nr: isize,
    events: *mut IoEvent,
    timeout: *const KernelTimespec,
    sigset: *const AioSigset,
) -> AxResult<isize> {
    let sigset = read_optional_sigset(memory, sigset)?;
    with_blocked_signals(sigset, || {
        sys_io_getevents(memory, ctx, min_nr, nr, events, timeout)
    })
}

pub fn sys_io_cancel<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ctx: u64,
    iocb: *const Iocb,
    result: *mut IoEvent,
) -> AxResult<isize> {
    let _ = VmPtr::vm_read(iocb, memory).map_err(map_usercopy_error)?;
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

    static NEXT_AIO_CTX_TEST_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn context_ids_wrap_skip_live_contexts_and_never_use_zero() {
        let _serial = NEXT_AIO_CTX_TEST_LOCK.lock();
        let saved_cursor = NEXT_AIO_CTX.swap(u64::MAX, Ordering::Relaxed);
        let mut manager = AioManager::new();
        let high = manager.allocate_context_id().unwrap();
        assert_eq!(high, u64::MAX);
        manager.contexts.insert(high, Arc::new(context(1)));

        let wrapped = manager.allocate_context_id().unwrap();
        assert_eq!(wrapped, 1);
        assert!(!manager.contexts.contains_key(&wrapped));
        NEXT_AIO_CTX.store(saved_cursor, Ordering::Relaxed);
    }

    #[test]
    fn context_id_exhaustion_leaves_existing_contexts_intact() {
        let _serial = NEXT_AIO_CTX_TEST_LOCK.lock();
        let saved_cursor = NEXT_AIO_CTX.swap(1, Ordering::Relaxed);
        let mut manager = AioManager::new();
        manager.contexts.insert(1, Arc::new(context(1)));
        manager.contexts.insert(2, Arc::new(context(2)));

        assert_eq!(
            manager.allocate_context_id_in_range(2),
            Err(LinuxError::EAGAIN.into())
        );
        assert_eq!(manager.contexts.len(), 2);
        assert!(manager.contexts.contains_key(&1));
        assert!(manager.contexts.contains_key(&2));
        NEXT_AIO_CTX.store(saved_cursor, Ordering::Relaxed);
    }
}
