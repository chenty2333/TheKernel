use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{
    FileIoCancelOutcome, FileIoCompletion, ImmediateFileIoResult, OwnedFileIoCompletion,
    SubmittedFileIo,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
#[cfg(not(test))]
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{
    CAP_SYS_ADMIN, CAP_SYS_NICE, POLLERR, POLLHUP, POLLIN, POLLMSG, POLLNVAL, POLLOUT, POLLPRI,
    POLLRDBAND, POLLRDHUP, POLLRDNORM, POLLREMOVE, POLLWRBAND, POLLWRNORM,
};
// These accounting tests do not initialize a scheduler/current task. Runtime
// sleeping-lock and wake behavior is exercised by guest tests; host tests use
// the same critical sections with a spin mutex.
#[cfg(test)]
use spin::Mutex;
use thekernel_linux_aio::{AioContextId as AbiAioContextId, AioContextSnapshot, plan_destroy};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::SignalSet;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use super::io::{
    ClassicAioOperation, ClassicAioOwnedPreparation, execute_classic_aio_operation_cancellable,
    prepare_classic_aio_operation, prepare_classic_aio_owned_operation,
};
use crate::{
    async_operation::{AsyncOperation, TerminalClaim},
    file::{FileHandle, FileLike, event::EventFd, get_file_like, get_typed_file},
    mm::{UserMemoryCapability, map_usercopy_error},
    readiness::{block_on_poll_set_uninterruptible, block_on_poll_set_until},
    task::{AsThread, with_blocked_signals},
};

const AIO_MAX_NR_DEFAULT: usize = 0x10000;
const IOCB_CMD_POLL: u16 = 5;
const IOCB_FLAG_RESFD: u32 = 1 << 0;
const IOCB_FLAG_IOPRIO: u32 = 1 << 1;
const KIOCB_KEY: u32 = 0;
const AIO_HARD_MAX_EVENTS: usize = 0x10000000 / size_of::<IoEvent>();

static NEXT_AIO_CTX: AtomicU64 = AtomicU64::new(1);
static AIO_NR: AtomicUsize = AtomicUsize::new(0);
static AIO_MAX_NR: AtomicUsize = AtomicUsize::new(AIO_MAX_NR_DEFAULT);
static AIO_CONTEXTS: Mutex<AioManager> = Mutex::new(AioManager::new());
static CLASSIC_AIO_ENGINE: Mutex<ClassicAioEngine> = Mutex::new(ClassicAioEngine::new());
static CLASSIC_AIO_ENGINE_WAKE: PollSet = PollSet::new();

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
    /// Requests which crossed the asynchronous issue boundary.  A request is
    /// removed by exactly one terminal claimant: its worker, io_cancel, or
    /// teardown.  Keeping this separate from completion delivery is what
    /// makes `io_cancel` race-safe instead of merely searching the CQ.
    requests: BTreeMap<u64, Arc<AioRequest>>,
    in_flight: usize,
    accepting: bool,
    teardown_release: bool,
}

/// One classic-AIO request retained after `io_submit` returns.
///
/// The object owns the exact open-file-description handle for a poll request,
/// so close/reuse of the numeric descriptor cannot change the object being
/// waited on. `operation` is deliberately distinct from the context CQ wait
/// set: it owns the shared cancellation/terminal-claim transition and wakes
/// an armed file-readiness wait immediately on cancellation or teardown.
struct AioRequest {
    iocb: u64,
    data: u64,
    resfd: Option<FileHandle<EventFd>>,
    operation: Arc<AsyncOperation>,
    /// Provider ownership is separate from the context request map.  Every
    /// external provider call first moves the owner out under this lock and
    /// then runs without either AIO lock held.
    provider: Mutex<AioProviderState>,
    owned_completion_target: Arc<Mutex<Option<(Weak<AioContext>, Weak<AioRequest>)>>>,
    /// Ownership handshake for a provider callback racing `io_cancel`.
    /// `CANCEL_PENDING` holds a normal completion until provider `cancel`
    /// reports whether it actually withdrew the request.  Losing cancellation
    /// restores that held result to the normal CQ route; winning cancellation
    /// discards it and leaves `io_cancel` as the sole ECANCELED reporter.
    completion_owner: core::sync::atomic::AtomicU8,
    pending_provider_result: Mutex<Option<i64>>,
}

const COMPLETION_NORMAL: u8 = 0;
const COMPLETION_CANCEL_PENDING: u8 = 1;
const COMPLETION_CANCEL_WON: u8 = 2;

enum AioProviderState {
    Legacy,
    Prepared(axfs::PreparedOwnedFileIo),
    Publishing,
    Submitted(SubmittedFileIo),
    InFlight,
    Terminal,
}

/// The VFS consumes this sink exactly once.  It retains only weak AIO links,
/// so a provider queue can outlive `io_destroy` without keeping its context
/// or CQ alive.
struct AioOwnedCompletion {
    target: Arc<Mutex<Option<(Weak<AioContext>, Weak<AioRequest>)>>>,
}

