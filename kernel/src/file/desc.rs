use alloc::{
    borrow::Cow,
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    any::Any,
    ops::Deref,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, Pollable};
use axtask::{WeakAxTaskRef, current, current_may_uninit};
use linux_raw_sys::general::{
    POLL_ERR, POLL_HUP, POLL_IN, POLL_MSG, POLL_OUT, POLL_PRI, POLLERR, POLLHUP, POLLIN, POLLMSG,
    POLLOUT, POLLPRI, POLLRDBAND, POLLRDNORM, POLLWRBAND, POLLWRNORM, SI_SIGIO,
};
use spin::Mutex;
use starry_process::{Pid, ProcessGroup};
use starry_signal::{SignalInfo, Signo};

use super::{
    executable::{self, ExecutableKey},
    fanotify::FanotifyFile,
    flock, lease,
    types::{FileLike, IoDst, IoSrc, Kstat},
};
use crate::{
    deferred_work::DeferredWorkAccount,
    task::{
        AsThread, Cred, ProcessData, get_process_data, get_process_group, get_visible_task,
        send_queued_signal_thread_inner, send_queued_signal_to_process_data,
        send_signal_thread_inner, send_signal_to_process_data,
    },
};

static FILE_DESCRIPTION_ID: AtomicU64 = AtomicU64::new(1);
static DESCRIPTION_CLEANUP_INCOMING: AtomicPtr<DescriptionCleanupWork> =
    AtomicPtr::new(ptr::null_mut());
static DESCRIPTION_CLEANUP_PENDING: AtomicPtr<DescriptionCleanupWork> =
    AtomicPtr::new(ptr::null_mut());
static DESCRIPTION_CLEANUP_DRAINING: AtomicBool = AtomicBool::new(false);
static DESCRIPTION_CLEANUP_CREDITS: AtomicUsize = AtomicUsize::new(0);

const FLOCK_RELEASE_BUDGET: usize = 16;
const RECORD_LOCK_RELEASE_BUDGET: usize = 16;
const MAX_LIVE_DESCRIPTION_CLEANUPS: usize = 65_536;

/// A fallibly allocated resource whose lifetime is exactly one open file
/// description. Subsystems use this for state which must survive `dup` but be
/// released on the final OFD close.
pub(crate) type DescriptionResource = Box<dyn Any + Send + Sync>;

/// Preallocated final-OFD policy work.  The final Arc drop only publishes this
/// intrusive node; lock-table scans, destructors, and waiter callbacks run in
/// task context with fixed per-invocation budgets.
struct DescriptionCleanupWork {
    next: AtomicPtr<Self>,
    owner: u64,
    flock_done: bool,
    record_lock_done: bool,
    lease_done: bool,
    write_open_key: Option<ExecutableKey>,
    resource: Option<DescriptionResource>,
    account: Option<Arc<DeferredWorkAccount>>,
}

impl DescriptionCleanupWork {
    fn try_new(owner: u64) -> AxResult<Box<Self>> {
        DESCRIPTION_CLEANUP_CREDITS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < MAX_LIVE_DESCRIPTION_CLEANUPS).then_some(live + 1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        let work = Box::try_new(Self {
            next: AtomicPtr::new(ptr::null_mut()),
            owner,
            flock_done: false,
            record_lock_done: false,
            lease_done: false,
            write_open_key: None,
            resource: None,
            account: None,
        });
        if work.is_err() {
            DESCRIPTION_CLEANUP_CREDITS.fetch_sub(1, Ordering::AcqRel);
        }
        work.map_err(|_| AxError::NoMemory)
    }

