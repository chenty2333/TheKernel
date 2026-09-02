use alloc::{
    borrow::Cow,
    boxed::Box,
    collections::VecDeque,
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ffi::c_int,
    mem::size_of,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::{FileBackend, FileFlags};
use axfs_ng_vfs::{Location, NodeType};
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex as BlockingMutex;
use axtask::current_may_uninit;
use spin::Mutex;
pub use thekernel_linux_fsnotify::{
    ALL_FANOTIFY_EVENT_BITS, FAN_ACCESS, FAN_ACCESS_PERM, FAN_CLASS_PRE_CONTENT, FAN_CLOEXEC,
    FAN_CLOSE, FAN_ENABLE_AUDIT, FAN_EPIDFD, FAN_EVENT_INFO_TYPE_PIDFD, FAN_EVENT_ON_CHILD,
    FAN_MARK_DONT_FOLLOW, FAN_MARK_EVICTABLE, FAN_MARK_FILESYSTEM, FAN_MARK_FLUSH, FAN_MARK_IGNORE,
    FAN_MARK_IGNORED_SURV_MODIFY, FAN_MARK_MOUNT, FAN_MARK_REMOVE, FAN_MODIFY, FAN_NOFD,
    FAN_NONBLOCK, FAN_NOPIDFD, FAN_ONDIR, FAN_OPEN, FAN_OPEN_EXEC, FAN_OPEN_EXEC_PERM,
    FAN_OPEN_PERM, FAN_Q_OVERFLOW, FAN_REPORT_PIDFD, FAN_REPORT_TID, FANOTIFY_FID_BITS,
    FANOTIFY_INIT_FLAGS, FANOTIFY_METADATA_VERSION, FANOTIFY_PERMISSION_CLASSES,
};
use thekernel_linux_fsnotify::{
    FanotifyEventInfoPidfd, FanotifyEventMetadata, FanotifyResponse, FanotifyResponsePlan,
    FanotifyResponseReject,
};

use crate::{
    file::{
        Directory, FdTable, File, FileDescription, FileDescriptionId, FileHandle, FileLike, IoDst,
        IoSrc, Kstat, OfdIoStatus, PidFd, ReservedFd, current_fd_table, get_file_like,
        inotify::WatchKey, reserve_fd,
    },
    readiness::{block_on_poll_io, block_on_poll_set},
    task::{AsThread, get_process_data},
};

pub const MAX_QUEUED_EVENTS: usize = 16384;
pub const MAX_USER_GROUPS: usize = 128;
pub const MAX_USER_MARKS: usize = 1048576;

/// Identity of the task that caused an event. Deferred delivery must carry
/// this copy-only snapshot instead of attributing the event to whichever
/// kernel/user task happens to drain a work queue later.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FanotifyEventActor {
    tid: c_int,
    tgid: c_int,
}

impl FanotifyEventActor {
    pub(crate) fn current() -> Self {
        current_may_uninit()
            .and_then(|task| {
                task.try_as_thread().map(|thread| Self {
                    tid: thread.tid() as c_int,
                    tgid: thread.proc_data.proc.pid() as c_int,
                })
            })
            .unwrap_or_default()
    }

    fn pid_for(self, file: &FanotifyFile) -> c_int {
        if file.flags & FAN_REPORT_TID != 0 {
            self.tid
        } else {
            self.tgid
        }
    }
}

#[derive(Clone, Copy)]
struct FanotifyMark {
    key: WatchKey,
    mask: u64,
    ignored_mask: u64,
    ignored_survives_modify: bool,
    user_flags: u32,
    is_dir: bool,
    scope: FanotifyScope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FanotifyScope {
    Inode,
    Mount(u64),
    Filesystem(u64),
}

struct FanotifyEvent {
    mask: u64,
    fd_loc: Option<Location>,
    permission_id: Option<u64>,
    pid: c_int,
}

struct FanotifyPermissionEvent {
    id: u64,
    fd: Option<FanotifyPermissionFd>,
    response: Option<FanotifyResponsePlan>,
}

/// The event fd as it was published, including the immutable OFD identity.
///
/// The response ABI carries only a numeric fd.  Rechecking this identity when
/// consuming a response prevents a close followed by numeric-fd reuse from
/// answering a different, newer permission event.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FanotifyPermissionFd {
    number: c_int,
    description_id: FileDescriptionId,
}

struct FanotifyState {
    marks: Vec<FanotifyMark>,
    queue: VecDeque<FanotifyEvent>,
    pending_permissions: Vec<FanotifyPermissionEvent>,
    overflowed: bool,
    next_permission_id: u64,
    released: bool,
}

const FANOTIFY_CLEANUP_BUDGET: usize = 64;

struct FanotifyCleanupWork {
    next: AtomicPtr<Self>,
    queue: VecDeque<FanotifyEvent>,
    marks: Vec<FanotifyMark>,
    pending_permissions: Vec<FanotifyPermissionEvent>,
}

static FANOTIFY_CLEANUP_INCOMING: AtomicPtr<FanotifyCleanupWork> = AtomicPtr::new(ptr::null_mut());
static FANOTIFY_CLEANUP_PENDING: AtomicPtr<FanotifyCleanupWork> = AtomicPtr::new(ptr::null_mut());
static FANOTIFY_CLEANUP_DRAINING: AtomicBool = AtomicBool::new(false);
static FANOTIFY_CLEANUP_WORKS: AtomicUsize = AtomicUsize::new(0);

struct FanotifyCleanupDrainGuard;

