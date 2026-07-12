use alloc::{
    borrow::Cow,
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    any::Any,
    ops::Deref,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::NodeFlags;
use axpoll::{IoEvents, PollSet, Pollable, RegisterError, RegistrationToken};
use axtask::{WeakAxTaskRef, current, current_may_uninit};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    POLL_ERR, POLL_HUP, POLL_IN, POLL_MSG, POLL_OUT, POLL_PRI, POLLERR, POLLHUP, POLLIN, POLLMSG,
    POLLOUT, POLLPRI, POLLRDBAND, POLLRDNORM, POLLWRBAND, POLLWRNORM, SI_SIGIO,
};
use spin::Mutex;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use thekernel_linux_fd::{ExternalOffset, OfdId, OpenFileDescriptionState};

use super::{
    executable::{self, ExecutableKey},
    fanotify::FanotifyFile,
    flock,
    fs::File,
    lease,
    types::{FileLike, IoDst, IoSrc, Kstat},
};
use crate::{
    deferred_work::DeferredWorkAccount,
    task::{
        AsThread, Cred, Kuid, ProcessData, ProcessGroup, Thread, get_process_data,
        get_process_group, get_visible_task, process_domain, send_queued_signal_thread_inner,
        send_queued_signal_to_process_data, send_signal_thread_inner, send_signal_to_process_data,
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
const DESCRIPTION_CLOSE_WAITER_SLOTS: usize = 64;

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
pub(crate) struct FileDescriptionId(OfdId);

impl FileDescriptionId {
    pub(crate) const fn get(self) -> u64 {
        self.0.get()
    }

    pub(crate) const fn linux_id(self) -> OfdId {
        self.0
    }

    fn allocate() -> AxResult<Self> {
        let raw = FILE_DESCRIPTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        OfdId::new(raw).map(Self).ok_or(AxError::BadState)
    }
}

#[derive(Default)]
struct DescriptorLifetimeState {
    references: usize,
    pending_publications: usize,
    ever_published: bool,
    terminal: bool,
    close_source: Option<Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>>,
}

impl DescriptorLifetimeState {
    fn admit_publication(&mut self) -> AxResult<()> {
        self.references
            .checked_add(self.pending_publications)
            .and_then(|total| total.checked_add(1))
            .ok_or(AxError::TooManyOpenFiles)?;
        self.pending_publications += 1;
        Ok(())
    }

    fn commit_publication(&mut self) {
        // A unique DescriptorPublication is created only after one successful
        // admission, so both exact operations are proven by that charge.
        self.pending_publications -= 1;
        self.references += 1;
        self.ever_published = true;
    }

    fn rollback_publication(&mut self) -> Option<Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>> {
        // The active token uniquely owns one pending charge.
        self.pending_publications -= 1;
        self.terminal_source_if_quiescent()
    }

    fn terminal_source_if_quiescent(
        &mut self,
    ) -> Option<Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>> {
        if self.ever_published
            && self.references == 0
            && self.pending_publications == 0
            && !self.terminal
        {
            self.terminal = true;
            self.close_source.clone()
        } else {
            None
        }
    }
}

/// Admission for publishing one descriptor which refers to an OFD.
///
/// The pending charge prevents a concurrent last close from retiring epoll
/// interests between fd-table admission and publication. Dropping an
/// uncommitted value rolls that charge back and performs terminal notification
/// if it was the last possible publication.
#[must_use = "descriptor publication must be committed or rolled back"]
pub(crate) struct DescriptorPublication {
    description: Arc<FileDescription>,
    active: bool,
}

impl DescriptorPublication {
    pub(crate) fn commit(mut self) {
        let mut lifetime = self.description.descriptor_lifetime.lock();
        lifetime.commit_publication();
        self.active = false;
    }
}

impl Drop for DescriptorPublication {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let close_source = {
            let mut lifetime = self.description.descriptor_lifetime.lock();
            lifetime.rollback_publication()
        };
        if let Some(source) = close_source {
            source.close();
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum DescriptorCloseRegistrationError {
    NoMemory,
    Full,
    Closed,
    TokenSpaceExhausted,
}

/// Owned, generation-safe notification for the terminal descriptor close of
/// one OFD. It does not retain a `FileDescription`, so the observer cannot
/// become a second source of OFD lifetime truth.
pub(crate) struct DescriptorCloseRegistration {
    source: Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>,
    token: Option<RegistrationToken>,
}

impl DescriptorCloseRegistration {
    pub(crate) fn cancel(&mut self) {
        if let Some(token) = self.token.take() {
            self.source.cancel(token);
        }
    }
}

impl Drop for DescriptorCloseRegistration {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenCredentials {
    pub(crate) uid: Kuid,
    pub(crate) euid: Kuid,
    pub(crate) suid: Kuid,
    pub(crate) fsuid: Kuid,
    pub(crate) is_initial_user_ns: bool,
    pub(crate) cgroup_ns_id: u64,
}

impl OpenCredentials {
    pub(crate) fn current() -> Self {
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
            is_initial_user_ns: cred.user_ns().is_initial(),
            cgroup_ns_id: proc_data.cgroup_ns_id(),
        }
    }

    const fn root() -> Self {
        Self {
            uid: Kuid::INITIAL_ROOT,
            euid: Kuid::INITIAL_ROOT,
            suid: Kuid::INITIAL_ROOT,
            fsuid: Kuid::INITIAL_ROOT,
            is_initial_user_ns: true,
            cgroup_ns_id: 0,
        }
    }
}

pub(crate) fn current_file_write_credentials() -> Option<OpenCredentials> {
    current_may_uninit().and_then(|task| {
        task.try_as_thread()
            .and_then(Thread::file_write_credentials)
    })
}

/// Returns the immutable Linux security credential captured by the open file
/// description whose write operation is currently executing.
pub(crate) fn current_file_operation_security_credential() -> Option<Arc<Cred>> {
    current_may_uninit().and_then(|task| {
        task.try_as_thread()
            .and_then(Thread::file_operation_credential)
    })
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
                .and_then(|group| {
                    let domain = process_domain().ok()?;
                    group
                        .any_process(domain.registry(), |process| !process.is_zombie())
                        .ok()
                })
                .unwrap_or(false),
        }
    }

    const fn is_none(&self) -> bool {
        matches!(self, Self::None(_))
    }
}

#[derive(Clone)]
struct AsyncIoCredentials {
    uid: Kuid,
    euid: Kuid,
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
            euid_is_global_root: ids.euid == Kuid::INITIAL_ROOT && cred.user_ns().is_initial(),
        })
    }

    fn may_signal(&self, target: &Cred) -> bool {
        let ids = target.ids();
        self.may_signal_ids(ids.ruid, ids.suid)
    }

    fn may_signal_ids(&self, uid: Kuid, suid: Kuid) -> bool {
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
            let Ok(domain) = process_domain() else {
                error!("process domain unavailable while delivering process-group SIGIO");
                return;
            };
            if let Err(error) = group.for_each_process(domain.registry(), |process| {
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
            }) {
                error!(
                    "failed to enumerate process group {} for SIGIO: {}",
                    group.pgid(),
                    error
                );
            }
        }
    }
}

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    open_credentials: OpenCredentials,
    open_security_credential: Option<Arc<Cred>>,
    /// Immutable lock-free cache of the identity also carried by `ofd`.
    /// Keeping this copy avoids taking the OFD state lock inside dnotify and
    /// flock lock domains; it can never diverge because identities do not
    /// mutate after construction.
    id: FileDescriptionId,
    ofd: Mutex<OpenFileDescriptionState<AsyncIoState, ExternalOffset>>,
    descriptor_lifetime: SpinNoIrq<DescriptorLifetimeState>,
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
        let needs_open_security_credential = inner.downcast_ref::<File>().is_some_and(|file| {
            file.inner()
                .location()
                .flags()
                .contains(NodeFlags::OPEN_CREDENTIAL)
        });
        let open_security_credential = needs_open_security_credential
            .then(|| {
                current_may_uninit()
                    .and_then(|task| task.try_as_thread().map(|thread| thread.current_cred()))
            })
            .flatten();
        Arc::try_new(Self {
            inner,
            open_credentials: OpenCredentials::current(),
            open_security_credential,
            id,
            ofd: Mutex::new(OpenFileDescriptionState::new_external(
                id.linux_id(),
                status_flags,
                AsyncIoState::default(),
            )),
            descriptor_lifetime: SpinNoIrq::new(DescriptorLifetimeState::default()),
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

    pub(crate) fn open_credentials(&self) -> OpenCredentials {
        self.open_credentials
    }

    pub(crate) fn open_security_credential(&self) -> Option<Arc<Cred>> {
        self.open_security_credential.clone()
    }

    pub fn status_flags(&self) -> u32 {
        self.ofd.lock().status_flags()
    }

    pub fn set_status_flags(&self, flags: u32) {
        self.ofd.lock().commit_status_flags(flags);
    }

    pub fn async_io_state(&self) -> AsyncIoState {
        self.ofd.lock().async_owner().clone()
    }

    pub fn set_async_io_owner(&self, owner: AsyncIoOwner) {
        let credentials = (!owner.is_none())
            .then(AsyncIoCredentials::current)
            .flatten();
        let mut ofd = self.ofd.lock();
        let state = ofd.async_owner_mut();
        state.owner = owner;
        state.credentials = credentials;
    }

    pub(crate) fn ensure_async_io_owner(&self, owner: AsyncIoOwner) {
        let credentials = AsyncIoCredentials::current();
        let mut ofd = self.ofd.lock();
        let state = ofd.async_owner_mut();
        if state.owner.is_none() {
            state.credentials = credentials;
            state.owner = owner;
        }
    }

    pub fn set_async_io_signal(&self, signal: u8) {
        self.ofd.lock().async_owner_mut().signal = signal;
    }

    pub(crate) fn mark_open_committed(&self) {
        self.open_committed.store(true, Ordering::Release);
    }

    pub(crate) fn begin_descriptor_publication(
        self: &Arc<Self>,
    ) -> AxResult<DescriptorPublication> {
        let retired_source = {
            let mut lifetime = self.descriptor_lifetime.lock();
            let retired_source = if lifetime.terminal {
                if lifetime.references != 0 || lifetime.pending_publications != 0 {
                    return Err(AxError::BadState);
                }
                // SCM_RIGHTS and other explicitly retained OFD owners may
                // publish a descriptor after the preceding descriptor epoch
                // reached zero. Old epoll watches stay terminal; the new epoch
                // receives a fresh close source and cannot revive stale
                // generation tokens.
                lifetime.terminal = false;
                lifetime.close_source.take()
            } else {
                None
            };
            lifetime.admit_publication()?;
            retired_source
        };
        drop(retired_source);
        Ok(DescriptorPublication {
            description: Arc::clone(self),
            active: true,
        })
    }

    pub(crate) fn descriptor_closed(&self) {
        let close_source = {
            let mut lifetime = self.descriptor_lifetime.lock();
            let Some(references) = lifetime.references.checked_sub(1) else {
                error!("descriptor close observed an unaccounted OFD reference");
                lifetime.terminal = true;
                return;
            };
            lifetime.references = references;
            lifetime.terminal_source_if_quiescent()
        };
        if let Some(source) = close_source {
            source.close();
        }
    }

    fn descriptor_close_source(
        &self,
    ) -> Result<Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>, DescriptorCloseRegistrationError>
    {
        {
            let lifetime = self.descriptor_lifetime.lock();
            if lifetime.terminal {
                return Err(DescriptorCloseRegistrationError::Closed);
            }
            if let Some(source) = lifetime.close_source.as_ref() {
                return Ok(source.clone());
            }
        }

        let candidate =
            Arc::try_new(PollSet::new()).map_err(|_| DescriptorCloseRegistrationError::NoMemory)?;
        let mut lifetime = self.descriptor_lifetime.lock();
        if lifetime.terminal {
            return Err(DescriptorCloseRegistrationError::Closed);
        }
        if let Some(source) = lifetime.close_source.as_ref() {
            Ok(source.clone())
        } else {
            lifetime.close_source = Some(candidate.clone());
            Ok(candidate)
        }
    }

    pub(crate) fn register_descriptor_close(
        &self,
        waker: &core::task::Waker,
    ) -> Result<DescriptorCloseRegistration, DescriptorCloseRegistrationError> {
        let source = self.descriptor_close_source()?;
        let token = source.register(waker).map_err(|error| match error {
            RegisterError::Full => DescriptorCloseRegistrationError::Full,
            RegisterError::Closed => DescriptorCloseRegistrationError::Closed,
            RegisterError::TokenSpaceExhausted => {
                DescriptorCloseRegistrationError::TokenSpaceExhausted
            }
        })?;
        Ok(DescriptorCloseRegistration {
            source,
            token: Some(token),
        })
    }

    #[cfg(test)]
    pub(crate) fn descriptor_reference_count(&self) -> usize {
        self.descriptor_lifetime.lock().references
    }

    #[cfg(test)]
    pub(crate) fn descriptor_pending_publication_count(&self) -> usize {
        self.descriptor_lifetime.lock().pending_publications
    }

    #[cfg(test)]
    pub(crate) fn open_committed_for_test(&self) -> bool {
        self.open_committed.load(Ordering::Acquire)
    }
}

impl Drop for FileDescription {
    fn drop(&mut self) {
        let close_source = {
            let mut lifetime = self.descriptor_lifetime.lock();
            if lifetime.references != 0 || lifetime.pending_publications != 0 {
                error!(
                    "FileDescription dropped with {} descriptor references and {} publications",
                    lifetime.references, lifetime.pending_publications
                );
            }
            lifetime.terminal = true;
            lifetime.close_source.take()
        };
        if let Some(source) = close_source {
            source.close();
        }
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

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        self.inner.register(context, events)
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

struct RestoreOnDrop<T, F: FnMut(T)> {
    previous: Option<T>,
    restore: F,
}

impl<T, F: FnMut(T)> RestoreOnDrop<T, F> {
    fn new(previous: T, restore: F) -> Self {
        Self {
            previous: Some(previous),
            restore,
        }
    }
}

impl<T, F: FnMut(T)> Drop for RestoreOnDrop<T, F> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            (self.restore)(previous);
        }
    }
}