impl OwnedFileIoCompletion for AioOwnedCompletion {
    fn complete(self: Box<Self>, completion: FileIoCompletion) {
        let target = self.target.lock().clone();
        let Some((context, request)) = target else {
            return;
        };
        let (Some(context), Some(request)) = (context.upgrade(), request.upgrade()) else {
            return;
        };
        *request.provider.lock() = AioProviderState::Terminal;
        let result = match completion.result {
            ImmediateFileIoResult::Completed(bytes) => bytes as i64,
            ImmediateFileIoResult::Cancelled => -LinuxError::ECANCELED.code() as i64,
            ImmediateFileIoResult::Failed(error) => -LinuxError::from(error).code() as i64,
        };
        deliver_owned_completion(&context, &request, result);
    }

    fn into_retry_completion(self: Box<Self>) -> Box<dyn OwnedFileIoCompletion> {
        self
    }
}

fn deliver_owned_completion(context: &AioContext, request: &Arc<AioRequest>, result: i64) {
    if request.completion_owner.load(Ordering::Acquire) != COMPLETION_CANCEL_PENDING {
        if request.completion_owner.load(Ordering::Acquire) == COMPLETION_NORMAL {
            finish_retained_request(context, request, result);
        }
        return;
    }
    let publish_now = {
        let mut held = request.pending_provider_result.lock();
        if request.completion_owner.load(Ordering::Acquire) == COMPLETION_CANCEL_PENDING {
            debug_assert!(held.is_none(), "owned file I/O completed twice");
            *held = Some(result);
            false
        } else {
            request.completion_owner.load(Ordering::Acquire) == COMPLETION_NORMAL
        }
    };
    if publish_now {
        finish_retained_request(context, request, result);
    }
}

/// Shared classic-AIO execution queue.  Kernel worker tasks have no Linux
/// Thread extension, so ioprio is scheduled here as operation metadata rather
/// than being incorrectly installed into a worker-local task context.
struct ClassicAioWork {
    context: Arc<AioContext>,
    request: Arc<AioRequest>,
    operation: ClassicAioOperation,
    published: bool,
}

struct ClassicAioEngine {
    work: VecDeque<ClassicAioWork>,
    workers_reserved: usize,
}

impl ClassicAioEngine {
    const fn new() -> Self {
        Self {
            work: VecDeque::new(),
            workers_reserved: 0,
        }
    }

    fn priority_key(operation: &AsyncOperation) -> (u8, u16) {
        let raw = operation.io_priority();
        let class = ((raw as u32 >> 13) & 0x7) as u8;
        let data = raw & 0x1fff;
        // Linux's RT/BE/IDLE classes are ordered before their data value.
        // CLASS_NONE inherits the submitting task's default at admission and
        // therefore shares the normal best-effort lane.
        let class_rank = match class {
            1 => 0,
            2 | 0 => 1,
            3 => 2,
            _ => 3,
        };
        (class_rank, data)
    }

    fn pop_highest_priority(&mut self) -> Option<ClassicAioWork> {
        let index = self
            .work
            .iter()
            .enumerate()
            .filter(|(_, work)| work.published)
            .min_by_key(|(_, work)| Self::priority_key(&work.request.operation))
            .map(|(index, _)| index)?;
        self.work.remove(index)
    }

    fn has_published_work(&self) -> bool {
        self.work.iter().any(|work| work.published)
    }

    fn remove_context(&mut self, context: &AioContext) {
        self.work
            .retain(|work| !core::ptr::eq(work.context.as_ref(), context));
    }
}

struct AioPollSource<'a> {
    file: &'a FileHandle<dyn FileLike>,
    request: &'a AioRequest,
}

impl Pollable for AioPollSource<'_> {
    fn poll(&self) -> IoEvents {
        self.file.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let mut prepared = axpoll::PreparedPollRegistration::try_new(2)?;
        prepared.arm_nested(|| self.file.register(context, events))?;
        prepared.arm(self.request.operation.waiters(), context.waker())?;
        prepared.commit()
    }
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

fn lifecycle_snapshot(
    context: &AioContext,
    state: &AioContextState,
) -> AxResult<AioContextSnapshot> {
    let capacity = u32::try_from(context.max_events).map_err(|_| AxError::InvalidInput)?;
    let outstanding = state.events.len().saturating_add(state.in_flight);
    let submitted = u32::try_from(outstanding).map_err(|_| AxError::InvalidInput)?;
    AioContextSnapshot::new(context.owner as u64, capacity)
        .map(|snapshot| AioContextSnapshot {
            submitted,
            ..snapshot
        })
        .map_err(|_| AxError::InvalidInput)
}