impl FanotifyCleanupDrainGuard {
    fn try_enter() -> Option<Self> {
        FANOTIFY_CLEANUP_DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for FanotifyCleanupDrainGuard {
    fn drop(&mut self) {
        FANOTIFY_CLEANUP_DRAINING.store(false, Ordering::Release);
    }
}

struct CleanupCreditGuard(bool);

impl CleanupCreditGuard {
    fn reserve() -> AxResult<Self> {
        FANOTIFY_CLEANUP_WORKS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < MAX_USER_GROUPS).then_some(live + 1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        Ok(Self(true))
    }

    fn transfer(mut self) {
        self.0 = false;
    }
}

impl Drop for CleanupCreditGuard {
    fn drop(&mut self) {
        if self.0 {
            FANOTIFY_CLEANUP_WORKS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

pub struct FanotifyFile {
    flags: u32,
    event_f_flags: u32,
    non_blocking: AtomicBool,
    state: Mutex<FanotifyState>,
    read_gate: BlockingMutex<()>,
    cleanup: Mutex<Option<Box<FanotifyCleanupWork>>>,
    poll_rx: PollSet,
}

static FANOTIFY_FILES: Mutex<[Option<Weak<FanotifyFile>>; MAX_USER_GROUPS]> =
    Mutex::new([const { None }; MAX_USER_GROUPS]);
static FANOTIFY_MARKS: AtomicUsize = AtomicUsize::new(0);

fn publish_cleanup_to(incoming: &AtomicPtr<FanotifyCleanupWork>, work: Box<FanotifyCleanupWork>) {
    let raw = Box::into_raw(work);
    let mut head = incoming.load(Ordering::Acquire);
    loop {
        // SAFETY: this producer exclusively owns `raw` until publication.
        unsafe { (*raw).next.store(head, Ordering::Relaxed) };
        match incoming.compare_exchange_weak(head, raw, Ordering::Release, Ordering::Acquire) {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

fn publish_cleanup(work: Box<FanotifyCleanupWork>) {
    publish_cleanup_to(&FANOTIFY_CLEANUP_INCOMING, work);
}

/// Reverses a detached Treiber batch into producer publication order.
///
/// # Safety
///
/// `current` must be a valid, acyclic list exclusively owned by the caller.
unsafe fn reverse_cleanup_batch(mut current: *mut FanotifyCleanupWork) -> *mut FanotifyCleanupWork {
    let mut reversed = ptr::null_mut();
    while !current.is_null() {
        let next = unsafe { (*current).next.load(Ordering::Relaxed) };
        unsafe { (*current).next.store(reversed, Ordering::Relaxed) };
        reversed = current;
        current = next;
    }
    reversed
}

fn refill_pending_cleanup_from(
    incoming: &AtomicPtr<FanotifyCleanupWork>,
    pending: &AtomicPtr<FanotifyCleanupWork>,
) {
    if !pending.load(Ordering::Relaxed).is_null() {
        return;
    }
    let incoming = incoming.swap(ptr::null_mut(), Ordering::AcqRel);
    // SAFETY: the atomic swap detached a finite producer batch. The drainer
    // guard guarantees that no second consumer can traverse it.
    let detached = unsafe { reverse_cleanup_batch(incoming) };
    pending.store(detached, Ordering::Relaxed);
}

fn pop_cleanup_from(
    incoming: &AtomicPtr<FanotifyCleanupWork>,
    pending: &AtomicPtr<FanotifyCleanupWork>,
) -> Option<Box<FanotifyCleanupWork>> {
    if pending.load(Ordering::Relaxed).is_null() {
        refill_pending_cleanup_from(incoming, pending);
    }
    let head = pending.load(Ordering::Relaxed);
    if head.is_null() {
        return None;
    }
    // SAFETY: the drainer guard admits one consumer, and producers only touch
    // the separate INCOMING stack.
    let next = unsafe { (*head).next.load(Ordering::Relaxed) };
    pending.store(next, Ordering::Relaxed);
    unsafe { (*head).next.store(ptr::null_mut(), Ordering::Relaxed) };
    Some(unsafe { Box::from_raw(head) })
}

fn pop_cleanup() -> Option<Box<FanotifyCleanupWork>> {
    pop_cleanup_from(&FANOTIFY_CLEANUP_INCOMING, &FANOTIFY_CLEANUP_PENDING)
}

pub(crate) fn has_deferred_cleanup_work() -> bool {
    !FANOTIFY_CLEANUP_INCOMING.load(Ordering::Acquire).is_null()
        || !FANOTIFY_CLEANUP_PENDING.load(Ordering::Acquire).is_null()
}

pub(crate) fn drain_deferred_cleanup_work() {
    let Some(_guard) = FanotifyCleanupDrainGuard::try_enter() else {
        return;
    };
    let Some(mut work) = pop_cleanup() else {
        return;
    };
    for _ in 0..FANOTIFY_CLEANUP_BUDGET {
        if let Some(event) = work.queue.pop_front() {
            drop(event);
        } else if work.marks.pop().is_some() {
            // `FanotifyMark` is plain `Copy` data, so removing it from the
            // vector is the entire release; there is no destructor to run.
            // It still spends one unit of the drain budget so that a long mark
            // list cannot monopolize a single pass.
        } else if let Some(permission) = work.pending_permissions.pop() {
            drop(permission);
        } else {
            break;
        }
    }
    if work.queue.is_empty() && work.marks.is_empty() && work.pending_permissions.is_empty() {
        drop(work);
        FANOTIFY_CLEANUP_WORKS.fetch_sub(1, Ordering::AcqRel);
    } else {
        publish_cleanup(work);
    }
}

pub fn validate_init_flags(flags: u32, event_f_flags: u32) -> AxResult<()> {
    match thekernel_linux_fsnotify::fanotify_init_admission(flags, event_f_flags) {
        Ok(()) => Ok(()),
        Err(thekernel_linux_fsnotify::FanotifyInitReject::Invalid) => Err(AxError::InvalidInput),
        Err(thekernel_linux_fsnotify::FanotifyInitReject::Unsupported) => {
            Err(AxError::OperationNotSupported)
        }
    }
}

impl FanotifyFile {
    pub fn new(flags: u32, event_f_flags: u32) -> AxResult<Arc<Self>> {
        // Registry slots become reusable as soon as the group Arc dies, while
        // its deferred cleanup can outlive it. A separate transferred credit
        // therefore bounds live groups plus cleanup backlog across churn.
        let cleanup_credit = CleanupCreditGuard::reserve()?;
        let mut queue = VecDeque::new();
        // Preserve one allocation-free FAN_Q_OVERFLOW slot. Every successful
        // event admission below keeps this spare capacity invariant.
        queue.try_reserve_exact(1).map_err(|_| AxError::NoMemory)?;
        let cleanup = Box::try_new(FanotifyCleanupWork {
            next: AtomicPtr::new(ptr::null_mut()),
            queue: VecDeque::new(),
            marks: Vec::new(),
            pending_permissions: Vec::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let file = Arc::try_new(Self {
            flags,
            event_f_flags,
            non_blocking: AtomicBool::new(flags & FAN_NONBLOCK != 0),
            state: Mutex::new(FanotifyState {
                marks: Vec::new(),
                queue,
                pending_permissions: Vec::new(),
                overflowed: false,
                next_permission_id: 1,
                released: false,
            }),
            read_gate: BlockingMutex::new(()),
            cleanup: Mutex::new(Some(cleanup)),
            poll_rx: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        cleanup_credit.transfer();
        let weak = Arc::downgrade(&file);
        let mut files = FANOTIFY_FILES.lock();
        let reusable = files.iter().position(|slot| {
            slot.as_ref()
                .is_none_or(|registered| registered.strong_count() == 0)
        });
        let retired = if let Some(slot) = reusable {
            files[slot].replace(weak)
        } else {
            return Err(AxError::TooManyOpenFiles);
        };
        drop(files);
        drop(retired);
        Ok(file)
    }

    pub fn mark(&self, flags: u32, mask: u64, loc: Option<&Location>) -> AxResult<()> {
        let mut state = self.state.lock();
        let target_is_dir = loc.map(Location::is_dir);
        let plan =
            thekernel_linux_fsnotify::plan_fanotify_mark(flags, mask, self.flags, target_is_dir)
                .map_err(|error| match error {
                    thekernel_linux_fsnotify::FanotifyMarkReject::Invalid => AxError::InvalidInput,
                    thekernel_linux_fsnotify::FanotifyMarkReject::NotDirectory => {
                        AxError::NotADirectory
                    }
                    thekernel_linux_fsnotify::FanotifyMarkReject::IsDirectory => {
                        AxError::IsADirectory
                    }
                })?;
        if plan == thekernel_linux_fsnotify::FanotifyMarkPlan::Flush {
            flush_marks(&mut state, flags);
            return Ok(());
        }

        let loc = loc.ok_or(AxError::BadFileDescriptor)?;
        let key = WatchKey::from_location(loc)?;
        let scope = mark_scope(flags, loc)?;

        match plan {
            thekernel_linux_fsnotify::FanotifyMarkPlan::Ignored => {
                update_ignored_mark(&mut state, key, scope, flags, mask);
            }
            thekernel_linux_fsnotify::FanotifyMarkPlan::Add => {
                add_mark(&mut state, key, scope, flags, mask, loc)?;
            }
            thekernel_linux_fsnotify::FanotifyMarkPlan::Remove => {
                remove_mark(&mut state, key, scope, mask)?;
            }
            thekernel_linux_fsnotify::FanotifyMarkPlan::Flush => unreachable!("handled above"),
        }
        Ok(())
    }

    fn has_events(&self) -> bool {
        let state = self.state.lock();
        !state.released && !state.queue.is_empty()
    }

    fn enqueue_overflow_locked(state: &mut FanotifyState) -> bool {
        if state.overflowed || state.released {
            return false;
        }
        if state.queue.len() == state.queue.capacity() {
            let Some(idx) = state
                .queue
                .iter()
                .rposition(|event| event.permission_id.is_none())
            else {
                return false;
            };
            state.queue.remove(idx);
        }
        state.overflowed = true;
        state.queue.push_back(FanotifyEvent {
            mask: FAN_Q_OVERFLOW,
            fd_loc: None,
            permission_id: None,
            pid: 0,
        });
        true
    }

    fn enqueue_locked(state: &mut FanotifyState, event: FanotifyEvent) -> bool {
        if state.released || state.overflowed {
            return false;
        }
        if matches!(
            thekernel_linux_fsnotify::plan_queue_admission(
                state.queue.len(),
                MAX_QUEUED_EVENTS,
                state.overflowed,
                event.mask == FAN_Q_OVERFLOW,
            ),
            thekernel_linux_fsnotify::QueueAdmission::Overflow
        ) || state.queue.try_reserve(2).is_err()
        {
            return Self::enqueue_overflow_locked(state);
        }
        state.queue.push_back(event);
        true
    }

    fn report_fid(&self) -> bool {
        self.flags & FANOTIFY_FID_BITS != 0
    }

    fn report_pidfd(&self) -> bool {
        self.flags & FAN_REPORT_PIDFD != 0
    }

    pub(in crate::file) fn release(&self) {
        let (queue, marks, pending_permissions) = {
            let mut state = self.state.lock();
            if state.released {
                return;
            }
            state.released = true;
            let queue = core::mem::take(&mut state.queue);
            let marks = core::mem::take(&mut state.marks);
            // Waiters treat a missing id in a released group as FAN_ALLOW, so
            // detach the whole vector in O(1) rather than walking 16K entries
            // from FileDescription::drop.
            let pending_permissions = core::mem::take(&mut state.pending_permissions);
            (queue, marks, pending_permissions)
        };
        let released_marks = marks.len();
        FANOTIFY_MARKS.fetch_sub(released_marks, Ordering::AcqRel);
        if let Some(mut work) = self.cleanup.lock().take() {
            work.queue = queue;
            work.marks = marks;
            work.pending_permissions = pending_permissions;
            publish_cleanup(work);
        } else {
            // This can only happen after a repeated internal release. Keep the
            // fallback outside the state lock so even a violated invariant
            // cannot cascade VFS destruction under a spin mutex.
            drop(queue);
            drop(marks);
            drop(pending_permissions);
        }
        self.poll_rx.wake();
    }

    pub fn fdinfo(&self) -> String {
        let state = self.state.lock();
        let mut out = format!(
            "fanotify flags:{:x} event-flags:{:x}\n",
            self.flags & FANOTIFY_INIT_FLAGS,
            self.event_f_flags
        );
        for mark in &state.marks {
            let mflags = mark.user_flags
                | if mark.ignored_survives_modify {
                    FAN_MARK_IGNORED_SURV_MODIFY
                } else {
                    0
                };
            match mark.scope {
                FanotifyScope::Inode => {
                    out.push_str(&format!(
                        "fanotify ino:{:x} sdev:{:x} mflags:{:x} mask:{:x} ignored_mask:{:x}\n",
                        mark.key.ino,
                        mark.key.dev,
                        mflags,
                        mark.mask as u32,
                        mark.ignored_mask as u32
                    ));
                }
                FanotifyScope::Mount(mnt_id) => {
                    out.push_str(&format!(
                        "fanotify mnt_id:{:x} mflags:{:x} mask:{:x} ignored_mask:{:x}\n",
                        mnt_id, mflags, mark.mask as u32, mark.ignored_mask as u32
                    ));
                }
                FanotifyScope::Filesystem(sdev) => {
                    out.push_str(&format!(
                        "fanotify sdev:{:x} mflags:{:x} mask:{:x} ignored_mask:{:x}\n",
                        sdev, mflags, mark.mask as u32, mark.ignored_mask as u32
                    ));
                }
            }
        }
        out
    }

    fn handle_permission_response(&self, fd: c_int, response: u32) -> AxResult<()> {
        self.handle_permission_response_in_table(&current_fd_table(), fd, response)
    }

    fn handle_permission_response_in_table(
        &self,
        table: &FdTable,
        fd: c_int,
        response: u32,
    ) -> AxResult<()> {
        let response = thekernel_linux_fsnotify::fanotify_response_admission(
            response,
            self.flags & FAN_ENABLE_AUDIT != 0,
            self.flags & FAN_CLASS_PRE_CONTENT != 0,
        )
        .map_err(|reject| match reject {
            FanotifyResponseReject::Invalid
            | FanotifyResponseReject::AuditNotEnabled
            | FanotifyResponseReject::InfoUnsupported => AxError::InvalidInput,
        })?;
        if fd < 0 {
            return Err(AxError::InvalidInput);
        }

        // This lookup is the response linearization point.  Retain the exact
        // OFD through the state update so a concurrent close/reuse after this
        // point cannot turn a valid response into one for a newer descriptor.
        let Some((current, _description)) = current_permission_fd(table, fd) else {
            return Err(LinuxError::ENOENT.into());
        };
        let expected = {
            let state = self.state.lock();
            state
                .pending_permissions
                .iter()
                .find(|event| event.fd == Some(current) && event.response.is_none())
                .and_then(|event| event.fd)
        };
        let Some(expected) = expected else {
            return Err(LinuxError::ENOENT.into());
        };

        let mut state = self.state.lock();
        let Some(event) = state
            .pending_permissions
            .iter_mut()
            .find(|event| event.fd == Some(expected) && event.response.is_none())
        else {
            return Err(LinuxError::ENOENT.into());
        };
        event.response = Some(response);
        drop(state);
        self.poll_rx.wake();
        Ok(())
    }

    /// Completes an event that has already been removed from the read queue.
    /// Linux consumes fanotify events even when the subsequent userspace copy
    /// faults. Permission events must additionally unblock the access which
    /// generated them with a denial instead of leaving an unanswerable request
    /// in `pending_permissions`.
    fn finish_consumed_event(
        &self,
        event: &FanotifyEvent,
        published_fd: Option<c_int>,
        published_description_id: Option<FileDescriptionId>,
        deny_permission: bool,
    ) {
        let mut wake = false;
        let mut state = self.state.lock();
        if event.mask == FAN_Q_OVERFLOW {
            state.overflowed = false;
        }
        if let Some(id) = event.permission_id
            && let Some(pending) = state.pending_permissions.iter_mut().find(|it| it.id == id)
        {
            if deny_permission {
                if pending.response.is_none() {
                    pending.response = Some(FanotifyResponsePlan::Deny { errno: None });
                    wake = true;
                }
            } else if let Some(fd) = published_fd {
                pending.fd = published_description_id.map(|description_id| FanotifyPermissionFd {
                    number: fd,
                    description_id,
                });
                if fd == FAN_NOFD && pending.response.is_none() {
                    pending.response = Some(FanotifyResponsePlan::Allow);
                    wake = true;
                }
            }
        }
        drop(state);
        if wake {
            self.poll_rx.wake();
        }
    }

    fn read_result_after_error(written: usize, error: AxError) -> AxResult<usize> {
        // fanotify_read(2) returns EFAULT even after earlier records were
        // copied. Other late failures follow the usual short-read rule.
        if written == 0 || error == AxError::BadAddress {
            Err(error)
        } else {
            Ok(written)
        }
    }

    /// Drains currently queued records. The outer `read` method supplies the
    /// blocking/read-serialization policy; keeping this core synchronous makes
    /// its dequeue/copy/fd-publication transaction directly testable.
    fn read_ready(&self, dst: &mut IoDst) -> AxResult<usize> {
        let metadata_len = size_of::<FanotifyEventMetadata>();
        let pidfd_info_len = size_of::<FanotifyEventInfoPidfd>();
        if dst.remaining_mut() < metadata_len {
            return Err(AxError::InvalidInput);
        }
        let mut written = 0usize;

        loop {
            let event_len = metadata_len
                + if self.report_pidfd() {
                    pidfd_info_len
                } else {
                    0
                };
            if dst.remaining_mut() < event_len {
                break;
            }
            let Some(event) = self.state.lock().queue.pop_front() else {
                break;
            };
            let event_fd = match prepared_opened_event_fd(self, event.fd_loc.as_ref()) {
                Ok(fd) => fd,
                Err(error) => {
                    self.finish_consumed_event(&event, None, None, true);
                    return Self::read_result_after_error(written, error);
                }
            };
            let pidfd = if self.report_pidfd() {
                Some(prepared_event_pidfd(event.pid))
            } else {
                None
            };
            let metadata = FanotifyEventMetadata {
                event_len: event_len as u32,
                vers: FANOTIFY_METADATA_VERSION,
                reserved: 0,
                metadata_len: metadata_len as u16,
                mask: event.mask,
                fd: event_fd.value(),
                pid: event.pid,
            };
            let mut encoded =
                [0_u8; size_of::<FanotifyEventMetadata>() + size_of::<FanotifyEventInfoPidfd>()];
            let metadata_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&metadata as *const FanotifyEventMetadata).cast::<u8>(),
                    metadata_len,
                )
            };
            encoded[..metadata_len].copy_from_slice(metadata_bytes);
            if let Some(pidfd) = pidfd.as_ref() {
                let info = FanotifyEventInfoPidfd {
                    info_type: FAN_EVENT_INFO_TYPE_PIDFD,
                    pad: 0,
                    len: pidfd_info_len as u16,
                    pidfd: pidfd.value(),
                };
                let info_bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&info as *const FanotifyEventInfoPidfd).cast::<u8>(),
                        pidfd_info_len,
                    )
                };
                encoded[metadata_len..event_len].copy_from_slice(info_bytes);
            }
            let copy_error = match dst.write(&encoded[..event_len]) {
                Ok(copied) if copied == event_len => None,
                Ok(_) => Some(AxError::BadAddress),
                Err(error) => Some(error),
            };
            if let Some(error) = copy_error {
                // Dropping these unpublished reservations exactly rolls back
                // the fd numbers before the denied permission waiter runs.
                drop(pidfd);
                drop(event_fd);
                self.finish_consumed_event(&event, None, None, true);
                return Self::read_result_after_error(written, error);
            }

            // fd publication is deliberately after the complete userspace
            // record copy. ReservedFd makes this allocation-free and prevents
            // another thread from reusing the copied number in between.
            let published_description_id = event_fd.description_id();
            let published_fd = match event_fd.publish() {
                Ok(fd) => fd,
                Err(error) => {
                    error!("fanotify event fd reservation commit failed: {error:?}");
                    FAN_NOFD
                }
            };
            if let Some(pidfd) = pidfd
                && let Err(error) = pidfd.publish()
            {
                error!("fanotify pidfd reservation commit failed: {error:?}");
            }

            self.finish_consumed_event(&event, Some(published_fd), published_description_id, false);
            written += event_len;
        }

        if written == 0 {
            Err(AxError::WouldBlock)
        } else {
            Ok(written)
        }
    }
}

fn current_permission_fd(
    table: &FdTable,
    number: c_int,
) -> Option<(FanotifyPermissionFd, Arc<FileDescription>)> {
    let description = table.get_description(number).ok()?;
    let permission_fd = FanotifyPermissionFd {
        number,
        description_id: description.id(),
    };
    Some((permission_fd, description))
}

impl Drop for FanotifyFile {
    fn drop(&mut self) {
        // A group which never reached FileDescription::drop still owns its
        // preallocated cleanup node directly. Published work owns the credit
        // until the policy worker drains it instead.
        if self.cleanup.get_mut().is_some() {
            FANOTIFY_CLEANUP_WORKS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl FileLike for FanotifyFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(super::anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        let _reader = self.read_gate.lock();
        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
            self.read_ready(dst)
        })
    }

    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        // A NOWAIT reader cannot sleep behind another event consumer.
        let _reader = if status.rwf_nowait() {
            self.read_gate.try_lock().ok_or(AxError::WouldBlock)?
        } else {
            self.read_gate.lock()
        };
        block_on_poll_io(
            self,
            IoEvents::READABLE,
            self.nonblocking() || status.rwf_nowait(),
            || self.read_ready(dst),
        )
    }

    fn write_with_operation_status(
        &self,
        _status: OfdIoStatus,
        src: &mut IoSrc,
    ) -> AxResult<usize> {
        // Permission replies have no readiness wait: validation and response
        // publication are one immediate operation.
        self.write(src)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        let len = src.remaining();
        if len < size_of::<FanotifyResponse>() {
            return Err(AxError::InvalidInput);
        }
        let mut response = [0_u8; size_of::<FanotifyResponse>()];
        src.read(&mut response)?;
        let fd = c_int::from_ne_bytes([response[0], response[1], response[2], response[3]]);
        let response = u32::from_ne_bytes([response[4], response[5], response[6], response[7]]);
        let mut discard = [0_u8; size_of::<FanotifyEventMetadata>()];
        while src.remaining() != 0 {
            let chunk = src.remaining().min(discard.len());
            src.read(&mut discard[..chunk])?;
        }
        self.handle_permission_response(fd, response)?;
        Ok(size_of::<FanotifyResponse>().min(len))
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> AxResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[fanotify]",
        )))
    }
}

impl Pollable for FanotifyFile {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, self.has_events());
        events.set(IoEvents::WRITABLE, true);
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            axpoll::PollRegistration::single(&self.poll_rx, context.waker())
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

fn mark_scope(flags: u32, loc: &Location) -> AxResult<FanotifyScope> {
    let meta = loc.metadata()?;
    if flags & FAN_MARK_MOUNT != 0 {
        Ok(FanotifyScope::Mount(loc.mountpoint().mount_id()))
    } else if flags & FAN_MARK_FILESYSTEM != 0 {
        Ok(FanotifyScope::Filesystem(meta.device))
    } else {
        Ok(FanotifyScope::Inode)
    }
}

fn flush_marks(state: &mut FanotifyState, flags: u32) {
    let previous_len = state.marks.len();
    state.marks.retain(|mark| match mark.scope {
        FanotifyScope::Inode => flags & (FAN_MARK_MOUNT | FAN_MARK_FILESYSTEM) != 0,
        FanotifyScope::Mount(_) => flags & FAN_MARK_MOUNT == 0,
        FanotifyScope::Filesystem(_) => flags & FAN_MARK_FILESYSTEM == 0,
    });
    FANOTIFY_MARKS.fetch_sub(previous_len - state.marks.len(), Ordering::AcqRel);
}

fn add_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    flags: u32,
    mask: u64,
    loc: &Location,
) -> AxResult<()> {
    if let Some(mark) = state
        .marks
        .iter_mut()
        .find(|mark| mark.key == key && mark.scope == scope)
    {
        mark.mask |= mask & ALL_FANOTIFY_EVENT_BITS;
        mark.user_flags |= flags & FAN_MARK_EVICTABLE;
        mark.is_dir = loc.is_dir();
        return Ok(());
    }
    if state.marks.len() >= MAX_USER_MARKS {
        return Err(AxError::StorageFull);
    }
    state.marks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    FANOTIFY_MARKS
        .try_update(Ordering::AcqRel, Ordering::Acquire, |marks| {
            (marks < MAX_USER_MARKS).then_some(marks + 1)
        })
        .map_err(|_| AxError::StorageFull)?;
    state.marks.push(FanotifyMark {
        key,
        mask: mask & ALL_FANOTIFY_EVENT_BITS,
        ignored_mask: 0,
        ignored_survives_modify: flags & FAN_MARK_IGNORED_SURV_MODIFY != 0,
        user_flags: flags & FAN_MARK_EVICTABLE,
        is_dir: loc.is_dir(),
        scope,
    });
    Ok(())
}

fn update_ignored_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    flags: u32,
    mask: u64,
) {
    if let Some(mark) = state
        .marks
        .iter_mut()
        .find(|mark| mark.key == key && mark.scope == scope)
    {
        if flags & FAN_MARK_REMOVE != 0 {
            mark.ignored_mask &= !mask;
        } else {
            mark.ignored_mask |= mask;
            mark.ignored_survives_modify = flags & FAN_MARK_IGNORED_SURV_MODIFY != 0;
            if flags & FAN_MARK_IGNORE != 0 {
                mark.user_flags |= FAN_MARK_IGNORE;
            }
        }
    }
}