impl<T: ?Sized> FileHandle<T> {
    /// Returns the stable key shared by handles for the same Linux open file
    /// description, including handles reached through `dup` or table cloning.
    pub(crate) fn open_file_description_key(&self) -> u64 {
        self.description.id().get()
    }

    pub fn status_flags(&self) -> u32 {
        self.description.status_flags()
    }

    fn with_security_credential<R>(&self, f: impl FnOnce() -> R) -> R {
        let Some(security_credential) = self.description.open_security_credential() else {
            return f();
        };
        let current = current();
        let thread = current.as_thread();
        let previous = thread.replace_file_operation_credential(Some(security_credential));
        let _guard = RestoreOnDrop::new(previous, |previous| {
            // `replace_file_operation_credential` releases its spin guard
            // before returning the displaced Arc, so its destructor cannot
            // run while the slot is locked.
            drop(thread.replace_file_operation_credential(previous));
        });
        f()
    }

    pub fn with_read_credentials<R>(&self, f: impl FnOnce() -> R) -> R {
        self.with_security_credential(f)
    }

    pub fn with_write_credentials<R>(&self, f: impl FnOnce() -> R) -> R {
        let credentials = self.description.open_credentials();
        let current = current();
        let thread = current.as_thread();
        let previous = thread.replace_file_write_credentials(Some(credentials));
        let _guard = RestoreOnDrop::new(previous, |previous| {
            let _ = thread.replace_file_write_credentials(previous);
        });
        self.with_security_credential(f)
    }
}