fn release_context_events(context: &AioContext) {
    let should_release = {
        let mut state = context.state.lock();
        if state.teardown_release && state.in_flight == 0 {
            state.teardown_release = false;
            true
        } else {
            false
        }
    };
    if should_release {
        release_aio_events(context.max_events);
    }
}

/// Completes a retained request.  The map removal is the terminal claim: a
/// cancellation or teardown which got there first makes this late worker a
/// no-op and prevents double eventfd/CQ publication.
fn finish_retained_request(context: &AioContext, request: &Arc<AioRequest>, result: i64) {
    // The request-map ownership and AsyncOperation terminal claim are one
    // linearization point.  Do not claim before acquiring `state`: a
    // concurrent io_cancel used to be able to remove the request between the
    // two steps, leaving a worker with a terminal token but no completion.
    let published = {
        let mut state = context.state.lock();
        let Some(current) = state.requests.get(&request.iocb) else {
            return;
        };
        if !Arc::ptr_eq(current, request) {
            // Pointer collisions cannot happen while a request is retained;
            // retain the explicit branch so a future key representation does
            // not silently let an obsolete worker finish a replacement.
            return;
        }
        let Some(terminal) = request.operation.claim_terminal() else {
            return;
        };
        state
            .requests
            .remove(&request.iocb)
            .expect("terminal claimant retained request map entry");
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.accepting {
            state.events.push_back(IoEvent {
                data: request.data,
                obj: request.iocb,
                res: if terminal == TerminalClaim::Cancelled {
                    -LinuxError::ECANCELED.code() as i64
                } else {
                    result
                },
                res2: 0,
            });
            true
        } else {
            false
        }
    };
    if published && let Some(event) = &request.resfd {
        let _ = event.signal(1);
    }
    context.waiters.wake();
    release_context_events(context);
}

/// Crosses the irreversible provider-issue boundary.  Context ownership,
/// teardown admission, and the operation transition are deliberately one
/// lock order (`AioContextState` then `AsyncOperation`): io_cancel or destroy
/// which removes the request first can never leave a worker issuing it later.
fn begin_retained_request(context: &AioContext, request: &Arc<AioRequest>) -> bool {
    let state = context.state.lock();
    state.accepting
        && state
            .requests
            .get(&request.iocb)
            .is_some_and(|current| Arc::ptr_eq(current, request))
        && request.operation.begin_issue()
}

/// Claims every live request during context teardown.  Teardown must not wait
/// for a provider which cannot be interrupted after it has crossed its I/O
/// boundary: the worker keeps its Arc and becomes a no-op on return, while the
/// context releases its AIO_NR reservation immediately.
fn retire_retained_requests(context: &AioContext) {
    let requests = {
        let mut state = context.state.lock();
        let requests = state.requests.values().cloned().collect::<Vec<_>>();
        for request in &requests {
            let _ = request.operation.request_cancel();
            let claimed = request.operation.claim_terminal();
            debug_assert!(
                claimed.is_some(),
                "live AIO map entry without terminal claim"
            );
        }
        state.requests.clear();
        state.in_flight = 0;
        requests
    };
    for request in requests {
        // Detach provider ownership without an AIO/context lock.  A queued
        // request is returned unpublished; a submitted provider is asked to
        // cancel but teardown never waits for an in-flight backend.
        enum RetiredProvider {
            Prepared(axfs::PreparedOwnedFileIo),
            Submitted(SubmittedFileIo),
            None,
        }
        let provider = {
            let mut state = request.provider.lock();
            match core::mem::replace(&mut *state, AioProviderState::Terminal) {
                AioProviderState::Prepared(prepared) => RetiredProvider::Prepared(prepared),
                AioProviderState::Submitted(submitted) => RetiredProvider::Submitted(submitted),
                _ => RetiredProvider::None,
            }
        };
        match provider {
            RetiredProvider::Prepared(prepared) => {
                let (owned_request, completion) = prepared.abort();
                drop(owned_request);
                drop(completion);
            }
            RetiredProvider::Submitted(submitted) => {
                let _ = submitted.cancel();
            }
            RetiredProvider::None => {}
        }
        request.operation.wake_waiters();
    }
    CLASSIC_AIO_ENGINE.lock().remove_context(context);
    CLASSIC_AIO_ENGINE_WAKE.wake();
    context.waiters.wake();
    release_context_events(context);
}