fn remove_mark(
    state: &mut FanotifyState,
    key: WatchKey,
    scope: FanotifyScope,
    mask: u64,
) -> AxResult<()> {
    let Some(idx) = state
        .marks
        .iter()
        .position(|mark| mark.key == key && mark.scope == scope)
    else {
        return Err(AxError::InvalidInput);
    };
    state.marks[idx].mask &= !mask;
    state.marks[idx].ignored_mask &= !mask;
    if state.marks[idx].mask == 0 && state.marks[idx].ignored_mask == 0 {
        state.marks.remove(idx);
        FANOTIFY_MARKS.fetch_sub(1, Ordering::AcqRel);
    }
    Ok(())
}

fn fanotify_registry_slots() -> usize {
    MAX_USER_GROUPS
}

fn live_fanotify_file(slot: usize) -> Option<Arc<FanotifyFile>> {
    let mut files = FANOTIFY_FILES.lock();
    let live = files.get(slot)?.as_ref()?.upgrade();
    let retired = if live.is_none() {
        files[slot].take()
    } else {
        None
    };
    drop(files);
    drop(retired);
    live
}

fn each_fanotify_file(mut f: impl FnMut(&Arc<FanotifyFile>)) {
    // Slots are stable and callbacks run after the registry lock is released.
    // New groups appended during a pass are observed by the next event.
    let slots = fanotify_registry_slots();
    for slot in 0..slots {
        if let Some(file) = live_fanotify_file(slot) {
            f(&file);
        }
    }
}

