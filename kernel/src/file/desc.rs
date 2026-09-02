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
use axfs_ng_vfs::{NodeFlags, WritebackErrorState};
use axpoll::{IoEvents, PollSet, Pollable, RegisterError, RegistrationToken};
#[cfg(not(test))]
use axsync::Mutex as StatusTransitionMutex;
use axtask::{WeakAxTaskRef, current, current_may_uninit};
use kspin::SpinNoIrq;
use linux_raw_sys::general::{
    O_APPEND, O_DIRECTORY, O_NONBLOCK, O_PATH, POLL_ERR, POLL_HUP, POLL_IN, POLL_MSG, POLL_OUT,
    POLL_PRI, POLLERR, POLLHUP, POLLIN, POLLMSG, POLLOUT, POLLPRI, POLLRDBAND, POLLRDNORM,
    POLLWRBAND, POLLWRNORM, SI_SIGIO,
};
use spin::Mutex;
use thekernel_linux_fd::{ExternalOffset, OfdId, OpenFileDescriptionState};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{SignalInfo, SignalPollPayload, Signo};

use super::{
    executable::{self, ExecutableKey},
    fanotify::{FanotifyEventActor, FanotifyFile},
    flock,
    fs::File,
    lease,
    permission::VfsSecurityContext,
    types::{FileLike, FileMmapRequest, IoDst, IoSrc, IoctlContext, Kstat, PreparedFileMmap},
};
use crate::{
    deferred_work::DeferredWorkAccount,
    task::{
        AsThread, Cred, Kuid, LandlockDomain, ProcessData, ProcessGroup, Thread, get_process_data,
        get_process_group, get_visible_task, process_domain, send_queued_signal_thread_inner,
        send_queued_signal_to_process_data, send_signal_thread_inner, send_signal_to_process_data,
    },
};

// Host tests do not initialize a scheduler/current task. Status transitions
// are short and never block in the tested backend setters, so use the same
// critical-section shape over a spin mutex in that configuration.
#[cfg(test)]
type StatusTransitionMutex<T> = Mutex<T>;

static FILE_DESCRIPTION_ID: AtomicU64 = AtomicU64::new(1);
static DESCRIPTION_CLEANUP_INCOMING: AtomicPtr<DescriptionCleanupWork> =
    AtomicPtr::new(ptr::null_mut());
static DESCRIPTION_CLEANUP_PENDING: AtomicPtr<DescriptionCleanupWork> =
    AtomicPtr::new(ptr::null_mut());
static DEFERRED_FILE_LEASES: AtomicPtr<DeferredFileLeaseInner> = AtomicPtr::new(ptr::null_mut());
static DEFERRED_FILE_LEASE_CREDITS: AtomicUsize = AtomicUsize::new(0);
static DESCRIPTION_CLEANUP_DRAINING: AtomicBool = AtomicBool::new(false);
static DESCRIPTION_CLEANUP_CREDITS: AtomicUsize = AtomicUsize::new(0);

const FLOCK_RELEASE_BUDGET: usize = 16;
const RECORD_LOCK_RELEASE_BUDGET: usize = 16;
const MAX_LIVE_DESCRIPTION_CLEANUPS: usize = 65_536;
const DESCRIPTION_CLOSE_WAITER_SLOTS: usize = 64;
const MAX_LIVE_DEFERRED_FILE_LEASES: usize = 65_536;

/// A fallibly allocated resource whose lifetime is exactly one open file
/// description. Subsystems use this for state which must survive `dup` but be
/// released on the final OFD close.
pub(crate) type DescriptionResource = Box<dyn Any + Send + Sync>;

/// Preallocated ownership retained by a mapping after fd close or reuse.
///
/// VMA split/fork operations clone this intrusive reference without allocating.
/// The final release only publishes the node; the exact FileHandle, retained
/// backing, and node allocation are destroyed by the policy worker. The
/// handle is the VMA's `vm_file` equivalent: it deliberately keeps the OFD
/// alive across fd close, VMA split, and fork.
struct DeferredFileLeaseInner {
    next: AtomicPtr<Self>,
    references: AtomicUsize,
    handle: FileHandle<dyn FileLike>,
    retained: Arc<dyn Any + Send + Sync>,
    _credit: DeferredFileLeaseCredit,
}

struct DeferredFileLeaseCredit;

impl Drop for DeferredFileLeaseCredit {
    fn drop(&mut self) {
        DEFERRED_FILE_LEASE_CREDITS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct DeferredFileLease {
    inner: ptr::NonNull<DeferredFileLeaseInner>,
}

// The allocation is immutable after construction except for its atomic
// reference count and publication link. The retained values are Send + Sync.
unsafe impl Send for DeferredFileLease {}
unsafe impl Sync for DeferredFileLease {}

impl DeferredFileLease {
    pub(crate) fn try_new(
        handle: FileHandle<dyn FileLike>,
        retained: Arc<dyn Any + Send + Sync>,
    ) -> AxResult<Self> {
        DEFERRED_FILE_LEASE_CREDITS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < MAX_LIVE_DEFERRED_FILE_LEASES).then_some(live + 1)
            })
            .map_err(|_| AxError::NoMemory)?;
        let inner = Box::try_new(DeferredFileLeaseInner {
            next: AtomicPtr::new(ptr::null_mut()),
            references: AtomicUsize::new(1),
            handle,
            retained,
            _credit: DeferredFileLeaseCredit,
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            inner: ptr::NonNull::from(Box::leak(inner)),
        })
    }

    pub(crate) fn identity(&self) -> usize {
        self.inner.as_ptr() as usize
    }

    #[cfg(test)]
    pub(crate) fn retained_reference_counts(&self) -> (usize, usize) {
        // SAFETY: a live lease owns one reference to this immutable node.
        let inner = unsafe { self.inner.as_ref() };
        (
            Arc::strong_count(&inner.handle.description),
            Arc::strong_count(&inner.retained),
        )
    }
}

impl Clone for DeferredFileLease {
    fn clone(&self) -> Self {
        // SAFETY: this live lease prevents the node from reaching zero while
        // the increment is performed.
        let inner = unsafe { self.inner.as_ref() };
        retain_deferred_file_lease(&inner.references);
        Self { inner: self.inner }
    }
}

impl Drop for DeferredFileLease {
    fn drop(&mut self) {
        // SAFETY: every clone owns exactly one counted reference.
        let inner = unsafe { self.inner.as_ref() };
        if !release_deferred_file_lease(&inner.references) {
            return;
        }

        let node = self.inner.as_ptr();
        let mut head = DEFERRED_FILE_LEASES.load(Ordering::Acquire);
        loop {
            // SAFETY: the final reference owns the unpublished node until the
            // successful release-store below.
            unsafe { (*node).next.store(head, Ordering::Relaxed) };
            match DEFERRED_FILE_LEASES.compare_exchange_weak(
                head,
                node,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }
}

fn retain_deferred_file_lease(references: &AtomicUsize) {
    let mut current = references.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            return;
        }
        // User-reachable clones are bounded by live VMA/backend values in one
        // addressable machine. If internal code leaks clones and nevertheless
        // reaches the sentinel, leaking the node is safer than a
        // user-triggerable panic or premature free.
        let next = current.saturating_add(1);
        match references.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn release_deferred_file_lease(references: &AtomicUsize) -> bool {
    let mut current = references.load(Ordering::Acquire);
    loop {
        if current == usize::MAX || current == 0 {
            return false;
        }
        match references.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return current == 1,
            Err(observed) => current = observed,
        }
    }
}

fn pop_deferred_file_lease() -> Option<Box<DeferredFileLeaseInner>> {
    let mut head = DEFERRED_FILE_LEASES.load(Ordering::Acquire);
    loop {
        if head.is_null() {
            return None;
        }
        // SAFETY: published nodes remain allocated until the only consumer
        // removes one successfully.
        let next = unsafe { (*head).next.load(Ordering::Relaxed) };
        match DEFERRED_FILE_LEASES.compare_exchange_weak(
            head,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: successful removal transfers unique ownership back
                // to this task-context consumer.
                return Some(unsafe { Box::from_raw(head) });
            }
            Err(observed) => head = observed,
        }
    }
}

/// Preallocated final-OFD policy work.  The final Arc drop only publishes this
/// intrusive node; lock-table scans, destructors, and waiter callbacks run in
/// task context with fixed per-invocation budgets.
struct DescriptionCleanupWork {
    next: AtomicPtr<Self>,
    owner: u64,
    flock_done: bool,
    record_lock_done: bool,
    lease_done: bool,
    open_lease_registration: Option<lease::OpenLeaseRegistration>,
    write_open_key: Option<ExecutableKey>,
    resource: Option<DescriptionResource>,
    account: Option<Arc<DeferredWorkAccount>>,
}