fn run_poll_request(
    context: Arc<AioContext>,
    request: Arc<AioRequest>,
    file: FileHandle<dyn FileLike>,
    interest: IoEvents,
) {
    let source = AioPollSource {
        file: &file,
        request: &request,
    };
    let result =
        crate::readiness::block_on_poll_io(&source, interest | IoEvents::ALWAYS, false, || {
            if request.operation.cancellation_requested() {
                Ok(-LinuxError::ECANCELED.code() as i64)
            } else {
                let ready = source.poll() & (interest | IoEvents::ALWAYS);
                if ready.is_empty() {
                    Err(AxError::WouldBlock)
                } else if !begin_retained_request(&context, &request) {
                    Ok(-LinuxError::ECANCELED.code() as i64)
                } else {
                    Ok(aio_poll_events_to_linux(ready) as i64)
                }
            }
        })
        .unwrap_or_else(|error| -LinuxError::from(error).code() as i64);
    finish_retained_request(&context, &request, result);
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
    if iocb.aio_reserved2 != 0 || iocb.aio_nbytes > isize::MAX as u64 {
        return Err(AxError::InvalidInput);
    }
    if iocb.aio_flags & !(IOCB_FLAG_RESFD | IOCB_FLAG_IOPRIO) != 0 {
        return Err(AxError::InvalidInput);
    }
    if iocb.aio_flags & IOCB_FLAG_IOPRIO == 0 && iocb.aio_reqprio != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

fn iocb_ioprio(iocb: &Iocb) -> AxResult<u16> {
    if iocb.aio_flags & IOCB_FLAG_IOPRIO == 0 {
        return crate::syscall::current_effective_ioprio();
    }
    let raw = iocb.aio_reqprio as u16;
    let class = (raw as u32 >> 13) & 0x7;
    let level = raw as u32 & 0x7;
    match class {
        0 if level == 0 => Ok(raw),
        1 | 2 | 3 => {
            if class == 1 {
                let cred = current().as_thread().current_cred();
                if !cred.has_effective_capability_in_own_user_ns(CAP_SYS_ADMIN)
                    && !cred.has_effective_capability_in_own_user_ns(CAP_SYS_NICE)
                {
                    return Err(AxError::OperationNotPermitted);
                }
            }
            Ok(raw)
        }
        _ => Err(AxError::InvalidInput),
    }
}

/// Transfers an IOCB poll request to a dedicated task.  Unlike the historical
/// implementation this does not inspect readiness and manufacture a CQE in
/// `io_submit`: the request remains in `AioContextState::requests` until its
/// readiness waiter, cancellation, or teardown wins the terminal claim.
fn submit_poll_request(
    context: &Arc<AioContext>,
    iocb_ptr: *const Iocb,
    iocb: &Iocb,
    resfd: Option<FileHandle<EventFd>>,
    ioprio: u16,
) -> AxResult {
    if iocb.aio_buf > u32::MAX as u64
        || iocb.aio_offset != 0
        || iocb.aio_nbytes != 0
        || iocb.aio_rw_flags != 0
    {
        return Err(AxError::InvalidInput);
    }
    let interest = aio_poll_events_from_linux(iocb.aio_buf as u32);
    let file = get_file_like(iocb.aio_fildes as i32)?;
    let owned_completion_target = Arc::try_new(Mutex::new(None)).map_err(|_| AxError::NoMemory)?;
    let request = Arc::try_new(AioRequest {
        iocb: iocb_ptr as u64,
        data: iocb.aio_data,
        resfd,
        operation: AsyncOperation::new_with_io_priority(ioprio),
        provider: Mutex::new(AioProviderState::Legacy),
        owned_completion_target,
        completion_owner: core::sync::atomic::AtomicU8::new(COMPLETION_NORMAL),
        pending_provider_result: Mutex::new(None),
    })
    .map_err(|_| AxError::NoMemory)?;
    let mut name = String::new();
    name.try_reserve_exact(8).map_err(|_| AxError::NoMemory)?;
    name.push_str("aio-poll");
    {
        let mut state = context.state.lock();
        if !state.accepting {
            return Err(AxError::InvalidInput);
        }
        if state.requests.contains_key(&request.iocb) {
            return Err(AxError::InvalidInput);
        }
        state.requests.insert(request.iocb, request.clone());
    }
    let worker_context = context.clone();
    let worker_request = request.clone();
    if let Err(error) = axtask::try_spawn_with_name(
        move || run_poll_request(worker_context, worker_request, file, interest),
        name,
    ) {
        let mut state = context.state.lock();
        if state
            .requests
            .get(&request.iocb)
            .is_some_and(|current| Arc::ptr_eq(current, &request))
        {
            let _ = request.operation.claim_terminal();
            state.requests.remove(&request.iocb);
        }
        return Err(error);
    }
    Ok(())
}

fn submit_classic_request(
    context: &Arc<AioContext>,
    iocb_ptr: *const Iocb,
    iocb: &Iocb,
    resfd: Option<FileHandle<EventFd>>,
    operation: ClassicAioOperation,
) -> AxResult {
    let ioprio = operation.ioprio();
    let owned_completion_target = Arc::try_new(Mutex::new(None)).map_err(|_| AxError::NoMemory)?;
    let request = Arc::try_new(AioRequest {
        iocb: iocb_ptr as u64,
        data: iocb.aio_data,
        resfd,
        operation: AsyncOperation::new_with_io_priority(ioprio),
        provider: Mutex::new(AioProviderState::Legacy),
        owned_completion_target,
        completion_owner: core::sync::atomic::AtomicU8::new(COMPLETION_NORMAL),
        pending_provider_result: Mutex::new(None),
    })
    .map_err(|_| AxError::NoMemory)?;
    *request.owned_completion_target.lock() =
        Some((Arc::downgrade(context), Arc::downgrade(&request)));
    let completion: Box<dyn OwnedFileIoCompletion> = Box::new(AioOwnedCompletion {
        target: request.owned_completion_target.clone(),
    });
    let owned = match prepare_classic_aio_owned_operation(&operation, completion) {
        Ok(owned) => owned,
        Err((error, _completion)) => return Err(error),
    };
    {
        let mut provider = request.provider.lock();
        match owned {
            ClassicAioOwnedPreparation::Prepared(prepared) => {
                *provider = AioProviderState::Prepared(prepared)
            }
            ClassicAioOwnedPreparation::Unsupported => {
                // NOWAIT is an immediate admitted operation, not a hint to
                // enqueue the old borrowed-buffer worker.  An unconverted
                // provider therefore reports its explicit unsupported mode.
                if operation.nowait() {
                    return Err(AxError::OperationNotSupported);
                }
            }
            ClassicAioOwnedPreparation::Zero => {
                // A zero-length classic read/write succeeds without a user
                // pin or provider publication.  It still uses the normal AIO
                // map/CQ linearization below so synchronous reentry is safe.
                *provider = AioProviderState::Terminal;
            }
        }
    }
    let mut worker_name = String::new();
    worker_name
        .try_reserve_exact(7)
        .map_err(|_| AxError::NoMemory)?;
    worker_name.push_str("aio-io");
    {
        let mut state = context.state.lock();
        if !state.accepting || state.requests.contains_key(&request.iocb) {
            return Err(AxError::InvalidInput);
        }
        state.requests.insert(request.iocb, request.clone());
    }
    if matches!(&*request.provider.lock(), AioProviderState::Terminal) {
        finish_retained_request(context, &request, 0);
        return Ok(());
    }
    let queue_result = {
        let mut engine = CLASSIC_AIO_ENGINE.lock();
        if engine.work.try_reserve(1).is_err() {
            Err(AxError::NoMemory)
        } else {
            engine.work.push_back(ClassicAioWork {
                context: context.clone(),
                request: request.clone(),
                operation,
                published: false,
            });
            engine.workers_reserved = engine.workers_reserved.saturating_add(1);
            Ok(true)
        }
    };
    let start_worker = match queue_result {
        Ok(start_worker) => start_worker,
        Err(error) => {
            let mut state = context.state.lock();
            if state
                .requests
                .get(&request.iocb)
                .is_some_and(|current| Arc::ptr_eq(current, &request))
            {
                let _ = request.operation.claim_terminal();
                state.requests.remove(&request.iocb);
            }
            return Err(error);
        }
    };
    if start_worker {
        if let Err(error) = axtask::try_spawn_with_name(run_classic_aio_engine, worker_name) {
            {
                let mut engine = CLASSIC_AIO_ENGINE.lock();
                if let Some(index) = engine
                    .work
                    .iter()
                    .position(|work| Arc::ptr_eq(&work.request, &request))
                {
                    engine.work.remove(index);
                }
                engine.workers_reserved = engine.workers_reserved.saturating_sub(1);
            }
            let mut state = context.state.lock();
            if state
                .requests
                .get(&request.iocb)
                .is_some_and(|current| Arc::ptr_eq(current, &request))
            {
                let _ = request.operation.claim_terminal();
                state.requests.remove(&request.iocb);
            }
            return Err(error);
        }
    }
    {
        let mut engine = CLASSIC_AIO_ENGINE.lock();
        let Some(work) = engine
            .work
            .iter_mut()
            .find(|work| Arc::ptr_eq(&work.request, &request))
        else {
            return Err(AxError::InvalidInput);
        };
        work.published = true;
    }
    CLASSIC_AIO_ENGINE_WAKE.wake();
    Ok(())
}

fn run_classic_aio_engine() {
    let work = loop {
        let work = {
            let mut engine = CLASSIC_AIO_ENGINE.lock();
            engine.pop_highest_priority()
        };
        if let Some(work) = work {
            break work;
        }
        if CLASSIC_AIO_ENGINE.lock().work.is_empty() {
            let mut engine = CLASSIC_AIO_ENGINE.lock();
            engine.workers_reserved = engine.workers_reserved.saturating_sub(1);
            return;
        }
        if block_on_poll_set_uninterruptible(&CLASSIC_AIO_ENGINE_WAKE, || {
            if CLASSIC_AIO_ENGINE.lock().has_published_work() {
                Ok(())
            } else {
                Err(AxError::WouldBlock)
            }
        })
        .is_err()
        {
            let mut engine = CLASSIC_AIO_ENGINE.lock();
            engine.workers_reserved = engine.workers_reserved.saturating_sub(1);
            return;
        }
    };
    let result = if work.request.operation.cancellation_requested() {
        -LinuxError::ECANCELED.code() as i64
    } else {
        match issue_owned_classic_request(&work.context, &work.request) {
            // Accepted provider-owned work completes through
            // `AioOwnedCompletion`; never manufacture a second CQE here.
            Ok(Err(())) => {
                let mut engine = CLASSIC_AIO_ENGINE.lock();
                engine.workers_reserved = engine.workers_reserved.saturating_sub(1);
                return;
            }
            Ok(Ok(OwnedIssue::Legacy)) => {
                if !begin_retained_request(&work.context, &work.request) {
                    -LinuxError::ECANCELED.code() as i64
                } else {
                    execute_classic_aio_operation_cancellable(
                        work.operation,
                        &work.request.operation,
                    )
                    .map(|value| value as i64)
                    .unwrap_or_else(|error| -LinuxError::from(error).code() as i64)
                }
            }
            Ok(Ok(OwnedIssue::Failed(result))) => result,
            Err(error) => -LinuxError::from(error).code() as i64,
        }
    };
    finish_retained_request(&work.context, &work.request, result);
    let mut engine = CLASSIC_AIO_ENGINE.lock();
    engine.workers_reserved = engine.workers_reserved.saturating_sub(1);
}

/// Publishes an already-reserved owned request without an AIO/context/engine
/// lock.  A synchronous provider completion is allowed to reenter the CQ
/// path; it changes `provider` to `Terminal`, in which case the returned
/// submission control is simply dropped after the callback's unique finish.
enum OwnedIssue {
    Legacy,
    Failed(i64),
}

fn issue_owned_classic_request(
    context: &AioContext,
    request: &Arc<AioRequest>,
) -> AxResult<Result<OwnedIssue, ()>> {
    let prepared = {
        let mut provider = request.provider.lock();
        match core::mem::replace(&mut *provider, AioProviderState::InFlight) {
            AioProviderState::Prepared(prepared) => {
                *provider = AioProviderState::Publishing;
                Some(prepared)
            }
            AioProviderState::Legacy => {
                *provider = AioProviderState::Legacy;
                None
            }
            provider_state @ (AioProviderState::Publishing
            | AioProviderState::Submitted(_)
            | AioProviderState::InFlight
            | AioProviderState::Terminal) => {
                *provider = provider_state;
                return Ok(Err(()));
            }
        }
    };
    let Some(prepared) = prepared else {
        return Ok(Ok(OwnedIssue::Legacy));
    };
    if !begin_retained_request(context, request) {
        let (owned_request, completion) = prepared.abort();
        drop(owned_request);
        drop(completion);
        *request.provider.lock() = AioProviderState::Terminal;
        return Ok(Ok(OwnedIssue::Failed(-LinuxError::ECANCELED.code() as i64)));
    }
    if prepared.is_nowait() {
        return match prepared.try_complete_immediate() {
            Ok(_) => Ok(Err(())),
            Err(error) => {
                let axfs_ng_vfs::FileIoPrepareError {
                    error,
                    request: owned_request,
                    completion,
                } = error;
                drop(owned_request);
                drop(completion);
                *request.provider.lock() = AioProviderState::Terminal;
                Ok(Ok(OwnedIssue::Failed(
                    -LinuxError::from(error).code() as i64
                )))
            }
        };
    }
    match prepared.submit() {
        Ok(submitted) => {
            let mut state = request.provider.lock();
            if matches!(&*state, AioProviderState::Publishing) {
                *state = AioProviderState::Submitted(submitted);
            }
            // If a synchronous completion won, `submitted` is dropped here;
            // its provider has already consumed the one terminal payload.
            Ok(Err(()))
        }
        Err(error) => {
            let axfs_ng_vfs::FileIoPrepareError {
                error,
                request: owned_request,
                completion,
            } = error;
            drop(owned_request);
            drop(completion);
            *request.provider.lock() = AioProviderState::Terminal;
            Ok(Ok(OwnedIssue::Failed(
                -LinuxError::from(error).code() as i64
            )))
        }
    }
}

fn aio_poll_events_from_linux(events: u32) -> IoEvents {
    let mut generic = IoEvents::ALWAYS;
    for (linux, event) in [
        (POLLIN, IoEvents::READABLE),
        (POLLPRI, IoEvents::PRIORITY),
        (POLLOUT, IoEvents::WRITABLE),
        (POLLERR, IoEvents::ERROR),
        (POLLHUP, IoEvents::HANGUP),
        (POLLNVAL, IoEvents::INVALID),
        (POLLRDNORM, IoEvents::READ_NORMAL),
        (POLLRDBAND, IoEvents::READ_BAND),
        (POLLWRNORM, IoEvents::WRITE_NORMAL),
        (POLLWRBAND, IoEvents::WRITE_BAND),
        (POLLMSG, IoEvents::MESSAGE),
        (POLLREMOVE, IoEvents::REMOVED),
        (POLLRDHUP, IoEvents::READ_HANGUP),
    ] {
        if events & linux != 0 {
            generic |= event;
        }
    }
    generic
}

fn aio_poll_events_to_linux(events: IoEvents) -> u32 {
    let mut linux = 0;
    for (event, bit) in [
        (IoEvents::READABLE, POLLIN),
        (IoEvents::PRIORITY, POLLPRI),
        (IoEvents::WRITABLE, POLLOUT),
        (IoEvents::ERROR, POLLERR),
        (IoEvents::HANGUP, POLLHUP),
        (IoEvents::INVALID, POLLNVAL),
        (IoEvents::READ_NORMAL, POLLRDNORM),
        (IoEvents::READ_BAND, POLLRDBAND),
        (IoEvents::WRITE_NORMAL, POLLWRNORM),
        (IoEvents::WRITE_BAND, POLLWRBAND),
        (IoEvents::MESSAGE, POLLMSG),
        (IoEvents::REMOVED, POLLREMOVE),
        (IoEvents::READ_HANGUP, POLLRDHUP),
    ] {
        if events.contains(event) {
            linux |= bit;
        }
    }
    linux
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
    // The ABI owns context-capacity admission; allocation/publication and
    // usercopy rollback remain in the kernel transaction below.
    AioContextSnapshot::new(current_aio_owner() as u64, nr_events as u32)
        .map_err(|_| AxError::InvalidInput)?;
    try_reserve_aio_events(nr_events)?;

    let context = Arc::new(AioContext {
        owner: current_aio_owner(),
        max_events: nr_events,
        state: Mutex::new(AioContextState {
            events: VecDeque::new(),
            requests: BTreeMap::new(),
            in_flight: 0,
            accepting: true,
            teardown_release: false,
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
    let abi_context = AbiAioContextId::new(ctx).ok_or(AxError::InvalidInput)?;
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
        let mut state = context.state.lock();
        let snapshot = lifecycle_snapshot(&context, &state)?;
        plan_destroy(abi_context, snapshot).map_err(|_| AxError::InvalidInput)?;
        state.accepting = false;
        state.teardown_release = true;
        drop(state);
        manager.contexts.remove(&ctx);
        context
    };
    // Context destruction detaches and cancels every retained request before
    // returning.  Waiting here would make exec/exit and io_destroy hostage to
    // a provider that has already entered an uncancellable backend call.
    retire_retained_requests(&context);
    context.state.lock().events.clear();
    release_context_events(&context);
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
    let mut retained = 0usize;
    let completions = VecDeque::new();

    // Slots are reserved as one admission transaction.  A poll request keeps
    // its slot until a terminal claimant removes it; all not-yet-issued and
    // synchronously completed slots are released on the common failure path.
    for index in 0..reserved {
        let ptr = match read_iocb_ptr(memory, iocbpp, index) {
            Ok(ptr) if !ptr.is_null() => ptr,
            Ok(_) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    AxError::BadAddress,
                );
            }
            Err(err) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
        };
        let iocb = match VmPtr::vm_read(ptr, memory).map_err(map_usercopy_error) {
            Ok(iocb) => iocb,
            Err(err) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
        };
        if let Err(err) = validate_iocb_common(&iocb) {
            return fail_io_submit(
                &context,
                reserved.saturating_sub(retained),
                completions,
                submitted,
                err,
            );
        }
        let resfd = match resfd_file(&iocb) {
            Ok(resfd) => resfd,
            Err(err) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
        };
        if let Err(err) = write_iocb_key(memory, ptr) {
            return fail_io_submit(
                &context,
                reserved.saturating_sub(retained),
                completions,
                submitted,
                err,
            );
        }
        let ioprio = match iocb_ioprio(&iocb) {
            Ok(ioprio) => ioprio,
            Err(err) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
        };

        if iocb.aio_lio_opcode == IOCB_CMD_POLL {
            if let Err(err) = submit_poll_request(&context, ptr, &iocb, resfd, ioprio) {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
            retained += 1;
            submitted += 1;
            continue;
        }

        let operation = match prepare_classic_aio_operation(
            capability.clone(),
            iocb.aio_lio_opcode,
            iocb.aio_fildes as i32,
            iocb.aio_buf,
            iocb.aio_nbytes,
            iocb.aio_offset,
            iocb.aio_rw_flags,
            ioprio,
        ) {
            Ok(operation) => operation,
            Err(err) => {
                return fail_io_submit(
                    &context,
                    reserved.saturating_sub(retained),
                    completions,
                    submitted,
                    err,
                );
            }
        };
        if let Err(err) = submit_classic_request(&context, ptr, &iocb, resfd, operation) {
            return fail_io_submit(
                &context,
                reserved.saturating_sub(retained),
                completions,
                submitted,
                err,
            );
        }
        retained += 1;
        submitted += 1;
        continue;
    }

    finish_io_submit(
        &context,
        reserved.saturating_sub(retained),
        completions,
        submitted,
    )
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

    // Copy and consume one event at a time.  A fault in the Nth destination
    // slot must not replay the successfully exposed prefix on the next call;
    // Linux's AIO ring advances exactly with successful usercopy.
    let mut copied = 0usize;
    while copied < count {
        let event = *state.events.front().ok_or(AxError::InvalidInput)?;
        match write_io_event(memory, events.wrapping_add(copied), event) {
            Ok(()) => {
                state.events.pop_front();
                copied += 1;
            }
            Err(_error) if copied != 0 => return Ok(copied as isize),
            Err(error) => return Err(error),
        }
    }
    Ok(copied as isize)
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
    let context = context_for_current(ctx)?;
    let request = context
        .state
        .lock()
        .requests
        .get(&(iocb as u64))
        .cloned()
        .ok_or(AxError::InvalidInput)?;

    enum CancelOwner {
        Legacy,
        Prepared(axfs::PreparedOwnedFileIo),
        Submitted(SubmittedFileIo),
    }
    let owner = {
        let mut provider = request.provider.lock();
        match core::mem::replace(&mut *provider, AioProviderState::InFlight) {
            AioProviderState::Legacy => {
                *provider = AioProviderState::Legacy;
                CancelOwner::Legacy
            }
            AioProviderState::Prepared(prepared) => {
                *provider = AioProviderState::Terminal;
                CancelOwner::Prepared(prepared)
            }
            AioProviderState::Submitted(submitted) => {
                // The provider may synchronously call back from cancel. Hold
                // that result until the provider tells us whether cancellation
                // won; unlike a boolean suppression this cannot lose a normal
                // completion when `cancel` reports InFlight/Terminal.
                if request
                    .completion_owner
                    .compare_exchange(
                        COMPLETION_NORMAL,
                        COMPLETION_CANCEL_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    *provider = AioProviderState::Terminal;
                    return Err(LinuxError::EAGAIN.into());
                }
                CancelOwner::Submitted(submitted)
            }
            state @ (AioProviderState::Publishing
            | AioProviderState::InFlight
            | AioProviderState::Terminal) => {
                *provider = state;
                return Err(LinuxError::EAGAIN.into());
            }
        }
    };

    let cancelled = match owner {
        CancelOwner::Legacy => request.operation.request_cancel(),
        CancelOwner::Prepared(prepared) => {
            let (owned_request, completion) = prepared.abort();
            drop(owned_request);
            drop(completion);
            request.operation.request_cancel()
        }
        CancelOwner::Submitted(submitted) => match submitted.cancel() {
            // The request crossed `begin_issue` before provider publication,
            // so AsyncOperation's cooperative bit intentionally rejects it.
            // A provider that still withdraws its own queued work is the
            // authoritative cancellation edge and io_cancel owns ECANCELED.
            FileIoCancelOutcome::Cancelled => {
                request
                    .completion_owner
                    .store(COMPLETION_CANCEL_WON, Ordering::Release);
                // A callback which arrived while cancellation was pending has
                // already released its pinned request into the held result.
                request.pending_provider_result.lock().take();
                true
            }
            FileIoCancelOutcome::InFlight | FileIoCancelOutcome::Terminal => {
                request
                    .completion_owner
                    .store(COMPLETION_NORMAL, Ordering::Release);
                if let Some(result) = request.pending_provider_result.lock().take() {
                    finish_retained_request(&context, &request, result);
                }
                false
            }
        },
    };
    if !cancelled {
        return Err(LinuxError::EAGAIN.into());
    }
    // No external/provider call occurs below this point.  A synchronous
    // provider callback may already have removed the map entry; that is the
    // expected result of the suppressed-CQ path.
    {
        let mut state = context.state.lock();
        if state
            .requests
            .get(&(iocb as u64))
            .is_some_and(|current| Arc::ptr_eq(current, &request))
        {
            let _ = request.operation.claim_terminal();
            state.requests.remove(&(iocb as u64));
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }
    request.operation.wake_waiters();
    write_io_event(
        memory,
        result,
        IoEvent {
            data: request.data,
            obj: request.iocb,
            res: -LinuxError::ECANCELED.code() as i64,
            res2: 0,
        },
    )?;
    context.waiters.wake();
    release_context_events(&context);
    Ok(0)
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

        {
            let mut state = context.state.lock();
            state.accepting = false;
            state.teardown_release = true;
            state.events.clear();
        }
        retire_retained_requests(&context);
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
                requests: BTreeMap::new(),
                in_flight: 0,
                accepting: true,
                teardown_release: false,
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