struct PreparedFanotifyFd {
    value: c_int,
    publication: Option<(ReservedFd, Arc<FileDescription>)>,
}

impl PreparedFanotifyFd {
    const fn sentinel(value: c_int) -> Self {
        Self {
            value,
            publication: None,
        }
    }

    const fn value(&self) -> c_int {
        self.value
    }

    fn description_id(&self) -> Option<FileDescriptionId> {
        self.publication
            .as_ref()
            .map(|(_, description)| description.id())
    }

    fn publish(self) -> AxResult<c_int> {
        match self.publication {
            Some((reservation, description)) => reservation.publish(description),
            None => Ok(self.value),
        }
    }
}

fn prepare_readonly_fd(loc: &Location, cloexec: bool) -> AxResult<PreparedFanotifyFd> {
    let node_type = loc.metadata()?.node_type;
    let reservation = reserve_fd(cloexec)?;
    let file: Arc<dyn FileLike> = if node_type == NodeType::Directory {
        Arc::try_new(Directory::new(loc.clone())).map_err(|_| AxError::NoMemory)?
    } else {
        let file = axfs::File::new(FileBackend::Direct(loc.clone()), FileFlags::READ);
        Arc::try_new(File::new(file)).map_err(|_| AxError::NoMemory)?
    };
    let description = FileDescription::new(file)?;
    Ok(PreparedFanotifyFd {
        value: reservation.fd(),
        publication: Some((reservation, description)),
    })
}