    fn run_batch(&mut self) -> bool {
        // Arbitrary subsystem destructors may wake tasks, release VFS objects,
        // or join a worker. They run only in the deferred policy worker, never
        // from the context which happened to drop the final FileDescription.
        drop(self.resource.take());
        executable::release_write_open(self.write_open_key.take());
        if !self.flock_done {
            self.flock_done = flock::release_owner_batch(self.owner, FLOCK_RELEASE_BUDGET);
        }
        if !self.record_lock_done {
            self.record_lock_done =
                flock::release_ofd_owner_batch(self.owner, RECORD_LOCK_RELEASE_BUDGET);
        }
        if !self.lease_done {
            lease::release_owner(self.owner);
            self.lease_done = true;
        }
        self.flock_done && self.record_lock_done && self.lease_done
    }

    fn attach_account(&mut self, account: Arc<DeferredWorkAccount>) {
        if self.account.is_none() && account.begin() {
            self.account = Some(account);
        }
    }
}

impl Drop for DescriptionCleanupWork {
    fn drop(&mut self) {
        executable::release_write_open(self.write_open_key.take());
        DESCRIPTION_CLEANUP_CREDITS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn publish_description_cleanup_to(
    incoming: &AtomicPtr<DescriptionCleanupWork>,
    work: Box<DescriptionCleanupWork>,
) {
    let work = Box::into_raw(work);
    let mut head = incoming.load(Ordering::Acquire);
    loop {
        // SAFETY: this producer owns `work` until the successful publication.
        unsafe { (*work).next.store(head, Ordering::Relaxed) };
        match incoming.compare_exchange_weak(head, work, Ordering::Release, Ordering::Acquire) {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

fn publish_description_cleanup(work: Box<DescriptionCleanupWork>) {
    publish_description_cleanup_to(&DESCRIPTION_CLEANUP_INCOMING, work);
}

fn defer_description_cleanup(mut work: Box<DescriptionCleanupWork>) {
    debug_assert!(work.account.is_none());
    if let Some(account) = current_may_uninit().and_then(|task| {
        task.try_as_thread()
            .map(|thread| thread.deferred_work_account())
    }) {
        work.attach_account(account);
    }
    publish_description_cleanup(work);
}

fn reverse_description_cleanup_list(
    mut current: *mut DescriptionCleanupWork,
) -> *mut DescriptionCleanupWork {
    let mut reversed = ptr::null_mut();
    while !current.is_null() {
        // SAFETY: the drain guard gives this consumer exclusive access to the
        // detached snapshot. Producers can only mutate the incoming head.
        let next = unsafe { (*current).next.load(Ordering::Relaxed) };
        unsafe { (*current).next.store(reversed, Ordering::Relaxed) };
        reversed = current;
        current = next;
    }
    reversed
}

fn pop_description_cleanup() -> Option<Box<DescriptionCleanupWork>> {
    if DESCRIPTION_CLEANUP_PENDING
        .load(Ordering::Relaxed)
        .is_null()
    {
        let incoming = DESCRIPTION_CLEANUP_INCOMING.swap(ptr::null_mut(), Ordering::AcqRel);
        DESCRIPTION_CLEANUP_PENDING.store(
            reverse_description_cleanup_list(incoming),
            Ordering::Relaxed,
        );
    }
    let head = DESCRIPTION_CLEANUP_PENDING.load(Ordering::Relaxed);
    if head.is_null() {
        return None;
    }
    // SAFETY: only the drain-guard owner accesses the pending FIFO.
    let next = unsafe { (*head).next.load(Ordering::Relaxed) };
    DESCRIPTION_CLEANUP_PENDING.store(next, Ordering::Relaxed);
    unsafe { (*head).next.store(ptr::null_mut(), Ordering::Relaxed) };
    // SAFETY: removing the head transfers its unique ownership to this caller.
    Some(unsafe { Box::from_raw(head) })
}

struct DescriptionCleanupDrainGuard;

impl DescriptionCleanupDrainGuard {
    fn try_enter() -> Option<Self> {
        DESCRIPTION_CLEANUP_DRAINING
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for DescriptionCleanupDrainGuard {
    fn drop(&mut self) {
        DESCRIPTION_CLEANUP_DRAINING.store(false, Ordering::Release);
    }
}

pub(crate) fn has_deferred_description_cleanup_work() -> bool {
    !DESCRIPTION_CLEANUP_INCOMING
        .load(Ordering::Acquire)
        .is_null()
        || !DESCRIPTION_CLEANUP_PENDING
            .load(Ordering::Acquire)
            .is_null()
}

/// Runs one bounded final-OFD cleanup batch.  An unfinished node is
/// republished without allocation so the policy worker can yield between
/// batches.
pub(crate) fn drain_deferred_description_cleanup() {
    let Some(_guard) = DescriptionCleanupDrainGuard::try_enter() else {
        return;
    };
    let Some(mut work) = pop_description_cleanup() else {
        return;
    };
    if work.run_batch() {
        if let Some(account) = work.account.take() {
            account.complete();
        }
    } else {
        // This is the same logical item and retains its original actor credit;
        // republishing must not increment the account again.
        publish_description_cleanup(work);
    }
}

/// Host-test adapter for a description which owns only a typed resource.
///
/// Kernel host tests do not initialize the task scheduler required by the
/// flock/lease tables. This helper exercises the real intrusive publication
/// and typed-resource handoff without pretending to validate those unrelated
/// task-context policies. Callers must create an owner with no flock, record
/// lock, lease, executable-write key, or deferred-work account.
#[cfg(test)]
pub(crate) fn drain_deferred_description_resource_only_for_test() {
    let Some(_guard) = DescriptionCleanupDrainGuard::try_enter() else {
        return;
    };
    let Some(mut work) = pop_description_cleanup() else {
        return;
    };
    debug_assert!(work.write_open_key.is_none());
    debug_assert!(work.account.is_none());
    drop(work.resource.take());
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FileDescriptionId(u64);

impl FileDescriptionId {
    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    fn allocate() -> AxResult<Self> {
        FILE_DESCRIPTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map(Self)
            .map_err(|_| AxError::TooManyOpenFiles)
    }
}

scope_local::scope_local! {
    pub static FILE_WRITE_CREDENTIALS: Option<OpenCredentials> = None;
}

#[derive(Clone, Copy, Debug)]
pub struct OpenCredentials {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub cgroup_ns_id: u64,
}

impl OpenCredentials {
    pub fn current() -> Self {
        let Some(task) = current_may_uninit() else {
            return Self::root();
        };
        let Some(thread) = task.try_as_thread() else {
            return Self::root();
        };
        let proc_data = &thread.proc_data;
        let cred = thread.current_cred();
        let ids = cred.ids();
        Self {
            uid: ids.ruid,
            euid: ids.euid,
            suid: ids.suid,
            fsuid: ids.fsuid,
            cgroup_ns_id: proc_data.cgroup_ns_id(),
        }
    }

    const fn root() -> Self {
        Self {
            uid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            cgroup_ns_id: 0,
        }
    }
}

pub fn current_file_write_credentials() -> Option<OpenCredentials> {
    *FILE_WRITE_CREDENTIALS
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncIoOwnerType {
    Tid,
    Pid,
    Pgrp,
}

#[derive(Clone)]
pub enum AsyncIoOwner {
    None(AsyncIoOwnerType),
    Tid { id: Pid, task: WeakAxTaskRef },
    Pid { id: Pid, process: Weak<ProcessData> },
    Pgrp { id: Pid, group: Weak<ProcessGroup> },
}

impl AsyncIoOwner {
    pub(crate) fn tid(id: Pid) -> AxResult<Self> {
        if id == 0 {
            return Ok(Self::None(AsyncIoOwnerType::Tid));
        }
        let task = get_visible_task(id)?;
        Ok(Self::Tid {
            id,
            task: Arc::downgrade(&task),
        })
    }

    pub(crate) fn pid(id: Pid) -> AxResult<Self> {
        if id == 0 {
            return Ok(Self::None(AsyncIoOwnerType::Pid));
        }
        let process = get_process_data(id)?;
        Ok(Self::Pid {
            id,
            process: Arc::downgrade(&process),
        })
    }

    pub(crate) fn pgrp(id: Pid) -> AxResult<Self> {
        if id == 0 {
            return Ok(Self::None(AsyncIoOwnerType::Pgrp));
        }
        let group = get_process_group(id)?;
        Ok(Self::Pgrp {
            id,
            group: Arc::downgrade(&group),
        })
    }

    pub(crate) fn current_process() -> Self {
        let process = current().as_thread().proc_data.clone();
        Self::Pid {
            id: process.proc.pid(),
            process: Arc::downgrade(&process),
        }
    }

    pub(crate) const fn owner_type(&self) -> AsyncIoOwnerType {
        match self {
            Self::None(owner_type) => *owner_type,
            Self::Pid { .. } => AsyncIoOwnerType::Pid,
            Self::Tid { .. } => AsyncIoOwnerType::Tid,
            Self::Pgrp { .. } => AsyncIoOwnerType::Pgrp,
        }
    }

    pub(crate) const fn id(&self) -> Pid {
        match self {
            Self::None(_) => 0,
            Self::Tid { id, .. } | Self::Pid { id, .. } | Self::Pgrp { id, .. } => *id,
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        match self {
            Self::None(_) => false,
            Self::Tid { task, .. } => task
                .upgrade()
                .and_then(|task| task.try_as_thread().map(|thread| !thread.pending_exit()))
                .unwrap_or(false),
            Self::Pid { process, .. } => process
                .upgrade()
                .is_some_and(|process| process.proc.is_live()),
            Self::Pgrp { group, .. } => group
                .upgrade()
                .is_some_and(|group| group.any_process(|process| !process.is_zombie())),
        }
    }

    const fn is_none(&self) -> bool {
        matches!(self, Self::None(_))
    }
}

#[derive(Clone)]
struct AsyncIoCredentials {
    uid: u32,
    euid: u32,
    euid_is_global_root: bool,
}

impl AsyncIoCredentials {
    fn current() -> Option<Self> {
        let task = current_may_uninit()?;
        let thread = task.try_as_thread()?;
        let cred = thread.current_cred();
        let ids = cred.ids();
        Some(Self {
            uid: ids.ruid,
            euid: ids.euid,
            // Credentials are not yet stored as mapped kernel IDs, so retain
            // the namespace fact needed to distinguish GLOBAL_ROOT_UID from
            // UID 0 inside a child user namespace.
            euid_is_global_root: ids.euid == 0 && cred.user_ns().is_initial(),
        })
    }

    fn may_signal(&self, target: &Cred) -> bool {
        let ids = target.ids();
        self.may_signal_ids(ids.ruid, ids.suid)
    }

    fn may_signal_ids(&self, uid: u32, suid: u32) -> bool {
        // Match Linux fs/fcntl.c::sigio_perm(): f_owner snapshots the setter's
        // real/effective kernel UIDs, and delivery admits an exact target
        // real/saved-UID match or the historical global-root effective UID.
        // This deliberately does not substitute kill(2)'s CAP_KILL rule.
        self.euid_is_global_root
            || self.uid == uid
            || self.uid == suid
            || self.euid == uid
            || self.euid == suid
    }
}

#[derive(Clone)]
pub struct AsyncIoState {
    pub owner: AsyncIoOwner,
    pub signal: u8,
    credentials: Option<AsyncIoCredentials>,
}

impl Default for AsyncIoState {
    fn default() -> Self {
        Self {
            owner: AsyncIoOwner::None(AsyncIoOwnerType::Pid),
            signal: 0,
            credentials: None,
        }
    }
}

fn sigio_band(reason: u32) -> u32 {
    match reason {
        POLL_IN => POLLIN | POLLRDNORM,
        POLL_OUT => POLLOUT | POLLWRNORM | POLLWRBAND,
        POLL_MSG => POLLIN | POLLRDNORM | POLLMSG,
        POLL_ERR => POLLERR,
        POLL_PRI => POLLPRI | POLLRDBAND,
        POLL_HUP => POLLHUP | POLLERR,
        _ => 0,
    }
}

fn signal_has_specific_sicodes(signo: Signo) -> bool {
    matches!(
        signo,
        Signo::SIGILL
            | Signo::SIGFPE
            | Signo::SIGSEGV
            | Signo::SIGBUS
            | Signo::SIGTRAP
            | Signo::SIGCHLD
            | Signo::SIGIO
            | Signo::SIGSYS
    )
}

fn sigio_code(signo: Signo, reason: u32) -> i32 {
    if signo != Signo::SIGIO && signal_has_specific_sicodes(signo) {
        SI_SIGIO
    } else {
        reason as i32
    }
}

fn sigio_info(signal: u8, fd: i32, reason: u32) -> SignalInfo {
    if signal == 0 {
        return SignalInfo::new_kernel(Signo::SIGIO);
    }

    let signo = Signo::from_repr(signal).unwrap_or(Signo::SIGIO);
    let mut info = SignalInfo::new_kernel(signo);
    info.set_code(sigio_code(signo, reason));
    unsafe {
        let sigpoll = &mut info.0.__bindgen_anon_1.__bindgen_anon_1._sifields._sigpoll;
        sigpoll._fd = fd;
        sigpoll._band = sigio_band(reason) as _;
    }
    info
}

fn send_sigio_to_process(process: &ProcessData, info: SignalInfo) {
    if info.signo().is_realtime() {
        match send_queued_signal_to_process_data(process, Some(info)) {
            Ok(_) => return,
            Err(AxError::WouldBlock) => {
                // Linux falls back from a saturated F_SETSIG real-time queue
                // to plain SIGIO. Delivering the selected RT number as
                // SI_USER would fabricate a different notification contract.
                let _ = send_signal_to_process_data(
                    process,
                    Some(SignalInfo::new_kernel(Signo::SIGIO)),
                );
            }
            Err(_) => {}
        }
    } else {
        let _ = send_signal_to_process_data(process, Some(info));
    }
}

/// Delivers SIGIO through the stable owner object captured by F_SETOWN.
/// Numeric IDs are retained only for ABI readback and are never looked up
/// again during delivery, so a recycled PID/TID/PGID cannot inherit signals.
pub(crate) fn send_sigio(state: &AsyncIoState, fd: i32, reason: u32) {
    let Some(credentials) = state.credentials.as_ref() else {
        return;
    };
    match &state.owner {
        AsyncIoOwner::None(_) => {}
        AsyncIoOwner::Tid { task, .. } => {
            let Some(task) = task.upgrade() else {
                return;
            };
            let Some(thread) = task.try_as_thread() else {
                return;
            };
            let target_cred = thread.current_cred();
            if thread.pending_exit() || !credentials.may_signal(&target_cred) {
                return;
            }
            let info = sigio_info(state.signal, fd, reason);
            if info.signo().is_realtime() {
                match send_queued_signal_thread_inner(&task, thread, info) {
                    Ok(_) => {}
                    Err(AxError::WouldBlock) => send_signal_thread_inner(
                        &task,
                        thread,
                        SignalInfo::new_kernel(Signo::SIGIO),
                    ),
                    Err(_) => {}
                }
            } else {
                send_signal_thread_inner(&task, thread, info);
            }
        }
        AsyncIoOwner::Pid { process, .. } => {
            let Some(process) = process.upgrade() else {
                return;
            };
            let target_cred = process.group_leader_cred();
            if credentials.may_signal(&target_cred) {
                send_sigio_to_process(&process, sigio_info(state.signal, fd, reason));
            }
        }
        AsyncIoOwner::Pgrp { group, .. } => {
            let Some(group) = group.upgrade() else {
                return;
            };
            group.for_each_process(|process| {
                if process.is_zombie() {
                    return;
                }
                let Ok(process_data) = get_process_data(process.pid()) else {
                    return;
                };
                let target_cred = process_data.group_leader_cred();
                if !Arc::ptr_eq(&process_data.proc, process)
                    || !credentials.may_signal(&target_cred)
                {
                    return;
                }
                send_sigio_to_process(&process_data, sigio_info(state.signal, fd, reason));
            });
        }
    }
}

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    open_credentials: OpenCredentials,
    id: FileDescriptionId,
    status_flags: AtomicU32,
    async_io: Mutex<AsyncIoState>,
    open_committed: AtomicBool,
    notification_work: Option<Box<super::inotify::CloseWork>>,
    cleanup_work: Option<Box<DescriptionCleanupWork>>,
}

struct WriteOpenRollback {
    key: Option<ExecutableKey>,
}

impl WriteOpenRollback {
    fn new(key: Option<ExecutableKey>) -> Self {
        Self { key }
    }

    fn transfer(mut self) -> Option<ExecutableKey> {
        self.key.take()
    }
}

impl Drop for WriteOpenRollback {
    fn drop(&mut self) {
        executable::release_write_open(self.key.take());
    }
}

impl FileDescription {
    pub(crate) fn new(inner: Arc<dyn FileLike>) -> AxResult<Arc<Self>> {
        Self::new_with_flags(inner, 0)
    }

    pub(in crate::file) fn new_with_flags(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner(inner, status_flags, None, None)
    }

    pub(in crate::file) fn new_with_write_open_key_and_resource(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner(inner, status_flags, write_open_key, resource)
    }

    fn new_inner(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
    ) -> AxResult<Arc<Self>> {
        // Before a complete FileDescription exists, this guard owns rollback.
        // Once transferred into the value, ordinary FileDescription::drop owns
        // it even if Arc allocation itself fails.
        let write_open_rollback = WriteOpenRollback::new(write_open_key);
        let notification_work = super::inotify::prepare_description_close(&inner)?;
        let id = FileDescriptionId::allocate()?;
        let mut cleanup_work = DescriptionCleanupWork::try_new(id.get())?;
        let write_open_key = write_open_rollback.transfer();
        cleanup_work.write_open_key = write_open_key;
        cleanup_work.resource = resource;
        Arc::try_new(Self {
            inner,
            open_credentials: OpenCredentials::current(),
            id,
            status_flags: AtomicU32::new(status_flags),
            async_io: Mutex::new(AsyncIoState::default()),
            open_committed: AtomicBool::new(false),
            notification_work,
            cleanup_work: Some(cleanup_work),
        })
        .map_err(|_| AxError::NoMemory)
    }

    pub fn flock_owner(&self) -> u64 {
        self.id.get()
    }

    pub(crate) fn id(&self) -> FileDescriptionId {
        self.id
    }

    pub fn open_credentials(&self) -> OpenCredentials {
        self.open_credentials
    }

    pub fn status_flags(&self) -> u32 {
        self.status_flags.load(Ordering::Relaxed)
    }

    pub fn set_status_flags(&self, flags: u32) {
        self.status_flags.store(flags, Ordering::Relaxed);
    }

    pub fn async_io_state(&self) -> AsyncIoState {
        self.async_io.lock().clone()
    }

    pub fn set_async_io_owner(&self, owner: AsyncIoOwner) {
        let credentials = (!owner.is_none())
            .then(AsyncIoCredentials::current)
            .flatten();
        let mut state = self.async_io.lock();
        state.owner = owner;
        state.credentials = credentials;
    }

    pub(crate) fn ensure_async_io_owner(&self, owner: AsyncIoOwner) {
        let mut state = self.async_io.lock();
        if state.owner.is_none() {
            state.credentials = AsyncIoCredentials::current();
            state.owner = owner;
        }
    }

    pub fn set_async_io_signal(&self, signal: u8) {
        self.async_io.lock().signal = signal;
    }

    pub(crate) fn mark_open_committed(&self) {
        self.open_committed.store(true, Ordering::Release);
    }
}

impl Drop for FileDescription {
    fn drop(&mut self) {
        if self.open_committed.load(Ordering::Acquire)
            && let Some(work) = self.notification_work.take()
        {
            super::inotify::defer_description_close(work);
        }
        if let Some(fanotify) = self.inner.downcast_ref::<FanotifyFile>() {
            fanotify.release();
        }
        if self.open_committed.load(Ordering::Acquire)
            && let Some(work) = self.cleanup_work.take()
        {
            defer_description_cleanup(work);
        }
    }
}

impl FileLike for FileDescription {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.inner.read(dst)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.inner.write(src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        self.inner.stat()
    }

    fn path(&self) -> AxResult<Cow<'_, str>> {
        self.inner.path()
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        self.inner.ioctl(cmd, arg)
    }

    fn nonblocking(&self) -> bool {
        self.inner.nonblocking()
    }

    fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
        self.inner.set_nonblocking(nonblocking)
    }
}

impl Pollable for FileDescription {
    fn poll(&self) -> IoEvents {
        self.inner.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner.register(context, events);
    }
}

pub struct FileHandle<T: ?Sized> {
    pub(in crate::file) description: Arc<FileDescription>,
    pub(in crate::file) file: Arc<T>,
}

impl<T: ?Sized> Clone for FileHandle<T> {
    fn clone(&self) -> Self {
        Self {
            description: self.description.clone(),
            file: self.file.clone(),
        }
    }
}

impl<T: ?Sized> Deref for FileHandle<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.file.as_ref()
    }
}

impl<T: ?Sized> AsRef<T> for FileHandle<T> {
    fn as_ref(&self) -> &T {
        self.file.as_ref()
    }
}

impl<T: ?Sized> FileHandle<T> {
    pub fn status_flags(&self) -> u32 {
        self.description.status_flags()
    }

    pub fn with_write_credentials<R>(&self, f: impl FnOnce() -> R) -> R {
        let credentials = self.description.open_credentials();
        let previous = current().as_thread().with_mut_scope(|scope| {
            let mut slot = FILE_WRITE_CREDENTIALS.scope_mut(scope);
            let previous = *slot;
            *slot = Some(credentials);
            previous
        });
        let result = f();
        current().as_thread().with_mut_scope(|scope| {
            *FILE_WRITE_CREDENTIALS.scope_mut(scope) = previous;
        });
        result
    }
}

#[derive(Clone)]
pub struct FileDescriptor {
    pub description: Arc<FileDescription>,
    pub cloexec: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pop_local_cleanup(head: &mut *mut DescriptionCleanupWork) -> Box<DescriptionCleanupWork> {
        let current = *head;
        assert!(!current.is_null());
        // SAFETY: the test owns every node in this detached local list.
        *head = unsafe { (*current).next.load(Ordering::Relaxed) };
        unsafe { (*current).next.store(ptr::null_mut(), Ordering::Relaxed) };
        // SAFETY: removing the node from the local list restores unique Box
        // ownership.
        unsafe { Box::from_raw(current) }
    }

    #[test]
    fn cleanup_republish_keeps_one_credit_and_fifo_snapshot_fairness() {
        let account = Arc::try_new(DeferredWorkAccount::new()).unwrap();
        let incoming = AtomicPtr::new(ptr::null_mut());

        let mut oldest = DescriptionCleanupWork::try_new(101).unwrap();
        oldest.attach_account(account.clone());
        // Attaching again must not begin another logical item.
        oldest.attach_account(account.clone());
        assert!(account.has_pending());
        publish_description_cleanup_to(&incoming, oldest);

        publish_description_cleanup_to(&incoming, DescriptionCleanupWork::try_new(202).unwrap());
        let snapshot = incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut pending = reverse_description_cleanup_list(snapshot);
        let oldest = pop_local_cleanup(&mut pending);
        assert_eq!(oldest.owner, 101);

        // An unfinished oldest item is republished before a continuous newer
        // arrival. Once the already detached snapshot is consumed, reversal
        // must put the unfinished old item ahead of the newer arrival.
        publish_description_cleanup_to(&incoming, oldest);
        for owner in 303..311 {
            publish_description_cleanup_to(
                &incoming,
                DescriptionCleanupWork::try_new(owner).unwrap(),
            );
        }
        let first_new = pop_local_cleanup(&mut pending);
        assert_eq!(first_new.owner, 202);
        drop(first_new);
        assert!(pending.is_null());

        let snapshot = incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        let mut pending = reverse_description_cleanup_list(snapshot);
        let mut oldest = pop_local_cleanup(&mut pending);
        assert_eq!(oldest.owner, 101);
        for expected in 303..311 {
            let later = pop_local_cleanup(&mut pending);
            assert_eq!(later.owner, expected);
            drop(later);
        }
        assert!(pending.is_null());

        // Multiple batches still represent one account item: one completion
        // must bring the counter back to zero.
        let credited = oldest.account.take().unwrap();
        credited.complete();
        assert!(!account.has_pending());
        drop(oldest);
    }

    #[test]
    fn stale_owner_keeps_abi_id_without_numeric_relookup() {
        let owner = AsyncIoOwner::Pid {
            id: 41,
            process: Weak::new(),
        };

        assert_eq!(owner.id(), 41);
        assert_eq!(owner.owner_type(), AsyncIoOwnerType::Pid);
        assert!(!owner.is_live());
    }

    #[test]
    fn sigio_permission_uses_linux_fown_uid_snapshot() {
        let credentials = AsyncIoCredentials {
            uid: 1000,
            euid: 1001,
            euid_is_global_root: false,
        };

        assert!(credentials.may_signal_ids(1000, 2000));
        assert!(credentials.may_signal_ids(2000, 1001));
        assert!(credentials.may_signal_ids(2000, 1000));
        assert!(!credentials.may_signal_ids(2000, 3000));
    }

    #[test]
    fn sigio_permission_preserves_linux_global_root_euid_rule() {
        let effective_root = AsyncIoCredentials {
            uid: 4000,
            euid: 0,
            euid_is_global_root: true,
        };
        assert!(effective_root.may_signal_ids(2000, 3000));

        let real_root_only = AsyncIoCredentials {
            uid: 0,
            euid: 4000,
            euid_is_global_root: false,
        };
        assert!(!real_root_only.may_signal_ids(2000, 3000));

        let user_namespace_root = AsyncIoCredentials {
            uid: 4000,
            euid: 0,
            euid_is_global_root: false,
        };
        assert!(!user_namespace_root.may_signal_ids(2000, 3000));
    }

    #[test]
    fn sigio_reason_is_converted_to_poll_band_bitmap() {
        assert_eq!(sigio_band(POLL_IN), POLLIN | POLLRDNORM);
        assert_eq!(sigio_band(POLL_MSG), POLLIN | POLLRDNORM | POLLMSG);
        assert_ne!(sigio_band(POLL_MSG), POLL_MSG);
    }

    #[test]
    fn queued_sigio_code_matches_linux_signal_specific_rules() {
        assert_eq!(sigio_code(Signo::SIGIO, POLL_MSG), POLL_MSG as i32);
        assert_eq!(sigio_code(Signo::SIGRT1, POLL_IN), POLL_IN as i32);
        assert_eq!(sigio_code(Signo::SIGCHLD, POLL_IN), SI_SIGIO);
    }

    #[test]
    fn empty_owner_preserves_requested_f_setown_ex_type() {
        assert_eq!(
            AsyncIoOwner::pgrp(0).unwrap().owner_type(),
            AsyncIoOwnerType::Pgrp
        );
        assert_eq!(
            AsyncIoOwner::tid(0).unwrap().owner_type(),
            AsyncIoOwnerType::Tid
        );
        assert_eq!(
            AsyncIoOwner::pid(0).unwrap().owner_type(),
            AsyncIoOwnerType::Pid
        );
    }
}