impl DescriptionCleanupWork {
    fn try_new(owner: u64) -> AxResult<Box<Self>> {
        DESCRIPTION_CLEANUP_CREDITS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < MAX_LIVE_DESCRIPTION_CLEANUPS).then_some(live + 1)
            })
            .map_err(|_| AxError::TooManyOpenFiles)?;
        let work = Box::try_new(Self {
            next: AtomicPtr::new(ptr::null_mut()),
            owner,
            flock_done: false,
            record_lock_done: false,
            lease_done: false,
            open_lease_registration: None,
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
        drop(self.open_lease_registration.take());
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

fn pop_description_cleanup_from(
    incoming: &AtomicPtr<DescriptionCleanupWork>,
    pending: &AtomicPtr<DescriptionCleanupWork>,
) -> Option<Box<DescriptionCleanupWork>> {
    if pending.load(Ordering::Relaxed).is_null() {
        let incoming = incoming.swap(ptr::null_mut(), Ordering::AcqRel);
        pending.store(
            reverse_description_cleanup_list(incoming),
            Ordering::Relaxed,
        );
    }
    let head = pending.load(Ordering::Relaxed);
    if head.is_null() {
        return None;
    }
    // SAFETY: only the drain-guard owner accesses the pending FIFO.
    let next = unsafe { (*head).next.load(Ordering::Relaxed) };
    pending.store(next, Ordering::Relaxed);
    unsafe { (*head).next.store(ptr::null_mut(), Ordering::Relaxed) };
    // SAFETY: removing the head transfers its unique ownership to this caller.
    Some(unsafe { Box::from_raw(head) })
}

fn pop_description_cleanup() -> Option<Box<DescriptionCleanupWork>> {
    pop_description_cleanup_from(&DESCRIPTION_CLEANUP_INCOMING, &DESCRIPTION_CLEANUP_PENDING)
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
        || !DEFERRED_FILE_LEASES.load(Ordering::Acquire).is_null()
}

/// Runs one bounded final-OFD cleanup batch.  An unfinished node is
/// republished without allocation so the policy worker can yield between
/// batches.
pub(crate) fn drain_deferred_description_cleanup() {
    let Some(_guard) = DescriptionCleanupDrainGuard::try_enter() else {
        return;
    };
    if let Some(mut work) = pop_description_cleanup() {
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
    drop(pop_deferred_file_lease());
}

/// Host-test adapter for a description which owns only a typed resource.
///
/// Kernel host tests do not initialize the task scheduler required by the
/// flock/lease tables. This helper exercises the real intrusive publication
/// and typed-resource handoff without pretending to validate those unrelated
/// task-context policies. Callers must create an owner with no flock, record
/// lock, open registration, lease, executable-write key, or deferred-work
/// account.
#[cfg(test)]
pub(crate) fn drain_deferred_description_resource_only_for_test() {
    let Some(_guard) = DescriptionCleanupDrainGuard::try_enter() else {
        return;
    };
    let _ = drain_deferred_description_resource_only_from_for_test(
        &DESCRIPTION_CLEANUP_INCOMING,
        &DESCRIPTION_CLEANUP_PENDING,
    );
}

/// Drains one resource-only cleanup through the same raw `Box` handoff as the
/// policy worker, with caller-owned queue state for deterministic host tests.
#[cfg(test)]
fn drain_deferred_description_resource_only_from_for_test(
    incoming: &AtomicPtr<DescriptionCleanupWork>,
    pending: &AtomicPtr<DescriptionCleanupWork>,
) -> bool {
    let Some(mut work) = pop_description_cleanup_from(incoming, pending) else {
        return false;
    };
    debug_assert!(work.write_open_key.is_none());
    debug_assert!(work.account.is_none());
    debug_assert!(work.open_lease_registration.is_none());
    drop(work.resource.take());
    true
}

#[cfg(test)]
pub(crate) fn drain_deferred_file_lease_for_test() -> bool {
    let Some(_guard) = DescriptionCleanupDrainGuard::try_enter() else {
        return false;
    };
    let Some(work) = pop_deferred_file_lease() else {
        return false;
    };
    drop(work);
    true
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
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
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
    /// SCM_RIGHTS queue custody is descriptor-publication authority: it keeps
    /// an OFD eligible for later fd installation, unlike epoll/VMA/worker Arc
    /// retention which merely keeps the object alive.
    scm_custodies: usize,
    ever_published: bool,
    terminal: bool,
    close_source: Option<Arc<PollSet<DESCRIPTION_CLOSE_WAITER_SLOTS>>>,
}

impl DescriptorLifetimeState {
    fn is_quiescent(&self) -> bool {
        self.ever_published
            && self.references == 0
            && self.pending_publications == 0
            && self.scm_custodies == 0
    }

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
        if self.is_quiescent() && !self.terminal {
            self.terminal = true;
            self.close_source.clone()
        } else {
            None
        }
    }
}

/// One queued SCM_RIGHTS authority for an OFD. Unlike an arbitrary retained
/// `Arc<FileDescription>` (epoll, VMA, async work), this promises that a new
/// descriptor may be installed later and therefore postpones `pre_close`.
pub(crate) struct ScmDescriptorCustody {
    description: Arc<FileDescription>,
}

impl ScmDescriptorCustody {
    pub(crate) fn description(&self) -> &Arc<FileDescription> {
        &self.description
    }
}

impl Drop for ScmDescriptorCustody {
    fn drop(&mut self) {
        let (close_source, should_pre_close) = {
            let mut lifetime = self.description.descriptor_lifetime.lock();
            debug_assert!(lifetime.scm_custodies != 0);
            lifetime.scm_custodies = lifetime.scm_custodies.saturating_sub(1);
            let should_pre_close = lifetime.is_quiescent();
            (lifetime.terminal_source_if_quiescent(), should_pre_close)
        };
        if should_pre_close && axtask::can_block_current() {
            self.description.inner.pre_close();
        }
        if let Some(source) = close_source {
            source.close();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OpenCredentials {
    pub(crate) uid: Kuid,
    pub(crate) euid: Kuid,
    pub(crate) suid: Kuid,
    pub(crate) fsuid: Kuid,
    pub(crate) cgroup_ns_id: u64,
}

/// Immutable, owned identity and OFD snapshot for one file I/O operation.
///
/// This is deliberately independent of a task, fd number, or borrowed VFS
/// state.  A submission thread can capture it before handing work to a
/// kernel worker; the worker can then make all status, set-id, and deferred
/// event decisions from the same values even after the submitting task has
/// changed credentials or exited.
#[derive(Clone)]
pub(crate) struct IoOperationContext {
    /// The exact open-file description which admitted this operation.  The
    /// strong reference keeps the OFD alive across submission/worker handoff;
    /// the monotonic id makes the binding generation-visible even if another
    /// descriptor is later allocated for the same numeric fd.
    description: Arc<FileDescription>,
    description_id: FileDescriptionId,
    security: VfsSecurityContext,
    status: OfdIoStatus,
    open_credentials: OpenCredentials,
    vfs_open_credential: Option<Arc<Cred>>,
    open_security_credential: Option<Arc<Cred>>,
    fanotify_actor: FanotifyEventActor,
    /// Per-request RWF bits which are not representable as an OFD status
    /// flag (currently DONTCACHE and NOSIGNAL).  This is captured with an
    /// asynchronous operation and never read from a worker's current task.
    rwf_flags: u32,
}

impl IoOperationContext {
    pub(crate) fn new(
        description: Arc<FileDescription>,
        security: VfsSecurityContext,
        status: OfdIoStatus,
        open_credentials: OpenCredentials,
        vfs_open_credential: Option<Arc<Cred>>,
        open_security_credential: Option<Arc<Cred>>,
        fanotify_actor: FanotifyEventActor,
    ) -> Self {
        Self {
            description_id: description.id(),
            description,
            security,
            status,
            open_credentials,
            vfs_open_credential,
            open_security_credential,
            fanotify_actor,
            rwf_flags: 0,
        }
    }

    pub(crate) const fn status(&self) -> OfdIoStatus {
        self.status
    }

    /// Derives an operation-local status snapshot.  Per-request RWF flags
    /// must affect only the captured asynchronous operation, never the shared
    /// open-file-description status observed by other syscalls.
    pub(crate) fn with_status(&self, status: OfdIoStatus) -> Self {
        let mut context = self.clone();
        context.status = status;
        context
    }

    pub(crate) fn with_rwf_flags(&self, rwf_flags: u32) -> Self {
        let mut context = self.clone();
        context.rwf_flags = rwf_flags;
        context
    }

    pub(crate) const fn rwf_flags(&self) -> u32 {
        self.rwf_flags
    }

    pub(crate) const fn security(&self) -> &VfsSecurityContext {
        &self.security
    }

    pub(crate) const fn open_credentials(&self) -> OpenCredentials {
        self.open_credentials
    }

    pub(crate) fn vfs_open_credential(&self) -> Option<Arc<Cred>> {
        self.vfs_open_credential.clone()
    }

    pub(crate) fn open_security_credential(&self) -> Option<Arc<Cred>> {
        self.open_security_credential.clone()
    }

    pub(crate) const fn fanotify_actor(&self) -> FanotifyEventActor {
        self.fanotify_actor
    }

    /// Validates that a retained operation is still executing against the
    /// exact OFD which admitted it.  Numeric fd reuse and a caller-provided
    /// status snapshot are never sufficient to establish this binding.
    pub(crate) fn validate_for(&self, description: &Arc<FileDescription>) -> AxResult<()> {
        if self.description_id != description.id()
            || !Arc::ptr_eq(&self.description, description)
            || self.open_credentials != description.open_credentials()
            || self.status.path_only()
        {
            return Err(AxError::BadFileDescriptor);
        }
        description.check_io_status(self.status)
    }
}

impl OpenCredentials {
    pub(crate) fn current() -> Self {
        let Some(task) = current_may_uninit() else {
            return Self::root();
        };
        let Some(thread) = task.try_as_thread() else {
            return Self::root();
        };
        let cred = thread.current_cred();
        let ids = cred.ids();
        Self {
            uid: ids.ruid,
            euid: ids.euid,
            suid: ids.suid,
            fsuid: ids.fsuid,
            // Cgroup namespaces are task-local.  ProcessData retains only a
            // creation snapshot, so consulting it after setns/unshare would
            // authorize a cgroup control write against the namespace this
            // thread has already left.
            cgroup_ns_id: thread.cgroup_ns().id(),
        }
    }

    const fn root() -> Self {
        Self {
            uid: Kuid::INITIAL_ROOT,
            euid: Kuid::INITIAL_ROOT,
            suid: Kuid::INITIAL_ROOT,
            fsuid: Kuid::INITIAL_ROOT,
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
        // The pgid arrives in the caller's pid namespace; the group registry
        // is keyed by kernel-global leader identity (the same translation
        // setpgid(2) performs).
        let id = current()
            .as_thread()
            .pid_ns()
            .resolve_visible_pid(id)
            .ok_or(AxError::NoSuchProcess)?;
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
    landlock: Option<LandlockDomain>,
    landlock_tgid: Option<Pid>,
}

impl Default for AsyncIoState {
    fn default() -> Self {
        Self {
            owner: AsyncIoOwner::None(AsyncIoOwnerType::Pid),
            signal: 0,
            credentials: None,
            landlock: None,
            landlock_tgid: None,
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
    let mut info = SignalInfo::new_poll(signo, SignalPollPayload::new(sigio_band(reason) as _, fd));
    info.set_code(sigio_code(signo, reason));
    info
}

fn send_sigio_to_process(process: &ProcessData, info: SignalInfo) {
    if info.signo().is_realtime() {
        match send_queued_signal_to_process_data(process, Some(info)) {
            Ok(_) => (),
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

fn sigio_in_scope(state: &AsyncIoState, target_tgid: Pid, target: &LandlockDomain) -> bool {
    state.landlock.as_ref().is_none_or(|actor| {
        state
            .landlock_tgid
            .is_some_and(|owner_tgid| owner_tgid == target_tgid)
            || actor.allows_scope_to(target, crate::task::security::LANDLOCK_SCOPE_SIGNAL)
    })
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
            if thread.pending_exit()
                || !credentials.may_signal(&target_cred)
                || !sigio_in_scope(
                    state,
                    thread.proc_data.proc.pid(),
                    &thread.landlock_domain(),
                )
            {
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
            if credentials.may_signal(&target_cred)
                && sigio_in_scope(
                    state,
                    process.proc.pid(),
                    &process.group_leader_landlock_domain(),
                )
            {
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
                    || !sigio_in_scope(
                        state,
                        process_data.proc.pid(),
                        &process_data.group_leader_landlock_domain(),
                    )
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

/// Immutable status sampled once for a complete OFD-derived I/O operation.
///
/// Callers pass this value through admission, limits/seals, placement, and
/// backend commit instead of re-reading mutable status midway through an
/// operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OfdIoStatus {
    raw: u32,
    rwf_nowait: bool,
}

impl OfdIoStatus {
    pub(crate) const fn new(raw: u32) -> Self {
        Self {
            raw,
            rwf_nowait: false,
        }
    }

    pub(crate) const fn raw(self) -> u32 {
        self.raw
    }

    pub(crate) const fn append(self) -> bool {
        self.raw & O_APPEND != 0
    }

    pub(crate) const fn nonblocking(self) -> bool {
        self.raw & O_NONBLOCK != 0
    }

    pub(crate) const fn rwf_nowait(self) -> bool {
        self.rwf_nowait
    }

    pub(crate) const fn with_rwf_nowait(mut self, rwf_nowait: bool) -> Self {
        self.rwf_nowait = rwf_nowait;
        self
    }

    pub(crate) const fn path_only(self) -> bool {
        self.raw & O_PATH != 0
    }
}

pub struct FileDescription {
    pub inner: Arc<dyn FileLike>,
    /// Mount identity and namespace topology through which this OFD was
    /// opened.  Relative pathwalk may cross a nested mount after setns(), so
    /// pinning only the starting mount's idmap is insufficient.
    vfs_mount_id: Option<u64>,
    vfs_mount_topology: Option<Arc<crate::mounts::MountTopology>>,
    /// Immutable mount-idmap selected by the mount instance through which
    /// this OFD was opened.  An fd can outlive `setns(CLONE_NEWNS)`, so
    /// descriptor-based policy must never rediscover this through the
    /// caller's current mount namespace.
    vfs_mount_idmap: Option<Arc<crate::mounts::MountIdmap>>,
    /// Immutable `O_DIRECTORY` admission fact.  It is not an F_GETFL status
    /// bit, but may_decode_fh's relaxed path needs the original open intent.
    directory_capability: bool,
    open_credentials: OpenCredentials,
    /// Exact Linux credential captured when this open file description was
    /// created. Linux `file::f_cred` is shared by dup/fork with the OFD and is
    /// distinct from credentials installed only for pseudo-file I/O.
    vfs_open_credential: Option<Arc<Cred>>,
    open_security_credential: Option<Arc<Cred>>,
    /// Immutable lock-free cache of the identity also carried by `ofd`.
    /// Keeping this copy avoids taking the OFD state lock inside dnotify and
    /// flock lock domains; it can never diverge because identities do not
    /// mutate after construction.
    id: FileDescriptionId,
    created_by_open: bool,
    /// Per-open-file-description write-lifetime hint (`F_{GET,SET}_RW_HINT`).
    /// It is explicitly shared by dup/fork just like Linux's `struct file`.
    rw_hint: AtomicU64,
    ofd: Mutex<OpenFileDescriptionState<AsyncIoState, ExternalOffset>>,
    /// Source is shared by independent opens of one VFS entry; the cursor is
    /// per OFD, and is consequently shared by dup.
    sync_error_source: Arc<WritebackErrorState>,
    sync_error: Mutex<SyncErrorCursor>,
    /// syncfs samples the filesystem (superblock) errseq separately from the
    /// inode errseq used by fsync/fdatasync.
    syncfs_error: Option<(Arc<WritebackErrorState>, Mutex<SyncErrorCursor>)>,
    /// Serializes only status snapshots and short backend/OFD transitions.
    /// No user fault, wait, VFS I/O, or device operation may run under it.
    status_transition: StatusTransitionMutex<()>,
    descriptor_lifetime: SpinNoIrq<DescriptorLifetimeState>,
    open_committed: AtomicBool,
    open_lease_publication: Option<lease::OpenLeasePublication>,
    notification_work: Option<Box<super::inotify::CloseWork>>,
    cleanup_work: Option<Box<DescriptionCleanupWork>>,
}

#[derive(Default)]
struct SyncErrorCursor {
    observed: u64,
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

    pub(crate) fn new_with_flags(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner(
            inner,
            status_flags,
            status_flags & O_DIRECTORY != 0,
            None,
            None,
            None,
            None,
            false,
        )
    }

    pub(in crate::file) fn new_with_write_open_key_and_resource(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner(
            inner,
            status_flags,
            status_flags & O_DIRECTORY != 0,
            write_open_key,
            resource,
            None,
            None,
            false,
        )
    }

    pub(in crate::file) fn new_with_open_lease_admission_and_resource(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        directory_capability: bool,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
        open_lease_admission: lease::OpenLeaseAdmission,
        vfs_open_credential: Arc<Cred>,
        vfs_mount_topology: Option<Arc<crate::mounts::MountTopology>>,
        created_by_open: bool,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner_with_mount_topology(
            inner,
            status_flags,
            directory_capability,
            write_open_key,
            resource,
            Some(open_lease_admission),
            Some(vfs_open_credential),
            vfs_mount_topology,
            created_by_open,
        )
    }

    fn new_inner(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        directory_capability: bool,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
        open_lease_admission: Option<lease::OpenLeaseAdmission>,
        vfs_open_credential: Option<Arc<Cred>>,
        created_by_open: bool,
    ) -> AxResult<Arc<Self>> {
        Self::new_inner_with_mount_topology(
            inner,
            status_flags,
            directory_capability,
            write_open_key,
            resource,
            open_lease_admission,
            vfs_open_credential,
            None,
            created_by_open,
        )
    }

    fn new_inner_with_mount_topology(
        inner: Arc<dyn FileLike>,
        status_flags: u32,
        directory_capability: bool,
        write_open_key: Option<ExecutableKey>,
        resource: Option<DescriptionResource>,
        open_lease_admission: Option<lease::OpenLeaseAdmission>,
        vfs_open_credential: Option<Arc<Cred>>,
        opening_mount_topology: Option<Arc<crate::mounts::MountTopology>>,
        created_by_open: bool,
    ) -> AxResult<Arc<Self>> {
        // Before a complete FileDescription exists, this guard owns rollback.
        // Once transferred into the value, ordinary FileDescription::drop owns
        // it even if Arc allocation itself fails.
        let write_open_rollback = WriteOpenRollback::new(write_open_key);
        // Linux O_PATH descriptions carry FMODE_NONOTIFY: neither ordinary
        // open nor final-close notifications are emitted for the pathname
        // handle.
        let notification_work = if status_flags & O_PATH != 0 {
            None
        } else {
            super::inotify::prepare_description_close(&inner)?
        };
        let id = FileDescriptionId::allocate()?;
        let mut cleanup_work = DescriptionCleanupWork::try_new(id.get())?;
        let open_lease_registration = match open_lease_admission {
            Some(admission) => admission.into_ofd(id.get())?,
            None => None,
        };
        let open_lease_publication = open_lease_registration
            .as_ref()
            .map(lease::OpenLeaseRegistration::publication);
        cleanup_work.open_lease_registration = open_lease_registration;
        let write_open_key = write_open_rollback.transfer();
        cleanup_work.write_open_key = write_open_key;
        cleanup_work.resource = resource;
        let vfs_open_credential = vfs_open_credential.or_else(|| {
            current_may_uninit()
                .and_then(|task| task.try_as_thread().map(|thread| thread.current_cred()))
        });
        let needs_open_security_credential = inner.downcast_ref::<File>().is_some_and(|file| {
            file.inner()
                .location()
                .flags()
                .contains(NodeFlags::OPEN_CREDENTIAL)
        });
        let open_security_credential = needs_open_security_credential
            .then(|| vfs_open_credential.clone())
            .flatten();
        let sync_error_source = inner.writeback_error_state()?;
        let syncfs_error = inner.syncfs_filesystem().map(|filesystem| {
            let source = filesystem.writeback_error_state();
            let observed = source.sample();
            (source, Mutex::new(SyncErrorCursor { observed }))
        });
        // A newly opened file samples the current errseq.  Earlier failures
        // belong to already-open descriptions; dup retains that description's
        // existing cursor instead of constructing a new one.
        let observed = sync_error_source.sample();
        let (vfs_mount_id, vfs_mount_topology, vfs_mount_idmap) =
            if let Some(location) = inner.vfs_location() {
                let mount_id = location.mountpoint().mount_id();
                let has_opening_topology = opening_mount_topology.is_some();
                let mut selected = None;
                if let Some(topology) = opening_mount_topology {
                    match topology.idmap_for_mount(mount_id) {
                        Ok(idmap) => selected = Some((topology, idmap)),
                        // Retain an explicit opening topology even when this
                        // location has no ledger record in it.  Dropping it here
                        // would let a later relative open rebind to `current()`.
                        Err(AxError::NotFound) => selected = Some((topology, None)),
                        Err(error) => return Err(error),
                    }
                }
                // An explicit opening topology is execution authority.  In
                // particular, io_uring workers must never replace it with their
                // own current namespace after a successful submitter-side open.
                if !has_opening_topology && selected.is_none() {
                    if let Some(task) = current_may_uninit()
                        && let Some(thread) = task.try_as_thread()
                    {
                        let topology = thread.mount_ns().topology();
                        match topology.idmap_for_mount(mount_id) {
                            Ok(idmap) => selected = Some((topology, idmap)),
                            Err(AxError::NotFound) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                if !has_opening_topology && selected.is_none() {
                    for namespace in crate::task::MountNamespace::live()? {
                        let topology = namespace.topology();
                        match topology.idmap_for_mount(mount_id) {
                            Ok(idmap) => {
                                selected = Some((topology, idmap));
                                break;
                            }
                            Err(AxError::NotFound) => {}
                            Err(error) => return Err(error),
                        }
                    }
                }
                let (topology, idmap) = selected.unzip();
                (Some(mount_id), topology, idmap.flatten())
            } else {
                (None, None, None)
            };
        Arc::try_new(Self {
            inner,
            vfs_mount_id,
            vfs_mount_topology,
            vfs_mount_idmap,
            directory_capability,
            open_credentials: OpenCredentials::current(),
            vfs_open_credential,
            open_security_credential,
            id,
            created_by_open,
            rw_hint: AtomicU64::new(0),
            ofd: Mutex::new(OpenFileDescriptionState::new_external(
                id.linux_id(),
                status_flags,
                AsyncIoState::default(),
            )),
            sync_error_source,
            sync_error: Mutex::new(SyncErrorCursor { observed }),
            syncfs_error,
            status_transition: StatusTransitionMutex::new(()),
            descriptor_lifetime: SpinNoIrq::new(DescriptorLifetimeState::default()),
            open_committed: AtomicBool::new(false),
            open_lease_publication,
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

    pub(crate) const fn created_by_open(&self) -> bool {
        self.created_by_open
    }

    pub(crate) fn rw_hint(&self) -> u64 {
        self.rw_hint.load(Ordering::Acquire)
    }

    pub(crate) fn set_rw_hint(&self, hint: u64) {
        self.rw_hint.store(hint, Ordering::Release);
    }

    pub(crate) fn open_credentials(&self) -> OpenCredentials {
        self.open_credentials
    }

    pub(crate) fn open_security_credential(&self) -> Option<Arc<Cred>> {
        self.open_security_credential.clone()
    }

    pub(crate) fn vfs_open_credential(&self) -> Option<Arc<Cred>> {
        self.vfs_open_credential.clone()
    }

    pub(crate) fn vfs_mount_idmap(&self) -> Option<Arc<crate::mounts::MountIdmap>> {
        self.vfs_mount_idmap.clone()
    }

    pub(crate) const fn vfs_mount_id(&self) -> Option<u64> {
        self.vfs_mount_id
    }

    pub(crate) fn vfs_mount_topology(&self) -> Option<Arc<crate::mounts::MountTopology>> {
        self.vfs_mount_topology.clone()
    }

    /// Captures all immutable state needed by a positioned file operation.
    ///
    /// The caller supplies the actor and event identity observed at syscall
    /// submission. No task-local state is retained in the returned value;
    /// every field is either copy-only or owned by an `Arc`.
    pub(crate) fn capture_io_operation_context(
        self: &Arc<Self>,
        security: VfsSecurityContext,
        fanotify_actor: FanotifyEventActor,
    ) -> IoOperationContext {
        IoOperationContext::new(
            self.clone(),
            security,
            self.io_status_snapshot(),
            self.open_credentials,
            self.vfs_open_credential.clone(),
            self.open_security_credential.clone(),
            fanotify_actor,
        )
    }

    /// Pins this exact open file description and erases only its inner type.
    ///
    /// Long-lived kernel operations use this after descriptor lookup or fixed
    /// resource registration. Re-looking up the numeric fd could observe a
    /// concurrent close-and-reuse and silently switch open-file descriptions.
    pub(crate) fn file_handle(self: &Arc<Self>) -> FileHandle<dyn FileLike> {
        FileHandle {
            description: self.clone(),
            file: self.inner.clone(),
        }
    }

    /// Samples authoritative mutable OFD status under the short transition
    /// lock. The returned value is independent of backend mirrors and remains
    /// stable for the caller's complete operation.
    pub(crate) fn io_status_snapshot(&self) -> OfdIoStatus {
        let _transition = self.status_transition.lock();
        OfdIoStatus::new(self.ofd.lock().status_flags())
    }

    pub fn status_flags(&self) -> u32 {
        self.io_status_snapshot().raw()
    }

    pub(crate) fn directory_capability(&self) -> bool {
        self.directory_capability
    }

    /// Admits an ordinary I/O operation on this description.
    ///
    /// `O_PATH` is an immutable open-file-description capability: the inner
    /// VFS object may still expose data, ioctl, synchronization, or readiness
    /// methods for ordinary opens, but an `O_PATH` description only carries
    /// pathname capability. Keep this fact on the OFD so fast paths and typed
    /// inner objects cannot diverge.
    pub(crate) fn check_io_access(&self) -> AxResult<()> {
        self.check_io_status(self.io_status_snapshot())
    }

    pub(crate) fn check_io_status(&self, status: OfdIoStatus) -> AxResult<()> {
        if status.path_only() {
            Err(AxError::BadFileDescriptor)
        } else {
            Ok(())
        }
    }

    /// Publishes a completed asynchronous writeback error to the shared VFS
    /// source.  Each independent OFD observes it once through its own cursor.
    pub(crate) fn publish_sync_error(&self, error: AxError) {
        self.sync_error_source.publish(error);
    }

    fn take_unseen_sync_error(&self) -> Option<AxError> {
        let mut cursor = self.sync_error.lock();
        self.sync_error_source
            .check_and_advance(&mut cursor.observed)
    }

    /// Advances this exact OFD's inode writeback errseq cursor.  Duplicated
    /// descriptors share this cursor; independently opened descriptions do
    /// not, matching Linux file_check_and_advance_wb_err semantics.
    pub(crate) fn check_and_advance_writeback_error(&self) -> AxResult<()> {
        self.take_unseen_sync_error().map_or(Ok(()), Err)
    }

    /// Synchronizes the retained OFD without another numeric-fd lookup.
    /// `O_PATH` installs empty file operations; fsync therefore returns
    /// `EINVAL` from the missing synchronization callback.
    pub(crate) fn sync(&self, data_only: bool) -> AxResult<()> {
        if self.io_status_snapshot().path_only() {
            return Err(AxError::InvalidInput);
        }

        self.inner.sync(data_only)?;
        self.take_unseen_sync_error().map_or(Ok(()), Err)
    }

    /// Retained-description variant used by cancellable async submission.
    /// The FileLike provider receives the shared operation token so a device
    /// or remote filesystem can wake/abort its own flush waiter.
    pub(crate) fn sync_cancellable(
        &self,
        data_only: bool,
        operation: &crate::async_operation::AsyncOperation,
    ) -> AxResult<()> {
        if self.io_status_snapshot().path_only() {
            return Err(AxError::InvalidInput);
        }
        self.inner.sync_cancellable(data_only, operation)?;
        self.take_unseen_sync_error().map_or(Ok(()), Err)
    }

    /// Linux syncfs is superblock-scoped, not an f_op sync callback: any
    /// retained fd with a filesystem anchor (including O_PATH and read-only
    /// opens) can drive it.  Always advance the superblock errseq even when
    /// the synchronous flush itself fails; that primary flush errno wins.
    pub(crate) fn sync_filesystem(&self) -> AxResult<()> {
        let Some(filesystem) = self.inner.syncfs_filesystem() else {
            return Err(AxError::InvalidInput);
        };
        let result = filesystem.flush();
        let async_error = self
            .syncfs_error
            .as_ref()
            .and_then(|(source, cursor)| source.check_and_advance(&mut cursor.lock().observed));
        result.and_then(|()| async_error.map_or(Ok(()), Err))
    }

    pub(crate) fn is_path_only(&self) -> bool {
        self.io_status_snapshot().path_only()
    }

    /// Atomically derives and commits new mutable status.
    ///
    /// `apply_backend` runs without the OFD state lock and must be short,
    /// nonblocking, and failure-atomic: `Err` must leave backend state exactly
    /// as it was. It must consume the supplied old/new snapshots rather than
    /// recursively sampling this description. Once it succeeds, publishing
    /// the authoritative OFD flags is infallible and occurs before the
    /// transition lock is released.
    pub(crate) fn transition_status_flags(
        &self,
        update: impl FnOnce(OfdIoStatus) -> u32,
        apply_backend: impl FnOnce(OfdIoStatus, OfdIoStatus) -> AxResult<()>,
    ) -> AxResult<OfdIoStatus> {
        let _transition = self.status_transition.lock();
        let old = OfdIoStatus::new(self.ofd.lock().status_flags());
        let new = OfdIoStatus::new(update(old));
        apply_backend(old, new)?;
        self.ofd.lock().commit_status_flags(new.raw());
        Ok(new)
    }

    pub fn async_io_state(&self) -> AsyncIoState {
        self.ofd.lock().async_owner().clone()
    }

    pub fn set_async_io_owner(&self, owner: AsyncIoOwner) {
        let (credentials, landlock, landlock_tgid) = if owner.is_none() {
            (None, None, None)
        } else {
            current_may_uninit()
                .and_then(|task| {
                    task.try_as_thread().map(|thread| {
                        let cred = thread.current_cred();
                        let ids = cred.ids();
                        (
                            Some(AsyncIoCredentials {
                                uid: ids.ruid,
                                euid: ids.euid,
                                euid_is_global_root: ids.euid == Kuid::INITIAL_ROOT
                                    && cred.user_ns().is_initial(),
                            }),
                            Some(thread.landlock_domain()),
                            Some(thread.proc_data.proc.pid()),
                        )
                    })
                })
                .unwrap_or((None, None, None))
        };
        let mut ofd = self.ofd.lock();
        let state = ofd.async_owner_mut();
        state.owner = owner;
        state.credentials = credentials;
        state.landlock = landlock;
        state.landlock_tgid = landlock_tgid;
    }

    pub(crate) fn ensure_async_io_owner(&self, owner: AsyncIoOwner) {
        let credentials = AsyncIoCredentials::current();
        let (landlock, landlock_tgid) = current_may_uninit()
            .and_then(|task| {
                task.try_as_thread()
                    .map(|thread| (thread.landlock_domain(), thread.proc_data.proc.pid()))
            })
            .map(|(domain, tgid)| (Some(domain), Some(tgid)))
            .unwrap_or((None, None));
        let mut ofd = self.ofd.lock();
        let state = ofd.async_owner_mut();
        if state.owner.is_none() {
            state.credentials = credentials;
            state.landlock = landlock;
            state.landlock_tgid = landlock_tgid;
            state.owner = owner;
        }
    }

    pub fn set_async_io_signal(&self, signal: u8) {
        self.ofd.lock().async_owner_mut().signal = signal;
    }

    pub(crate) fn mark_open_committed(&self) {
        if self.open_committed.load(Ordering::Acquire) {
            return;
        }
        if let Some(publication) = self.open_lease_publication.as_ref() {
            publication.publish();
        }
        self.open_committed.store(true, Ordering::Release);
    }

    pub(crate) fn begin_descriptor_publication(
        self: &Arc<Self>,
    ) -> AxResult<DescriptorPublication> {
        {
            let mut lifetime = self.descriptor_lifetime.lock();
            // A bare Arc (epoll/VMA/worker retention) is not descriptor
            // publication authority. Queued SCM_RIGHTS obtains custody before
            // the source table can lose its final descriptor, so it keeps this
            // state non-terminal and reaches this path normally.
            if lifetime.terminal {
                return Err(AxError::BadState);
            }
            lifetime.admit_publication()?;
        }
        Ok(DescriptorPublication {
            description: Arc::clone(self),
            active: true,
        })
    }

    pub(crate) fn descriptor_closed(&self) {
        let (close_source, last_descriptor) = {
            let mut lifetime = self.descriptor_lifetime.lock();
            let Some(references) = lifetime.references.checked_sub(1) else {
                error!("descriptor close observed an unaccounted OFD reference");
                lifetime.terminal = true;
                return;
            };
            lifetime.references = references;
            let last_descriptor = lifetime.is_quiescent();
            (lifetime.terminal_source_if_quiescent(), last_descriptor)
        };
        // The descriptor is still retained by the caller at this point.  A
        // perf event can therefore synchronously settle remote PMU custody
        // before its last FileLike reference becomes eligible for final_drop.
        // `final_close` remains the mandatory IRQ-safe fallback for paths
        // (such as table destruction) which cannot wait here.
        if last_descriptor && axtask::can_block_current() {
            self.inner.pre_close();
        }
        if let Some(source) = close_source {
            source.close();
        }
    }

    /// Whether an fd table still roots this exact OFD. SCM_RIGHTS cycle
    /// collection uses this explicit count rather than mistaking a transient
    /// close-path Arc for a userspace-visible descriptor root.
    pub(crate) fn has_live_descriptor_references(&self) -> bool {
        self.descriptor_lifetime.lock().references != 0
    }

    /// Captures republishable SCM_RIGHTS custody while the caller still holds
    /// an fd-table reference. The returned guard owns the exact OFD and keeps
    /// last-descriptor close from running `pre_close` until the queued right
    /// is delivered, discarded, or swept.
    pub(crate) fn acquire_scm_custody(self: &Arc<Self>) -> ScmDescriptorCustody {
        let mut lifetime = self.descriptor_lifetime.lock();
        debug_assert!(lifetime.references != 0 || lifetime.scm_custodies != 0);
        lifetime.scm_custodies = lifetime.scm_custodies.saturating_add(1);
        ScmDescriptorCustody {
            description: Arc::clone(self),
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
        // This is the one true final-OFD boundary. It runs before this
        // structure releases `inner`; every legal OFD owner, including VMA
        // split/fork leases, has already released its FileHandle. Do not
        // report a prepared but never-published description as an open-file
        // close. The FileLike contract makes this direct final-drop path
        // IRQ-safe.
        if self.open_committed.load(Ordering::Acquire) {
            self.inner.final_close();
        }
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
        self.check_io_access()?;
        self.inner.read(dst)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.check_io_access()?;
        self.inner.write(src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        if let Some(location) = self.inner.vfs_location() {
            return super::fs::location_to_kstat_with_idmap(
                location,
                self.vfs_mount_idmap.as_deref(),
            );
        }
        self.inner.stat()
    }

    fn cachestat(&self, first_page: u64, last_page: u64) -> AxResult<axfs::CachedFileCacheStat> {
        self.inner.cachestat(first_page, last_page)
    }

    fn update_timestamps(
        &self,
        atime: Option<axfs_ng_vfs::Timestamp>,
        mtime: Option<axfs_ng_vfs::Timestamp>,
        ctime: axfs_ng_vfs::Timestamp,
    ) -> AxResult<()> {
        self.inner.update_timestamps(atime, mtime, ctime)
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        self.inner.path()
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        self.inner.ioctl(context, cmd, arg)
    }

    fn sync(&self, data_only: bool) -> AxResult<()> {
        FileDescription::sync(self, data_only)
    }

    fn prepare_mmap(&self, request: FileMmapRequest) -> AxResult<Option<PreparedFileMmap>> {
        self.check_io_access()?;
        self.inner.prepare_mmap(request)
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
    /// Retains the typed object independently of this descriptor handle.
    /// Subsystems that store an object reference (rather than an FD number)
    /// use this to make descriptor replacement/close race-free.
    pub(crate) fn clone_object(&self) -> Arc<T> {
        self.file.clone()
    }

    /// Returns the stable key shared by handles for the same Linux open file
    /// description, including handles reached through `dup` or table cloning.
    pub(crate) fn open_file_description_key(&self) -> u64 {
        self.description.id().get()
    }

    pub(crate) fn io_status_snapshot(&self) -> OfdIoStatus {
        self.description.io_status_snapshot()
    }

    pub(crate) fn directory_capability(&self) -> bool {
        self.description.directory_capability()
    }

    /// Returns the mount idmap pinned when this exact OFD was created.
    pub(crate) fn vfs_mount_idmap(&self) -> Option<Arc<crate::mounts::MountIdmap>> {
        self.description.vfs_mount_idmap()
    }

    pub(crate) fn vfs_mount_id(&self) -> Option<u64> {
        self.description.vfs_mount_id()
    }

    pub(crate) fn vfs_mount_topology(&self) -> Option<Arc<crate::mounts::MountTopology>> {
        self.description.vfs_mount_topology()
    }

    pub(crate) fn stat_with_open_mount(&self) -> AxResult<Kstat> {
        self.description.stat()
    }

    pub(crate) fn capture_io_operation_context(
        &self,
        security: VfsSecurityContext,
        fanotify_actor: FanotifyEventActor,
    ) -> IoOperationContext {
        self.description
            .capture_io_operation_context(security, fanotify_actor)
    }

    pub(crate) fn transition_status_flags(
        &self,
        update: impl FnOnce(OfdIoStatus) -> u32,
        apply_backend: impl FnOnce(OfdIoStatus, OfdIoStatus) -> AxResult<()>,
    ) -> AxResult<OfdIoStatus> {
        self.description
            .transition_status_flags(update, apply_backend)
    }

    pub fn status_flags(&self) -> u32 {
        self.io_status_snapshot().raw()
    }

    /// Applies the open-file-description gate shared by ordinary I/O.
    pub(crate) fn check_io_access(&self) -> AxResult<()> {
        self.description.check_io_access()
    }

    pub(crate) fn check_io_status(&self, status: OfdIoStatus) -> AxResult<()> {
        self.description.check_io_status(status)
    }

    pub(crate) fn is_path_only(&self) -> bool {
        self.description.is_path_only()
    }

    fn with_security_credential<R>(&self, f: impl FnOnce() -> R) -> R {
        let Some(task) = current_may_uninit() else {
            return f();
        };
        let Some(thread) = task.try_as_thread() else {
            return f();
        };
        // Always install this OFD's exact state, including `None`. Otherwise a
        // nested operation on an ordinary file could inherit the immutable
        // opener credential installed by an outer OPEN_CREDENTIAL operation.
        let previous =
            thread.replace_file_operation_credential(self.description.open_security_credential());
        let _guard = RestoreOnDrop::new(previous, |previous| {
            // `replace_file_operation_credential` releases its spin guard
            // before returning the displaced Arc, so its destructor cannot
            // run while the slot is locked.
            drop(thread.replace_file_operation_credential(previous));
        });
        f()
    }

    pub fn with_read_credentials<R>(&self, f: impl FnOnce() -> AxResult<R>) -> AxResult<R> {
        self.check_io_access()?;
        self.with_security_credential(f)
    }

    pub(crate) fn with_write_credentials<R>(
        &self,
        f: impl FnOnce(OfdIoStatus) -> AxResult<R>,
    ) -> AxResult<R> {
        let status = self.io_status_snapshot();
        self.with_write_credentials_for_status(status, || f(status))
    }

    /// Reuses an operation's already captured status while installing the
    /// same opener/security credentials for one backend chunk. Multi-chunk
    /// copy/splice loops call this without resampling mutable OFD state.
    pub(crate) fn with_write_credentials_for_status<R>(
        &self,
        status: OfdIoStatus,
        f: impl FnOnce() -> AxResult<R>,
    ) -> AxResult<R> {
        self.description.check_io_status(status)?;
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

impl<T: FileLike + 'static> FileHandle<T> {
    /// Erases only the typed inner Arc while preserving this exact OFD and its
    /// immutable description identity. Callers must use this instead of a
    /// second numeric-fd lookup that could observe a concurrent replacement.
    pub(crate) fn into_file_like(self) -> FileHandle<dyn FileLike> {
        let Self { description, file } = self;
        FileHandle { description, file }
    }
}

impl FileHandle<dyn FileLike> {
    #[cfg(test)]
    pub(crate) fn from_description_for_test(description: Arc<FileDescription>) -> Self {
        description.file_handle()
    }

    /// Downcasts the inner object without repeating fd-table lookup; the
    /// returned typed handle retains this exact description identity.
    pub(crate) fn downcast<T: FileLike + 'static>(&self) -> AxResult<FileHandle<T>> {
        let file = self
            .file
            .clone()
            .downcast_arc()
            .map_err(|_| AxError::InvalidInput)?;
        Ok(FileHandle {
            description: self.description.clone(),
            file,
        })
    }
}

impl<T: FileLike + ?Sized> FileHandle<T> {
    /// Cancellable synchronization through the retained open description.
    /// This preserves O_PATH and writeback-errseq behavior while passing the
    /// shared async token to the provider.
    pub(crate) fn sync_cancellable(
        &self,
        data_only: bool,
        operation: &crate::async_operation::AsyncOperation,
    ) -> AxResult<()> {
        self.description.sync_cancellable(data_only, operation)
    }

    /// Runs mmap preparation against this exact OFD and inner object. This is
    /// intentionally an inherent method so deref dispatch cannot bypass O_PATH
    /// admission on the retained description.
    pub(crate) fn prepare_mmap(
        &self,
        request: FileMmapRequest,
    ) -> AxResult<Option<PreparedFileMmap>> {
        self.description.check_io_access()?;
        self.file.prepare_mmap(request)
    }

    /// Applies an `O_NONBLOCK` update as one short OFD/backend transaction.
    ///
    /// `FIONBIO` uses this instead of mutating only the backend mirror.  A
    /// failed backend update leaves the authoritative status snapshot
    /// unchanged, while a successful update is immediately visible through
    /// every descriptor sharing this open file description.
    pub(crate) fn set_nonblocking_status(&self, nonblocking: bool) -> AxResult<OfdIoStatus> {
        self.transition_status_flags(
            |old| {
                if nonblocking {
                    old.raw() | O_NONBLOCK
                } else {
                    old.raw() & !O_NONBLOCK
                }
            },
            |old, new| {
                if old.nonblocking() != new.nonblocking() {
                    self.file.set_nonblocking(new.nonblocking())?;
                }
                Ok(())
            },
        )
    }

    /// Reads through this exact open file description.
    pub(crate) fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.check_io_access()?;
        self.file.read(dst)
    }

    /// Writes through this exact open file description.
    pub(crate) fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.check_io_access()?;
        self.file.write(src)
    }

    /// Poll semantics differ from select for Linux `O_PATH` descriptions.
    pub(crate) fn poll_events_for_poll(&self) -> IoEvents {
        if self.is_path_only() {
            IoEvents::INVALID
        } else {
            self.file.poll()
        }
    }
}

#[derive(Clone)]
pub struct FileDescriptor {
    pub description: Arc<FileDescription>,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::borrow::Cow;
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::Context,
    };

    use axfs::{FileBackend, FileFlags};
    use axfs_ng_vfs::{Mountpoint, NodePermission, NodeType};
    use axpoll::{IoEvents, Pollable};
    use linux_raw_sys::general::{O_PATH, O_WRONLY};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    struct SyncProbe {
        syncs: AtomicUsize,
        errors: Arc<WritebackErrorState>,
        fail_next_sync: AtomicBool,
    }

    fn sync_probe() -> Arc<SyncProbe> {
        Arc::new(SyncProbe {
            syncs: AtomicUsize::new(0),
            errors: Arc::new(WritebackErrorState::default()),
            fail_next_sync: AtomicBool::new(false),
        })
    }

    impl Pollable for SyncProbe {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for SyncProbe {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
            Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(b"sync-probe")))
        }

        fn sync(&self, _data_only: bool) -> AxResult<()> {
            self.syncs.fetch_add(1, Ordering::AcqRel);
            if self.fail_next_sync.swap(false, Ordering::AcqRel) {
                Err(AxError::Io)
            } else {
                Ok(())
            }
        }

        fn writeback_error_state(&self) -> AxResult<Arc<WritebackErrorState>> {
            Ok(self.errors.clone())
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    struct FinalCloseProbe {
        closes: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
        observed_live: Arc<AtomicBool>,
    }

    impl Drop for FinalCloseProbe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl Pollable for FinalCloseProbe {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for FinalCloseProbe {
        fn final_close(&self) {
            self.observed_live
                .store(!self.dropped.load(Ordering::Acquire), Ordering::Release);
            self.closes.fetch_add(1, Ordering::AcqRel);
        }

        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
            Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
                b"final-close-probe",
            )))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    #[test]
    fn final_close_is_once_per_ofd_and_precedes_inner_drop() {
        let closes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let observed_live = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(FinalCloseProbe {
            closes: closes.clone(),
            dropped: dropped.clone(),
            observed_live: observed_live.clone(),
        });

        let first = FileDescription::new(inner.clone()).unwrap();
        let duplicated = first.clone();
        let independent = FileDescription::new(inner.clone()).unwrap();
        first.mark_open_committed();
        independent.mark_open_committed();

        let first_publication = first.begin_descriptor_publication().unwrap();
        first_publication.commit();
        let duplicated_publication = first.begin_descriptor_publication().unwrap();
        duplicated_publication.commit();
        // An abandoned fd reservation must not create another close or keep
        // the committed duplicate alive.
        drop(first.begin_descriptor_publication().unwrap());

        first.descriptor_closed();
        drop(first);
        assert_eq!(closes.load(Ordering::Acquire), 0);
        duplicated.descriptor_closed();
        drop(duplicated);
        assert_eq!(closes.load(Ordering::Acquire), 1);
        assert!(observed_live.load(Ordering::Acquire));
        assert!(!dropped.load(Ordering::Acquire));

        drop(independent);
        assert_eq!(closes.load(Ordering::Acquire), 2);
        assert!(observed_live.load(Ordering::Acquire));
        drop(inner);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn nested_file_descriptions_do_not_forward_final_close() {
        let closes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let observed_live = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(FinalCloseProbe {
            closes: closes.clone(),
            dropped,
            observed_live,
        });
        let nested = FileDescription::new(inner).unwrap();
        nested.mark_open_committed();
        let wrapper_inner: Arc<dyn FileLike> = nested.clone();
        let wrapper = FileDescription::new(wrapper_inner).unwrap();
        wrapper.mark_open_committed();

        drop(wrapper);
        assert_eq!(closes.load(Ordering::Acquire), 0);
        drop(nested);
        assert_eq!(closes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn sync_uses_retained_description_not_a_replacement() {
        let original = sync_probe();
        let replacement = sync_probe();
        let retained = FileDescription::new(original.clone()).unwrap();
        let _reused_number_now_names = FileDescription::new(replacement.clone()).unwrap();

        retained.sync(false).unwrap();
        assert_eq!(original.syncs.load(Ordering::Acquire), 1);
        assert_eq!(replacement.syncs.load(Ordering::Acquire), 0);
    }

    #[test]
    fn opath_sync_is_einval_without_invoking_backend() {
        let probe = sync_probe();
        let description = FileDescription::new_with_flags(probe.clone(), O_PATH).unwrap();

        assert_eq!(description.sync(false), Err(AxError::InvalidInput));
        assert_eq!(probe.syncs.load(Ordering::Acquire), 0);
    }

    #[test]
    fn syncfs_uses_filesystem_anchor_for_opath_and_has_an_ofd_cursor() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let location = mount
            .root_location()
            .create(
                "syncfs-anchor",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(location),
            FileFlags::READ,
        )));
        let first = FileDescription::new_with_flags(file.clone(), O_PATH).unwrap();
        let second = FileDescription::new(file).unwrap();

        // O_PATH carries a VFS anchor even though ordinary fsync rejects it.
        assert_eq!(first.sync(false), Err(AxError::InvalidInput));
        assert_eq!(first.sync_filesystem(), Ok(()));

        let filesystem = first.inner.syncfs_filesystem().unwrap();
        filesystem
            .writeback_error_state()
            .publish(axfs_ng_vfs::VfsError::Io);
        assert_eq!(first.sync_filesystem(), Err(AxError::Io));
        assert_eq!(first.sync_filesystem(), Ok(()));
        // Independently opened OFDs own independent superblock errseq cursors.
        assert_eq!(second.sync_filesystem(), Err(AxError::Io));
    }

    #[test]
    fn sync_writeback_error_is_reported_once_per_ofd_cursor() {
        let probe = sync_probe();
        let description = FileDescription::new(probe.clone()).unwrap();
        let duplicated_descriptor = description.clone();
        let independently_opened = FileDescription::new(probe).unwrap();
        description.publish_sync_error(AxError::Io);

        assert_eq!(description.sync(false), Err(AxError::Io));
        assert_eq!(duplicated_descriptor.sync(false), Ok(()));

        // A new open has an independent cursor over the same backend source,
        // so it sees the error published after it was opened once.
        assert_eq!(independently_opened.sync(false), Err(AxError::Io));
        assert_eq!(independently_opened.sync(false), Ok(()));
    }

    #[test]
    fn direct_sync_failure_is_not_published_as_writeback_error() {
        let probe = sync_probe();
        let first = FileDescription::new(probe.clone()).unwrap();
        let independently_opened = FileDescription::new(probe.clone()).unwrap();
        probe.fail_next_sync.store(true, Ordering::Release);

        assert_eq!(first.sync(false), Err(AxError::Io));
        assert_eq!(independently_opened.sync(false), Ok(()));
    }

    #[test]
    fn errseq_sample_and_advance_follow_linux_seen_semantics() {
        let probe = sync_probe();
        let before_publish = FileDescription::new(probe.clone()).unwrap();
        before_publish.publish_sync_error(AxError::Io);

        // The error is not globally SEEN yet, so a later open samples zero
        // and reports it too.
        let opened_before_seen = FileDescription::new(probe.clone()).unwrap();
        assert_eq!(opened_before_seen.sync(false), Err(AxError::Io));

        // Advancing any cursor marks this error globally seen. New opens now
        // start at the current sequence, whereas an already-open cursor still
        // reports its unseen event once.
        let opened_after_seen = FileDescription::new(probe.clone()).unwrap();
        assert_eq!(opened_after_seen.sync(false), Ok(()));
        assert_eq!(before_publish.sync(false), Err(AxError::Io));

        before_publish.publish_sync_error(AxError::Io);
        assert_eq!(opened_after_seen.sync(false), Err(AxError::Io));
    }

    #[test]
    fn hardlink_alias_and_relookup_share_the_inode_writeback_error_source() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let root = mount.root_location();
        let source = root
            .create(
                "writeback-source",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let alias = root.link("writeback-alias", &source).unwrap();
        let rebuilt_alias = root.lookup_no_follow_in_mount("writeback-alias").unwrap();
        let source_file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(source),
            FileFlags::READ,
        )));
        let alias_file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(alias),
            FileFlags::READ,
        )));
        let rebuilt_file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(rebuilt_alias),
            FileFlags::READ,
        )));
        let source_description = FileDescription::new(source_file).unwrap();
        let alias_description = FileDescription::new(alias_file).unwrap();
        let rebuilt_description = FileDescription::new(rebuilt_file).unwrap();

        source_description.publish_sync_error(AxError::Io);
        assert_eq!(alias_description.sync(false), Err(AxError::Io));
        assert_eq!(rebuilt_description.sync(false), Err(AxError::Io));
    }

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    #[test]
    fn deferred_file_lease_reference_saturation_is_non_panicking_and_fail_closed() {
        let ordinary = AtomicUsize::new(1);
        retain_deferred_file_lease(&ordinary);
        assert_eq!(ordinary.load(Ordering::Relaxed), 2);
        assert!(!release_deferred_file_lease(&ordinary));
        assert!(release_deferred_file_lease(&ordinary));

        let saturated = AtomicUsize::new(usize::MAX - 1);
        retain_deferred_file_lease(&saturated);
        assert_eq!(saturated.load(Ordering::Relaxed), usize::MAX);
        retain_deferred_file_lease(&saturated);
        assert_eq!(saturated.load(Ordering::Relaxed), usize::MAX);
        assert!(!release_deferred_file_lease(&saturated));
        assert_eq!(saturated.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn status_snapshot_is_authoritative_without_an_axfs_append_mirror() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "mutable-append",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(loc),
            FileFlags::WRITE,
        )));
        let description = FileDescription::new_with_flags(file.clone(), O_WRONLY).unwrap();

        let initial = description.io_status_snapshot();
        assert_eq!(initial.raw(), O_WRONLY);
        assert!(!initial.append());
        assert!(!initial.nonblocking());
        assert!(!file.inner().flags().contains(FileFlags::APPEND));

        let updated = description
            .transition_status_flags(
                |old| old.raw() | O_APPEND | O_NONBLOCK,
                |_old, new| file.set_nonblocking(new.nonblocking()),
            )
            .unwrap();
        assert!(updated.append());
        assert!(updated.nonblocking());
        assert!(file.nonblocking());
        assert!(!file.inner().flags().contains(FileFlags::APPEND));

        let cleared = description
            .transition_status_flags(
                |old| old.raw() & !(O_APPEND | O_NONBLOCK),
                |_old, new| file.set_nonblocking(new.nonblocking()),
            )
            .unwrap();
        assert!(!cleared.append());
        assert!(!cleared.nonblocking());
        assert!(!file.nonblocking());
        assert!(!file.inner().flags().contains(FileFlags::APPEND));

        let failed = description
            .transition_status_flags(|old| old.raw() | O_NONBLOCK, |_old, _new| Err(AxError::Io));
        assert!(matches!(failed, Err(AxError::Io)));
        assert_eq!(description.io_status_snapshot(), cleared);
        assert!(!file.nonblocking());

        let read_loc = mount
            .root_location()
            .create(
                "read-only-append",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let read_file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(read_loc),
            FileFlags::READ,
        )));
        let read_description =
            FileDescription::new_with_flags(read_file.clone(), O_APPEND).unwrap();
        assert!(read_description.io_status_snapshot().append());
        assert!(!read_file.inner().flags().contains(FileFlags::APPEND));
        assert!(!read_file.inner().flags().contains(FileFlags::WRITE));
        assert!(matches!(
            read_file.inner().access(FileFlags::APPEND),
            Err(axfs_ng_vfs::VfsError::BadFileDescriptor)
        ));
    }

    #[test]
    fn fionbio_transition_keeps_regular_backend_and_f_getfl_snapshot_in_sync() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "fionbio-regular",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(loc),
            FileFlags::WRITE,
        )));
        let description =
            FileDescription::new_with_flags(file.clone(), O_WRONLY | O_APPEND).unwrap();
        let handle = FileHandle {
            description: description.clone(),
            file: file.clone(),
        };
        let duplicate = handle.clone();

        let enabled = handle.set_nonblocking_status(true).unwrap();
        assert_eq!(enabled.raw(), O_WRONLY | O_APPEND | O_NONBLOCK);
        // F_GETFL reads this same authoritative OFD snapshot, including
        // updates made through another descriptor for the description.
        assert_eq!(duplicate.status_flags(), enabled.raw());
        assert_eq!(description.io_status_snapshot(), enabled);
        assert!(file.nonblocking());

        let disabled = duplicate.set_nonblocking_status(false).unwrap();
        assert_eq!(disabled.raw(), O_WRONLY | O_APPEND);
        assert_eq!(handle.status_flags(), disabled.raw());
        assert_eq!(description.io_status_snapshot(), disabled);
        assert!(!file.nonblocking());
    }

    struct RejectingNonblockingFile {
        nonblocking: AtomicBool,
        reject_next: AtomicBool,
    }

    impl Pollable for RejectingNonblockingFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for RejectingNonblockingFile {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
            Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
                b"rejecting-nonblocking",
            )))
        }

        fn nonblocking(&self) -> bool {
            self.nonblocking.load(Ordering::Acquire)
        }

        fn set_nonblocking(&self, nonblocking: bool) -> AxResult {
            if self.reject_next.swap(false, Ordering::AcqRel) {
                return Err(AxError::Io);
            }
            self.nonblocking.store(nonblocking, Ordering::Release);
            Ok(())
        }
    }

    #[test]
    fn fionbio_backend_failure_does_not_publish_a_new_ofd_snapshot() {
        let file = Arc::new(RejectingNonblockingFile {
            nonblocking: AtomicBool::new(false),
            reject_next: AtomicBool::new(true),
        });
        let description =
            FileDescription::new_with_flags(file.clone(), O_WRONLY | O_APPEND).unwrap();
        let handle = FileHandle {
            description: description.clone(),
            file: file.clone(),
        };
        let before = description.io_status_snapshot();

        assert_eq!(handle.set_nonblocking_status(true), Err(AxError::Io));
        assert_eq!(description.io_status_snapshot(), before);
        assert_eq!(handle.status_flags(), before.raw());
        assert!(!file.nonblocking());

        let committed = handle.set_nonblocking_status(true).unwrap();
        assert!(committed.nonblocking());
        assert_eq!(description.io_status_snapshot(), committed);
        assert!(file.nonblocking());
    }

    #[test]
    fn opath_description_does_not_prepare_close_notification() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let path_loc = mount
            .root_location()
            .create(
                "path-only-no-notify",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let path_file = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(path_loc),
            FileFlags::PATH,
        )));
        let description = FileDescription::new_with_flags(path_file, O_PATH).unwrap();
        assert!(description.notification_work.is_none());
    }

    #[test]
    fn explicit_vfs_open_credential_is_retained_by_exact_identity() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "credential-bound-open",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file: Arc<dyn FileLike> = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(loc),
            FileFlags::READ,
        )));
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace).unwrap();
        let description = FileDescription::new_inner(
            file,
            0,
            false,
            None,
            None,
            None,
            Some(credential.clone()),
            false,
        )
        .unwrap();

        let retained = description.vfs_open_credential().unwrap();
        assert!(Arc::ptr_eq(&retained, &credential));
        assert!(description.open_security_credential().is_none());
    }

    #[test]
    fn io_operation_context_owns_open_identity_without_task_state() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<IoOperationContext>();

        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "context-bound-open",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file: Arc<dyn FileLike> = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(loc),
            FileFlags::READ,
        )));
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace).unwrap();
        let description = FileDescription::new_inner(
            file,
            0,
            false,
            None,
            None,
            None,
            Some(credential.clone()),
            false,
        )
        .unwrap();
        let context = description.capture_io_operation_context(
            VfsSecurityContext::new(credential.clone()),
            FanotifyEventActor::default(),
        );
        let retained_description = context.description.clone();
        let replacement_namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let replacement_credential = Cred::try_root(replacement_namespace).unwrap();

        assert_eq!(context.status().raw(), 0);
        assert!(Arc::ptr_eq(context.security().actor_arc(), &credential));
        assert!(Arc::ptr_eq(
            &context.vfs_open_credential().unwrap(),
            &credential
        ));
        assert!(context.open_security_credential().is_none());
        assert_eq!(context.open_credentials(), description.open_credentials());
        assert_eq!(context.fanotify_actor(), FanotifyEventActor::default());
        // Changing the submitter's current credential must not alter the
        // admitted actor, and dropping the submitter's last local OFD handle
        // must not end the context's exact identity.
        assert!(!Arc::ptr_eq(
            context.security().actor_arc(),
            &replacement_credential
        ));
        drop(description);
        assert!(context.validate_for(&retained_description).is_ok());
    }

    #[test]
    fn io_operation_context_rejects_wrong_generation_and_path_only_ofd() {
        let fs = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&fs);
        let loc = mount
            .root_location()
            .create(
                "context-generation-bound",
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        let file: Arc<dyn FileLike> = Arc::new(File::new(axfs::File::new(
            FileBackend::Direct(loc),
            FileFlags::READ,
        )));
        let namespace = crate::task::UserNamespace::try_new_root().unwrap();
        let credential = Cred::try_root(namespace).unwrap();
        let first = FileDescription::new_inner(
            file.clone(),
            0,
            false,
            None,
            None,
            None,
            Some(credential.clone()),
            false,
        )
        .unwrap();
        let second = FileDescription::new_inner(
            file.clone(),
            0,
            false,
            None,
            None,
            None,
            Some(credential.clone()),
            false,
        )
        .unwrap();
        let context = first.capture_io_operation_context(
            VfsSecurityContext::new(credential.clone()),
            FanotifyEventActor::default(),
        );
        assert_eq!(
            context.validate_for(&second),
            Err(AxError::BadFileDescriptor)
        );

        let path_only = FileDescription::new_with_flags(file, O_PATH).unwrap();
        let path_context = path_only.capture_io_operation_context(
            VfsSecurityContext::new(credential),
            FanotifyEventActor::default(),
        );
        assert_eq!(
            path_context.validate_for(&path_only),
            Err(AxError::BadFileDescriptor)
        );
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
    fn cleanup_raw_box_is_unlinked_once_before_its_resource_is_dropped() {
        struct DropProbe(Arc<AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let incoming = AtomicPtr::new(ptr::null_mut());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut work = DescriptionCleanupWork::try_new(901).unwrap();
        work.resource = Some(Box::new(DropProbe(drops.clone())));

        // This is the real Box::into_raw publication path, but with a local
        // incoming head so parallel host tests cannot consume this node.
        publish_description_cleanup_to(&incoming, work);
        assert!(!incoming.load(Ordering::Acquire).is_null());
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        let pending = AtomicPtr::new(ptr::null_mut());
        assert!(drain_deferred_description_resource_only_from_for_test(
            &incoming, &pending
        ));
        assert!(incoming.load(Ordering::Acquire).is_null());
        assert!(pending.load(Ordering::Acquire).is_null());

        // The production pop path restored unique Box ownership before this
        // test adapter released its typed resource.
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(!drain_deferred_description_resource_only_from_for_test(
            &incoming, &pending
        ));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
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