fn prepared_opened_event_fd(
    file: &FanotifyFile,
    loc: Option<&Location>,
) -> AxResult<PreparedFanotifyFd> {
    let Some(loc) = loc.filter(|_| !file.report_fid()) else {
        return Ok(PreparedFanotifyFd::sentinel(FAN_NOFD));
    };
    prepare_readonly_fd(
        loc,
        file.event_f_flags & linux_raw_sys::general::O_CLOEXEC != 0,
    )
}

fn prepared_event_pidfd(pid: c_int) -> PreparedFanotifyFd {
    if pid <= 0 {
        return PreparedFanotifyFd::sentinel(FAN_NOPIDFD);
    }
    let Ok(proc_data) = get_process_data(pid as u32) else {
        return PreparedFanotifyFd::sentinel(FAN_NOPIDFD);
    };
    let prepared = (|| {
        let reservation = reserve_fd(true)?;
        let file: Arc<dyn FileLike> =
            Arc::try_new(PidFd::new_process(&proc_data)).map_err(|_| AxError::NoMemory)?;
        let description = FileDescription::new(file)?;
        Ok::<_, AxError>(PreparedFanotifyFd {
            value: reservation.fd(),
            publication: Some((reservation, description)),
        })
    })();
    prepared.unwrap_or_else(|_| PreparedFanotifyFd::sentinel(FAN_EPIDFD))
}

