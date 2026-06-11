use alloc::collections::{BTreeMap, VecDeque};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::__kernel_off_t;
use starry_vm::{VmMutPtr, VmPtr, vm_read_slice};

use super::{sys_fdatasync, sys_fsync, sys_pread64, sys_preadv, sys_pwrite64, sys_pwritev};
use crate::{
    file::{FileHandle, event::EventFd, get_typed_file},
    mm::IoVec,
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
    max_events: usize,
    events: VecDeque<IoEvent>,
}

struct AioManager {
    contexts: BTreeMap<u64, AioContext>,
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

fn read_iocb_ptr(iocbpp: *const *const Iocb, index: usize) -> AxResult<*const Iocb> {
    Ok(unsafe { iocbpp.wrapping_add(index).vm_read_uninit()?.assume_init() })
}

fn validate_optional_timespec(timeout: *const KernelTimespec) -> AxResult {
    if !timeout.is_null() {
        let _ = timeout.vm_read()?;
    }
    Ok(())
}

fn validate_optional_sigset(sigset: *const AioSigset) -> AxResult {
    if !sigset.is_null() {
        let sigset = sigset.vm_read()?;
        if !sigset.sigmask.is_null() && sigset.sigsetsize > 0 {
            let len = sigset.sigsetsize.min(128);
            let mut buf = [core::mem::MaybeUninit::<u8>::uninit(); 128];
            vm_read_slice(sigset.sigmask, &mut buf[..len])?;
        }
    }
    Ok(())
}

fn resfd_file(iocb: &Iocb) -> AxResult<Option<FileHandle<EventFd>>> {
    if iocb.aio_flags & IOCB_FLAG_RESFD == 0 {
        return Ok(None);
    }

    Ok(Some(get_typed_file::<EventFd>(iocb.aio_resfd as i32)?))
}

fn execute_iocb(iocb: &Iocb) -> AxResult<isize> {
    if iocb.aio_reserved2 != 0 || iocb.aio_rw_flags != 0 {
        return Err(AxError::InvalidInput);
    }
    if iocb.aio_flags & !(IOCB_FLAG_RESFD | IOCB_FLAG_IOPRIO) != 0 {
        return Err(AxError::InvalidInput);
    }

    let fd = iocb.aio_fildes as i32;
    let offset = iocb.aio_offset as __kernel_off_t;
    match iocb.aio_lio_opcode {
        IOCB_CMD_PREAD => sys_pread64(
            fd,
            iocb.aio_buf as *mut u8,
            iocb.aio_nbytes as usize,
            offset,
        ),
        IOCB_CMD_PWRITE => sys_pwrite64(
            fd,
            iocb.aio_buf as *const u8,
            iocb.aio_nbytes as usize,
            offset,
        ),
        IOCB_CMD_FSYNC => sys_fsync(fd),
        IOCB_CMD_FDSYNC => sys_fdatasync(fd),
        IOCB_CMD_NOOP => Ok(0),
        IOCB_CMD_PREADV => sys_preadv(
            fd,
            iocb.aio_buf as *const IoVec,
            iocb.aio_nbytes as usize,
            offset,
        ),
        IOCB_CMD_PWRITEV => sys_pwritev(
            fd,
            iocb.aio_buf as *const IoVec,
            iocb.aio_nbytes as usize,
            offset,
        ),
        IOCB_CMD_POLL => Err(AxError::Unsupported),
        _ => Err(AxError::InvalidInput),
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
        AioContext {
            max_events: nr_events,
            events: VecDeque::new(),
        },
    );
    if let Err(err) = ctxp.vm_write(id) {
        AIO_CONTEXTS.lock().contexts.remove(&id);
        release_aio_events(nr_events);
        return Err(err.into());
    }
    Ok(0)
}

pub fn sys_io_destroy(ctx: u64) -> AxResult<isize> {
    let context = AIO_CONTEXTS
        .lock()
        .contexts
        .remove(&ctx)
        .ok_or(AxError::InvalidInput)?;
    release_aio_events(context.max_events);
    Ok(0)
}

pub fn sys_io_submit(ctx: u64, nr: isize, iocbpp: *const *const Iocb) -> AxResult<isize> {
    if nr < 0 {
        return Err(AxError::InvalidInput);
    }
    if nr == 0 {
        return Ok(0);
    }

    {
        let manager = AIO_CONTEXTS.lock();
        if !manager.contexts.contains_key(&ctx) {
            return Err(AxError::InvalidInput);
        }
    }

    let mut submitted = 0isize;
    let mut completions = VecDeque::new();

    for index in 0..nr as usize {
        let ptr = match read_iocb_ptr(iocbpp, index) {
            Ok(ptr) if !ptr.is_null() => ptr,
            Ok(_) => {
                return if submitted == 0 {
                    Err(AxError::BadAddress)
                } else {
                    Ok(submitted)
                };
            }
            Err(err) => {
                return if submitted == 0 {
                    Err(err)
                } else {
                    Ok(submitted)
                };
            }
        };
        let iocb = match ptr.vm_read() {
            Ok(iocb) => iocb,
            Err(err) => {
                return if submitted == 0 {
                    Err(err.into())
                } else {
                    Ok(submitted)
                };
            }
        };

        let resfd = match resfd_file(&iocb) {
            Ok(resfd) => resfd,
            Err(err) => {
                return if submitted == 0 {
                    Err(err)
                } else {
                    Ok(submitted)
                };
            }
        };
        let res = match execute_iocb(&iocb) {
            Ok(res) => res,
            Err(err) => {
                return if submitted == 0 {
                    Err(err)
                } else {
                    Ok(submitted)
                };
            }
        };
        if let Some(event) = resfd {
            if let Err(err) = event.signal(1) {
                return if submitted == 0 {
                    Err(err)
                } else {
                    Ok(submitted)
                };
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

    let mut manager = AIO_CONTEXTS.lock();
    let context = manager
        .contexts
        .get_mut(&ctx)
        .ok_or(AxError::InvalidInput)?;
    context.events.extend(completions);
    Ok(submitted)
}

pub fn sys_io_getevents(
    ctx: u64,
    min_nr: isize,
    nr: isize,
    events: *mut IoEvent,
    timeout: *const KernelTimespec,
) -> AxResult<isize> {
    validate_optional_timespec(timeout)?;
    if min_nr < 0 || nr < 0 || min_nr > nr {
        return Err(AxError::InvalidInput);
    }

    let mut manager = AIO_CONTEXTS.lock();
    let context = manager
        .contexts
        .get_mut(&ctx)
        .ok_or(AxError::InvalidInput)?;

    let count = (nr as usize).min(context.events.len());
    if count == 0 {
        return Ok(0);
    }

    for index in 0..count {
        let event = context
            .events
            .pop_front()
            .expect("count was bounded by len");
        events.wrapping_add(index).vm_write(event)?;
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
    validate_optional_sigset(sigset)?;
    sys_io_getevents(ctx, min_nr, nr, events, timeout)
}

pub fn sys_io_cancel(ctx: u64, iocb: *const Iocb, result: *mut IoEvent) -> AxResult<isize> {
    let _ = iocb.vm_read()?;
    if result.is_null() {
        return Err(AxError::BadAddress);
    }
    let manager = AIO_CONTEXTS.lock();
    if !manager.contexts.contains_key(&ctx) {
        return Err(AxError::InvalidInput);
    }
    Err(AxError::InvalidInput)
}