#[derive(Clone)]
pub struct FileDescriptor {
    pub description: Arc<FileDescription>,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    #[test]
    fn descriptor_publication_charges_commit_and_rollback_exactly() {
        let mut lifetime = DescriptorLifetimeState::default();
        lifetime.admit_publication().unwrap();
        lifetime.admit_publication().unwrap();
        assert_eq!(lifetime.references, 0);
        assert_eq!(lifetime.pending_publications, 2);

        lifetime.commit_publication();
        assert_eq!(lifetime.references, 1);
        assert_eq!(lifetime.pending_publications, 1);
        assert!(lifetime.ever_published);

        assert!(lifetime.rollback_publication().is_none());
        assert_eq!(lifetime.references, 1);
        assert_eq!(lifetime.pending_publications, 0);
    }

    #[test]
    fn descriptor_publication_rejects_overflow_without_saturation() {
        let mut lifetime = DescriptorLifetimeState {
            references: usize::MAX - 1,
            ..DescriptorLifetimeState::default()
        };

        lifetime.admit_publication().unwrap();
        assert_eq!(lifetime.admit_publication(), Err(AxError::TooManyOpenFiles));
        lifetime.commit_publication();
        assert_eq!(lifetime.references, usize::MAX);
        assert_eq!(lifetime.pending_publications, 0);
    }