fn fanotify_mark_matches(
    mark: &FanotifyMark,
    event_key: WatchKey,
    event_mount_id: u64,
    watch_key: WatchKey,
    is_dir: bool,
    parent_event: bool,
) -> bool {
    let matched = if parent_event {
        mark.key == watch_key && mark.is_dir && mark.mask & FAN_EVENT_ON_CHILD != 0
    } else {
        mark.key == event_key || scope_matches(mark.scope, event_key, event_mount_id)
    };
    matched && (!is_dir || mark.mask & FAN_ONDIR != 0 || parent_event)
}

fn scope_matches(scope: FanotifyScope, key: WatchKey, mount_id: u64) -> bool {
    match scope {
        FanotifyScope::Inode => false,
        FanotifyScope::Mount(mark_mount_id) => mark_mount_id == mount_id,
        FanotifyScope::Filesystem(dev) => dev == key.dev,
    }
}

fn enqueue_permission_event(
    file: &Arc<FanotifyFile>,
    state: &mut FanotifyState,
    event_loc: &Location,
    mask: u64,
    actor: FanotifyEventActor,
) -> AxResult<u64> {
    if state.released {
        return Err(AxError::Interrupted);
    }
    if state.queue.len() >= MAX_QUEUED_EVENTS
        || state.pending_permissions.len() >= MAX_QUEUED_EVENTS
    {
        return Err(LinuxError::EPERM.into());
    }
    let id = state.next_permission_id;
    let next_permission_id = state
        .next_permission_id
        .checked_add(1)
        .ok_or(AxError::OutOfRange)?;
    state
        .pending_permissions
        .try_reserve(1)
        .map_err(|_| AxError::NoMemory)?;
    // Two spare entries before publication preserve one queue slot for a
    // later non-permission overflow marker.
    state.queue.try_reserve(2).map_err(|_| AxError::NoMemory)?;
    state.next_permission_id = next_permission_id;
    state.pending_permissions.push(FanotifyPermissionEvent {
        id,
        fd: None,
        response: None,
    });
    state.queue.push_back(FanotifyEvent {
        mask,
        fd_loc: Some(event_loc.clone()),
        permission_id: Some(id),
        pid: actor.pid_for(file),
    });
    Ok(id)
}

fn cancel_permission_event(file: &FanotifyFile, id: u64) {
    let mut state = file.state.lock();
    let pending = state
        .pending_permissions
        .iter()
        .position(|event| event.id == id)
        .map(|idx| state.pending_permissions.remove(idx));
    let queued = state
        .queue
        .iter()
        .position(|event| event.permission_id == Some(id))
        .and_then(|idx| state.queue.remove(idx));
    drop(state);
    // Location and any future event-owned resources are destroyed outside the
    // spin lock. One permission id has exactly one queue entry.
    drop(queued);
    drop(pending);
}

fn wait_for_permission_response(
    file: &Arc<FanotifyFile>,
    id: u64,
) -> AxResult<FanotifyResponsePlan> {
    block_on_poll_set(&file.poll_rx, || {
        let mut state = file.state.lock();
        if let Some(idx) = state
            .pending_permissions
            .iter()
            .position(|event| event.id == id && event.response.is_some())
        {
            let event = state.pending_permissions.remove(idx);
            return Ok(event.response.unwrap_or(FanotifyResponsePlan::Allow));
        }
        if !state.pending_permissions.iter().any(|event| event.id == id) || state.released {
            return Ok(FanotifyResponsePlan::Allow);
        }
        Err(AxError::WouldBlock)
    })
}

fn fanotify_denial_error(errno: Option<u8>) -> AxError {
    match errno {
        None | Some(1) => LinuxError::EPERM.into(),
        Some(5) => LinuxError::EIO.into(),
        Some(11) => LinuxError::EAGAIN.into(),
        Some(16) => LinuxError::EBUSY.into(),
        Some(26) => LinuxError::ETXTBSY.into(),
        Some(28) => LinuxError::ENOSPC.into(),
        Some(122) => LinuxError::EDQUOT.into(),
        Some(_) => LinuxError::EPERM.into(),
    }
}

pub(crate) fn permission_check(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
) -> AxResult<()> {
    permission_check_with_actor(
        event_loc,
        watch_loc,
        mask,
        is_dir,
        parent_event,
        FanotifyEventActor::current(),
    )
}

pub(crate) fn permission_check_with_actor(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    let event_key = WatchKey::from_location(event_loc)?;
    let event_mount_id = event_loc.mountpoint().mount_id();
    let watch_key = WatchKey::from_location(watch_loc)?;
    // Linux dispatches pre-content permission groups before content groups.
    // Do not expose a lower-priority group to an operation that a higher
    // priority group already denied; doing so also preserves a pre-content
    // FAN_DENY_ERRNO result rather than replacing it with a later EPERM.
    for class in FANOTIFY_PERMISSION_CLASSES {
        let mut waits: Vec<(Arc<FanotifyFile>, u64)> = Vec::new();
        waits
            .try_reserve(MAX_USER_GROUPS)
            .map_err(|_| AxError::NoMemory)?;

        let slots = fanotify_registry_slots();
        for slot in 0..slots {
            let Some(file) = live_fanotify_file(slot) else {
                continue;
            };
            if file.flags & class == 0 {
                continue;
            }
            let mut state = file.state.lock();
            if state.released {
                continue;
            }
            let should_queue = state.marks.iter().any(|mark| {
                fanotify_mark_matches(
                    mark,
                    event_key,
                    event_mount_id,
                    watch_key,
                    is_dir,
                    parent_event,
                ) && mask & mark.mask != 0
                    && mark.ignored_mask & mask == 0
            });
            if should_queue {
                let id = match enqueue_permission_event(&file, &mut state, event_loc, mask, actor) {
                    Ok(id) => id,
                    Err(error) => {
                        drop(state);
                        for (queued_file, queued_id) in waits.drain(..) {
                            cancel_permission_event(&queued_file, queued_id);
                        }
                        return Err(error);
                    }
                };
                waits.push((file.clone(), id));
            }
            drop(state);
            if should_queue {
                file.poll_rx.wake();
            }
        }

        let mut denial = None;
        for (file, id) in waits {
            match wait_for_permission_response(&file, id) {
                Ok(FanotifyResponsePlan::Allow) => {}
                Ok(FanotifyResponsePlan::Deny { errno }) => {
                    if denial.is_none() {
                        denial = Some(errno);
                    }
                }
                Err(err) => return Err(err),
            }
        }
        if let Some(errno) = denial {
            return Err(fanotify_denial_error(errno));
        }
    }
    Ok(())
}

pub(crate) fn permission_check_file_like(
    file_like: &FileHandle<dyn FileLike>,
    mask: u64,
) -> AxResult<()> {
    permission_check_file_like_with_actor(file_like, mask, FanotifyEventActor::current())
}

pub(crate) fn permission_check_file_like_with_actor(
    file_like: &FileHandle<dyn FileLike>,
    mask: u64,
    actor: FanotifyEventActor,
) -> AxResult<()> {
    permission_check_file_like_with_actor_and_status(
        file_like,
        mask,
        actor,
        file_like.io_status_snapshot(),
    )
}

pub(crate) fn permission_check_file_like_with_actor_and_status(
    file_like: &FileHandle<dyn FileLike>,
    mask: u64,
    actor: FanotifyEventActor,
    status: OfdIoStatus,
) -> AxResult<()> {
    file_like.check_io_status(status)?;
    let loc = if let Some(file) = file_like.downcast_ref::<File>() {
        file.inner().location().clone()
    } else if let Some(dir) = file_like.downcast_ref::<Directory>() {
        dir.inner().clone()
    } else {
        return Ok(());
    };
    permission_check_with_actor(&loc, &loc, mask, loc.is_dir(), false, actor)?;
    Ok(())
}

pub(crate) fn permission_check_fd(fd: c_int, mask: u64) -> AxResult<()> {
    permission_check_file_like(&get_file_like(fd)?, mask)
}

pub(crate) fn notify(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
) {
    notify_with_actor(
        event_loc,
        watch_loc,
        mask,
        is_dir,
        parent_event,
        FanotifyEventActor::current(),
    );
}