    #[test]
    fn final_pending_rollback_retires_an_empty_published_epoch() {
        let mut lifetime = DescriptorLifetimeState {
            ever_published: true,
            ..DescriptorLifetimeState::default()
        };
        lifetime.admit_publication().unwrap();

        assert!(lifetime.rollback_publication().is_none());
        assert_eq!(lifetime.pending_publications, 0);
        assert!(lifetime.terminal);
    }

    #[test]
    fn restore_guard_unwinds_nested_overrides_in_lifo_order() {
        let value = core::cell::Cell::new(1u32);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let previous = value.replace(2);
            let _outer = RestoreOnDrop::new(previous, |previous| value.set(previous));
            {
                let previous = value.replace(3);
                let _inner = RestoreOnDrop::new(previous, |previous| value.set(previous));
                assert_eq!(value.get(), 3);
            }
            assert_eq!(value.get(), 2);
            panic!("exercise unwind restoration");
        }));

        assert!(result.is_err());
        assert_eq!(value.get(), 1);
    }

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
            uid: kuid(1000),
            euid: kuid(1001),
            euid_is_global_root: false,
        };

        assert!(credentials.may_signal_ids(kuid(1000), kuid(2000)));
        assert!(credentials.may_signal_ids(kuid(2000), kuid(1001)));
        assert!(credentials.may_signal_ids(kuid(2000), kuid(1000)));
        assert!(!credentials.may_signal_ids(kuid(2000), kuid(3000)));
    }

    #[test]
    fn sigio_permission_preserves_linux_global_root_euid_rule() {
        let effective_root = AsyncIoCredentials {
            uid: kuid(4000),
            euid: Kuid::INITIAL_ROOT,
            euid_is_global_root: true,
        };
        assert!(effective_root.may_signal_ids(kuid(2000), kuid(3000)));

        let real_root_only = AsyncIoCredentials {
            uid: Kuid::INITIAL_ROOT,
            euid: kuid(4000),
            euid_is_global_root: false,
        };
        assert!(!real_root_only.may_signal_ids(kuid(2000), kuid(3000)));

        let user_namespace_root = AsyncIoCredentials {
            uid: kuid(4000),
            euid: Kuid::INITIAL_ROOT,
            euid_is_global_root: false,
        };
        assert!(!user_namespace_root.may_signal_ids(kuid(2000), kuid(3000)));
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