pub(crate) fn notify_with_actor(
    event_loc: &Location,
    watch_loc: &Location,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
    actor: FanotifyEventActor,
) {
    let Ok(event_key) = WatchKey::from_location(event_loc) else {
        return;
    };
    let event_mount_id = event_loc.mountpoint().mount_id();
    let Ok(watch_key) = WatchKey::from_location(watch_loc) else {
        return;
    };
    notify_with_keys_and_actor(
        event_loc,
        event_key,
        event_mount_id,
        watch_key,
        mask,
        is_dir,
        parent_event,
        actor,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn notify_with_keys_and_actor(
    event_loc: &Location,
    event_key: WatchKey,
    event_mount_id: u64,
    watch_key: WatchKey,
    mask: u64,
    is_dir: bool,
    parent_event: bool,
    actor: FanotifyEventActor,
) {
    each_fanotify_file(|file| {
        let mut state = file.state.lock();
        if state.released {
            return;
        }
        let mut wake = false;
        let mut mark_index = 0;
        while mark_index < state.marks.len() {
            let mark = state.marks[mark_index];
            if !fanotify_mark_matches(
                &mark,
                event_key,
                event_mount_id,
                watch_key,
                is_dir,
                parent_event,
            ) {
                mark_index += 1;
                continue;
            }
            let event_mask = mask & mark.mask & !FAN_EVENT_ON_CHILD;
            if event_mask == 0 || mark.ignored_mask & event_mask != 0 {
                if event_mask & FAN_MODIFY != 0 && !mark.ignored_survives_modify {
                    state.marks[mark_index].ignored_mask = 0;
                }
                mark_index += 1;
                continue;
            }
            let fd_loc = if event_mask
                & (FAN_ACCESS | FAN_MODIFY | FAN_CLOSE | FAN_OPEN | FAN_OPEN_EXEC)
                != 0
            {
                Some(event_loc.clone())
            } else {
                None
            };
            wake |= FanotifyFile::enqueue_locked(
                &mut state,
                FanotifyEvent {
                    mask: event_mask,
                    fd_loc,
                    permission_id: None,
                    pid: actor.pid_for(file),
                },
            );
            if event_mask & FAN_MODIFY != 0 && !mark.ignored_survives_modify {
                state.marks[mark_index].ignored_mask = 0;
            }
            mark_index += 1;
        }
        drop(state);
        if wake {
            file.poll_rx.wake();
        }
    });
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, collections::VecDeque, vec::Vec};
    use core::{ptr, sync::atomic::AtomicPtr};

    use axerrno::{AxError, AxResult, LinuxError};
    use axio::{IoBufMut, Write};

    use super::{
        FAN_ACCESS, FAN_CLASS_PRE_CONTENT, FAN_NONBLOCK, FAN_OPEN_PERM,
        FANOTIFY_PERMISSION_CLASSES, FanotifyCleanupWork, FanotifyEvent, FanotifyFile,
        FanotifyPermissionEvent, FanotifyPermissionFd, FanotifyResponsePlan, current_permission_fd,
        fanotify_denial_error, pop_cleanup_from, publish_cleanup_to, validate_init_flags,
        wait_for_permission_response,
    };
    use crate::file::{FdTable, FileDescription};

    struct FaultAfterWrites {
        remaining: usize,
        successful_writes: usize,
    }

    impl Write for FaultAfterWrites {
        fn write(&mut self, buf: &[u8]) -> AxResult<usize> {
            if self.successful_writes == 0 {
                return Err(AxError::BadAddress);
            }
            self.successful_writes -= 1;
            self.remaining -= buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> AxResult<()> {
            Ok(())
        }
    }

    impl IoBufMut for FaultAfterWrites {
        fn remaining_mut(&self) -> usize {
            self.remaining
        }
    }

    fn cleanup_work(id: i32) -> Box<FanotifyCleanupWork> {
        let mut queue = VecDeque::new();
        queue.push_back(FanotifyEvent {
            mask: FAN_ACCESS,
            fd_loc: None,
            permission_id: None,
            pid: id,
        });
        Box::new(FanotifyCleanupWork {
            next: AtomicPtr::new(ptr::null_mut()),
            queue,
            marks: Vec::new(),
            pending_permissions: Vec::new(),
        })
    }

    fn cleanup_id(work: &FanotifyCleanupWork) -> i32 {
        work.queue.front().unwrap().pid
    }

    #[test]
    fn cleanup_fifo_snapshot_gives_old_continuation_finite_progress() {
        let incoming = AtomicPtr::new(ptr::null_mut());
        let pending = AtomicPtr::new(ptr::null_mut());
        publish_cleanup_to(&incoming, cleanup_work(1));
        publish_cleanup_to(&incoming, cleanup_work(2));

        let oldest = pop_cleanup_from(&incoming, &pending).unwrap();
        assert_eq!(cleanup_id(&oldest), 1);
        // A partially drained old item is republished before a new producer.
        publish_cleanup_to(&incoming, oldest);
        publish_cleanup_to(&incoming, cleanup_work(3));

        let second = pop_cleanup_from(&incoming, &pending).unwrap();
        assert_eq!(cleanup_id(&second), 2);
        drop(second);
        let continuation = pop_cleanup_from(&incoming, &pending).unwrap();
        assert_eq!(cleanup_id(&continuation), 1);
        drop(continuation);
        let newest = pop_cleanup_from(&incoming, &pending).unwrap();
        assert_eq!(cleanup_id(&newest), 3);
        drop(newest);
        assert!(pop_cleanup_from(&incoming, &pending).is_none());
    }

    #[test]
    fn read_fault_consumes_event_and_denies_permission_after_prior_record() {
        let file = FanotifyFile::new(FAN_NONBLOCK, 0).unwrap();
        let permission_id = 7;
        let event_len = core::mem::size_of::<super::FanotifyEventMetadata>();
        {
            let mut state = file.state.lock();
            state.queue.try_reserve(2).unwrap();
            state.pending_permissions.try_reserve(1).unwrap();
            state.queue.push_back(FanotifyEvent {
                mask: FAN_ACCESS,
                fd_loc: None,
                permission_id: None,
                pid: 1,
            });
            state.queue.push_back(FanotifyEvent {
                mask: FAN_OPEN_PERM,
                fd_loc: None,
                permission_id: Some(permission_id),
                pid: 1,
            });
            state.pending_permissions.push(FanotifyPermissionEvent {
                id: permission_id,
                fd: None,
                response: None,
            });
        }
        let mut dst = FaultAfterWrites {
            remaining: event_len * 2,
            successful_writes: 1,
        };

        assert_eq!(file.read_ready(&mut dst), Err(AxError::BadAddress));
        let state = file.state.lock();
        assert!(state.queue.is_empty());
        assert_eq!(
            state.pending_permissions[0].response,
            Some(FanotifyResponsePlan::Deny { errno: None })
        );
    }

    #[test]
    fn root_enacts_abi_admission_without_redefining_it() {
        assert_eq!(FAN_ACCESS, thekernel_linux_fsnotify::FAN_ACCESS);
        assert_eq!(
            validate_init_flags(thekernel_linux_fsnotify::FAN_UNLIMITED_QUEUE, 0),
            Err(AxError::OperationNotSupported)
        );

        let file = FanotifyFile::new(FAN_NONBLOCK, 0).unwrap();
        let table = FdTable::new().unwrap();
        assert_eq!(
            file.handle_permission_response_in_table(
                &table,
                0,
                thekernel_linux_fsnotify::FAN_DENY | thekernel_linux_fsnotify::FAN_AUDIT,
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn pre_content_deny_errno_is_preserved_for_the_permission_waiter() {
        assert_eq!(
            FANOTIFY_PERMISSION_CLASSES,
            thekernel_linux_fsnotify::FANOTIFY_PERMISSION_CLASSES
        );
        let file = FanotifyFile::new(FAN_NONBLOCK | FAN_CLASS_PRE_CONTENT, 0).unwrap();
        let table = FdTable::new().unwrap();
        let description = FileDescription::new(file.clone()).unwrap();
        let description_id = description.id();
        let event_fd = table.add_at_least(description, 9, 10, false).unwrap();
        {
            let mut state = file.state.lock();
            state.pending_permissions.push(FanotifyPermissionEvent {
                id: 1,
                fd: Some(FanotifyPermissionFd {
                    number: event_fd,
                    description_id,
                }),
                response: None,
            });
        }

        file.handle_permission_response_in_table(
            &table,
            event_fd,
            thekernel_linux_fsnotify::fan_deny_errno(5),
        )
        .unwrap();
        assert_eq!(
            wait_for_permission_response(&file, 1),
            Ok(FanotifyResponsePlan::Deny { errno: Some(5) })
        );
        assert_eq!(fanotify_denial_error(Some(5)), LinuxError::EIO.into());
        let ordinary = FanotifyFile::new(FAN_NONBLOCK, 0).unwrap();
        assert_eq!(
            ordinary.handle_permission_response_in_table(
                &table,
                9,
                thekernel_linux_fsnotify::fan_deny_errno(5),
            ),
            Err(AxError::InvalidInput)
        );
    }

    #[test]
    fn permission_response_rejects_a_closed_event_fd_reused_for_another_ofd() {
        let table = FdTable::new().unwrap();
        let original = FileDescription::new(FanotifyFile::new(FAN_NONBLOCK, 0).unwrap()).unwrap();
        let expected = FanotifyPermissionFd {
            number: 9,
            description_id: original.id(),
        };
        let replacement =
            FileDescription::new(FanotifyFile::new(FAN_NONBLOCK, 0).unwrap()).unwrap();
        table.add_at_least(replacement, 9, 10, false).unwrap();

        assert!(current_permission_fd(&table, 9).map(|(fd, _)| fd) != Some(expected));
    }
}
